// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/MfiSapAuthSetup.kt (DiPlay
// commit 615b32c); MFi certificates only.

//! /auth-setup: the receiver proves to the iPhone that it holds an MFi identity.
//!
//! The request is a version byte (1) and the iPhone's X25519 public key. The answer is the receiver's ephemeral
//! public key, its MFi certificate and the authenticator's signature over both public keys, each length-prefixed,
//! the signature encrypted with AES-128-CTR under a key and IV derived from the shared secret.

use std::fmt;

use aes::cipher::{KeyIvInit, StreamCipher};
use carbridge_iap2::mfi::Authenticator;
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::crypto::X25519;

const VERSION: u8 = 1;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Why auth-setup failed.
#[derive(Debug)]
pub enum Failure {
    /// Not a version 1 request with a 32-byte public key, or a key that gives no secret.
    Malformed,
    /// The authenticator could not give its certificate or a signature.
    Authenticator(std::io::Error),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Malformed => f.write_str("malformed auth-setup request"),
            Failure::Authenticator(error) => write!(f, "MFi authenticator failed: {error}"),
        }
    }
}

impl std::error::Error for Failure {}

fn digest<D: Digest>(parts: &[&[u8]]) -> Vec<u8> {
    let mut digest = D::new();
    for part in parts {
        digest.update(part);
    }
    digest.finalize().to_vec()
}

/// The answer to an auth-setup request.
pub fn auth_setup(request: &[u8], authenticator: &dyn Authenticator) -> Result<Vec<u8>, Failure> {
    let [VERSION, peer @ ..] = request else {
        return Err(Failure::Malformed);
    };
    if peer.len() != 32 {
        return Err(Failure::Malformed);
    }
    respond(peer, &X25519::generate(), authenticator)
}

fn respond(
    peer: &[u8],
    own: &X25519,
    authenticator: &dyn Authenticator,
) -> Result<Vec<u8>, Failure> {
    let shared = own.shared_secret(peer).ok_or(Failure::Malformed)?;
    let public = own.public_key();
    let key = &digest::<Sha1>(&[b"AES-KEY", &shared])[..16];
    let iv = &digest::<Sha1>(&[b"AES-IV", &shared])[..16];
    let signed = if authenticator.protocol_major() == 2 {
        digest::<Sha1>(&[&public, peer])
    } else {
        digest::<Sha256>(&[&public, peer])
    };
    let certificate = authenticator
        .certificate()
        .map_err(Failure::Authenticator)?;
    let mut signature = authenticator
        .sign(&signed)
        .map_err(Failure::Authenticator)?;
    Aes128Ctr::new(key.into(), iv.into()).apply_keystream(&mut signature);

    let mut response = Vec::with_capacity(32 + 4 + certificate.len() + 4 + signature.len());
    response.extend_from_slice(&public);
    response.extend_from_slice(&(certificate.len() as u32).to_be_bytes());
    response.extend_from_slice(&certificate);
    response.extend_from_slice(&(signature.len() as u32).to_be_bytes());
    response.extend_from_slice(&signature);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct Fake {
        major: u8,
    }

    impl Authenticator for Fake {
        fn certificate(&self) -> io::Result<Vec<u8>> {
            Ok(b"certificate".to_vec())
        }

        /// Echoes the digest, so the test can check what was signed.
        fn sign(&self, challenge: &[u8]) -> io::Result<Vec<u8>> {
            Ok(challenge.to_vec())
        }

        fn protocol_major(&self) -> u8 {
            self.major
        }
    }

    fn u32_at(bytes: &[u8], offset: usize) -> usize {
        u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
    }

    #[test]
    fn the_answer_carries_key_certificate_and_encrypted_signature() {
        for (major, digest_len) in [(3, 32), (2, 20)] {
            let phone = X25519::generate();
            let request = [&[1u8][..], &phone.public_key()].concat();
            let response = auth_setup(&request, &Fake { major }).unwrap();
            let receiver_public = &response[..32];
            assert_eq!(u32_at(&response, 32), 11);
            assert_eq!(&response[36..47], b"certificate");
            assert_eq!(u32_at(&response, 47), digest_len);
            let mut signature = response[51..].to_vec();
            assert_eq!(signature.len(), digest_len);

            // The iPhone's side: the same secret gives the key and IV that decrypt the signature.
            let shared = phone.shared_secret(receiver_public).unwrap();
            let key = &digest::<Sha1>(&[b"AES-KEY", &shared])[..16];
            let iv = &digest::<Sha1>(&[b"AES-IV", &shared])[..16];
            Aes128Ctr::new(key.into(), iv.into()).apply_keystream(&mut signature);
            let expected = if major == 2 {
                digest::<Sha1>(&[receiver_public, &phone.public_key()])
            } else {
                digest::<Sha256>(&[receiver_public, &phone.public_key()])
            };
            assert_eq!(signature, expected);
        }
    }

    #[test]
    fn malformed_requests_are_refused() {
        let fake = Fake { major: 3 };
        assert!(matches!(auth_setup(&[], &fake), Err(Failure::Malformed)));
        assert!(matches!(
            auth_setup(&[2; 33], &fake),
            Err(Failure::Malformed)
        ));
        assert!(matches!(
            auth_setup(&[1; 32], &fake),
            Err(Failure::Malformed)
        ));
        let zero = [&[1u8][..], &[0; 32]].concat();
        assert!(matches!(auth_setup(&zero, &fake), Err(Failure::Malformed)));
    }

    #[test]
    fn aes_ctr_matches_the_nist_vector() {
        // NIST SP 800-38A F.5.1, CTR-AES128.Encrypt, first block.
        let key = crate::crypto::from_hex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = crate::crypto::from_hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let mut block = crate::crypto::from_hex("6bc1bee22e409f96e93d7e117393172a");
        Aes128Ctr::new(key.as_slice().into(), iv.as_slice().into()).apply_keystream(&mut block);
        assert_eq!(
            crate::crypto::to_hex(&block),
            "874d6191b620e3261bef6864990db6ce"
        );
    }
}
