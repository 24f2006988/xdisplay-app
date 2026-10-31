use std::io::{BufRead, BufReader};
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use tracing::{info, warn};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect,
    RoundRect, SelectObject, SetBkMode, SetTextColor,
    DT_CENTER, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, PAINTSTRUCT,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, IsWindowVisible,
    DispatchMessageW, GetClientRect, GetCursorPos, GetMessageW, InsertMenuW, KillTimer, LoadIconW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, ShowWindow, TrackPopupMenu,
    RegisterWindowMessageW, TranslateMessage, HICON, HMENU, IDI_APPLICATION, MF_BYPOSITION, MF_GRAYED, MF_SEPARATOR, MF_STRING,
    MSG, SW_HIDE, SW_RESTORE, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RIGHTBUTTON,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_ERASEBKGND, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_TIMER, WM_USER, WNDCLASSW,
    MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO, WS_CAPTION, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
    WS_VISIBLE,
};

use crate::shared::HostShared;
use crate::transport::lan_addresses;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const WM_TRAYICON: u32 = WM_USER + 100;
const TIMER_TELEMETRY: usize = 1;
/// Resource id of the application icon embedded by build.rs.
const APP_ICON_ID: usize = 1;

/// "TaskbarCreated": Explorer broadcasts it after it restarts, when every tray icon must be added again.
static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

unsafe fn app_icon() -> HICON {
    let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
        .map(|h| windows::Win32::Foundation::HINSTANCE(h.0))
        .unwrap_or_default();
    LoadIconW(hinstance, PCWSTR(APP_ICON_ID as *const u16))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))
        .unwrap_or_default()
}

unsafe fn tray_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = app_icon();
    let tip_wide = to_wide_null("XDisplay - running");
    let copy_len = tip_wide.len().min(nid.szTip.len());
    nid.szTip[..copy_len].copy_from_slice(&tip_wide[..copy_len]);
    nid
}

/// Interactive stylus test area; the mouse handler and the painter must agree on it.
const TEST_BOX: RECT = RECT { left: 24, top: 520, right: 720, bottom: 600 };
const PAIRING_CARD: RECT = RECT { left: 24, top: 392, right: 720, bottom: 472 };

// Menu IDs
const ID_TRAY_DASHBOARD: usize = 1000;
const ID_TRAY_STATUS: usize = 1001;
const ID_TRAY_ADB_REVERSE: usize = 1002;
const ID_TRAY_EXIT: usize = 1003;

// Button Action Identifiers
#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionId {
    Fps60,
    Fps90,
    Fps120,
    BitrateAuto,
    Bitrate15,
    Bitrate25,
    Bitrate35,
    Bitrate50,
    RunAdbReverse,
    OpenDisplaySettings,
    RequestKeyframe,
    MinimizeToTray,
    NewPin,
    ForgetTablets,
    #[allow(dead_code)]
    ExitApp,
}

struct ButtonDef {
    id: ActionId,
    rect: RECT,
    label: &'static str,
    active: bool,
}

fn to_wide_null(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

#[link(name = "user32")]
extern "system" {
    /// Exported by user32 since Windows XP (undocumented but stable): a MessageBox that
    /// closes itself after `milliseconds`, returning 32000.
    fn MessageBoxTimeoutW(hwnd: HWND, text: PCWSTR, caption: PCWSTR, style: u32, language: u16, milliseconds: u32) -> i32;
}

/// Asks the user whether a tablet that just appeared on USB may use this PC. Blocks until the
/// user answers or `USB_APPROVAL_TIMEOUT` passes (then: no). "No" is the default button, so a
/// stray Enter does not admit anything.
pub fn usb_approval_prompt(device_name: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let text = to_wide_null(&format!(
        "A tablet named \"{device_name}\" connected over USB and wants to use this PC: \
         see its screen and control the keyboard, mouse and pen.\n\n\
         Allow it?\n\n\
         Choose Yes only if you just connected your own tablet. Any app installed on a tablet \
         can use its USB link. You will only be asked once per tablet; \"Forget tablets\" on \
         the dashboard revokes it.\n\n\
         (No answer within {} seconds counts as No.)",
        crate::transport::pairing::USB_APPROVAL_TIMEOUT.as_secs()
    ));
    let caption = to_wide_null("XDisplay: allow new USB tablet?");
    let style = (MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND).0;
    let ms = crate::transport::pairing::USB_APPROVAL_TIMEOUT.as_millis() as u32;
    let answer = unsafe {
        MessageBoxTimeoutW(HWND::default(), PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), style, 0, ms)
    };
    answer == IDYES.0
}

fn find_adb_executable() -> Option<std::path::PathBuf> {
    // 1. App-relative paths (portable / installed bundle)
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            for sub in [r"tools\adb.exe", r"platform-tools\adb.exe", "adb.exe"] {
                let candidate = exe_dir.join(sub);
                if candidate.exists() {
                    return Some(candidate);
                }
            }
        }
    }
    // 2. Android SDK standard locations
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let sdk_adb = std::path::Path::new(&local_app_data).join(r"Android\Sdk\platform-tools\adb.exe");
        if sdk_adb.exists() {
            return Some(sdk_adb);
        }
        // 3. WinGet package directory (e.g. Google.PlatformTools)
        let winget_dir = std::path::Path::new(&local_app_data).join(r"Microsoft\WinGet\Packages");
        if winget_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&winget_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let candidate = path.join(r"platform-tools\adb.exe");
                        if candidate.exists() {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
    }
    if let Ok(android_home) = std::env::var("ANDROID_HOME") {
        let sdk_adb = std::path::Path::new(&android_home).join(r"platform-tools\adb.exe");
        if sdk_adb.exists() {
            return Some(sdk_adb);
        }
    }
    if let Ok(android_root) = std::env::var("ANDROID_SDK_ROOT") {
        let sdk_adb = std::path::Path::new(&android_root).join(r"platform-tools\adb.exe");
        if sdk_adb.exists() {
            return Some(sdk_adb);
        }
    }
    None
}

fn adb_command() -> Command {
    if let Some(path) = find_adb_executable() {
        Command::new(path)
    } else {
        Command::new("adb")
    }
}

/// Maps the tablet's `127.0.0.1:59100` to the port the host really listens on.
pub fn run_adb_reverse(host_port: u16) -> bool {
    match adb_command()
        .args(["reverse", "tcp:59100", &format!("tcp:{host_port}")])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(out) if out.status.success() => {
            info!("ADB port forwarding verified: reverse tcp:59100 tcp:{host_port} OK");
            true
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!("ADB reverse command returned non-zero: {}", stderr.trim());
            false
        }
        Err(e) => {
            warn!("Failed to execute 'adb reverse': {}", e);
            false
        }
    }
}

pub fn spawn_adb_watcher(shared: Arc<HostShared>) {
    thread::Builder::new()
        .name("xdisplay-adb-device-tracker".into())
        .spawn(move || {
            // `adb track-devices` is an event stream maintained by the ADB server.  In
            // contrast to repeatedly spawning `adb devices`, it is idle while the USB
            // topology is unchanged and wakes this thread only for an attach/detach.
            info!("ADB device tracker started (event-driven)");
            let mut last_devices = Vec::new();
            // A restarted host loses the reverse mapping but sees no device-set change, so
            // map once up front instead of waiting for an attach event.
            run_adb_reverse(shared.tcp_port.load(Ordering::Relaxed));
            while shared.is_running.load(Ordering::Relaxed) {
                let mut command = adb_command();
                let Ok(mut child) = command
                    .arg("track-devices")
                    .creation_flags(CREATE_NO_WINDOW)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    // ADB is optional; retry infrequently if it is installed after the host.
                    thread::sleep(Duration::from_secs(10));
                    continue;
                };

                let Some(stdout) = child.stdout.take() else {
                    let _ = child.kill();
                    thread::sleep(Duration::from_secs(10));
                    continue;
                };
                let mut current = Vec::new();
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if line.is_empty() {
                        if !current.is_empty() && current != last_devices {
                            info!("Android device set changed: {:?}; configuring ADB reverse", current);
                            run_adb_reverse(shared.tcp_port.load(Ordering::Relaxed));
                        }
                        last_devices = std::mem::take(&mut current);
                    } else {
                        let parts: Vec<_> = line.split_whitespace().collect();
                        if parts.len() >= 2 && parts[1] == "device" {
                            current.push(parts[0].to_owned());
                        }
                    }
                }
                let _ = child.kill();
                if shared.is_running.load(Ordering::Relaxed) {
                    // Only recover from an exited ADB server; this is not a device poll.
                    thread::sleep(Duration::from_secs(2));
                }
            }
            info!("ADB device tracker stopped");
        })
        .expect("Failed to spawn ADB device tracker thread");
}

// Global reference for Win32 message procedure
static GLOBAL_SHARED: OnceLock<Arc<HostShared>> = OnceLock::new();
static GLOBAL_STATUS_MSG: Mutex<Option<String>> = Mutex::new(None);
static TEST_STYLUS_POS: Mutex<Option<(i32, i32, f32)>> = Mutex::new(None);

fn set_status(msg: impl Into<String>) {
    *GLOBAL_STATUS_MSG.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.into());
}

pub fn spawn_gui_dashboard(shared: Arc<HostShared>, show_window: bool) {
    let _ = GLOBAL_SHARED.set(shared.clone());
    set_status("Ready for tablet connection over USB or Wi-Fi");

    thread::Builder::new()
        .name("xdisplay-gui-dashboard".into())
        .spawn(move || {
            crate::shared::attach_to_default_desktop();
            unsafe {
                let hinstance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                    .map(|h| windows::Win32::Foundation::HINSTANCE(h.0))
                    .unwrap_or_default();
                let class_name = to_wide_null("XDisplayDashboardClass");
                let wnd_class = WNDCLASSW {
                    lpfnWndProc: Some(dashboard_window_proc),
                    hInstance: hinstance,
                    hIcon: app_icon(),
                    lpszClassName: PCWSTR(class_name.as_ptr()),
                    ..Default::default()
                };

                let _ = RegisterClassW(&wnd_class);

                // Window dimensions: 760 x 690 px
                let win_w = 760;
                let win_h = 690;
                let title = to_wide_null("XDisplay");
                let visible = if show_window { WS_VISIBLE } else { WINDOW_STYLE(0) };

                let hwnd = match CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    PCWSTR(class_name.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | visible,
                    150,
                    120,
                    win_w,
                    win_h,
                    HWND(std::ptr::null_mut()),
                    HMENU(std::ptr::null_mut()),
                    hinstance,
                    None,
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        warn!("Failed to create XDisplay dashboard window: {}", e);
                        return;
                    }
                };

                // Enable Windows 10/11 immersive dark mode title bar
                let dark_mode: i32 = 1;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    DWMWINDOWATTRIBUTE(20), // DWMWA_USE_IMMERSIVE_DARK_MODE
                    &dark_mode as *const _ as *const _,
                    std::mem::size_of::<i32>() as u32,
                );

                // System tray icon (added again whenever Explorer restarts).
                let taskbar_created = to_wide_null("TaskbarCreated");
                TASKBAR_CREATED.store(RegisterWindowMessageW(PCWSTR(taskbar_created.as_ptr())), Ordering::Relaxed);
                let nid = tray_icon_data(hwnd);
                let _ = Shell_NotifyIconW(NIM_ADD, &nid);
                info!("XDisplay dashboard & tray icon initialized");

                // The dashboard is diagnostic UI, not a render surface. Refresh it at 1 Hz and
                // suppress invalidations while it is minimized to the tray.
                SetTimer(hwnd, TIMER_TELEMETRY, 1000, None);

                let mut msg = MSG::default();
                while shared.is_running.load(Ordering::Relaxed)
                    && GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0).as_bool()
                {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                let _ = KillTimer(hwnd, TIMER_TELEMETRY);
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                let _ = DestroyWindow(hwnd);
                info!("XDisplay dashboard closed");
            }
        })
        .expect("Failed to spawn GUI dashboard thread");
}

unsafe extern "system" fn dashboard_window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let taskbar_created = TASKBAR_CREATED.load(Ordering::Relaxed);
    if taskbar_created != 0 && msg == taskbar_created {
        let nid = tray_icon_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        return LRESULT(0);
    }
    match msg {
        // Windows is shutting down or the user is signing out: take the tablet monitor off the
        // desktop now, because the process is ended without running the normal exit path.
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                if let Some(shared) = GLOBAL_SHARED.get() {
                    shared.is_running.store(false, Ordering::SeqCst);
                }
                crate::capture::virtual_display::detach();
            }
            LRESULT(0)
        }
        WM_TRAYICON => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if event == WM_RBUTTONUP {
                show_tray_menu(hwnd);
            } else if event == WM_LBUTTONDBLCLK || event == WM_LBUTTONUP {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_TELEMETRY && IsWindowVisible(hwnd).as_bool() {
                let _ = InvalidateRect(hwnd, None, false);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_buffered(hwnd, hdc, &ps.rcPaint);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // The whole client area is repainted from an off-screen buffer; letting Windows erase
        // it first is what made the dashboard flash on every telemetry tick.
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            handle_click(hwnd, x, y);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            // Check if cursor is inside the test canvas area
            if (TEST_BOX.left..=TEST_BOX.right).contains(&x) && (TEST_BOX.top..=TEST_BOX.bottom).contains(&y) {
                let pressure = if (wparam.0 & 0x0001) != 0 { 0.85f32 } else { 0.25f32 };
                let next = Some((x, y, pressure));
                let mut pos = TEST_STYLUS_POS.lock().unwrap_or_else(|e| e.into_inner());
                if *pos != next {
                    *pos = next;
                    drop(pos);
                    let _ = InvalidateRect(hwnd, Some(&TEST_BOX), false);
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd_id = wparam.0 & 0xFFFF;
            match cmd_id {
                ID_TRAY_DASHBOARD => {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                    let _ = SetForegroundWindow(hwnd);
                }
                ID_TRAY_ADB_REVERSE => {
                    let port = GLOBAL_SHARED.get().map_or(59100, |s| s.tcp_port.load(Ordering::Relaxed));
                    run_adb_reverse(port);
                    set_status(format!("Executed: adb reverse tcp:59100 tcp:{port}"));
                    let _ = InvalidateRect(hwnd, None, false);
                }
                ID_TRAY_EXIT => {
                    if let Some(shared) = GLOBAL_SHARED.get() {
                        shared.is_running.store(false, Ordering::SeqCst);
                    }
                    crate::capture::virtual_display::detach();
                    PostQuitMessage(0);
                    std::process::exit(0);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Minimize to system tray instead of exiting
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let title = to_wide_null("XDisplay");
    let dash_str = to_wide_null("🖥️ Open Dashboard");
    let adb_str = to_wide_null("⚡ Run ADB Reverse (Port 59100)");
    let exit_str = to_wide_null("✕ Exit XDisplay");

    let _ = InsertMenuW(menu, 0, MF_BYPOSITION | MF_GRAYED, ID_TRAY_STATUS, PCWSTR(title.as_ptr()));
    let _ = InsertMenuW(menu, 1, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
    let _ = InsertMenuW(menu, 2, MF_BYPOSITION | MF_STRING, ID_TRAY_DASHBOARD, PCWSTR(dash_str.as_ptr()));
    let _ = InsertMenuW(menu, 3, MF_BYPOSITION | MF_STRING, ID_TRAY_ADB_REVERSE, PCWSTR(adb_str.as_ptr()));
    let _ = InsertMenuW(menu, 4, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
    let _ = InsertMenuW(menu, 5, MF_BYPOSITION | MF_STRING, ID_TRAY_EXIT, PCWSTR(exit_str.as_ptr()));

    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_BOTTOMALIGN | TPM_LEFTALIGN | TPM_RIGHTBUTTON,
        cursor.x,
        cursor.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

unsafe fn create_font(size: i32, bold: bool) -> HFONT {
    let weight = if bold { 700 } else { 400 };
    let font_name = to_wide_null("Segoe UI");
    CreateFontW(
        size,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        PCWSTR(font_name.as_ptr()),
    )
}

unsafe fn draw_text_helper(
    hdc: HDC,
    text: &str,
    rect: &RECT,
    format: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    let mut wide = to_wide_null(text);
    let mut r = *rect;
    // DT_NOPREFIX: a literal '&' (e.g. "Extension & Stylus") must not be eaten as a mnemonic.
    let _ = DrawTextW(hdc, &mut wide, &mut r as *mut _, format | DT_NOPREFIX);
}

/// Renders the dashboard into a memory bitmap and blits only the invalidated region, so the
/// window never shows a half-drawn frame.
unsafe fn paint_buffered(hwnd: HWND, hdc: HDC, dirty: &RECT) {
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let (w, h) = (client.right - client.left, client.bottom - client.top);
    if w <= 0 || h <= 0 {
        return;
    }

    let mem_dc = CreateCompatibleDC(hdc);
    let bitmap = CreateCompatibleBitmap(hdc, w, h);
    if mem_dc.is_invalid() || bitmap.is_invalid() {
        // Out of GDI resources: draw directly rather than show nothing.
        paint_dashboard(hwnd, hdc);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        return;
    }

    let old_bitmap = SelectObject(mem_dc, bitmap);
    paint_dashboard(hwnd, mem_dc);
    let _ = BitBlt(
        hdc,
        dirty.left,
        dirty.top,
        dirty.right - dirty.left,
        dirty.bottom - dirty.top,
        mem_dc,
        dirty.left,
        dirty.top,
        SRCCOPY,
    );
    SelectObject(mem_dc, old_bitmap);
    let _ = DeleteObject(bitmap);
    let _ = DeleteDC(mem_dc);
}

unsafe fn paint_dashboard(hwnd: HWND, hdc: HDC) {
    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);

    // Color definitions
    let bg_color = rgb(14, 18, 27);         // Dark obsidian
    let card_bg = rgb(22, 28, 42);         // Glass card fill
    let card_border = rgb(38, 51, 74);     // Border outline
    let text_bright = rgb(240, 244, 250);   // Primary text
    let text_muted = rgb(138, 156, 180);    // Secondary text
    let cyan = rgb(0, 240, 255);           // Cyber cyan accent
    let green = rgb(0, 230, 118);          // Active green
    let yellow = rgb(255, 214, 0);         // Warning / Ready
    let btn_active_bg = rgb(0, 100, 140);  // Active chip background

    // Background fill
    let bg_brush = CreateSolidBrush(bg_color);
    FillRect(hdc, &rect, bg_brush);
    let _ = DeleteObject(bg_brush);

    SetBkMode(hdc, TRANSPARENT);

    let shared = match GLOBAL_SHARED.get() {
        Some(s) => s,
        None => return,
    };

    let is_connected = shared.client_connected.load(Ordering::Relaxed);
    let cur_fps = shared.target_fps.load(Ordering::Relaxed);
    let cur_kbps = shared.target_bitrate_kbps.load(Ordering::Relaxed);
    let cur_mbps = cur_kbps / 1000;

    let (disp_w, disp_h, disp_hz, disp_left, disp_top) = {
        let d = shared.display.lock().unwrap_or_else(|e| e.into_inner());
        (d.width, d.height, d.refresh_hz, d.left, d.top)
    };

    // Fonts
    let font_title = create_font(26, true);
    let font_section = create_font(18, true);
    let font_body = create_font(15, false);
    let font_small = create_font(13, false);
    let font_badge = create_font(14, true);

    // 1. Header Bar: Logo & Title
    let old_font = SelectObject(hdc, font_title);
    SetTextColor(hdc, text_bright);
    let header_rect = RECT { left: 24, top: 20, right: 450, bottom: 50 };
    draw_text_helper(hdc, "✦ XDisplay Host", &header_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, font_small);
    SetTextColor(hdc, text_muted);
    let sub_rect = RECT { left: 24, top: 48, right: 450, bottom: 68 };
    draw_text_helper(hdc, "Use a phone or tablet as a second screen with pen input", &sub_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // Status Badge Pill (Right side of header)
    let badge_rect = RECT { left: 490, top: 22, right: 720, bottom: 58 };
    let session = shared.session.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (badge_border_col, badge_text, badge_col) = match (&session, is_connected) {
        (Some(info), true) => {
            let name: String = info.device_name.chars().take(16).collect();
            let link = if info.remote { "Wi-Fi 🔒" } else { "USB" };
            (green, format!("● {name} · {link}"), green)
        }
        (_, true) => (green, "● CONNECTED (Streaming)".to_string(), green),
        _ => (yellow, "○ READY (Waiting for USB/Wi-Fi)".to_string(), yellow),
    };
    draw_card_box(hdc, &badge_rect, card_bg, badge_border_col);
    SelectObject(hdc, font_badge);
    SetTextColor(hdc, badge_col);
    draw_text_helper(hdc, &badge_text, &badge_rect, DT_CENTER | DT_SINGLELINE | DT_VCENTER);

    // 2. Telemetry Grid (4 Cards)
    let card_w = 162;
    let card_h = 76;
    let grid_y = 80;

    // Card 1: Connection
    let c1 = RECT { left: 24, top: grid_y, right: 24 + card_w, bottom: grid_y + card_h };
    draw_card_box(hdc, &c1, card_bg, card_border);
    let port_text = format!("Port {}", shared.tcp_port.load(Ordering::Relaxed));
    draw_card_content(hdc, &c1, "TRANSPORT", &port_text, "ADB Reverse & Wi-Fi", cyan, text_bright, text_muted, font_small, font_section);

    // Card 2: Display
    let c2 = RECT { left: 198, top: grid_y, right: 198 + card_w, bottom: grid_y + card_h };
    draw_card_box(hdc, &c2, card_bg, card_border);
    let res_text = format!("{}x{} @ {}Hz", disp_w, disp_h, disp_hz);
    let off_text = format!("Offset: ({}, {})", disp_left, disp_top);
    draw_card_content(hdc, &c2, "DISPLAY TARGET", &res_text, &off_text, cyan, text_bright, text_muted, font_small, font_section);

    // Card 3: Video Stream
    let c3 = RECT { left: 372, top: grid_y, right: 372 + card_w, bottom: grid_y + card_h };
    draw_card_box(hdc, &c3, card_bg, card_border);
    let mode = if shared.bitrate_is_auto() { "Auto " } else { "" };
    // Short enough for the card at any bitrate; the rate goes on the second line.
    let stream_text = format!("{mode}{cur_mbps} Mbps");
    let stream_detail = format!("{cur_fps} fps · GPU encoder");
    draw_card_content(hdc, &c3, "VIDEO ENCODER", &stream_text, &stream_detail, cyan, text_bright, text_muted, font_small, font_section);

    // Card 4: Input Digitizer
    let c4 = RECT { left: 546, top: grid_y, right: 546 + card_w, bottom: grid_y + card_h };
    draw_card_box(hdc, &c4, card_bg, card_border);
    draw_card_content(hdc, &c4, "STYLUS DIGITIZER", "2D Tilt & Pressure", "Synthetic Pen & Mouse", cyan, text_bright, text_muted, font_small, font_section);

    // 3. Stream Controls Section
    let sec_y = 175;
    SelectObject(hdc, font_section);
    SetTextColor(hdc, text_bright);
    let s1_rect = RECT { left: 24, top: sec_y, right: 300, bottom: sec_y + 24 };
    draw_text_helper(hdc, "⚡ Stream Settings", &s1_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // FPS Chips
    let buttons = get_buttons(shared);
    for btn in &buttons {
        let (fill, border, txt_col) = if btn.active {
            (btn_active_bg, cyan, text_bright)
        } else {
            (card_bg, card_border, text_muted)
        };
        draw_button_box(hdc, &btn.rect, fill, border);
        SelectObject(hdc, font_body);
        SetTextColor(hdc, txt_col);
        draw_text_helper(hdc, btn.label, &btn.rect, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
    }

    // 4. Quick Actions Section
    let act_y = 280;
    SelectObject(hdc, font_section);
    SetTextColor(hdc, text_bright);
    let s2_rect = RECT { left: 24, top: act_y, right: 300, bottom: act_y + 24 };
    draw_text_helper(hdc, "🛠️ Quick Actions", &s2_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // 5. Wi-Fi & Pairing
    let wifi_y = 365;
    SelectObject(hdc, font_section);
    SetTextColor(hdc, text_bright);
    let s_wifi = RECT { left: 24, top: wifi_y, right: 400, bottom: wifi_y + 24 };
    draw_text_helper(hdc, "📡 Wi-Fi & Pairing", &s_wifi, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    draw_card_box(hdc, &PAIRING_CARD, card_bg, card_border);

    let pin = shared.pairing.pin();
    let pin_text = if pin.len() == 6 { format!("{} {}", &pin[..3], &pin[3..]) } else { pin };
    SelectObject(hdc, font_small);
    SetTextColor(hdc, cyan);
    let pin_hdr = RECT { left: 38, top: PAIRING_CARD.top + 8, right: 230, bottom: PAIRING_CARD.top + 24 };
    draw_text_helper(hdc, "PAIRING PIN", &pin_hdr, DT_LEFT | DT_SINGLELINE);
    SelectObject(hdc, font_title);
    SetTextColor(hdc, text_bright);
    let pin_rect = RECT { left: 38, top: PAIRING_CARD.top + 24, right: 230, bottom: PAIRING_CARD.top + 54 };
    draw_text_helper(hdc, &pin_text, &pin_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    SelectObject(hdc, font_small);
    SetTextColor(hdc, text_muted);
    let pin_hint = RECT { left: 38, top: PAIRING_CARD.top + 56, right: 240, bottom: PAIRING_CARD.top + 74 };
    // After repeated wrong PINs pairing is paused (brute-force guard); New PIN resumes it.
    let hint = match shared.pairing.pairing_paused() {
        Some(left) => format!("Paused {}s after wrong PINs: New PIN", left.as_secs().max(1)),
        None => "Enter once on a new tablet".to_string(),
    };
    draw_text_helper(hdc, &hint, &pin_hint, DT_LEFT | DT_SINGLELINE);

    let addrs = lan_addresses();
    let addr_text = if addrs.is_empty() {
        "No network connection".to_string()
    } else {
        addrs.iter().take(3).map(|a| a.to_string()).collect::<Vec<_>>().join("  ·  ")
    };
    SetTextColor(hdc, cyan);
    let addr_hdr = RECT { left: 260, top: PAIRING_CARD.top + 8, right: 540, bottom: PAIRING_CARD.top + 24 };
    draw_text_helper(hdc, "THIS PC ON THE NETWORK", &addr_hdr, DT_LEFT | DT_SINGLELINE);
    SelectObject(hdc, font_body);
    SetTextColor(hdc, text_bright);
    let addr_rect = RECT { left: 260, top: PAIRING_CARD.top + 28, right: 545, bottom: PAIRING_CARD.top + 48 };
    draw_text_helper(hdc, &addr_text, &addr_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    SelectObject(hdc, font_small);
    SetTextColor(hdc, text_muted);
    let paired = shared.pairing.device_names();
    let paired_text = match paired.len() {
        0 => "No paired tablets yet".to_string(),
        1 => format!("Paired: {}", paired[0]),
        n => format!("Paired: {} and {} more", paired[0], n - 1),
    };
    let paired_rect = RECT { left: 260, top: PAIRING_CARD.top + 54, right: 545, bottom: PAIRING_CARD.top + 74 };
    draw_text_helper(hdc, &paired_text, &paired_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // 6. Stylus & Diagnostics Interactive Canvas
    let diag_y = 490;
    SelectObject(hdc, font_section);
    SetTextColor(hdc, text_bright);
    let s3_rect = RECT { left: 24, top: diag_y, right: 400, bottom: diag_y + 24 };
    draw_text_helper(hdc, "✏️ Live Stylus & Coordinate Monitor", &s3_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    draw_card_box(hdc, &TEST_BOX, rgb(18, 22, 34), card_border);

    // Info inside test canvas
    SelectObject(hdc, font_body);
    SetTextColor(hdc, cyan);
    let stylus_pos = *TEST_STYLUS_POS.lock().unwrap_or_else(|e| e.into_inner());
    let test_info = if let Some((x, y, p)) = stylus_pos {
        format!("Live Pointer Contact: Canvas X: {} Y: {} | Pressure: {:.2} | Target Monitor: ({}, {})", x, y, p, disp_left, disp_top)
    } else {
        "Hover or draw here with your stylus / mouse to test digitizer input".to_string()
    };
    let test_lbl_rect = RECT { left: 36, top: TEST_BOX.top + 8, right: 700, bottom: TEST_BOX.top + 32 };
    draw_text_helper(hdc, &test_info, &test_lbl_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, font_small);
    SetTextColor(hdc, text_muted);
    let help_rect = RECT { left: 36, top: TEST_BOX.top + 40, right: 700, bottom: TEST_BOX.top + 68 };
    draw_text_helper(hdc, "✨ In drawing apps (MS Paint, Krita, Photoshop): Make sure the drawing window is placed on the extended tablet screen!", &help_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    // 7. Footer Status Line
    let footer_y = 616;
    let status_msg = GLOBAL_STATUS_MSG.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(status_msg) = status_msg {
        SelectObject(hdc, font_small);
        SetTextColor(hdc, green);
        let f_rect = RECT { left: 24, top: footer_y, right: 720, bottom: footer_y + 20 };
        draw_text_helper(hdc, &format!("Status: {}", status_msg), &f_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
    }

    // Cleanup fonts
    SelectObject(hdc, old_font);
    let _ = DeleteObject(font_title);
    let _ = DeleteObject(font_section);
    let _ = DeleteObject(font_body);
    let _ = DeleteObject(font_small);
    let _ = DeleteObject(font_badge);
}

unsafe fn draw_card_box(hdc: HDC, rect: &RECT, fill_color: COLORREF, border_color: COLORREF) {
    let brush = CreateSolidBrush(fill_color);
    let pen = CreatePen(PS_SOLID, 1, border_color);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);

    let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, 12, 12);

    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
}

unsafe fn draw_button_box(hdc: HDC, rect: &RECT, fill_color: COLORREF, border_color: COLORREF) {
    let brush = CreateSolidBrush(fill_color);
    let pen = CreatePen(PS_SOLID, 1, border_color);
    let old_brush = SelectObject(hdc, brush);
    let old_pen = SelectObject(hdc, pen);

    let _ = RoundRect(hdc, rect.left, rect.top, rect.right, rect.bottom, 8, 8);

    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_card_content(
    hdc: HDC,
    rect: &RECT,
    header: &str,
    primary: &str,
    secondary: &str,
    header_col: COLORREF,
    primary_col: COLORREF,
    secondary_col: COLORREF,
    font_small: HFONT,
    font_large: HFONT,
) {
    let pad_x = 12;
    let top_y = rect.top + 8;

    SelectObject(hdc, font_small);
    SetTextColor(hdc, header_col);
    let h_rect = RECT { left: rect.left + pad_x, top: top_y, right: rect.right - pad_x, bottom: top_y + 16 };
    draw_text_helper(hdc, header, &h_rect, DT_LEFT | DT_SINGLELINE);

    SelectObject(hdc, font_large);
    SetTextColor(hdc, primary_col);
    let p_rect = RECT { left: rect.left + pad_x, top: top_y + 18, right: rect.right - pad_x, bottom: top_y + 40 };
    draw_text_helper(hdc, primary, &p_rect, DT_LEFT | DT_SINGLELINE | DT_VCENTER);

    SelectObject(hdc, font_small);
    SetTextColor(hdc, secondary_col);
    let s_rect = RECT { left: rect.left + pad_x, top: top_y + 42, right: rect.right - pad_x, bottom: rect.bottom - 6 };
    draw_text_helper(hdc, secondary, &s_rect, DT_LEFT | DT_SINGLELINE);
}

fn get_buttons(shared: &HostShared) -> Vec<ButtonDef> {
    let cur_fps = shared.target_fps.load(Ordering::Relaxed);
    let manual_mbps = shared.manual_bitrate_kbps.load(Ordering::Relaxed) / 1000;

    let row1_y = 205;
    let act_y = 310;
    let btn_h = 28;

    vec![
        // FPS Chips
        ButtonDef { id: ActionId::Fps60, rect: RECT { left: 24, top: row1_y, right: 104, bottom: row1_y + btn_h }, label: "60 Hz", active: cur_fps == 60 },
        ButtonDef { id: ActionId::Fps90, rect: RECT { left: 114, top: row1_y, right: 194, bottom: row1_y + btn_h }, label: "90 Hz", active: cur_fps == 90 },
        ButtonDef { id: ActionId::Fps120, rect: RECT { left: 204, top: row1_y, right: 284, bottom: row1_y + btn_h }, label: "120 Hz", active: cur_fps == 120 },

        // Bitrate Chips
        ButtonDef { id: ActionId::BitrateAuto, rect: RECT { left: 320, top: row1_y, right: 390, bottom: row1_y + btn_h }, label: "Auto", active: manual_mbps == 0 },
        ButtonDef { id: ActionId::Bitrate15, rect: RECT { left: 398, top: row1_y, right: 468, bottom: row1_y + btn_h }, label: "15 Mbps", active: manual_mbps == 15 },
        ButtonDef { id: ActionId::Bitrate25, rect: RECT { left: 476, top: row1_y, right: 546, bottom: row1_y + btn_h }, label: "25 Mbps", active: manual_mbps == 25 },
        ButtonDef { id: ActionId::Bitrate35, rect: RECT { left: 554, top: row1_y, right: 624, bottom: row1_y + btn_h }, label: "35 Mbps", active: manual_mbps == 35 },
        ButtonDef { id: ActionId::Bitrate50, rect: RECT { left: 632, top: row1_y, right: 710, bottom: row1_y + btn_h }, label: "50 Mbps", active: manual_mbps == 50 },

        // Quick Actions
        ButtonDef { id: ActionId::RunAdbReverse, rect: RECT { left: 24, top: act_y, right: 184, bottom: act_y + 36 }, label: "⚡ Run ADB Reverse", active: false },
        ButtonDef { id: ActionId::OpenDisplaySettings, rect: RECT { left: 196, top: act_y, right: 366, bottom: act_y + 36 }, label: "🖥️ Windows Displays", active: false },
        ButtonDef { id: ActionId::RequestKeyframe, rect: RECT { left: 378, top: act_y, right: 538, bottom: act_y + 36 }, label: "🔄 Force Keyframe", active: false },
        ButtonDef { id: ActionId::MinimizeToTray, rect: RECT { left: 550, top: act_y, right: 710, bottom: act_y + 36 }, label: "📥 Minimize to Tray", active: false },

        // Pairing
        ButtonDef { id: ActionId::NewPin, rect: RECT { left: 560, top: PAIRING_CARD.top + 10, right: 708, bottom: PAIRING_CARD.top + 38 }, label: "🔑 New PIN", active: false },
        ButtonDef { id: ActionId::ForgetTablets, rect: RECT { left: 560, top: PAIRING_CARD.top + 44, right: 708, bottom: PAIRING_CARD.top + 72 }, label: "🗑 Forget tablets", active: false },
    ]
}

unsafe fn handle_click(hwnd: HWND, x: i32, y: i32) {
    let shared = match GLOBAL_SHARED.get() {
        Some(s) => s,
        None => return,
    };

    let buttons = get_buttons(shared);
    for btn in buttons {
        if x >= btn.rect.left && x <= btn.rect.right && y >= btn.rect.top && y <= btn.rect.bottom {
            match btn.id {
                ActionId::Fps60 => {
                    shared.target_fps.store(60, Ordering::Relaxed);
                    shared.reconfigure_encoder.store(true, Ordering::Relaxed);
                }
                ActionId::Fps90 => {
                    shared.target_fps.store(90, Ordering::Relaxed);
                    shared.reconfigure_encoder.store(true, Ordering::Relaxed);
                }
                ActionId::Fps120 => {
                    shared.target_fps.store(120, Ordering::Relaxed);
                    shared.reconfigure_encoder.store(true, Ordering::Relaxed);
                }
                ActionId::BitrateAuto => {
                    shared.choose_auto_bitrate();
                }
                ActionId::Bitrate15 => {
                    shared.choose_bitrate_kbps(15000);
                }
                ActionId::Bitrate25 => {
                    shared.choose_bitrate_kbps(25000);
                }
                ActionId::Bitrate35 => {
                    shared.choose_bitrate_kbps(35000);
                }
                ActionId::Bitrate50 => {
                    shared.choose_bitrate_kbps(50000);
                }
                ActionId::RunAdbReverse => {
                    let port = shared.tcp_port.load(Ordering::Relaxed);
                    let ok = run_adb_reverse(port);
                    set_status(if ok {
                        format!("ADB port forwarding verified: reverse tcp:59100 tcp:{port} OK")
                    } else {
                        "ADB reverse failed. Verify tablet USB debugging is connected.".to_string()
                    });
                }
                ActionId::OpenDisplaySettings => {
                    let url = to_wide_null("ms-settings:display");
                    let verb = to_wide_null("open");
                    let _ = windows::Win32::UI::Shell::ShellExecuteW(
                        hwnd,
                        PCWSTR(verb.as_ptr()),
                        PCWSTR(url.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
                    );
                    set_status("Opened Windows Display Settings");
                }
                ActionId::RequestKeyframe => {
                    shared.request_idr.store(true, Ordering::SeqCst);
                    set_status("Forced IDR keyframe resync");
                }
                ActionId::MinimizeToTray => {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                ActionId::NewPin => {
                    shared.pairing.rotate_pin();
                    set_status("Generated a new pairing PIN");
                }
                ActionId::ForgetTablets => {
                    let count = shared.pairing.device_count();
                    if count == 0 {
                        set_status("No paired tablets to forget");
                    } else {
                        let text = to_wide_null(&format!(
                            "Forget {count} paired tablet(s)? They will need the PIN again over Wi-Fi, or your approval again over USB."
                        ));
                        let title = to_wide_null("XDisplay");
                        let answer = MessageBoxW(hwnd, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_YESNO | MB_ICONQUESTION);
                        if answer == IDYES {
                            shared.pairing.forget_all();
                            set_status("Forgot all paired tablets");
                        }
                    }
                }
                ActionId::ExitApp => {
                    shared.is_running.store(false, Ordering::SeqCst);
                    crate::capture::virtual_display::detach();
                    PostQuitMessage(0);
                    std::process::exit(0);
                }
            }
            let _ = InvalidateRect(hwnd, None, false);
            break;
        }
    }
}
