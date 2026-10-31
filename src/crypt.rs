//! Room-key encryption: optional XChaCha20-Poly1305 envelope around every
//! UDP packet. Key = SHA-256("lanvc:" + passphrase). Empty passphrase =
//! no envelope (plain LAN mode, byte-compatible with keyless peers).

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

const ENVELOPE_MAGIC: &[u8; 4] = b"LVE1";

/// Cached cipher for a passphrase; rebuilds only when the text changes.
#[derive(Default)]
pub struct KeyCache {
    raw: String,
    cipher: Option<XChaCha20Poly1305>,
    counter: AtomicU64,
}

impl KeyCache {
    /// Returns the cipher for `passphrase` (None = plaintext mode).
    pub fn refresh(&mut self, passphrase: &str) -> Option<&XChaCha20Poly1305> {
        if self.raw != passphrase {
            self.raw = passphrase.to_string();
            self.cipher = if passphrase.is_empty() {
                None
            } else {
                let hash = Sha256::digest(format!("lanvc:{passphrase}").as_bytes());
                Some(XChaCha20Poly1305::new((&hash).into()))
            };
        }
        self.cipher.as_ref()
    }

    /// Wrap `msg` for sending from `sender_session` (nonce uniqueness comes
    /// from session id + monotonic counter under each key).
    pub fn seal(&mut self, cipher: Option<&XChaCha20Poly1305>, msg: &[u8], sender_session: u32) -> Vec<u8> {
        let Some(cipher) = cipher else {
            return msg.to_vec();
        };
        let c = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut nonce = [0u8; 24];
        nonce[0..4].copy_from_slice(&sender_session.to_le_bytes());
        nonce[4..12].copy_from_slice(&c.to_le_bytes());
        let ct = cipher
            .encrypt(XNonce::from_slice(&nonce), Payload { msg, aad: ENVELOPE_MAGIC })
            .expect("xchacha20poly1305 encrypt");
        let mut out = Vec::with_capacity(4 + nonce.len() + ct.len());
        out.extend_from_slice(ENVELOPE_MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// Open an incoming datagram; None for plaintext-mode passthrough is NOT
    /// handled here — callers decide via their own cipher presence.
    pub fn open(cipher: &XChaCha20Poly1305, buf: &[u8]) -> Option<Vec<u8>> {
        if buf.len() < 4 + 24 + 16 || &buf[..4] != ENVELOPE_MAGIC {
            return None;
        }
        let nonce = XNonce::from_slice(&buf[4..28]);
        cipher
            .decrypt(nonce, Payload { msg: &buf[28..], aad: ENVELOPE_MAGIC })
            .ok()
    }
}
