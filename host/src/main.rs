// Release builds are a windowed app (dashboard + tray icon) with no console window; the log
// goes to %LOCALAPPDATA%\XDisplay\logs. Debug builds keep the console for development.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

mod capture;
#[cfg(windows)]
mod driver_setup;
mod input;
mod log_writer;
mod shared;
mod stats;
mod transport;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
use capture::{detect_display, start_wgc_capture};
use input::PointerInjector;
use shared::HostShared;
use transport::{spawn_discovery_server, TransportServer, DEFAULT_PORT};

const STATS_INTERVAL: Duration = Duration::from_secs(5);

pub use shared::attach_to_default_desktop;

#[cfg(windows)]
unsafe fn acquire_single_instance_mutex() -> Option<windows::Win32::Foundation::HANDLE> {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
    use windows::core::w;

    match CreateMutexW(None, true, w!("Global\\XDisplayHostSingleInstanceMutex")) {
        Ok(handle) => {
            if GetLastError() == ERROR_ALREADY_EXISTS {
                if let Ok(hwnd) = FindWindowW(w!("XDisplayDashboardClass"), None) {
                    if !hwnd.0.is_null() {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                        let _ = SetForegroundWindow(hwnd);
                    }
                }
                None
            } else {
                Some(handle)
            }
        }
        Err(_) => None,
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        log_writer::report_fatal(&format!("XDisplay could not start: {e:#}"));
        // Let the log thread write the error out.
        std::thread::sleep(Duration::from_millis(200));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    #[cfg(windows)]
    {
        let mut args = std::env::args().skip(1);
        match args.next().as_deref() {
            Some(mode @ (driver_setup::INSTALL_ARG | driver_setup::UNINSTALL_ARG | driver_setup::QUERY_ARG)) => {
                std::process::exit(driver_setup::run_cli(mode, args.next()));
            }
            Some(capture::virtual_display::GUARD_ARG) => {
                if let Some(pid) = args.next().and_then(|p| p.parse().ok()) {
                    capture::virtual_display::run_guard(pid);
                }
                return Ok(());
            }
            Some(capture::virtual_display::LOGON_CLEANUP_ARG) => {
                capture::virtual_display::run_logon_cleanup();
                return Ok(());
            }
            Some(capture::virtual_display::WATCHDOG_ARG) => {
                capture::virtual_display::run_watchdog();
                return Ok(());
            }
            Some(capture::virtual_display::STOP_WATCHDOG_ARG) => {
                capture::virtual_display::stop_watchdog();
                return Ok(());
            }
            Some(capture::virtual_display::REMOVE_WATCHDOG_ARG) => {
                capture::virtual_display::remove_watchdog();
                return Ok(());
            }
            _ => {}
        }
    }

    #[cfg(windows)]
    attach_to_default_desktop();

    // Before logging starts: a second launch only brings the running dashboard forward, and must
    // not rotate the running host's log file.
    #[cfg(windows)]
    let _instance_lock = match unsafe { acquire_single_instance_mutex() } {
        Some(handle) => handle,
        None => return Ok(()),
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "xdisplay_host=debug,info".into()),
        )
        .with_writer(log_writer::NonBlockingStdout::new())
        .with_ansi(false)
        .init();


    // Monitor rects, capture sizes and injected pointer coordinates must all be physical pixels.
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            warn!("Could not enable per-monitor DPI awareness: {}", e);
        }
    }

    info!("=======================================================");
    info!("   XDisplay Host Service (Rust Ultra-Low Latency)");
    info!("   Screen Extension & Stylus Digitizer Server");
    info!("=======================================================");

    let disp = detect_display()?;
    info!(
        "Secondary display at ({}, {}) [{}x{} @ {} Hz]",
        disp.left, disp.top, disp.width, disp.height, disp.refresh_hz
    );

    // No tablet yet: take its monitor off the desktop so the cursor and new windows stay away.
    #[cfg(windows)]
    {
        capture::virtual_display::init();
    }

    let mut injector = PointerInjector::new(disp.width, disp.height)?;
    injector.set_monitor_rect(disp.left, disp.top, disp.width, disp.height);
    let shared = Arc::new(HostShared::new(injector, disp));
    // New tablets on USB are admitted only after the user says yes at the PC.
    #[cfg(windows)]
    shared.pairing.set_usb_approver(Arc::new(gui::usb_approval_prompt));

    // Bind before anything advertises the port (adb reverse, dashboard, Wi-Fi beacons).
    let server = TransportServer::bind(DEFAULT_PORT, shared.clone()).await?;
    shared.tcp_port.store(server.port(), Ordering::Relaxed);
    if server.port() != DEFAULT_PORT {
        warn!("Listening on port {} instead of {DEFAULT_PORT}; USB is remapped with adb reverse", server.port());
    }

    #[cfg(windows)]
    {
        transport::spawn_aoa_manager(shared.clone());
        gui::spawn_adb_watcher(shared.clone());
        // Autostart passes --minimized: start in the tray instead of opening the dashboard.
        let minimized = std::env::args().any(|a| a == "--minimized");
        gui::spawn_gui_dashboard(shared.clone(), !minimized);
    }

    // Two slots: one frame being written to the socket, one waiting. Anything more is latency.
    let (video_tx, video_rx) = mpsc::channel::<capture::VideoPacket>(2);

    info!("Starting Windows Graphics Capture (WGC) for extended secondary screen...");
    start_wgc_capture(video_tx, shared.clone())?;

    let stats_shared = shared.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(STATS_INTERVAL);
        tick.tick().await;
        loop {
            tick.tick().await;
            stats_shared.stats.log_and_reset(STATS_INTERVAL);
        }
    });

    spawn_discovery_server(server.port(), shared.clone());

    let server_handle = tokio::spawn(async move {
        if let Err(e) = server.run(video_rx).await {
            error!("Transport server error: {}", e);
        }
    });

    tokio::select! {
        _ = server_handle => {
            info!("Transport server stopped");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown signal received. Exiting XDisplay host.");
        }
        _ = console_closing() => {
            info!("Console closing. Exiting XDisplay host.");
        }
    }

    shared.is_running.store(false, Ordering::SeqCst);
    #[cfg(windows)]
    capture::virtual_display::detach();
    Ok(())
}


/// Resolves when the console window is closed or the session ends, so the tablet display is still detached.
#[cfg(windows)]
async fn console_closing() {
    use tokio::signal::windows;
    let (Ok(mut close), Ok(mut brk), Ok(mut shutdown)) =
        (windows::ctrl_close(), windows::ctrl_break(), windows::ctrl_shutdown())
    else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = close.recv() => {}
        _ = brk.recv() => {}
        _ = shutdown.recv() => {}
    }
}

#[cfg(not(windows))]
async fn console_closing() {
    std::future::pending::<()>().await
}
