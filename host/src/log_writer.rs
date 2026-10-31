//! Log output that can never stall the stream.
//!
//! The host writes its log to whatever console launched it. A Windows console write blocks while
//! text is selected in the window (QuickEdit) or while nobody drains a redirected console, and
//! every thread that logged then waited with it: capture, encoding and the network send loop fell
//! to a few frames per second, with updates reaching the tablet hundreds of milliseconds late.
//! Lines are now handed to a writer thread and dropped when it falls behind.
//!
//! The installed host has no console, so the same thread also appends every line to
//! `%LOCALAPPDATA%\XDisplay\logs\xdisplay-host.log` (the previous run is kept as `.old.log`).

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::thread;
use tracing_subscriber::fmt::MakeWriter;

/// Lines buffered while the console is not taking output.
const QUEUE_LINES: usize = 1024;
/// The log file starts over once it reaches this size, so a long session cannot fill the disk.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

static DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct NonBlockingStdout {
    tx: SyncSender<Vec<u8>>,
}

impl NonBlockingStdout {
    pub fn new() -> Self {
        static TX: OnceLock<SyncSender<Vec<u8>>> = OnceLock::new();
        let tx = TX
            .get_or_init(|| {
                let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_LINES);
                let _ = thread::Builder::new().name("xdisplay-log".into()).spawn(move || {
                    let mut out = io::stdout();
                    let mut file = LogFile::open();
                    for line in rx {
                        let dropped = DROPPED.swap(0, Ordering::Relaxed);
                        if dropped > 0 {
                            let note = format!("({dropped} log lines dropped while the output was blocked)\n");
                            let _ = out.write_all(note.as_bytes());
                            if let Some(f) = file.as_mut() {
                                f.write(note.as_bytes());
                            }
                        }
                        if let Some(f) = file.as_mut() {
                            f.write(&line);
                        }
                        let _ = out.write_all(&line);
                        let _ = out.flush();
                    }
                });
                tx
            })
            .clone();
        Self { tx }
    }

    fn submit(&self, line: Vec<u8>) {
        match self.tx.try_send(line) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// Folder that holds the host's log files.
pub fn log_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("XDisplay").join("logs"))
}

/// Append-only log file with a size cap.
struct LogFile {
    path: PathBuf,
    file: File,
    written: u64,
}

impl LogFile {
    fn open() -> Option<Self> {
        let dir = log_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("xdisplay-host.log");
        // Keep the previous run for bug reports. A second host instance exits before it logs.
        let _ = std::fs::rename(&path, dir.join("xdisplay-host.old.log"));
        let file = File::create(&path).ok()?;
        Some(Self { path, file, written: 0 })
    }

    fn write(&mut self, data: &[u8]) {
        if self.written + data.len() as u64 > MAX_LOG_BYTES {
            if let Ok(f) = File::create(&self.path) {
                self.file = f;
                self.written = 0;
            }
        }
        if self.file.write_all(data).is_ok() {
            self.written += data.len() as u64;
        }
    }
}

/// Reports an error that stops the host. The installed app has no console, so the message is
/// also shown in a dialog.
pub fn report_fatal(message: &str) {
    tracing::error!("{message}");
    eprintln!("{message}");
    #[cfg(windows)]
    unsafe {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
        let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "XDisplay".encode_utf16().chain(Some(0)).collect();
        MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

/// Collects one formatted event and queues it when the formatter is done with it.
pub struct LineWriter {
    sink: NonBlockingStdout,
    buf: Vec<u8>,
}

impl Write for LineWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            self.sink.submit(std::mem::take(&mut self.buf));
        }
    }
}

impl<'a> MakeWriter<'a> for NonBlockingStdout {
    type Writer = LineWriter;

    fn make_writer(&'a self) -> LineWriter {
        LineWriter { sink: self.clone(), buf: Vec::with_capacity(256) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_stalled_console_never_blocks_the_caller() {
        // Nothing drains this channel, like a console that stopped taking output.
        let (tx, _rx) = sync_channel::<Vec<u8>>(4);
        let sink = NonBlockingStdout { tx };
        let start = Instant::now();
        for _ in 0..10_000 {
            let mut w = sink.make_writer();
            w.write_all(b"pipeline: stats line\n").unwrap();
        }
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(DROPPED.load(Ordering::Relaxed) >= 9_000);
    }
}
