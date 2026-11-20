// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/AirPlayIdentity.kt and
// airplay/PairingStore.kt (DiPlay commit 615b32c).

//! The receiver's long-term identity, and the controllers (iPhones) that paired with it.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::Mutex;

use ed25519_dalek::{Signer, SigningKey};

use crate::crypto::{random, to_hex};

/// The receiver's long-term Ed25519 key and pairing identifier. The host keeps them across runs.
#[derive(Clone)]
pub struct Identity {
    key: SigningKey,
    pairing_id: String,
}

impl Identity {
    /// A fresh key, with a random UUID as pairing identifier.
    pub fn generate() -> Identity {
        Identity::new(random(), uuid_v4(random()))
    }

    /// An identity from its saved secret key (the 32-byte Ed25519 seed) and pairing identifier.
    pub fn new(secret_key: [u8; 32], pairing_id: String) -> Identity {
        Identity {
            key: SigningKey::from_bytes(&secret_key),
            pairing_id,
        }
    }

    /// The 32-byte Ed25519 seed, for the host to save.
    pub fn secret_key(&self) -> [u8; 32] {
        self.key.to_bytes()
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    /// The public key in lower-case hex, as the iPhone is told it.
    pub fn public_key_hex(&self) -> String {
        to_hex(&self.public_key())
    }

    pub fn pairing_id(&self) -> &str {
        &self.pairing_id
    }

    /// The Ed25519 signature of `message`.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("public_key", &self.public_key_hex())
            .field("pairing_id", &self.pairing_id)
            .finish_non_exhaustive()
    }
}

/// A version-4 UUID from 16 random bytes, in lower-case hex.
fn uuid_v4(mut bytes: [u8; 16]) -> String {
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = to_hex(&bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Where the long-term public keys of paired controllers are kept, by controller identifier. Pair-setup adds
/// one; pair-verify looks it up.
pub trait Pairings: Send + Sync {
    fn remember(&self, controller: &str, public_key: &[u8; 32]) -> io::Result<()>;
    fn lookup(&self, controller: &str) -> io::Result<Option<[u8; 32]>>;
}

/// Pairings kept in memory only.
#[derive(Debug, Default)]
pub struct MemoryPairings {
    keys: Mutex<HashMap<String, [u8; 32]>>,
}

impl MemoryPairings {
    pub fn new() -> MemoryPairings {
        MemoryPairings::default()
    }
}

impl Pairings for MemoryPairings {
    fn remember(&self, controller: &str, public_key: &[u8; 32]) -> io::Result<()> {
        self.keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(controller.to_owned(), *public_key);
        Ok(())
    }

    fn lookup(&self, controller: &str) -> io::Result<Option<[u8; 32]>> {
        Ok(self
            .keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(controller)
            .copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::ed25519_verify;

    #[test]
    fn an_identity_survives_saving() {
        let identity = Identity::generate();
        let again = Identity::new(identity.secret_key(), identity.pairing_id().to_owned());
        assert_eq!(again.public_key(), identity.public_key());
        assert_eq!(identity.public_key_hex().len(), 64);
        let signature = again.sign(b"message");
        assert!(ed25519_verify(
            &identity.public_key(),
            b"message",
            &signature
        ));
    }

    #[test]
    fn pairing_ids_are_version_4_uuids() {
        assert_eq!(uuid_v4([0xff; 16]), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(uuid_v4([0; 16]), "00000000-0000-4000-8000-000000000000");
        assert_ne!(
            Identity::generate().pairing_id(),
            Identity::generate().pairing_id()
        );
    }

    #[test]
    fn memory_pairings() {
        let pairings = MemoryPairings::new();
        assert_eq!(pairings.lookup("phone").unwrap(), None);
        pairings.remember("phone", &[3; 32]).unwrap();
        assert_eq!(pairings.lookup("phone").unwrap(), Some([3; 32]));
    }
}
