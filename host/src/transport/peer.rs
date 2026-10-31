//! Identifies the local process behind a loopback connection.
//!
//! USB sessions arrive from `adb.exe` (the adb server forwards the tablet's `adb reverse`
//! connection to the host port) and skip the PIN. Any other local process, such as another user's
//! session or malware, could otherwise connect to the port and inject keyboard and pen input
//! without pairing.

use std::net::SocketAddr;

/// Whether a loopback connection may skip pairing. Fails closed: anything unidentifiable pairs.
pub fn is_trusted_loopback(peer: SocketAddr, host_port: u16) -> bool {
    if !peer.ip().is_loopback() {
        return false;
    }
    match owner_image_name(peer, host_port) {
        Some(name) => is_trusted_image(&name),
        None => false,
    }
}

/// `adb.exe` (from Android SDK / platform-tools) or `xdisplay-host.exe` (internal USB / AOA loopback bridge).
/// Matched by file name only: the check separates the adb server and host bridge from arbitrary local programs.
fn is_trusted_image(path: &str) -> bool {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.eq_ignore_ascii_case("adb.exe")
        || file.eq_ignore_ascii_case("xdisplay-host.exe")
        || file.eq_ignore_ascii_case("xdisplay_host.exe")
}

#[allow(dead_code)]
fn is_adb_image(path: &str) -> bool {
    is_trusted_image(path)
}

#[cfg(windows)]
fn owner_image_name(peer: SocketAddr, host_port: u16) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    };
    use windows::Win32::Networking::WinSock::AF_INET;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // The table only covers IPv4; the host binds an IPv4 address and adb connects to 127.0.0.1.
    let SocketAddr::V4(peer4) = peer else { return None };
    let want_local = u16::to_be(peer4.port()) as u32;
    let want_remote = u16::to_be(host_port) as u32;

    // The table can change between sizing and reading, so retry a few times.
    let mut buf: Vec<u8> = Vec::new();
    let mut size = 0u32;
    for _ in 0..4 {
        let rc = unsafe {
            GetExtendedTcpTable(
                (!buf.is_empty()).then_some(buf.as_mut_ptr() as *mut _),
                &mut size,
                false,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if rc == NO_ERROR.0 {
            break;
        }
        if rc != ERROR_INSUFFICIENT_BUFFER.0 {
            return None;
        }
        buf.resize(size as usize, 0);
    }
    if buf.len() < std::mem::size_of::<u32>() {
        return None;
    }

    let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
    let count = unsafe { (*table).dwNumEntries } as usize;
    let rows = unsafe { std::slice::from_raw_parts((*table).table.as_ptr(), count) };
    let pid = rows
        .iter()
        .find(|r| r.dwLocalPort == want_local && r.dwRemotePort == want_remote)
        .map(|r| r.dwOwningPid)?;

    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut name = vec![0u16; 1024];
        let mut len = name.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(name.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        ok.ok()?;
        Some(String::from_utf16_lossy(&name[..len as usize]))
    }
}

#[cfg(not(windows))]
fn owner_image_name(_peer: SocketAddr, _host_port: u16) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_adb_is_trusted() {
        assert!(is_trusted_image(r"C:\Users\x\AppData\Local\Android\Sdk\platform-tools\adb.exe"));
        assert!(is_trusted_image("ADB.EXE"));
        assert!(is_trusted_image(r"D:\xdisplay\target\release\xdisplay-host.exe"));
        assert!(is_trusted_image("xdisplay_host.exe"));
        assert!(!is_trusted_image(r"C:\temp\notadb.exe"));
        assert!(!is_trusted_image(r"C:\adb.exe\evil.exe"));
        assert!(!is_trusted_image(""));
    }

    #[test]
    fn network_peers_are_never_trusted() {
        assert!(!is_trusted_loopback("192.168.1.9:50000".parse().unwrap(), 59100));
    }

    #[tokio::test]
    async fn this_test_process_is_not_adb() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (_server, peer) = listener.accept().await.unwrap();
        #[cfg(windows)]
        {
            let name = owner_image_name(peer, port).expect("own connection is in the TCP table");
            assert!(!is_adb_image(&name), "{name}");
        }
        assert!(!is_trusted_loopback(peer, port));
        drop(client);
    }
}
