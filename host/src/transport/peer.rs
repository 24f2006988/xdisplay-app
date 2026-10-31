//! Identifies the local process behind a loopback connection.
//!
//! USB sessions skip the PIN. They arrive over loopback from one of two places:
//! * the **adb server** (`adb.exe`), which forwards the tablet's `adb reverse` connection, or
//! * the **AOA accessory bridge**, which runs inside this host process (`transport::aoa`).
//!
//! Any other local process (another user's session, a sandboxed or malicious program) could
//! otherwise connect to the port and inject keyboard and pen input without pairing, so:
//! * the bridge is recognised by **process id** (it is this process), not by file name, which
//!   anything can copy;
//! * adb is trusted only when its image is named `adb.exe` **and it runs as the same Windows
//!   user as the host**, so other accounts on the PC (fast user switching, RDP) never get in.
//!
//! Residual risk (documented in `context/14_SECURITY_REVIEW_2026-10-31.md`): a program already
//! running as the same user can still pose as `adb.exe`. Such a program can inject input into
//! the user's desktop directly anyway, so this does not widen what it can do.

use std::net::SocketAddr;

/// Who owns the client end of a loopback connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopbackOwner {
    pub pid: u32,
    /// Full image path, when the process could be opened.
    pub image: Option<String>,
    /// Whether the process token's user is the same account as this host's.
    pub same_user: bool,
}

/// Whether a loopback connection may skip pairing. Fails closed: anything unidentifiable pairs.
pub fn is_trusted_loopback(peer: SocketAddr, host_port: u16) -> bool {
    if !peer.ip().is_loopback() {
        return false;
    }
    match loopback_owner(peer, host_port) {
        Some(owner) => is_trusted_owner(&owner, std::process::id()),
        None => false,
    }
}

/// The trust rule, separated from the OS lookups so it can be tested exhaustively.
pub fn is_trusted_owner(owner: &LoopbackOwner, own_pid: u32) -> bool {
    if owner.pid == own_pid {
        return true; // the in-process AOA bridge
    }
    owner.same_user && owner.image.as_deref().is_some_and(is_adb_image)
}

/// `adb.exe`, matched by file name (Android SDK, platform-tools or the bundled copy).
fn is_adb_image(path: &str) -> bool {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path);
    file.eq_ignore_ascii_case("adb.exe")
}

/// Process that owns the client side (`peer`) of a connection to `host_port`.
#[cfg(windows)]
pub fn loopback_owner(peer: SocketAddr, host_port: u16) -> Option<LoopbackOwner> {
    let pid = owning_pid(peer, host_port)?;
    if pid == std::process::id() {
        return Some(LoopbackOwner { pid, image: None, same_user: true });
    }
    Some(win::describe(pid))
}

#[cfg(not(windows))]
pub fn loopback_owner(_peer: SocketAddr, _host_port: u16) -> Option<LoopbackOwner> {
    None
}

#[cfg(windows)]
fn owning_pid(peer: SocketAddr, host_port: u16) -> Option<u32> {
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL};
    use windows::Win32::Networking::WinSock::AF_INET;

    // The table only covers IPv4; the host binds an IPv4 address and adb connects to 127.0.0.1.
    let SocketAddr::V4(peer4) = peer else { return None };
    let want_local = u16::to_be(peer4.port()) as u32;
    let want_remote = u16::to_be(host_port) as u32;
    let want_addr = u32::from_ne_bytes(peer4.ip().octets());

    // The table can change between sizing and reading, so retry a few times.
    let mut buf: Vec<u8> = Vec::new();
    let mut size = 0u32;
    let mut filled = false;
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
        if rc == NO_ERROR.0 && !buf.is_empty() {
            filled = true;
            break;
        }
        if rc != ERROR_INSUFFICIENT_BUFFER.0 && rc != NO_ERROR.0 {
            return None;
        }
        buf.resize(size as usize, 0);
    }
    if !filled || buf.len() < std::mem::size_of::<u32>() {
        return None;
    }

    let table = buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID;
    let count = unsafe { (*table).dwNumEntries } as usize;
    // Never trust the count beyond what the buffer holds.
    let max_rows = (buf.len() - std::mem::size_of::<u32>()) / std::mem::size_of::<windows::Win32::NetworkManagement::IpHelper::MIB_TCPROW_OWNER_PID>();
    let rows = unsafe { std::slice::from_raw_parts((*table).table.as_ptr(), count.min(max_rows)) };
    rows.iter()
        .find(|r| r.dwLocalPort == want_local && r.dwRemotePort == want_remote && r.dwLocalAddr == want_addr)
        .map(|r| r.dwOwningPid)
}

#[cfg(windows)]
mod win {
    use super::LoopbackOwner;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{EqualSid, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// The token's user SID, as the raw `TOKEN_USER` buffer (the SID points into it).
    fn token_user(process: HANDLE) -> Option<Vec<u64>> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
            let token = Handle(token);
            let mut len = 0u32;
            let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
            if len == 0 || len > 4096 {
                return None;
            }
            // u64 storage keeps the TOKEN_USER struct aligned.
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            GetTokenInformation(token.0, TokenUser, Some(buf.as_mut_ptr() as *mut _), len, &mut len).ok()?;
            Some(buf)
        }
    }

    fn same_user(other: HANDLE) -> bool {
        let (Some(mine), Some(theirs)) = (token_user(unsafe { GetCurrentProcess() }), token_user(other)) else {
            return false;
        };
        unsafe {
            let a = &*(mine.as_ptr() as *const TOKEN_USER);
            let b = &*(theirs.as_ptr() as *const TOKEN_USER);
            EqualSid(a.User.Sid, b.User.Sid).is_ok()
        }
    }

    pub fn describe(pid: u32) -> LoopbackOwner {
        let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
            // A process we cannot even query (another user's, or protected) is not trusted.
            return LoopbackOwner { pid, image: None, same_user: false };
        };
        let process = Handle(process);
        let image = unsafe {
            let mut name = vec![0u16; 1024];
            let mut len = name.len() as u32;
            QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, PWSTR(name.as_mut_ptr()), &mut len)
                .ok()
                .map(|_| String::from_utf16_lossy(&name[..len as usize]))
        };
        LoopbackOwner { pid, image, same_user: same_user(process.0) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 4242;

    fn owner(pid: u32, image: &str, same_user: bool) -> LoopbackOwner {
        LoopbackOwner { pid, image: (!image.is_empty()).then(|| image.to_string()), same_user }
    }

    #[test]
    fn adb_by_the_same_user_is_trusted() {
        assert!(is_trusted_owner(&owner(7, r"C:\Users\x\AppData\Local\Android\Sdk\platform-tools\adb.exe", true), ME));
        assert!(is_trusted_owner(&owner(7, "ADB.EXE", true), ME));
    }

    #[test]
    fn adb_run_by_another_user_is_not_trusted() {
        assert!(!is_trusted_owner(&owner(7, r"C:\platform-tools\adb.exe", false), ME));
    }

    #[test]
    fn the_host_name_is_no_longer_a_pass() {
        // A copied or renamed binary in another process must pair like anything else; only this
        // very process (the AOA bridge) is trusted, by pid.
        assert!(!is_trusted_owner(&owner(7, r"C:\temp\xdisplay-host.exe", true), ME));
        assert!(!is_trusted_owner(&owner(7, "xdisplay_host.exe", true), ME));
        assert!(is_trusted_owner(&owner(ME, "", true), ME));
    }

    #[test]
    fn look_alike_names_are_not_trusted() {
        for name in [r"C:\temp\notadb.exe", r"C:\adb.exe\evil.exe", "adb.exe.exe", "adb", "", r"C:\x\adb .exe"] {
            assert!(!is_trusted_owner(&owner(7, name, true), ME), "{name}");
        }
    }

    #[test]
    fn network_peers_are_never_trusted() {
        assert!(!is_trusted_loopback("192.168.1.9:50000".parse().unwrap(), 59100));
        assert!(!is_trusted_loopback("[::1]:50000".parse().unwrap(), 59100), "IPv6 loopback cannot be identified");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn this_process_is_recognised_by_pid() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (_server, peer) = listener.accept().await.unwrap();
        let found = loopback_owner(peer, port).expect("own connection is in the TCP table");
        assert_eq!(found.pid, std::process::id());
        assert!(is_trusted_loopback(peer, port), "the in-process bridge is trusted");
        drop(client);
    }

    /// A different local program (PowerShell here) connecting to the port must pair.
    #[cfg(windows)]
    #[tokio::test]
    async fn another_local_process_is_not_trusted() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = format!(
            "$c = New-Object System.Net.Sockets.TcpClient('127.0.0.1', {port}); Start-Sleep -Seconds 5; $c.Close()"
        );
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("powershell is available on Windows");
        let accepted = tokio::time::timeout(std::time::Duration::from_secs(20), listener.accept()).await;
        let (_server, peer) = accepted.expect("child connected").unwrap();
        let found = tokio::task::spawn_blocking(move || loopback_owner(peer, port)).await.unwrap().expect("owner found");
        assert_ne!(found.pid, std::process::id());
        assert!(found.image.as_deref().is_some_and(|i| i.to_ascii_lowercase().ends_with("powershell.exe")), "{found:?}");
        assert!(found.same_user, "child runs as this user");
        assert!(!is_trusted_loopback(peer, port));
        let _ = child.kill();
    }
}
