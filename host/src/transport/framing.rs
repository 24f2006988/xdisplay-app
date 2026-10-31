//! Packet framing over a byte stream, with optional per-packet encryption.

use std::io::IoSlice;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use xdisplay_common::protocol::{flags, PacketHeader, HEADER_SIZE, PROTOCOL_VERSION};
use xdisplay_common::{Opener, Sealer};

/// Writes header and payload with as few syscalls as possible (normally one), so the 24-byte
/// header never goes out as its own TCP segment under TCP_NODELAY.
pub async fn write_packet<W: AsyncWrite + Unpin>(
    writer: &mut W,
    header: &PacketHeader,
    payload: &[u8],
) -> std::io::Result<()> {
    let hdr = header.to_bytes();
    let mut bufs = [IoSlice::new(&hdr), IoSlice::new(payload)];
    let mut slices: &mut [IoSlice<'_>] = &mut bufs;
    while !slices.is_empty() {
        let n = writer.write_vectored(slices).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        IoSlice::advance_slices(&mut slices, n);
    }
    writer.flush().await
}

/// Reads one packet at a time into a reused buffer; decrypts when the session is encrypted.
pub struct PacketReader<R> {
    reader: R,
    opener: Option<Opener>,
    header_buf: [u8; HEADER_SIZE],
    /// Plaintext payload of the packet returned by the last `read`.
    pub payload: Vec<u8>,
}

impl<R: AsyncRead + Unpin> PacketReader<R> {
    pub fn new(reader: R, opener: Option<Opener>) -> Self {
        Self { reader, opener, header_buf: [0; HEADER_SIZE], payload: Vec::with_capacity(4096) }
    }

    /// Switches the stream to encrypted mode once the handshake has produced keys.
    pub fn set_opener(&mut self, opener: Option<Opener>) {
        self.opener = opener;
    }

    /// Reads the next packet. Fails if nothing arrives within `idle` (a dead Wi-Fi link never
    /// sends a FIN, so without this a vanished tablet would hold the session forever).
    pub async fn read(&mut self, idle: Duration, max_payload: usize) -> Result<PacketHeader> {
        tokio::time::timeout(idle, self.reader.read_exact(&mut self.header_buf))
            .await
            .context("link idle")?
            .context("read header")?;
        let header = PacketHeader::from_bytes(&self.header_buf).context("bad protocol magic")?;
        if header.version != PROTOCOL_VERSION {
            bail!("protocol version {} (expected {})", header.version, PROTOCOL_VERSION);
        }
        let len = header.payload_len as usize;
        if len > max_payload {
            bail!("packet of {len} bytes exceeds the {max_payload} byte limit");
        }
        self.payload.resize(len, 0);
        tokio::time::timeout(idle, self.reader.read_exact(&mut self.payload))
            .await
            .context("link idle")?
            .context("read payload")?;

        let encrypted = header.flags & flags::ENCRYPTED != 0;
        match (&mut self.opener, encrypted) {
            (Some(opener), true) => opener.open(&header, &mut self.payload).context("packet failed authentication")?,
            // A secured session never falls back to plaintext, and a plain one never expects tags.
            (Some(_), false) => bail!("plaintext packet on an encrypted session"),
            (None, true) => bail!("encrypted packet on a plaintext session"),
            (None, false) => {}
        }
        Ok(header)
    }
}

/// Writes packets, encrypting them when the session is secured.
pub struct PacketWriter<W> {
    writer: W,
    sealer: Option<Sealer>,
    scratch: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> PacketWriter<W> {
    pub fn new(writer: W, sealer: Option<Sealer>) -> Self {
        Self { writer, sealer, scratch: Vec::new() }
    }

    pub async fn send(&mut self, mut header: PacketHeader, payload: &[u8]) -> std::io::Result<()> {
        match &mut self.sealer {
            None => write_packet(&mut self.writer, &header, payload).await,
            Some(sealer) => {
                self.scratch.clear();
                self.scratch.extend_from_slice(payload);
                sealer
                    .seal(&mut header, &mut self.scratch)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                write_packet(&mut self.writer, &header, &self.scratch).await
            }
        }
    }
}
