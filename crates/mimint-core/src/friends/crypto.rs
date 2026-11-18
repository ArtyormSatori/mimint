//! Cryptography for the friends protocol (Phase 7, §2).
//!
//! One cipher suite, no negotiation: P-256 ECDH (ephemeral) +
//! HKDF-SHA256 + AES-256-GCM + ECDSA P-256 (SHA-256, ASN.1).
//! Everything goes through `ring` 0.17.14. HKDF is spelled out
//! with `ring::hmac` (Extract + one-block Expand for 32-byte
//! output) so the design's fixed test vector — including the
//! intermediate PRK, which `ring::hkdf` does not expose — can be
//! locked byte-for-byte.
//!
//! The single direction convention (§2.2): in a pair of bodies,
//! the side with the smaller `device_id` (byte comparison) is A,
//! the other is B. This convention drives BOTH the AEAD nonce
//! prefixes AND the HKDF info strings. TCP roles (initiator /
//! listener) never appear in any crypto string.
//!
//! No secret, key, nonce, or SAS value is ever logged or placed
//! in an error message from this module.

use crate::error::MimintError;
use crate::friends::util::{hex_encode, random_bytes};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::agreement::{
    agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey as AgreementPublicKey, ECDH_P256,
};
use ring::digest::{digest, SHA256};
use ring::rand::SystemRandom;
use ring::signature::{
    EcdsaKeyPair, KeyPair, UnparsedPublicKey as SignaturePublicKey, ECDSA_P256_SHA256_ASN1,
    ECDSA_P256_SHA256_ASN1_SIGNING,
};
use zeroize::Zeroizing;

/// Length of an uncompressed P-256 public key point.
pub const PUBKEY_LEN: usize = 65;

/// Length of a session key (AES-256).
pub const SESSION_KEY_LEN: usize = 32;

/// Length of an AEAD nonce.
pub const NONCE_LEN: usize = 12;

/// Length of a handshake/ceremony nonce.
pub const HELLO_NONCE_LEN: usize = 16;

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let out = digest(&SHA256, data);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(out.as_ref());
    hash
}

/// HMAC-SHA256.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key);
    let tag = ring::hmac::sign(&key, message);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

/// HKDF-Extract (RFC 5869 §2.2): PRK = HMAC-SHA256(salt, IKM).
pub fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, ikm)
}

/// HKDF-Expand for exactly 32 bytes of output (RFC 5869 §2.3):
/// T(1) = HMAC-SHA256(PRK, info || 0x01). One block suffices
/// because the hash output is 32 bytes.
pub fn hkdf_expand_32(prk: &[u8; 32], info: &[u8]) -> [u8; 32] {
    let mut message = Vec::with_capacity(info.len() + 1);
    message.extend_from_slice(info);
    message.push(0x01);
    hmac_sha256(prk, &message)
}

/// Full HKDF-SHA256 to 32 bytes: Extract then Expand.
pub fn hkdf_sha256_32(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let prk = hkdf_extract(salt, ikm);
    hkdf_expand_32(&prk, info)
}

/// Direction of travel in a pairwise session, per the §2.2
/// convention (A = smaller device_id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    AtoB,
    BtoA,
}

impl Direction {
    /// The direction for a message sent by the local side.
    /// `local_is_a` is true when the local `device_id` is the
    /// smaller of the pair.
    pub fn for_sender(local_is_a: bool) -> Self {
        if local_is_a {
            Direction::AtoB
        } else {
            Direction::BtoA
        }
    }

    /// The direction for a message received by the local side.
    pub fn for_receiver(local_is_a: bool) -> Self {
        if local_is_a {
            Direction::BtoA
        } else {
            Direction::AtoB
        }
    }

    /// The 4-byte nonce prefix for this direction (§2.2).
    pub fn nonce_prefix(self) -> [u8; 4] {
        match self {
            Direction::AtoB => *b"A->B",
            Direction::BtoA => *b"B->A",
        }
    }

    /// The HKDF info string for this direction's session key.
    /// `rekey_n` is `None` for the initial keys, `Some(n)` for
    /// the n-th rekey of the session (§2.4 step 6).
    pub fn hkdf_info(self, rekey_n: Option<u64>) -> Vec<u8> {
        let base = match self {
            Direction::AtoB => "mimint friends v2 session A->B",
            Direction::BtoA => "mimint friends v2 session B->A",
        };
        match rekey_n {
            None => base.as_bytes().to_vec(),
            Some(n) => format!("{base}/rekey/{n}").into_bytes(),
        }
    }
}

/// Build an AEAD nonce: 4-byte direction prefix + 8-byte
/// big-endian counter, starting from 0 (§2.2). Never reused
/// within a session key epoch.
pub fn make_nonce(direction: Direction, counter: u64) -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..4].copy_from_slice(&direction.nonce_prefix());
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// A session key: 32 bytes, zeroized on drop. `LessSafeKey` is
/// built per operation (cheap); the raw bytes stay in this
/// wrapper so they are wiped when the session ends.
#[derive(Clone)]
pub struct SessionKey(Zeroizing<[u8; SESSION_KEY_LEN]>);

impl SessionKey {
    pub fn from_bytes(bytes: [u8; SESSION_KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Seal `plaintext`, appending the 16-byte GCM tag.
    pub fn seal(&self, nonce: &[u8; NONCE_LEN], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, self.0.as_ref()).expect("32-byte AES-256-GCM key"),
        );
        let mut buf = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(*nonce),
            Aad::from(aad),
            &mut buf,
        )
        .expect("sealing with a valid key cannot fail");
        buf
    }

    /// Open `ciphertext` (ciphertext + 16-byte tag). Any failure
    /// — wrong key, tampered ciphertext, wrong AAD — is a single
    /// `None`, with no detail about which check failed.
    pub fn open(&self, nonce: &[u8; NONCE_LEN], aad: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, self.0.as_ref()).expect("32-byte AES-256-GCM key"),
        );
        let mut buf = ciphertext.to_vec();
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(*nonce),
                Aad::from(aad),
                &mut buf,
            )
            .ok()?;
        Some(plain.to_vec())
    }

    /// Zeroize the key material now (also happens on drop).
    pub fn zeroize(&mut self) {
        self.0 = Zeroizing::new([0u8; SESSION_KEY_LEN]);
    }
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionKey([redacted])")
    }
}

/// Derive the two directional session keys from an ECDH shared
/// secret, following §2.4 step 4: salt = SHA-256(nonce_A ||
/// nonce_B), info per direction. Returns (key A->B, key B->A)
/// as raw bytes; callers wrap them in [`SessionKey`] at once.
pub fn derive_directional_key_bytes(
    shared_secret: &[u8],
    nonce_a: &[u8; HELLO_NONCE_LEN],
    nonce_b: &[u8; HELLO_NONCE_LEN],
    rekey_n: Option<u64>,
) -> ([u8; SESSION_KEY_LEN], [u8; SESSION_KEY_LEN]) {
    let mut salt_input = [0u8; 32];
    salt_input[..16].copy_from_slice(nonce_a);
    salt_input[16..].copy_from_slice(nonce_b);
    let salt = sha256(&salt_input);
    let prk = hkdf_extract(&salt, shared_secret);
    let key_ab = hkdf_expand_32(&prk, &Direction::AtoB.hkdf_info(rekey_n));
    let key_ba = hkdf_expand_32(&prk, &Direction::BtoA.hkdf_info(rekey_n));
    (key_ab, key_ba)
}

/// Derive the two directional session keys, wrapped.
pub fn derive_directional_keys(
    shared_secret: &[u8],
    nonce_a: &[u8; HELLO_NONCE_LEN],
    nonce_b: &[u8; HELLO_NONCE_LEN],
    rekey_n: Option<u64>,
) -> (SessionKey, SessionKey) {
    let (ab, ba) = derive_directional_key_bytes(shared_secret, nonce_a, nonce_b, rekey_n);
    (SessionKey::from_bytes(ab), SessionKey::from_bytes(ba))
}

/// Generate a fresh ephemeral ECDH keypair. The private key is
/// consumed by [`agree_and_derive`]; the public key (65 bytes,
/// uncompressed) goes on the wire.
pub fn generate_ephemeral() -> (EphemeralPrivateKey, [u8; PUBKEY_LEN]) {
    let rng = SystemRandom::new();
    let private = EphemeralPrivateKey::generate(&ECDH_P256, &rng).expect("ECDH keygen");
    let public = private.compute_public_key().expect("ECDH public key");
    let mut bytes = [0u8; PUBKEY_LEN];
    bytes.copy_from_slice(public.as_ref());
    (private, bytes)
}

/// Agree on the shared secret with the peer's ephemeral public
/// key and derive both directional session keys inside the
/// agreement closure, so the shared secret never leaves `ring`.
/// Returns (key A->B, key B->A).
pub fn agree_and_derive(
    mine: EphemeralPrivateKey,
    peer_public: &[u8; PUBKEY_LEN],
    nonce_a: &[u8; HELLO_NONCE_LEN],
    nonce_b: &[u8; HELLO_NONCE_LEN],
    rekey_n: Option<u64>,
) -> Result<(SessionKey, SessionKey), MimintError> {
    let peer = AgreementPublicKey::new(&ECDH_P256, peer_public.as_slice());
    agree_ephemeral(mine, &peer, |shared| {
        derive_directional_keys(shared, nonce_a, nonce_b, rekey_n)
    })
    .map_err(|_| MimintError::Config("ECDH agreement failed".to_string()))
}

/// Generate a fresh identity keypair and return its PKCS#8 DER
/// encoding (zeroized on drop). The DER is what
/// [`crate::friends::identity::DeviceKeyStore`] persists.
pub fn generate_identity_pkcs8() -> Zeroizing<Vec<u8>> {
    let rng = SystemRandom::new();
    let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .expect("identity keygen");
    Zeroizing::new(document.as_ref().to_vec())
}

/// The uncompressed public key (65 bytes) for a PKCS#8 identity key.
pub fn identity_public_key(pkcs8_der: &[u8]) -> Result<[u8; PUBKEY_LEN], MimintError> {
    let rng = SystemRandom::new();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8_der, &rng)
        .map_err(|_| MimintError::Config("invalid identity key".to_string()))?;
    let mut bytes = [0u8; PUBKEY_LEN];
    bytes.copy_from_slice(pair.public_key().as_ref());
    Ok(bytes)
}

/// Sign `message` with an identity key. Returns the ASN.1 DER
/// signature.
pub fn identity_sign(pkcs8_der: &[u8], message: &[u8]) -> Result<Vec<u8>, MimintError> {
    let rng = SystemRandom::new();
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8_der, &rng)
        .map_err(|_| MimintError::Config("invalid identity key".to_string()))?;
    let signature = pair
        .sign(&rng, message)
        .map_err(|_| MimintError::Config("signing failed".to_string()))?;
    Ok(signature.as_ref().to_vec())
}

/// Verify an ECDSA signature against a public key. `false` on
/// any failure (bad key, bad signature, bad encoding).
pub fn identity_verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let key = SignaturePublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key);
    key.verify(message, signature).is_ok()
}

/// Transcript hash (§2.4): SHA-256 over the length-prefixed
/// concatenation `u32(version) || u32(len(first)) || first ||
/// u32(len(second)) || second`, where `first`/`second` are the
/// exact JSON bytes sent/received, in fixed role order
/// (initiator first, listener second for the handshake;
/// request first, response second for the ceremony).
pub fn transcript_hash(version: u32, first: &[u8], second: &[u8]) -> [u8; 32] {
    let mut transcript = Vec::with_capacity(12 + first.len() + second.len());
    transcript.extend_from_slice(&version.to_be_bytes());
    transcript.extend_from_slice(&(first.len() as u32).to_be_bytes());
    transcript.extend_from_slice(first);
    transcript.extend_from_slice(&(second.len() as u32).to_be_bytes());
    transcript.extend_from_slice(second);
    sha256(&transcript)
}

/// The 6-digit SAS (§3.3): HMAC-SHA256(key = transcript hash,
/// msg = "mimint friends v2 sas"), first 4 bytes mod 1_000_000,
/// zero-padded.
pub fn sas_from_transcript(transcript: &[u8; 32]) -> String {
    let tag = hmac_sha256(transcript, b"mimint friends v2 sas");
    let value = u32::from_be_bytes([tag[0], tag[1], tag[2], tag[3]]) % 1_000_000;
    format!("{value:06}")
}

/// `device_id`: first 16 bytes of SHA-256(uncompressed public
/// key), as 32 hex characters.
pub fn device_id_from_public_key(public_key: &[u8]) -> String {
    let hash = sha256(public_key);
    hex_encode(&hash[..16])
}

/// Full fingerprint: SHA-256(uncompressed public key), 64 hex.
pub fn fingerprint_from_public_key(public_key: &[u8]) -> String {
    hex_encode(&sha256(public_key))
}

/// Short fingerprint for display (§3.3): first 12 + "…" + last
/// 12 characters, grouped by 4.
pub fn fingerprint_short(fingerprint: &str) -> String {
    if fingerprint.len() < 24 {
        return fingerprint.to_string();
    }
    let head = &fingerprint[..12];
    let tail = &fingerprint[fingerprint.len() - 12..];
    let group = |s: &str| {
        s.as_bytes()
            .chunks(4)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!("{} … {}", group(head), group(tail))
}

/// A fresh 16-byte handshake/ceremony nonce.
pub fn fresh_nonce16() -> [u8; HELLO_NONCE_LEN] {
    let mut nonce = [0u8; HELLO_NONCE_LEN];
    random_bytes(&mut nonce);
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::friends::util::hex_decode;

    fn bytes(hex: &str) -> Vec<u8> {
        hex_decode(hex).expect("valid test hex")
    }

    /// §2.4 fixed test vector, locked byte-for-byte: salt, PRK,
    /// and both directional keys from the design's fixed inputs.
    #[test]
    fn friends_key_derivation_test_vector() {
        let nonce_a_vec = bytes("a0a1a2a3a4a5a6a7a8a9aaabacadaeaf");
        let nonce_b_vec = bytes("b0b1b2b3b4b5b6b7b8b9babbbcbdbebf");
        let shared = bytes("c0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedf");
        let mut nonce_a = [0u8; HELLO_NONCE_LEN];
        let mut nonce_b = [0u8; HELLO_NONCE_LEN];
        nonce_a.copy_from_slice(&nonce_a_vec);
        nonce_b.copy_from_slice(&nonce_b_vec);

        let mut salt_input = [0u8; 32];
        salt_input[..16].copy_from_slice(&nonce_a);
        salt_input[16..].copy_from_slice(&nonce_b);
        let salt = sha256(&salt_input);
        assert_eq!(
            hex_encode(&salt),
            "00e988677eecf94c0bb9233371c7c0d6f4db8ebdcdecb7c5ebaa666f17249227"
        );
        let prk = hkdf_extract(&salt, &shared);
        assert_eq!(
            hex_encode(&prk),
            "86b81109d3268e633aaf699b5381c73d3822fe2806eeee14b77312bee8555ee9"
        );
        let (key_ab, key_ba) = derive_directional_key_bytes(&shared, &nonce_a, &nonce_b, None);
        assert_eq!(
            hex_encode(&key_ab),
            "7e70e8ce4edcfb747dbb1c6443359d71e7faa3e5146eb03fce79f768338d2bb9"
        );
        assert_eq!(
            hex_encode(&key_ba),
            "377441c9d277f30a2a7fd8106a64da399836de9bf64ed92fa778b2eac5030f95"
        );
        // Rekey suffix changes both keys.
        let (re_ab, re_ba) = derive_directional_key_bytes(&shared, &nonce_a, &nonce_b, Some(1));
        assert_ne!(re_ab, key_ab);
        assert_ne!(re_ba, key_ba);
        let (re2_ab, _) = derive_directional_key_bytes(&shared, &nonce_a, &nonce_b, Some(2));
        assert_ne!(re2_ab, re_ab);
    }

    #[test]
    fn ecdh_both_directions_agree_and_aead_round_trips() {
        let (alice_priv, alice_pub) = generate_ephemeral();
        let (bob_priv, bob_pub) = generate_ephemeral();
        assert_eq!(alice_pub.len(), PUBKEY_LEN);
        assert_ne!(alice_pub, bob_pub);
        let nonce_a = fresh_nonce16();
        let nonce_b = fresh_nonce16();
        let (a_ab, a_ba) =
            agree_and_derive(alice_priv, &bob_pub, &nonce_a, &nonce_b, None).expect("alice agree");
        let (b_ab, b_ba) =
            agree_and_derive(bob_priv, &alice_pub, &nonce_a, &nonce_b, None).expect("bob agree");
        // Alice sends A->B with a_ab; Bob opens with b_ab.
        let nonce = make_nonce(Direction::AtoB, 0);
        let aad = b"header-bytes";
        let sealed = a_ab.seal(&nonce, aad, b"secret message");
        assert_eq!(sealed.len(), 14 + 16);
        assert_eq!(b_ab.open(&nonce, aad, &sealed).unwrap(), b"secret message");
        // Reverse direction: Bob sends B->A with b_ba; Alice opens with a_ba.
        let nonce_ba = make_nonce(Direction::BtoA, 0);
        let sealed_ba = b_ba.seal(&nonce_ba, aad, b"reply");
        assert_eq!(a_ba.open(&nonce_ba, aad, &sealed_ba).unwrap(), b"reply");
        // Wrong key fails; tampered ciphertext fails; wrong AAD fails.
        assert!(a_ba.open(&nonce, aad, &sealed).is_none());
        let mut tampered = sealed.clone();
        tampered[0] ^= 1;
        assert!(b_ab.open(&nonce, aad, &tampered).is_none());
        assert!(b_ab.open(&nonce, b"other aad", &sealed).is_none());
        // Nonce prefixes differ per direction; counters differ.
        assert_ne!(
            make_nonce(Direction::AtoB, 0),
            make_nonce(Direction::BtoA, 0)
        );
        assert_ne!(
            make_nonce(Direction::AtoB, 0),
            make_nonce(Direction::AtoB, 1)
        );
    }

    #[test]
    fn identity_sign_verify_round_trip_and_wrong_key_fails() {
        let der = generate_identity_pkcs8();
        let public = identity_public_key(&der).expect("public key");
        assert_eq!(public.len(), PUBKEY_LEN);
        assert_eq!(public[0], 0x04, "uncompressed point");
        let signature = identity_sign(&der, b"transcript bytes").expect("sign");
        assert!(identity_verify(&public, b"transcript bytes", &signature));
        assert!(!identity_verify(&public, b"other bytes", &signature));
        let other_der = generate_identity_pkcs8();
        let other_public = identity_public_key(&other_der).expect("other public key");
        assert!(!identity_verify(
            &other_public,
            b"transcript bytes",
            &signature
        ));
        // device_id and fingerprint derive from the public key.
        let device_id = device_id_from_public_key(&public);
        let fingerprint = fingerprint_from_public_key(&public);
        assert_eq!(device_id.len(), 32);
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.starts_with(&device_id));
        let short = fingerprint_short(&fingerprint);
        assert_eq!(short.chars().count(), 14 + 3 + 14, "grouped 4-4-4 … 4-4-4");
        assert!(short.contains('…'));
        assert!(short.starts_with(&fingerprint[..4]));
    }

    #[test]
    fn sas_is_six_digits_and_transcript_sensitive() {
        let hash = transcript_hash(2, b"hello-one", b"hello-two");
        let sas = sas_from_transcript(&hash);
        assert_eq!(sas.len(), 6);
        assert!(sas.bytes().all(|b| b.is_ascii_digit()));
        let other = transcript_hash(2, b"hello-one", b"hello-tampered");
        assert_ne!(sas_from_transcript(&other), sas);
        // Order matters: initiator-first is not interchangeable.
        let swapped = transcript_hash(2, b"hello-two", b"hello-one");
        assert_ne!(sas_from_transcript(&swapped), sas);
    }
}
