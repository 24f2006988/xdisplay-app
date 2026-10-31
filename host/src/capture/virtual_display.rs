//! Keeps the tablet's virtual monitor on the desktop only while a tablet is connected.
//!
//! The virtual display driver (MttVDD) presents a monitor permanently. Left attached, Windows lets
//! the cursor wander onto it and opens windows there even with no tablet to show them. Detaching
//! it (the same as "Disconnect this display" in Settings) removes it from the desktop, and Windows
//! moves its windows back to the remaining monitors. The mode and position the user had are saved
//! so attaching restores the same layout.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_NORESET, CDS_UPDATEREGISTRY,
    DEVMODEW, DISPLAY_DEVICEW, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    DM_POSITION, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, ENUM_REGISTRY_SETTINGS,
};

const DISPLAY_DEVICE_ATTACHED_TO_DESKTOP: u32 = 0x1;
const DEVICE_ID: &str = "Root\\MttVDD";
const DEVICE_STRING: &str = "Virtual Display Driver";

/// Refresh rates the tablet display is run at. The stream tops out at 144 fps, so a faster mode
/// (the driver offers up to 244 Hz) only desynchronizes capture from encode and inflates the
/// bitrate.
const MIN_HZ: u32 = 60;
const MAX_HZ: u32 = 144;

/// True while this host has the virtual monitor detached; capture must not run meanwhile.
static DETACHED: AtomicBool = AtomicBool::new(false);
/// Serializes attach and detach, which race when a tablet reconnects during the detach grace.
static LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq)]
struct Mode {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    hz: u32,
}

/// Whether the host detached the virtual monitor (no tablet connected).
pub fn is_detached() -> bool {
    DETACHED.load(Ordering::Relaxed)
}

/// Startup: detaches the virtual monitor, since no tablet is connected yet.
pub fn init() {
    detach_if(|| true);
}

/// Detaches the virtual monitor, e.g. when the host exits.
pub fn detach() {
    detach_if(|| true);
}

/// Detaches the virtual monitor if `still_wanted` holds once no attach can run concurrently.
pub fn detach_if(still_wanted: impl FnOnce() -> bool) {
    if display_changes_disabled() {
        return;
    }
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !still_wanted() {
        return;
    }
    let Some((device, attached)) = find_device() else { return };
    if !attached {
        DETACHED.store(true, Ordering::Relaxed);
        return;
    }
    if let Some(mode) = current_mode(&device) {
        save_mode(mode);
    }
    // Stop capture first so it does not fall back to another monitor while this one disappears.
    DETACHED.store(true, Ordering::Relaxed);
    let off = Mode { x: 0, y: 0, width: 0, height: 0, hz: 0 };
    if apply_mode(&device, off) || detach_via_ccd() {
        info!("Tablet display detached from the desktop (no tablet connected)");
    } else {
        warn!("Could not detach the tablet display; the cursor and windows can still reach it");
    }
}

fn attach_via_ccd() -> bool {
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::StationsAndDesktops::{OpenDesktopW, SetThreadDesktop, DESKTOP_CONTROL_FLAGS};
    use windows::core::w;

    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x00000001;
    const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xffff_ffff;

    unsafe {
        if let Ok(hdesk) = OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, 0x01FF) {
            let _ = SetThreadDesktop(hdesk);
        }

        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count) != WIN32_ERROR(0) {
            return false;
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];

        if QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        ) != WIN32_ERROR(0) {
            return false;
        }

        let mut activated = false;
        for path in paths[..path_count as usize].iter_mut() {
            if !path.targetInfo.targetAvailable.as_bool() {
                continue;
            }
            let mut target_name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target_name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target_name.header.adapterId = path.targetInfo.adapterId;
            target_name.header.id = path.targetInfo.id;

            if DisplayConfigGetDeviceInfo(&mut target_name.header) == 0 {
                let friendly = String::from_utf16_lossy(&target_name.monitorFriendlyDeviceName);
                let friendly_trim = friendly.trim_matches('\0');
                if friendly_trim.contains("VDD") || friendly_trim.contains("MTT") || friendly_trim.contains("Virtual") {
                    path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
                    path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                    path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
                    activated = true;
                    break;
                }
            }
        }

        if !activated {
            return false;
        }

        SetDisplayConfig(
            Some(&paths[..path_count as usize]),
            Some(&modes[..mode_count as usize]),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        ) == 0
    }
}

fn detach_via_ccd() -> bool {
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Foundation::WIN32_ERROR;
    use windows::Win32::System::StationsAndDesktops::{OpenDesktopW, SetThreadDesktop, DESKTOP_CONTROL_FLAGS};
    use windows::core::w;

    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x00000001;

    unsafe {
        if let Ok(hdesk) = OpenDesktopW(w!("Default"), DESKTOP_CONTROL_FLAGS(0), false, 0x01FF) {
            let _ = SetThreadDesktop(hdesk);
        }

        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count) != WIN32_ERROR(0) {
            return false;
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];

        if QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        ) != WIN32_ERROR(0) {
            return false;
        }

        let mut deactivated = false;
        for path in paths[..path_count as usize].iter_mut() {
            let mut target_name = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target_name.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target_name.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target_name.header.adapterId = path.targetInfo.adapterId;
            target_name.header.id = path.targetInfo.id;

            if DisplayConfigGetDeviceInfo(&mut target_name.header) == 0 {
                let friendly = String::from_utf16_lossy(&target_name.monitorFriendlyDeviceName);
                let friendly_trim = friendly.trim_matches('\0');
                if friendly_trim.contains("VDD") || friendly_trim.contains("MTT") || friendly_trim.contains("Virtual") {
                    path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
                    deactivated = true;
                }
            }
        }

        if !deactivated {
            return false;
        }

        SetDisplayConfig(
            Some(&paths[..path_count as usize]),
            Some(&modes[..mode_count as usize]),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        ) == 0
    }
}

/// Refresh rate to run the virtual monitor at for a panel that can do `panel_hz`.
///
/// It should be the panel's own rate: Windows composes frames on the clock of the display they
/// are shown on, so a 120 Hz monitor feeding a 144 Hz panel (or the reverse) makes the panel
/// repeat or skip a frame at irregular intervals, which looks like judder on any connection.
pub fn display_hz_for(panel_hz: u32) -> u32 {
    panel_hz.clamp(MIN_HZ, MAX_HZ)
}

/// Attaches the virtual monitor for a connecting tablet whose panel runs up to `panel_hz`, and
/// waits until Windows lists it. The monitor is sized to the panel's own resolution
/// (`panel_w`x`panel_h`) when the driver offers it, so the stream maps 1:1 onto the panel's
/// pixels instead of being upscaled. Returns false when there is no virtual display driver or
/// attaching failed.
/// Test harnesses (`test_security_e2e`) set `XDISPLAY_NO_DISPLAY_CHANGES=1` so driving the real
/// transport server never attaches, re-modes or detaches the user's virtual monitor.
fn display_changes_disabled() -> bool {
    std::env::var_os("XDISPLAY_NO_DISPLAY_CHANGES").is_some_and(|v| v == "1")
}

fn reload_driver_pipe() -> bool {
    use std::io::Write;
    let Ok(mut pipe) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(r"\\.\pipe\MTTVirtualDisplayPipe")
    else {
        return false;
    };
    let cmd: Vec<u8> = "RELOAD_DRIVER\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    pipe.write_all(&cmd).is_ok()
}

pub fn attach(panel_hz: u32, panel_w: u32, panel_h: u32) -> bool {
    if display_changes_disabled() {
        return false;
    }
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let hz = display_hz_for(panel_hz);
    let Some((mut device, mut attached)) = find_device() else {
        return false;
    };
    if driver_sizes(&device).is_empty() && reload_driver_pipe() {
        info!("Reloaded MttVDD via pipe because virtual display reported 0 modes");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            if let Some((dev, att)) = find_device() {
                device = dev;
                attached = att;
                if !driver_sizes(&device).is_empty() {
                    break;
                }
            }
        }
    }

    let primary_width = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN)
    };

    if attached {
        if let Some(current) = current_mode(&device) {
            let (x, y) = if current.x == 0 && current.y == 0 && primary_width > 0 {
                (primary_width, 0)
            } else {
                (current.x, current.y)
            };
            let wanted = sized_for_panel(&device, Mode { x, y, hz, ..current }, panel_w, panel_h);
            if wanted != current && apply_mode_any_rate(&device, wanted) {
                info!(
                    "Tablet display switched from {}x{} @ {} Hz to {}x{} @ {} Hz at {},{}",
                    current.width, current.height, current.hz, wanted.width, wanted.height, wanted.hz, wanted.x, wanted.y
                );
            }
        }
        DETACHED.store(false, Ordering::Relaxed);
        spawn_guard();
        return true;
    }

    let base = load_mode().or_else(|| fallback_mode(&device));
    let Some(base) = base else {
        warn!("No display mode known for the tablet display; cannot attach it");
        return false;
    };
    let (x, y) = if base.x == 0 && base.y == 0 && primary_width > 0 {
        (primary_width, 0)
    } else {
        (base.x, base.y)
    };
    let mode = sized_for_panel(&device, Mode { x, y, hz, ..base }, panel_w, panel_h);

    let mut applied = apply_mode_any_rate(&device, mode);
    if !applied && attach_via_ccd() {
        info!("Tablet display path activated via Windows CCD topology API");
        if let Some((dev, _)) = find_device() {
            device = dev;
        }
        applied = apply_mode_any_rate(&device, mode) || find_device().map(|(_, a)| a).unwrap_or(false);
    }
    if !applied {
        warn!("Could not attach the tablet display ({}x{} at {},{})", mode.width, mode.height, mode.x, mode.y);
        return false;
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    while !find_device().map(|(_, a)| a).unwrap_or(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if !find_device().map(|(_, a)| a).unwrap_or(false) {
        warn!("Tablet display mode was staged, but Windows did not attach it to the desktop");
        return false;
    }
    DETACHED.store(false, Ordering::Relaxed);
    info!("Tablet display attached ({}x{} @ {} Hz at {},{})", mode.width, mode.height, mode.hz, mode.x, mode.y);
    spawn_guard();
    true
}

/// Argument that makes the executable run as the display guard instead of the host.
pub const GUARD_ARG: &str = "--display-guard";
/// One-shot logon safety action installed by the production installer. It has no resident
/// process: it detaches a stale virtual monitor only when the host is not running.
pub const LOGON_CLEANUP_ARG: &str = "--detach-display-if-host-absent";

static GUARD_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Starts a hidden copy of this executable that detaches the virtual monitor once the host is gone,
/// however it went: a kill or crash runs none of the host's own cleanup.
fn spawn_guard() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if GUARD_SPAWNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let spawned = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .arg(GUARD_ARG)
            .arg(std::process::id().to_string())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    });
    if let Err(e) = spawned {
        GUARD_SPAWNED.store(false, Ordering::Relaxed);
        warn!("Could not start the display guard: {e}");
    }
}

/// Guard process body: waits for the host `pid` to exit, then detaches the virtual monitor.
pub fn run_guard(pid: u32) {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE};
    let Ok(host) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        // The host is already gone.
        detach();
        return;
    };
    if unsafe { WaitForSingleObject(host, INFINITE) } == WAIT_OBJECT_0 {
        detach();
    }
}

/// Argument that makes the executable run as the logon watchdog instead of the host.
pub const WATCHDOG_ARG: &str = "--display-watchdog";
/// Argument that stops a running watchdog (the installer uses it before replacing files).
pub const STOP_WATCHDOG_ARG: &str = "--stop-watchdog";
/// Argument that stops the watchdog and removes the per-user autostart a portable run created.
pub const REMOVE_WATCHDOG_ARG: &str = "--remove-watchdog";

const HOST_MUTEX: &str = "Global\\XDisplayHostSingleInstanceMutex";
const WATCHDOG_MUTEX: &str = "Global\\XDisplayWatchdogMutex";
const WATCHDOG_STOP_EVENT: &str = "Global\\XDisplayWatchdogStop";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "XDisplayWatchdog";

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn mutex_exists(name: &str) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let wname = wstr(name);
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wname.as_ptr())) } {
        Ok(h) => {
            let _ = unsafe { CloseHandle(h) };
            true
        }
        Err(_) => false,
    }
}

/// Watchdog body. The driver can put its monitor back on the desktop without the host (after a
/// reboot, driver restart, display change or a killed host), and then windows open on a screen
/// nobody can see. While no host is running this detaches it whenever it shows up.
pub fn run_watchdog() {
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, WaitForSingleObject};
    let mutex = wstr(WATCHDOG_MUTEX);
    let Ok(_instance) = (unsafe { CreateMutexW(None, true, PCWSTR(mutex.as_ptr())) }) else { return };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    let event = wstr(WATCHDOG_STOP_EVENT);
    let Ok(stop) = (unsafe { CreateEventW(None, true, false, PCWSTR(event.as_ptr())) }) else { return };
    loop {
        if !mutex_exists(HOST_MUTEX) {
            // Re-check under the lock so a host that starts meanwhile is not undone.
            detach_if(|| !mutex_exists(HOST_MUTEX));
        }
        if unsafe { WaitForSingleObject(stop, 2000) } == WAIT_OBJECT_0 {
            return;
        }
    }
}

/// Asks a running watchdog to exit and waits for it to do so.
pub fn stop_watchdog() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    let event = wstr(WATCHDOG_STOP_EVENT);
    if let Ok(h) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event.as_ptr())) } {
        let _ = unsafe { SetEvent(h) };
        let _ = unsafe { CloseHandle(h) };
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while mutex_exists(WATCHDOG_MUTEX) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stops a legacy watchdog and removes its per-user autostart entry. New installations do not
/// create this process; this exists only to clean up upgrades and uninstalls.
pub fn remove_watchdog() {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };
    stop_watchdog();
    let (key_name, value) = (wstr(RUN_KEY), wstr(RUN_VALUE));
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key_name.as_ptr()), 0, KEY_SET_VALUE, &mut key).is_ok() {
            let _ = RegDeleteValueW(key, PCWSTR(value.as_ptr()));
            let _ = RegCloseKey(key);
        }
    }
}

/// Cleans up a stale monitor left by a reboot or driver restart without racing a running host.
/// Invoked once per user logon by Task Scheduler, then exits.
pub fn run_logon_cleanup() {
    detach_if(|| !mutex_exists(HOST_MUTEX));
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

/// Returns the virtual display's adapter name (NUL-terminated) and whether it is on the desktop.
fn find_device() -> Option<(Vec<u16>, bool)> {
    for i in 0.. {
        let mut dd = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        let id = wide(&dd.DeviceID);
        if id.eq_ignore_ascii_case(DEVICE_ID) || wide(&dd.DeviceString) == DEVICE_STRING {
            let end = dd.DeviceName.iter().position(|&c| c == 0).unwrap_or(dd.DeviceName.len());
            let mut name = dd.DeviceName[..end].to_vec();
            name.push(0);
            return Some((name, dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0));
        }
    }
    None
}

fn read_mode(device: &[u16], which: ENUM_DISPLAY_SETTINGS_MODE) -> Option<Mode> {
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    if !unsafe { EnumDisplaySettingsW(PCWSTR(device.as_ptr()), which, &mut dm) }.as_bool() {
        return None;
    }
    let pos = unsafe { dm.Anonymous1.Anonymous2.dmPosition };
    (dm.dmPelsWidth > 0 && dm.dmPelsHeight > 0).then_some(Mode {
        x: pos.x,
        y: pos.y,
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
        hz: dm.dmDisplayFrequency,
    })
}

fn current_mode(device: &[u16]) -> Option<Mode> {
    read_mode(device, ENUM_CURRENT_SETTINGS)
}

/// Used when nothing was saved yet: the registry mode, else the largest mode, right of the primary.
fn fallback_mode(device: &[u16]) -> Option<Mode> {
    let primary_width = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN)
    };
    if let Some(m) = read_mode(device, ENUM_REGISTRY_SETTINGS).filter(is_plausible) {
        let (x, y) = if m.x == 0 && m.y == 0 && primary_width > 0 {
            (primary_width, 0)
        } else {
            (m.x, m.y)
        };
        return Some(Mode { x, y, ..m });
    }
    let mut best: Option<Mode> = None;
    for i in 0..512 {
        let Some(m) = read_mode(device, ENUM_DISPLAY_SETTINGS_MODE(i)) else { break };
        let area = |m: &Mode| (m.width * m.height, m.hz);
        if best.map(|b| area(&m) > area(&b)).unwrap_or(true) {
            best = Some(m);
        }
    }
    best.map(|m| Mode { x: primary_width, y: 0, ..m })
}

/// Applies `mode` to the device (a zero size detaches it) and commits the new layout.
fn apply_mode(device: &[u16], mode: Mode) -> bool {
    let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    dm.dmFields = DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT;
    dm.dmPelsWidth = mode.width;
    dm.dmPelsHeight = mode.height;
    dm.Anonymous1.Anonymous2.dmPosition.x = mode.x;
    dm.Anonymous1.Anonymous2.dmPosition.y = mode.y;
    if mode.hz > 1 {
        dm.dmFields |= DM_DISPLAYFREQUENCY;
        dm.dmDisplayFrequency = mode.hz;
    }
    unsafe {
        let staged = ChangeDisplaySettingsExW(
            PCWSTR(device.as_ptr()),
            Some(&dm),
            HWND::default(),
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        );
        if staged != DISP_CHANGE_SUCCESSFUL {
            warn!("ChangeDisplaySettingsExW({}) returned {}", wide(device), staged.0);
            return false;
        }
        ChangeDisplaySettingsExW(PCWSTR::null(), None, HWND::default(), Default::default(), None)
            == DISP_CHANGE_SUCCESSFUL
    }
}

/// Applies `mode`, retrying at whatever rate the driver picks if it lacks the requested one.
fn apply_mode_any_rate(device: &[u16], mode: Mode) -> bool {
    apply_mode(device, mode) || apply_mode(device, Mode { hz: 0, ..mode })
}

/// Every size the driver offers for the monitor.
fn driver_sizes(device: &[u16]) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    for i in 0..1024 {
        let Some(m) = read_mode(device, ENUM_DISPLAY_SETTINGS_MODE(i)) else { break };
        if !sizes.contains(&(m.width, m.height)) {
            sizes.push((m.width, m.height));
        }
    }
    sizes
}

/// Picks the monitor size for a `panel_w`x`panel_h` tablet panel out of the driver's `sizes`.
///
/// The client runs in landscape, so the panel is taken as landscape. Prefers the exact panel size
/// (1:1 pixels, nothing to scale); else the largest size of the same shape that still fits the
/// panel; else the size whose shape is closest to the panel's (preferring sizes that fit), so a
/// device the driver has no exact mode for is stretched as little as possible.
fn pick_panel_size(sizes: &[(u32, u32)], panel_w: u32, panel_h: u32) -> Option<(u32, u32)> {
    let (pw, ph) = (panel_w.max(panel_h), panel_w.min(panel_h));
    if pw == 0 || ph == 0 {
        return None;
    }
    if sizes.contains(&(pw, ph)) {
        return Some((pw, ph));
    }
    let panel_ratio = pw as f32 / ph as f32;
    let shape_error = |&(w, h): &(u32, u32)| (w as f32 / h as f32 - panel_ratio).abs();
    let usable: Vec<(u32, u32)> =
        sizes.iter().copied().filter(|&(w, h)| w >= 1024 && h >= 600 && w >= h).collect();
    let same_shape = usable
        .iter()
        .copied()
        .filter(|s| s.0 <= pw && s.1 <= ph && shape_error(s) < 0.02)
        .max_by_key(|&(w, h)| w * h);
    if same_shape.is_some() {
        return same_shape;
    }
    let fitting: Vec<(u32, u32)> = usable.iter().copied().filter(|&(w, h)| w <= pw && h <= ph).collect();
    let pool = if fitting.is_empty() { usable } else { fitting };
    // Closest shape first (in steps of 0.01, so near-equal shapes count as equal), then the largest.
    pool.into_iter().min_by_key(|s| ((shape_error(s) * 100.0).round() as u32, std::cmp::Reverse(s.0 * s.1)))
}

/// `mode` resized to suit the tablet panel, keeping its position and refresh rate.
fn sized_for_panel(device: &[u16], mode: Mode, panel_w: u32, panel_h: u32) -> Mode {
    match pick_panel_size(&driver_sizes(device), panel_w, panel_h) {
        Some((width, height)) => Mode { width, height, ..mode },
        None => {
            warn!("Virtual display driver offers no mode matching the {panel_w}x{panel_h} tablet panel; keeping {}x{}", mode.width, mode.height);
            mode
        }
    }
}

/// Modes below this are Windows' fallbacks (e.g. 800x600 after a mode-less attach), never a
/// layout the user chose, so they are neither saved nor restored.
fn is_plausible(m: &Mode) -> bool {
    m.width >= 1024 && m.height >= 600
}

fn mode_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("XDisplay").join("tablet_display.conf"))
}

fn save_mode(m: Mode) {
    if !is_plausible(&m) {
        return;
    }
    let Some(path) = mode_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{} {} {} {} {}\n", m.x, m.y, m.width, m.height, m.hz));
}

fn load_mode() -> Option<Mode> {
    parse_mode(&std::fs::read_to_string(mode_path()?).ok()?).filter(is_plausible)
}

fn parse_mode(text: &str) -> Option<Mode> {
    let v: Vec<i64> = text.split_whitespace().map(|t| t.parse().ok()).collect::<Option<_>>()?;
    let [x, y, w, h, hz] = v[..] else { return None };
    (w > 0 && h > 0).then_some(Mode { x: x as i32, y: y as i32, width: w as u32, height: h as u32, hz: hz as u32 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_rate_follows_the_panel_within_the_stream_limits() {
        assert_eq!(display_hz_for(144), 144);
        assert_eq!(display_hz_for(120), 120);
        assert_eq!(display_hz_for(60), 60);
        assert_eq!(display_hz_for(165), MAX_HZ);
        assert_eq!(display_hz_for(30), MIN_HZ);
        assert_eq!(display_hz_for(0), MIN_HZ);
    }

    #[test]
    fn picks_native_panel_size_when_offered() {
        let sizes = [(800, 600), (1800, 1200), (2400, 1600), (3200, 2136), (3840, 2160)];
        // Portrait-reported panel is treated as landscape.
        assert_eq!(pick_panel_size(&sizes, 2136, 3200), Some((3200, 2136)));
        assert_eq!(pick_panel_size(&sizes, 3200, 2136), Some((3200, 2136)));
        // No exact match: largest same-shape size that fits.
        assert_eq!(pick_panel_size(&sizes, 2560, 1707), Some((2400, 1600)));
        // Nothing of the right shape: the closest usable shape (800x600 is a Windows fallback).
        assert_eq!(pick_panel_size(&[(800, 600), (1920, 1080)], 3200, 2136), Some((1920, 1080)));
        assert_eq!(pick_panel_size(&sizes, 0, 0), None);
        assert_eq!(pick_panel_size(&[], 2560, 1600), None);
    }

    #[test]
    fn unusual_panels_get_the_closest_shape() {
        let sizes = [(1920, 1080), (2560, 1600), (2400, 1600), (2048, 1536), (2400, 1080)];
        // 19.5:9 phone with no exact mode; the wider 2400x1080 does not fit, so the closest fitting shape.
        assert_eq!(pick_panel_size(&sizes, 1080, 2340), Some((1920, 1080)));
        // 20:9 phone: 2400x1080 is the same shape and fits.
        assert_eq!(pick_panel_size(&sizes, 3200, 1440), Some((2400, 1080)));
        // 7:5 tablet (2800x2000): closest fitting shape is 4:3.
        assert_eq!(pick_panel_size(&sizes, 2800, 2000), Some((2048, 1536)));
        // Panel smaller than every mode: still the closest shape rather than nothing.
        assert_eq!(pick_panel_size(&sizes, 1280, 800), Some((2560, 1600)));
    }

    #[test]
    fn fallback_modes_are_not_plausible() {
        assert!(!is_plausible(&Mode { x: 1920, y: 0, width: 800, height: 600, hz: 30 }));
        assert!(is_plausible(&Mode { x: 1920, y: 0, width: 1800, height: 1200, hz: 120 }));
    }

    #[test]
    fn parses_saved_mode() {
        assert_eq!(
            parse_mode("1920 0 1800 1200 120\n"),
            Some(Mode { x: 1920, y: 0, width: 1800, height: 1200, hz: 120 })
        );
        assert_eq!(parse_mode("-1800 -200 1800 1200 60").map(|m| (m.x, m.y)), Some((-1800, -200)));
        assert_eq!(parse_mode("0 0 0 0 60"), None);
        assert_eq!(parse_mode("garbage"), None);
        assert_eq!(parse_mode("1 2 3"), None);
    }
}
