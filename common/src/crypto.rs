//! Session security for wireless links.
//!
//! Authentication is a SPAKE2 password-authenticated key exchange, so a PIN that is only a few
//! digits long cannot be recovered offline from a captured handshake. The password is the
//! one-time pairing PIN the first time a tablet connects, and a long random per-device secret
//! (issued during pairing) on every later connection.
//!
//! The SPAKE2 key is confirmed in both directions (proving each side knew the password) and then
//! expanded into one ChaCha20-Poly1305 key per direction. Nonces are packet counters, so a nonce
//! is never reused under a key; the 24-byte packet header is authenticated as AAD.

use chacha20poly1305::{
    aead::{AeadInPlace, KeyInit},
    ChaCha20Poly1305, Key, Nonce, Tag,
};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use crate::protocol::{flags, PacketHeader};

pub const TAG_LEN: usize = 16;
pub const HOST_ID_LEN: usize = 16;
pub const CLIENT_ID_LEN: usize = 16;
pub const DEVICE_SECRET_LEN: usize = 32;
pub const CONFIRM_LEN: usize = 32;

pub type HostId = [u8; HOST_ID_LEN];
pub type ClientId = [u8; CLIENT_ID_LEN];
pub type DeviceSecret = [u8; DEVICE_SECRET_LEN];

#[derive(Debug, PartialEq, Eq)]
pub enum CryptoError {
    /// Tag or key confirmation did not verify: wrong password, tampering, or desynchronized stream.
    AuthenticationFailed,
    /// Malformed key-exchange message.
    InvalidPayload,
    /// The operating system could not provide randomness.
    Random,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::AuthenticationFailed => write!(f, "authentication failed"),
            CryptoError::InvalidPayload => write!(f, "invalid key-exchange payload"),
            CryptoError::Random => write!(f, "system randomness unavailable"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Which end of the link a [`PakeKey`] / channel belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Host,
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut buf = [0u8; N];
    getrandom::getrandom(&mut buf).map_err(|_| CryptoError::Random)?;
    Ok(buf)
}

/// Uniformly random 6-digit pairing PIN.
pub fn random_pin() -> Result<String, CryptoError> {
    // Rejection sampling keeps every PIN equally likely.
    const LIMIT: u32 = u32::MAX - (u32::MAX % 1_000_000);
    loop {
        let v = u32::from_le_bytes(random_bytes::<4>()?);
        if v < LIMIT {
            return Ok(format!("{:06}", v % 1_000_000));
        }
    }
}

const ID_CLIENT: &[u8] = b"xdisplay-client";

fn identities(host_id: &HostId) -> (Identity, Identity) {
    (Identity::new(ID_CLIENT), Identity::new(host_id))
}

/// Everything both sides have exchanged in the clear; confirmations and session keys bind to it.
pub fn transcript(client_id: &ClientId, host_id: &HostId, client_msg: &[u8], host_msg: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(64 + client_msg.len() + host_msg.len());
    t.extend_from_slice(b"xdisplay-handshake-v2");
    t.extend_from_slice(client_id);
    t.extend_from_slice(host_id);
    t.extend_from_slice(&(client_msg.len() as u16).to_le_bytes());
    t.extend_from_slice(client_msg);
    t.extend_from_slice(&(host_msg.len() as u16).to_le_bytes());
    t.extend_from_slice(host_msg);
    t
}

/// Client side of the key exchange (SPAKE2 side A).
pub struct PakeClient(Spake2<Ed25519Group>);

impl PakeClient {
    /// Returns the state and the message to send to the host.
    pub fn start(password: &[u8], host_id: &HostId) -> (Self, Vec<u8>) {
        let (id_a, id_b) = identities(host_id);
        let (state, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(password), &id_a, &id_b);
        (Self(state), msg)
    }

    pub fn finish(self, host_msg: &[u8]) -> Result<PakeKey, CryptoError> {
        self.0.finish(host_msg).map(PakeKey).map_err(|_| CryptoError::InvalidPayload)
    }
}

/// Host side (SPAKE2 side B): consumes the client's message and returns the reply plus the key.
pub fn host_respond(password: &[u8], host_id: &HostId, client_msg: &[u8]) -> Result<(Vec<u8>, PakeKey), CryptoError> {
    let (id_a, id_b) = identities(host_id);
    let (state, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(password), &id_a, &id_b);
    let key = state.finish(client_msg).map_err(|_| CryptoError::InvalidPayload)?;
    Ok((msg, PakeKey(key)))
}

/// Shared secret from the key exchange. Equal on both sides only if the passwords matched.
pub struct PakeKey(Vec<u8>);

type HmacSha256 = Hmac<Sha256>;

impl PakeKey {
    fn mac(&self, role: Role, transcript: &[u8]) -> HmacSha256 {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(match role {
            Role::Client => b"xdisplay-confirm-client",
            Role::Host => b"xdisplay-confirm-host\0\0",
        });
        mac.update(transcript);
        mac
    }

    /// Proof that the holder of this key took part in the exchange as `role`.
    pub fn confirmation(&self, role: Role, transcript: &[u8]) -> [u8; CONFIRM_LEN] {
        self.mac(role, transcript).finalize().into_bytes().into()
    }

    /// Constant-time check of the other side's proof.
    pub fn verify(&self, role: Role, transcript: &[u8], proof: &[u8]) -> bool {
        self.mac(role, transcript).verify_slice(proof).is_ok()
    }

    /// Expands into the two directional ciphers for `role`.
    pub fn into_channel(self, role: Role, transcript: &[u8]) -> (Sealer, Opener) {
        let salt = Sha256::digest(transcript);
        let hk = Hkdf::<Sha256>::new(Some(&salt), &self.0);
        let mut c2h = [0u8; 32];
        let mut h2c = [0u8; 32];
        hk.expand(b"xdisplay client->host", &mut c2h).expect("32 bytes is a valid HKDF length");
        hk.expand(b"xdisplay host->client", &mut h2c).expect("32 bytes is a valid HKDF length");
        let (send, recv) = match role {
            Role::Client => (c2h, h2c),
            Role::Host => (h2c, c2h),
        };
        (Sealer::new(&send), Opener::new(&recv))
    }
}

fn nonce_for(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

/// Encrypts outgoing packets. The nonce is a per-direction counter, so every packet must go
/// through the same `Sealer` in the order it is written to the socket.
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), counter: 0 }
    }

    /// Encrypts `payload` in place, appends the tag, and updates `header` (flag and length).
    pub fn seal(&mut self, header: &mut PacketHeader, payload: &mut Vec<u8>) -> Result<(), CryptoError> {
        header.flags |= flags::ENCRYPTED;
        header.payload_len = (payload.len() + TAG_LEN) as u32;
        let aad = header.to_bytes();
        let tag = self
            .cipher
            .encrypt_in_place_detached(&nonce_for(self.counter), &aad, payload)
            .map_err(|_| CryptoError::AuthenticationFailed)?;
        self.counter += 1;
        payload.extend_from_slice(&tag);
        Ok(())
    }
}

/// Decrypts incoming packets; the counter advances once per packet, mirroring the sender.
pub struct Opener {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Opener {
    fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), counter: 0 }
    }

    /// Verifies and decrypts `payload` (ciphertext + tag) in place, leaving the plaintext.
    pub fn open(&mut self, header: &PacketHeader, payload: &mut Vec<u8>) -> Result<(), CryptoError> {
        if header.flags & flags::ENCRYPTED == 0 || payload.len() < TAG_LEN {
            return Err(CryptoError::AuthenticationFailed);
        }
        let split = payload.len() - TAG_LEN;
        let tag = Tag::clone_from_slice(&payload[split..]);
        payload.truncate(split);
        let aad = header.to_bytes();
        self.cipher
            .decrypt_in_place_detached(&nonce_for(self.counter), &aad, payload, &tag)
            .map_err(|_| CryptoError::AuthenticationFailed)?;
        self.counter += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::msg_types;

    const CLIENT: ClientId = [1; CLIENT_ID_LEN];
    const HOST: HostId = [2; HOST_ID_LEN];

    /// Runs both sides of the handshake; returns each side's key.
    fn exchange(client_pw: &[u8], host_pw: &[u8]) -> (PakeKey, PakeKey, Vec<u8>) {
        let (client, client_msg) = PakeClient::start(client_pw, &HOST);
        let (host_msg, host_key) = host_respond(host_pw, &HOST, &client_msg).unwrap();
        let client_key = client.finish(&host_msg).unwrap();
        let t = transcript(&CLIENT, &HOST, &client_msg, &host_msg);
        (client_key, host_key, t)
    }

    #[test]
    fn matching_passwords_confirm_both_ways() {
        let (ck, hk, t) = exchange(b"123456", b"123456");
        assert!(hk.verify(Role::Client, &t, &ck.confirmation(Role::Client, &t)));
        assert!(ck.verify(Role::Host, &t, &hk.confirmation(Role::Host, &t)));
    }

    #[test]
    fn wrong_password_fails_confirmation() {
        let (ck, hk, t) = exchange(b"123456", b"654321");
        assert!(!hk.verify(Role::Client, &t, &ck.confirmation(Role::Client, &t)));
        assert!(!ck.verify(Role::Host, &t, &hk.confirmation(Role::Host, &t)));
    }

    #[test]
    fn confirmations_are_role_bound() {
        let (ck, hk, t) = exchange(b"123456", b"123456");
        // A reflected proof must not be accepted as the other side's.
        assert!(!hk.verify(Role::Client, &t, &ck.confirmation(Role::Host, &t)));
    }

    fn channel_pair() -> ((Sealer, Opener), (Sealer, Opener)) {
        let (ck, hk, t) = exchange(b"pw", b"pw");
        (ck.into_channel(Role::Client, &t), hk.into_channel(Role::Host, &t))
    }

    fn header(seq: u32) -> PacketHeader {
        PacketHeader::new(msg_types::VIDEO_RAW, flags::NONE, 0, seq, 0, 7)
    }

    #[test]
    fn packets_roundtrip_in_both_directions() {
        let ((mut c_seal, mut c_open), (mut h_seal, mut h_open)) = channel_pair();
        for seq in 0..5u32 {
            let mut hdr = header(seq);
            let mut buf = b"frame data".to_vec();
            h_seal.seal(&mut hdr, &mut buf).unwrap();
            assert_eq!(hdr.payload_len as usize, buf.len());
            assert_ne!(&buf[..10], b"frame data");
            c_open.open(&hdr, &mut buf).unwrap();
            assert_eq!(buf, b"frame data");

            let mut hdr = header(seq);
            let mut buf = b"pen event".to_vec();
            c_seal.seal(&mut hdr, &mut buf).unwrap();
            h_open.open(&hdr, &mut buf).unwrap();
            assert_eq!(buf, b"pen event");
        }
    }

    #[test]
    fn identical_plaintexts_never_repeat_ciphertext() {
        let ((_, _), (mut h_seal, _)) = channel_pair();
        let (mut a, mut b) = (b"same".to_vec(), b"same".to_vec());
        h_seal.seal(&mut header(0), &mut a).unwrap();
        h_seal.seal(&mut header(0), &mut b).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn tampering_is_detected() {
        let ((_, mut c_open), (mut h_seal, _)) = channel_pair();
        let mut hdr = header(1);
        let mut buf = b"payload".to_vec();
        h_seal.seal(&mut hdr, &mut buf).unwrap();

        let mut bad_body = buf.clone();
        bad_body[0] ^= 1;
        assert!(c_open.open(&hdr, &mut bad_body).is_err());

        // Failed attempts must not advance the counter: the genuine packet still opens.
        let mut bad_hdr = hdr;
        bad_hdr.sequence += 1;
        assert!(c_open.open(&bad_hdr, &mut buf.clone()).is_err());
        c_open.open(&hdr, &mut buf).unwrap();
    }

    #[test]
    fn replay_and_reorder_are_rejected() {
        let ((_, mut c_open), (mut h_seal, _)) = channel_pair();
        let (mut h0, mut b0) = (header(0), b"zero".to_vec());
        let (mut h1, mut b1) = (header(1), b"one".to_vec());
        h_seal.seal(&mut h0, &mut b0).unwrap();
        h_seal.seal(&mut h1, &mut b1).unwrap();
        assert!(c_open.open(&h1, &mut b1.clone()).is_err()); // skipped packet 0
        c_open.open(&h0, &mut b0.clone()).unwrap();
        assert!(c_open.open(&h0, &mut b0).is_err()); // replay
    }

    #[test]
    fn pins_are_six_digits() {
        for _ in 0..50 {
            let pin = random_pin().unwrap();
            assert_eq!(pin.len(), 6);
            assert!(pin.bytes().all(|b| b.is_ascii_digit()));
        }
    }
}
