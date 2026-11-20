// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/AirPlayCrypto.kt (DiPlay
// commit 615b32c).

//! The cryptographic primitives of pairing and the encrypted channels: SHA-512, HKDF-SHA-512,
//! ChaCha20-Poly1305 with AirPlay's nonces, Ed25519 and X25519.

use std::fmt;
use std::io;

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha512};

/// The length of a ChaCha20-Poly1305 tag.
pub const TAG_BYTES: usize = 16;
/// The length of a ChaCha20-Poly1305 nonce.
pub const NONCE_BYTES: usize = 12;

/// A sealed message whose tag did not match: changed data, a wrong key or nonce, or wrong associated data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthenticationFailed;

impl fmt::Display for AuthenticationFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChaCha20-Poly1305 authentication failed")
    }
}

impl std::error::Error for AuthenticationFailed {}

impl From<AuthenticationFailed> for io::Error {
    fn from(error: AuthenticationFailed) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// SHA-512 of the parts one after another.
pub fn sha512(parts: &[&[u8]]) -> [u8; 64] {
    let mut digest = Sha512::new();
    for part in parts {
        digest.update(part);
    }
    digest.finalize().into()
}

/// A 32-byte key from HKDF-SHA-512.
pub fn hkdf_sha512(input: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let mut key = [0; 32];
    Hkdf::<Sha512>::new(Some(salt), input)
        .expand(info, &mut key)
        .expect("32 bytes is a valid HKDF-SHA-512 length");
    key
}

/// A nonce of four zero bytes and a 64-bit little-endian counter, as the encrypted channels use.
pub fn counter_nonce(counter: u64) -> [u8; NONCE_BYTES] {
    let mut nonce = [0; NONCE_BYTES];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

/// A nonce of four zero bytes and an ASCII label of up to eight bytes, as pairing uses (`"PV-Msg02"`).
pub fn label_nonce(label: &str) -> [u8; NONCE_BYTES] {
    let mut nonce = [0; NONCE_BYTES];
    let label = &label.as_bytes()[..label.len().min(8)];
    nonce[4..4 + label.len()].copy_from_slice(label);
    nonce
}

/// Encrypts `buffer` in place and appends the tag.
pub fn seal_in_place(key: &[u8; 32], nonce: &[u8; NONCE_BYTES], aad: &[u8], buffer: &mut Vec<u8>) {
    let tag = ChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt_in_place_detached(Nonce::from_slice(nonce), aad, buffer)
        .expect("ChaCha20-Poly1305 seals any message an iPhone exchanges");
    buffer.extend_from_slice(&tag);
}

/// Checks the tag at the end of `buffer`, removes it and decrypts the rest in place.
pub fn open_in_place(
    key: &[u8; 32],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    buffer: &mut Vec<u8>,
) -> Result<(), AuthenticationFailed> {
    let Some(body) = buffer.len().checked_sub(TAG_BYTES) else {
        return Err(AuthenticationFailed);
    };
    let tag = *Tag::from_slice(&buffer[body..]);
    buffer.truncate(body);
    ChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt_in_place_detached(Nonce::from_slice(nonce), aad, buffer, &tag)
        .map_err(|_| AuthenticationFailed)
}

/// `plaintext` encrypted, followed by its tag.
pub fn seal(key: &[u8; 32], nonce: &[u8; NONCE_BYTES], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(plaintext.len() + TAG_BYTES);
    buffer.extend_from_slice(plaintext);
    seal_in_place(key, nonce, aad, &mut buffer);
    buffer
}

/// The plaintext of a message sealed by [`seal`].
pub fn open(
    key: &[u8; 32],
    nonce: &[u8; NONCE_BYTES],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, AuthenticationFailed> {
    let mut buffer = sealed.to_vec();
    open_in_place(key, nonce, aad, &mut buffer)?;
    Ok(buffer)
}

/// Whether `signature` is `public_key`'s Ed25519 signature of `message`. Keys and signatures of the wrong
/// length, and keys that are not points of the curve, do not verify.
pub fn ed25519_verify(public_key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let (Ok(public_key), Ok(signature)) = (
        <[u8; 32]>::try_from(public_key),
        <[u8; 64]>::try_from(signature),
    ) else {
        return false;
    };
    let Ok(public_key) = VerifyingKey::from_bytes(&public_key) else {
        return false;
    };
    public_key
        .verify(message, &Signature::from_bytes(&signature))
        .is_ok()
}

/// An X25519 key pair for one exchange.
pub struct X25519 {
    secret: x25519_dalek::StaticSecret,
    public: [u8; 32],
}

impl X25519 {
    pub fn generate() -> X25519 {
        X25519::from_secret(random())
    }

    pub fn from_secret(secret: [u8; 32]) -> X25519 {
        let secret = x25519_dalek::StaticSecret::from(secret);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        X25519 { secret, public }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    /// The secret shared with the holder of `peer`, or `None` for a key of the wrong length or one of the curve's
    /// small-order points, which would make the secret known to anyone.
    pub fn shared_secret(&self, peer: &[u8]) -> Option<[u8; 32]> {
        let peer = x25519_dalek::PublicKey::from(<[u8; 32]>::try_from(peer).ok()?);
        let shared = self.secret.diffie_hellman(&peer);
        shared.was_contributory().then(|| shared.to_bytes())
    }
}

/// Bytes from the operating system's random source.
pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0; N];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Whether two byte strings are equal, in a time that depends only on their lengths.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Lower-case hex.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

#[cfg(test)]
pub(crate) fn from_hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc8439_vector() {
        let key: [u8; 32] =
            from_hex("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f")
                .try_into()
                .unwrap();
        let nonce: [u8; 12] = from_hex("070000004041424344454647").try_into().unwrap();
        let aad = from_hex("50515253c0c1c2c3c4c5c6c7");
        let plaintext =
            b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip \
for the future, sunscreen would be it.";
        let sealed = seal(&key, &nonce, &aad, plaintext);
        assert_eq!(
            to_hex(&sealed[sealed.len() - 16..]),
            "1ae10b594f09e26a7e902ecbd0600691"
        );
        assert_eq!(to_hex(&sealed[..16]), "d31a8d34648e60db7b86afbc53ef7ec2");
        assert_eq!(open(&key, &nonce, &aad, &sealed).unwrap(), plaintext);
    }

    #[test]
    fn a_changed_byte_or_aad_fails_to_open() {
        let key = [7u8; 32];
        let nonce = counter_nonce(1);
        let sealed = seal(&key, &nonce, b"aad", &[3u8; 100]);
        let mut tampered = sealed.clone();
        tampered[10] ^= 1;
        assert_eq!(
            open(&key, &nonce, b"aad", &tampered),
            Err(AuthenticationFailed)
        );
        assert_eq!(
            open(&key, &nonce, b"other", &sealed),
            Err(AuthenticationFailed)
        );
        assert_eq!(
            open(&key, &counter_nonce(2), b"aad", &sealed),
            Err(AuthenticationFailed)
        );
        assert_eq!(
            open(&key, &nonce, b"aad", &sealed[..15]),
            Err(AuthenticationFailed)
        );
        assert_eq!(open(&key, &nonce, b"aad", &sealed).unwrap(), [3u8; 100]);
    }

    #[test]
    fn seals_every_size() {
        let key = random::<32>();
        for size in [0usize, 1, 63, 64, 65, 1400, 65_537, 500_000] {
            let nonce = counter_nonce(size as u64);
            let plaintext: Vec<u8> = (0..size).map(|i| (i * 31) as u8).collect();
            let sealed = seal(&key, &nonce, &[], &plaintext);
            assert_eq!(sealed.len(), size + TAG_BYTES);
            assert_eq!(open(&key, &nonce, &[], &sealed).unwrap(), plaintext);
        }
    }

    #[test]
    fn nonces() {
        assert_eq!(
            counter_nonce(0x0102_0304_0506_0708),
            [0, 0, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1]
        );
        assert_eq!(&label_nonce("PV-Msg02"), b"\0\0\0\0PV-Msg02");
        assert_eq!(&label_nonce("PS-Msg0"), b"\0\0\0\0PS-Msg0\0");
    }

    #[test]
    fn hkdf_sha512_of_a_fixed_input() {
        // RFC 5869 extract and expand with HMAC-SHA-512, computed with Python's hmac module.
        let key = hkdf_sha512(b"input", b"Control-Salt", b"Control-Read-Encryption-Key");
        assert_eq!(
            to_hex(&key),
            "38eee5d35b8e32df980fa00232aa323bc3527d410c227dc10877d5318be9f442"
        );
    }

    #[test]
    fn sha512_of_parts_is_sha512_of_their_concatenation() {
        assert_eq!(sha512(&[b"ab", b"c"]), sha512(&[b"abc"]));
        assert_eq!(to_hex(&sha512(&[b"abc"])[..8]), "ddaf35a193617aba");
    }

    #[test]
    fn ed25519_rejects_malformed_input() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let public = key.verifying_key().to_bytes();
        let signature = ed25519_dalek::Signer::sign(&key, b"message").to_bytes();
        assert!(ed25519_verify(&public, b"message", &signature));
        assert!(!ed25519_verify(&public, b"massage", &signature));
        assert!(!ed25519_verify(&public[..31], b"message", &signature));
        assert!(!ed25519_verify(&public, b"message", &signature[..63]));
    }

    #[test]
    fn x25519_agrees_and_refuses_small_order_points() {
        let (a, b) = (X25519::generate(), X25519::generate());
        assert_eq!(
            a.shared_secret(&b.public_key()),
            b.shared_secret(&a.public_key())
        );
        assert!(a.shared_secret(&[0; 32]).is_none());
        assert!(a.shared_secret(&[1; 31]).is_none());
    }

    #[test]
    fn constant_time_eq_compares() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
