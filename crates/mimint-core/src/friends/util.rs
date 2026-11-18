//! Shared helpers for the friends protocol (Phase 7).
//!
//! Hex encoding, constant-time comparison, randomness, base64, and
//! an injectable clock. No secrets are ever logged from this module.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Lowercase hex encoding (same shape as `hex_lower` in the Android
/// protocol module; re-implemented here so `mimint-core` does not
/// depend on `mimint-body`).
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode hex (either case). `None` on odd length or a non-hex digit.
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = hex_value(pair[0])?;
        let lo = hex_value(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Compare two byte strings without early exit on the first
/// differing byte (same discipline as `tokens_equal` in the Android
/// protocol module). Lengths are compared first.
pub fn ct_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Fill `buf` with randomness from the operating system.
pub fn random_bytes(buf: &mut [u8]) {
    use ring::rand::{SecureRandom, SystemRandom};
    SystemRandom::new()
        .fill(buf)
        .expect("SystemRandom must be available");
}

/// `n` random bytes, hex-encoded (2*n characters).
pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    random_bytes(&mut buf);
    hex_encode(&buf)
}

/// Standard base64 encoding (RFC 4648 alphabet with `=` padding).
/// Implemented by hand: the tree has no base64 dependency, and the
/// WebSocket handshake (RFC 6455) needs it for `Sec-WebSocket-Accept`.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Standard base64 decoding. `None` on bad padding, bad alphabet,
/// or trailing garbage. Whitespace is not skipped.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.as_chunks::<4>().0 {
        let mut values = [0u32; 4];
        let mut padding = 0usize;
        for (i, &byte) in chunk.iter().enumerate() {
            if byte == b'=' {
                padding += 1;
                values[i] = 0;
            } else {
                if padding > 0 {
                    return None;
                }
                values[i] = base64_value(byte)? as u32;
            }
        }
        if padding > 2 {
            return None;
        }
        let triple = (values[0] << 18) | (values[1] << 12) | (values[2] << 6) | values[3];
        out.push((triple >> 16) as u8);
        if padding < 2 {
            out.push((triple >> 8) as u8);
        }
        if padding < 1 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// An injectable clock (epoch milliseconds). Production uses the
/// system clock; tests use a manual clock to prove expiry, lockout,
/// tombstone purge, and lease behaviour without sleeping.
#[derive(Clone)]
pub struct Clock {
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl Clock {
    /// The system clock.
    pub fn system() -> Self {
        Self {
            now: Arc::new(crate::audit::now_ms),
        }
    }

    /// A clock fixed at one instant.
    pub fn fixed(ms: u64) -> Self {
        Self {
            now: Arc::new(move || ms),
        }
    }

    /// A clock backed by a shared atomic: tests advance it.
    pub fn manual(start_ms: u64) -> (Self, Arc<AtomicU64>) {
        let cell = Arc::new(AtomicU64::new(start_ms));
        let reader = Arc::clone(&cell);
        (
            Self {
                now: Arc::new(move || reader.load(Ordering::SeqCst)),
            },
            cell,
        )
    }

    /// Current time in epoch milliseconds.
    pub fn now_ms(&self) -> u64 {
        (self.now)()
    }
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Clock")
            .field("now_ms", &self.now_ms())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip_and_rejects_bad_input() {
        assert_eq!(hex_encode(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex_decode("00abff"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(hex_decode("00ABFF"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(hex_decode("0"), None);
        assert_eq!(hex_decode("zz"), None);
        assert_eq!(hex_decode(""), Some(vec![]));
    }

    #[test]
    fn ct_equal_behaves() {
        assert!(ct_equal(b"abc", b"abc"));
        assert!(!ct_equal(b"abc", b"abd"));
        assert!(!ct_equal(b"abc", b"abcd"));
        assert!(ct_equal(b"", b""));
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        // RFC 4648 §10 test vectors.
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_decode(encoded), Some(plain.as_bytes().to_vec()));
        }
        assert_eq!(base64_decode("Zm9v!"), None);
        assert_eq!(base64_decode("Zg"), None);
        assert_eq!(base64_decode("Zm=v"), None);
    }

    #[test]
    fn manual_clock_advances() {
        let (clock, cell) = Clock::manual(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        cell.fetch_add(500, Ordering::SeqCst);
        assert_eq!(clock.now_ms(), 1_500);
    }
}
