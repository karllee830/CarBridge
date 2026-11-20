// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Encrypted records. An encrypted body, and each event on an encrypted input back channel, is a sequence of:
//!
//! | Size | Field |
//! | --- | --- |
//! | 4 | IV length, 12 |
//! | 12 | IV, random for each record |
//! | 4 | ciphertext length |
//! | n | AES-128-GCM ciphertext and its 16-byte tag, no associated data |

use std::fmt;

use aes_gcm::aead::{Aead, AeadCore, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use rand_core::OsRng;

use crate::Error;

/// IV length of every record.
pub const IV_LEN: usize = 12;
/// Length of the GCM tag at the end of each ciphertext.
pub const TAG_LEN: usize = 16;
/// Bytes a record adds to its plaintext.
pub const RECORD_OVERHEAD: usize = 4 + IV_LEN + 4 + TAG_LEN;

/// The AES-128 key of a session, shared by all its encrypted channels.
#[derive(Clone)]
pub struct SessionKey {
    cipher: Aes128Gcm,
}

impl fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionKey(..)")
    }
}

impl SessionKey {
    pub fn new(key: &[u8; 16]) -> SessionKey {
        SessionKey {
            cipher: Aes128Gcm::new(key.into()),
        }
    }

    /// Encrypts `plaintext` as one record with a fresh random IV.
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plaintext.len() + RECORD_OVERHEAD);
        self.seal_into(plaintext, &mut out);
        out
    }

    /// Appends `plaintext`, encrypted as one record, to `out`.
    pub fn seal_into(&self, plaintext: &[u8], out: &mut Vec<u8>) {
        let nonce = Aes128Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("AES-GCM encryption does not fail for in-memory buffers");
        out.extend_from_slice(&(IV_LEN as u32).to_be_bytes());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
        out.extend_from_slice(&ciphertext);
    }

    /// Decrypts a body made of one or more whole records and returns their plaintexts joined together.
    pub fn open(&self, mut records: &[u8]) -> Result<Vec<u8>, Error> {
        let mut plaintext = Vec::with_capacity(records.len().saturating_sub(RECORD_OVERHEAD));
        while !records.is_empty() {
            let (record, len) = Record::split(records)?.ok_or(Error::Record)?;
            plaintext.extend_from_slice(&self.open_record(&record)?);
            records = &records[len..];
        }
        Ok(plaintext)
    }

    /// Decrypts one record.
    pub fn open_record(&self, record: &Record<'_>) -> Result<Vec<u8>, Error> {
        self.cipher
            .decrypt(Nonce::from_slice(record.iv), record.ciphertext)
            .map_err(|_| Error::Decrypt)
    }
}

/// One record, borrowed from the bytes it was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    pub iv: &'a [u8],
    /// Ciphertext followed by the tag.
    pub ciphertext: &'a [u8],
}

impl<'a> Record<'a> {
    /// Reads the record at the start of `buf`. Returns the record and the number of bytes it takes, or `None` when
    /// `buf` does not hold all of it yet.
    pub fn split(buf: &'a [u8]) -> Result<Option<(Record<'a>, usize)>, Error> {
        let Some(iv_len) = read_u32(buf, 0) else {
            return Ok(None);
        };
        if iv_len as usize != IV_LEN {
            return Err(Error::Record);
        }
        let ct_at = 4 + IV_LEN;
        let Some(ct_len) = read_u32(buf, ct_at) else {
            return Ok(None);
        };
        let ct_len = ct_len as usize;
        if ct_len < TAG_LEN {
            return Err(Error::Record);
        }
        let end = match (ct_at + 4).checked_add(ct_len) {
            Some(end) => end,
            None => return Err(Error::Record),
        };
        if buf.len() < end {
            return Ok(None);
        }
        Ok(Some((
            Record {
                iv: &buf[4..ct_at],
                ciphertext: &buf[ct_at + 4..end],
            },
            end,
        )))
    }
}

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at + 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SessionKey {
        SessionKey::new(&[0x42; 16])
    }

    #[test]
    fn seal_layout() {
        let record = key().seal(b"hello");
        assert_eq!(record.len(), 5 + RECORD_OVERHEAD);
        assert_eq!(&record[0..4], &[0, 0, 0, 12]);
        assert_eq!(&record[16..20], &(5 + TAG_LEN as u32).to_be_bytes());
    }

    #[test]
    fn round_trip() {
        let key = key();
        assert_eq!(
            key.open(&key.seal(b"192.168.1.2:0")).unwrap(),
            b"192.168.1.2:0"
        );
    }

    #[test]
    fn fresh_iv_each_time() {
        let key = key();
        assert_ne!(key.seal(b"x"), key.seal(b"x"));
    }

    #[test]
    fn several_records_join() {
        let key = key();
        let mut body = key.seal(b"abc");
        key.seal_into(b"", &mut body);
        key.seal_into(b"def", &mut body);
        assert_eq!(key.open(&body).unwrap(), b"abcdef");
    }

    #[test]
    fn known_vector() {
        // AES-128-GCM, NIST test case 3 key and IV, no associated data.
        let key: [u8; 16] = hex("feffe9928665731c6d6a8f9467308308").try_into().unwrap();
        let iv = hex("cafebabefacedbaddecaf888");
        let plaintext = hex(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        );
        let mut ciphertext = hex(
            "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985",
        );
        ciphertext.extend(hex("4d5c2af327cd64a62cf35abd2ba6fab4"));
        let mut body = Vec::new();
        body.extend_from_slice(&12u32.to_be_bytes());
        body.extend_from_slice(&iv);
        body.extend_from_slice(&(ciphertext.len() as u32).to_be_bytes());
        body.extend_from_slice(&ciphertext);
        assert_eq!(SessionKey::new(&key).open(&body).unwrap(), plaintext);
    }

    #[test]
    fn tampered_record() {
        let key = key();
        let mut record = key.seal(b"hello");
        *record.last_mut().unwrap() ^= 1;
        assert_eq!(key.open(&record), Err(Error::Decrypt));
    }

    #[test]
    fn wrong_key() {
        let record = key().seal(b"hello");
        assert_eq!(SessionKey::new(&[0; 16]).open(&record), Err(Error::Decrypt));
    }

    #[test]
    fn split_waits_for_whole_record() {
        let record = key().seal(b"hello");
        for len in 0..record.len() {
            assert_eq!(Record::split(&record[..len]), Ok(None));
        }
        let (parsed, used) = Record::split(&record).unwrap().unwrap();
        assert_eq!(used, record.len());
        assert_eq!(parsed.iv, &record[4..16]);
    }

    #[test]
    fn split_rejects_bad_lengths() {
        let mut record = key().seal(b"hello");
        record[3] = 16;
        assert_eq!(Record::split(&record), Err(Error::Record));
        let mut short = key().seal(b"");
        short[19] = 15;
        assert_eq!(Record::split(&short), Err(Error::Record));
    }

    #[test]
    fn truncated_body() {
        let key = key();
        let record = key.seal(b"hello");
        assert_eq!(key.open(&record[..record.len() - 1]), Err(Error::Record));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
