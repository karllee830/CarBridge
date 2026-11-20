// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! How the head unit recognises a phone it has connected before.

use sha2::{Digest, Sha256};

use crate::advert::Address;

/// The phone's address hash: SHA-256 of the address as text, upper case with colons, in lower-case hex. The phone
/// sends it in `BluetoothMacInfo`, and the head unit stores it.
pub fn address_hash(address: Address) -> String {
    Sha256::digest(address.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The two bytes the head unit appends to its advertisement when it waits for the phone whose stored hash is `hash`:
/// the low 16 bits of the CRC-32 of the hash decoded from hex. Returns `None` when `hash` is not hex.
pub fn phone_tag(hash: &str) -> Option<u16> {
    if hash.len() % 2 != 0 {
        return None;
    }
    let bytes: Vec<u8> = (0..hash.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hash.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    Some(crc32fast::hash(&bytes) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_of_upper_case_text() {
        let hash = address_hash(Address([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x0F]));
        let expected: String = Sha256::digest(b"AA:BB:CC:DD:EE:0F")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, expected);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn tag_is_low_crc_bits_of_the_bytes() {
        // CRC-32 of the bytes "123456789" is 0xCBF43926.
        assert_eq!(phone_tag("313233343536373839"), Some(0x3926));
        assert_eq!(phone_tag("31323334353637383"), None);
        assert_eq!(phone_tag("zz"), None);
    }
}
