//! Installs and removes the virtual display driver (MttVDD) without devcon or winget.
//!
//! The driver is root-enumerated, so Windows will not create its device by itself: a device
//! node with the `Root\MttVDD` hardware id has to be registered first, then the driver bound to
//! it. This is what `devcon install MttVDD.inf Root\MttVDD` does. The installer runs these
//! commands elevated; they need administrator rights.

use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DiUninstallDevice, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiSetDeviceRegistryPropertyW, UpdateDriverForPlugAndPlayDevicesW, DICD_GENERATE_ID, DIF_REGISTERDEVICE,
    DIGCF_ALLCLASSES, GUID_DEVCLASS_DISPLAY, HDEVINFO, INSTALLFLAG_FORCE, SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};
use windows::Win32::Foundation::{BOOL, HWND};

/// Hardware id the MttVDD INF matches.
pub const HARDWARE_ID: &str = "Root\\MttVDD";
/// Installs the driver from the INF path that follows it.
pub const INSTALL_ARG: &str = "--install-display-driver";
/// Removes the driver's device node.
pub const UNINSTALL_ARG: &str = "--uninstall-display-driver";
/// Exits 0 when the driver's device exists, 1 when it does not.
pub const QUERY_ARG: &str = "--query-display-driver";

/// Process exit code that tells the installer a restart is needed (same as msiexec).
pub const EXIT_REBOOT: i32 = 3010;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A device information set, destroyed on drop.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Every device (present or not) whose hardware ids include [`HARDWARE_ID`].
fn matching_devices() -> windows::core::Result<(DevInfo, Vec<SP_DEVINFO_DATA>)> {
    let set = DevInfo(unsafe { SetupDiGetClassDevsW(None, PCWSTR::null(), HWND::default(), DIGCF_ALLCLASSES)? });
    let mut found = Vec::new();
    for index in 0.. {
        let mut data = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) }.is_err() {
            break;
        }
        if hardware_ids(&set, &data).iter().any(|id| id.eq_ignore_ascii_case(HARDWARE_ID)) {
            found.push(data);
        }
    }
    Ok((set, found))
}

fn hardware_ids(set: &DevInfo, data: &SP_DEVINFO_DATA) -> Vec<String> {
    let mut buffer = vec![0u8; 2048];
    let mut needed = 0u32;
    let read = unsafe {
        SetupDiGetDeviceRegistryPropertyW(set.0, data, SPDRP_HARDWAREID, None, Some(&mut buffer), Some(&mut needed))
    };
    if read.is_err() {
        return Vec::new();
    }
    let len = (needed as usize).min(buffer.len()) / 2;
    let units: Vec<u16> = buffer.as_chunks::<2>().0.iter().take(len).map(|c| u16::from_le_bytes(*c)).collect();
    parse_multi_sz(&units)
}

/// Splits a REG_MULTI_SZ (NUL-separated, double-NUL-terminated) UTF-16 buffer.
fn parse_multi_sz(units: &[u16]) -> Vec<String> {
    units
        .split(|&u| u == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// True when the virtual display driver's device node exists.
pub fn is_installed() -> bool {
    matching_devices().map(|(_, devices)| !devices.is_empty()).unwrap_or(false)
}

/// Registers the root device (once) and installs the driver from `inf` onto it.
/// Returns whether Windows asked for a restart.
pub fn install(inf: &Path) -> anyhow::Result<bool> {
    let inf = std::fs::canonicalize(inf).map_err(|e| anyhow::anyhow!("{}: {e}", inf.display()))?;
    // canonicalize returns a \\?\ path, which the driver installer rejects.
    let inf_text = inf.to_string_lossy().trim_start_matches(r"\\?\").to_string();

    if !is_installed() {
        register_root_device()?;
    }
    let (hwid, inf_w) = (wide(HARDWARE_ID), wide(&inf_text));
    let mut reboot = BOOL(0);
    unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            HWND::default(),
            PCWSTR(hwid.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        )
    }
    .map_err(|e| anyhow::anyhow!("installing the driver from {inf_text} failed: {e}"))?;
    Ok(reboot.as_bool())
}

fn register_root_device() -> anyhow::Result<()> {
    let set = DevInfo(unsafe { SetupDiCreateDeviceInfoList(Some(&GUID_DEVCLASS_DISPLAY), HWND::default())? });
    let mut data = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
    let class_name = wide("Display");
    unsafe {
        SetupDiCreateDeviceInfoW(
            set.0,
            PCWSTR(class_name.as_ptr()),
            &GUID_DEVCLASS_DISPLAY,
            PCWSTR::null(),
            HWND::default(),
            DICD_GENERATE_ID,
            Some(&mut data),
        )?;
        // REG_MULTI_SZ: the id, then an empty string.
        let ids: Vec<u8> = HARDWARE_ID
            .encode_utf16()
            .chain([0, 0])
            .flat_map(|u| u.to_le_bytes())
            .collect();
        SetupDiSetDeviceRegistryPropertyW(set.0, &mut data, SPDRP_HARDWAREID, Some(&ids))?;
        SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set.0, Some(&data))?;
    }
    Ok(())
}

/// Removes every device node of the virtual display driver. Returns whether a restart is needed.
pub fn uninstall() -> anyhow::Result<bool> {
    let (set, devices) = matching_devices()?;
    let mut reboot_any = false;
    for data in &devices {
        let mut reboot = BOOL(0);
        unsafe { DiUninstallDevice(HWND::default(), set.0, data, 0, Some(&mut reboot))? };
        reboot_any |= reboot.as_bool();
    }
    Ok(reboot_any)
}

/// Entry point for the command-line modes; returns the process exit code.
pub fn run_cli(mode: &str, arg: Option<String>) -> i32 {
    let result = match mode {
        INSTALL_ARG => match arg {
            Some(inf) => install(Path::new(&inf)),
            None => Err(anyhow::anyhow!("usage: {INSTALL_ARG} <path to MttVDD.inf>")),
        },
        UNINSTALL_ARG => uninstall(),
        QUERY_ARG => return if is_installed() { 0 } else { 1 },
        _ => return 2,
    };
    match result {
        Ok(true) => EXIT_REBOOT,
        Ok(false) => 0,
        Err(e) => {
            // The installer shows its own message; just report why on stderr.
            eprintln!("XDisplay driver setup: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_sz_is_split_into_ids() {
        let units: Vec<u16> = "Root\\MttVDD\0MttVDD\0\0".encode_utf16().collect();
        assert_eq!(parse_multi_sz(&units), vec!["Root\\MttVDD".to_string(), "MttVDD".to_string()]);
    }

    #[test]
    fn empty_multi_sz_has_no_ids() {
        assert!(parse_multi_sz(&[0, 0]).is_empty());
    }
}
