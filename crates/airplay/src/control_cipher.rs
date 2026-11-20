// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/ControlCipher.kt (DiPlay
// commit 615b32c).

//! The encryption of the control channel after pair-verify.
//!
//! Each frame is a 2-byte little-endian length of up to 16 KiB, that many encrypted bytes and a 16-byte tag. The
//! length is the frame's associated data. Each direction has its own key and counts its frames in the nonce. The
//! two directions are separate values so that the receiving and sending threads each own one.

use crate::crypto::{self, AuthenticationFailed, TAG_BYTES};
use crate::pairing::ControlKeys;

const HEADER_BYTES: usize = 2;
/// The most plaintext one frame carries.
pub const MAX_FRAME_PAYLOAD: usize = 0x4000;

/// Opens the frames the controller sends.
pub struct Opener {
    key: [u8; 32],
    counter: u64,
}

/// Seals what the receiver sends.
pub struct Sealer {
    key: [u8; 32],
    counter: u64,
}

/// The opener and sealer of one control connection.
pub fn split(keys: &ControlKeys) -> (Opener, Sealer) {
    (
        Opener {
            key: keys.read,
            counter: 0,
        },
        Sealer {
            key: keys.write,
            counter: 0,
        },
    )
}

impl Opener {
    /// Opens frames sealed with `key`, from the first.
    pub fn new(key: [u8; 32]) -> Opener {
        Opener { key, counter: 0 }
    }

    /// Opens every complete frame at the start of `buffer` and removes them from it, leaving a partial frame for
    /// later. A frame that fails to open ends the channel: nothing after it can be opened either.
    pub fn open(&mut self, buffer: &mut Vec<u8>) -> Result<Vec<u8>, AuthenticationFailed> {
        let mut plaintext = Vec::new();
        let mut offset = 0;
        while buffer.len() - offset >= HEADER_BYTES {
            let header = [buffer[offset], buffer[offset + 1]];
            let end = offset + HEADER_BYTES + usize::from(u16::from_le_bytes(header)) + TAG_BYTES;
            if buffer.len() < end {
                break;
            }
            let mut frame = buffer[offset + HEADER_BYTES..end].to_vec();
            crypto::open_in_place(
                &self.key,
                &crypto::counter_nonce(self.counter),
                &header,
                &mut frame,
            )?;
            self.counter += 1;
            plaintext.extend_from_slice(&frame);
            offset = end;
        }
        buffer.drain(..offset);
        Ok(plaintext)
    }
}

impl Sealer {
    /// Seals frames with `key`, from the first.
    pub fn new(key: [u8; 32]) -> Sealer {
        Sealer { key, counter: 0 }
    }

    /// `plaintext` as frames; an empty message is one empty frame.
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let frames = plaintext.len().div_ceil(MAX_FRAME_PAYLOAD).max(1);
        let mut out = Vec::with_capacity(plaintext.len() + frames * (HEADER_BYTES + TAG_BYTES));
        let mut chunks = plaintext.chunks(MAX_FRAME_PAYLOAD);
        for _ in 0..frames {
            let chunk = chunks.next().unwrap_or(&[]);
            let header = (chunk.len() as u16).to_le_bytes();
            let mut frame = chunk.to_vec();
            crypto::seal_in_place(
                &self.key,
                &crypto::counter_nonce(self.counter),
                &header,
                &mut frame,
            );
            self.counter += 1;
            out.extend_from_slice(&header);
            out.extend_from_slice(&frame);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> ((Opener, Sealer), (Opener, Sealer)) {
        let receiver = ControlKeys {
            read: [1; 32],
            write: [2; 32],
        };
        let controller = ControlKeys {
            read: [2; 32],
            write: [1; 32],
        };
        (split(&receiver), split(&controller))
    }

    #[test]
    fn frames_round_trip_in_both_directions() {
        let ((mut open, mut seal), (mut peer_open, mut peer_seal)) = pair();
        let request = b"GET /info RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let mut wire = peer_seal.seal(request);
        assert_eq!(wire.len(), 2 + request.len() + 16);
        assert_eq!(
            u16::from_le_bytes([wire[0], wire[1]]) as usize,
            request.len()
        );
        assert_eq!(open.open(&mut wire).unwrap(), request);
        assert!(wire.is_empty());

        let mut reply = seal.seal(b"RTSP/1.0 200 OK\r\n\r\n");
        assert_eq!(
            peer_open.open(&mut reply).unwrap(),
            b"RTSP/1.0 200 OK\r\n\r\n"
        );
    }

    #[test]
    fn large_messages_split_and_partial_frames_wait() {
        let ((mut open, _), (_, mut peer_seal)) = pair();
        let message: Vec<u8> = (0..40_000).map(|i| i as u8).collect();
        let wire = peer_seal.seal(&message);
        assert_eq!(wire.len(), message.len() + 3 * (2 + 16));
        let mut buffer = Vec::new();
        let mut received = Vec::new();
        for chunk in wire.chunks(1000) {
            buffer.extend_from_slice(chunk);
            received.extend(open.open(&mut buffer).unwrap());
        }
        assert_eq!(received, message);
        assert!(buffer.is_empty());
    }

    #[test]
    fn empty_messages_and_counters() {
        let ((mut open, _), (_, mut peer_seal)) = pair();
        let mut wire = peer_seal.seal(&[]);
        assert_eq!(wire.len(), 18);
        wire.extend(peer_seal.seal(b"second"));
        assert_eq!(open.open(&mut wire).unwrap(), b"second");
    }

    #[test]
    fn a_changed_or_replayed_frame_fails() {
        let ((mut open, _), (_, mut peer_seal)) = pair();
        let first = peer_seal.seal(b"hello");
        let mut tampered = first.clone();
        tampered[4] ^= 1;
        assert!(open.open(&mut tampered.clone()).is_err());
        assert_eq!(open.open(&mut first.clone()).unwrap(), b"hello");
        // The same frame again uses the next counter, so it no longer opens.
        assert!(open.open(&mut first.clone()).is_err());
    }
}
