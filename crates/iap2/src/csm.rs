// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's iap2/wire/Iap2CsmFramer.kt (DiPlay
// commit 615b32c).

//! Framing of control session messages in the bytes of the link's control session: `0x4040 | u16 length |
//! u16 message id | body`, without a checksum. A message may arrive split over several link packets, or several
//! in one.

use crate::wire::{Frame, ProtocolError, FRAME_HEADER_BYTES, FRAME_START, MAX_FRAME_BYTES};

/// The largest payload of one link packet, which bounds the pieces a message is split into.
pub const MAX_LINK_CHUNK_BYTES: usize = 65_525;

/// Collects control session bytes into messages. It holds at most one largest message.
#[derive(Debug, Default)]
pub struct Deframer {
    buffer: Vec<u8>,
}

impl Deframer {
    pub fn new() -> Deframer {
        Deframer::default()
    }

    /// Adds received bytes and gives each message they complete. Bytes before a `0x4040` start, and a header
    /// whose length is below the header's own, are skipped a byte at a time.
    pub fn push(&mut self, mut chunk: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        while !chunk.is_empty() {
            let room = MAX_FRAME_BYTES - self.buffer.len();
            let (now, later) = chunk.split_at(room.min(chunk.len()));
            self.buffer.extend_from_slice(now);
            chunk = later;
            self.drain(&mut frames);
        }
        frames
    }

    fn drain(&mut self, frames: &mut Vec<Frame>) {
        let start = FRAME_START.to_be_bytes();
        let mut offset = 0;
        loop {
            let rest = &self.buffer[offset..];
            if rest.len() >= 2 && rest[..2] != start {
                offset += 1;
                continue;
            }
            if rest.len() < FRAME_HEADER_BYTES {
                break;
            }
            let length = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
            if length < FRAME_HEADER_BYTES {
                offset += 1;
                continue;
            }
            if rest.len() < length {
                break;
            }
            let message_id = u16::from_be_bytes([rest[4], rest[5]]);
            let body = rest[FRAME_HEADER_BYTES..length].to_vec();
            frames.push(
                Frame::new(message_id, body).expect("a frame's length field bounds its body"),
            );
            offset += length;
        }
        self.buffer.drain(..offset);
    }
}

/// Splits one complete encoded message into link payloads of at most `chunk_size` bytes.
pub fn split_for_link(
    frame: &[u8],
    chunk_size: usize,
) -> Result<std::slice::Chunks<'_, u8>, ProtocolError> {
    if !(1..=MAX_LINK_CHUNK_BYTES).contains(&chunk_size) {
        return Err(ProtocolError::new(format!(
            "link chunk size must be 1..={MAX_LINK_CHUNK_BYTES}"
        )));
    }
    let complete = (FRAME_HEADER_BYTES..=MAX_FRAME_BYTES).contains(&frame.len())
        && frame[..2] == FRAME_START.to_be_bytes()
        && usize::from(u16::from_be_bytes([frame[2], frame[3]])) == frame.len();
    if !complete {
        return Err(ProtocolError::new(
            "not one complete control session message",
        ));
    }
    Ok(frame.chunks(chunk_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u16, body: &[u8]) -> Frame {
        Frame::new(id, body.to_vec()).unwrap()
    }

    #[test]
    fn fragmented_and_concatenated_frames() {
        let first = frame(0x4300, &[0, 4, 0, 0]);
        let second = frame(0x4301, &[0, 5, 0, 0, 1]);
        let bytes = [first.encode(), second.encode()].concat();
        let mut deframer = Deframer::new();
        assert!(deframer.push(&bytes[..3]).is_empty());
        assert_eq!(deframer.push(&bytes[3..]), [first, second]);
    }

    #[test]
    fn byte_at_a_time() {
        let frames = [frame(0x1d00, &[]), frame(0x5703, &[0, 6, 0, 0, b'x', 0])];
        let bytes: Vec<u8> = frames.iter().flat_map(Frame::encode).collect();
        let mut deframer = Deframer::new();
        let received: Vec<Frame> = bytes
            .iter()
            .flat_map(|byte| deframer.push(&[*byte]))
            .collect();
        assert_eq!(received, frames);
    }

    #[test]
    fn garbage_and_bad_lengths_are_skipped() {
        let wanted = frame(0x4e0d, &[0, 5, 0, 0, 1]);
        let mut bytes = vec![0x00, 0x40, 0x13, 0x40, 0x40, 0x00, 0x02, 0xaa];
        bytes.extend(wanted.encode());
        assert_eq!(Deframer::new().push(&bytes), [wanted]);
    }

    #[test]
    fn the_largest_frame_passes() {
        let big = frame(0x1234, &vec![7; crate::wire::MAX_BODY_BYTES]);
        let bytes = [big.encode(), frame(1, &[]).encode()].concat();
        let mut deframer = Deframer::new();
        let mut received = Vec::new();
        for chunk in bytes.chunks(10_000) {
            received.extend(deframer.push(chunk));
        }
        assert_eq!(received, [big, frame(1, &[])]);
    }

    #[test]
    fn splitting_for_the_link() {
        let encoded = frame(0x4301, &[1; 20]).encode();
        let chunks: Vec<&[u8]> = split_for_link(&encoded, 10).unwrap().collect();
        assert_eq!(
            chunks.iter().map(|c| c.len()).collect::<Vec<_>>(),
            [10, 10, 6]
        );
        assert_eq!(chunks.concat(), encoded);
        assert!(split_for_link(&encoded, 0).is_err());
        assert!(split_for_link(&encoded, MAX_LINK_CHUNK_BYTES + 1).is_err());
        assert!(split_for_link(&encoded[..25], 10).is_err());
        assert!(split_for_link(&[0x40, 0x41, 0, 6, 0, 0], 10).is_err());
    }
}
