// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/ScreenStream.kt (DiPlay
// commit 615b32c).

//! A screen stream: on its own TCP connection, messages of a 128-byte header and a body. A video configuration
//! (opcode 1) carries the avcC or hvcC record in the clear; a video frame (opcode 0) is sealed with
//! ChaCha20-Poly1305 under the stream's key, the header as associated data and a counter as nonce, and holds
//! length-prefixed NAL units, passed on with Annex B start codes.

use std::io;

use crate::crypto::{self, AuthenticationFailed, TAG_BYTES};
use crate::net::{read_exact, Stream};

pub const HEADER_BYTES: usize = 128;
const OPCODE: usize = 4;
const SENDER_TIME: usize = 8;
const FRAME: u8 = 0;
const CONFIG: u8 = 1;
/// The largest message body taken.
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
}

/// What a screen stream delivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScreenMessage {
    /// The codec and its configuration record (avcC or hvcC).
    Config { codec: VideoCodec, record: Vec<u8> },
    /// One frame in Annex B, and the iPhone's time for it in nanoseconds.
    Frame { data: Vec<u8>, sender_nanos: u64 },
}

/// The frame time in a header: bytes 8 to 15, NTP 32.32 little-endian, in nanoseconds.
pub fn sender_nanos(header: &[u8]) -> u64 {
    let Some(raw) = header.get(SENDER_TIME..SENDER_TIME + 8) else {
        return 0;
    };
    crate::ntp::nanos_from_ntp(u64::from_le_bytes(raw.try_into().expect("eight bytes")))
}

/// Replaces each four-byte big-endian NAL length with a start code, in place, when every length is valid; any
/// other payload is left as it is. Gives whether it converted.
pub fn to_annex_b(payload: &mut [u8]) -> bool {
    if payload.len() < 4 || payload[..4] == [0, 0, 0, 1] {
        return false;
    }
    let length_at = |payload: &[u8], offset: usize| {
        u32::from_be_bytes(payload[offset..offset + 4].try_into().expect("four bytes")) as usize
    };
    let mut offset = 0;
    while offset + 4 <= payload.len() {
        let length = length_at(payload, offset);
        if length == 0 || length > payload.len() - offset - 4 {
            return false;
        }
        offset += 4 + length;
    }
    if offset != payload.len() {
        return false;
    }
    offset = 0;
    while offset < payload.len() {
        let length = length_at(payload, offset);
        payload[offset..offset + 4].copy_from_slice(&[0, 0, 0, 1]);
        offset += 4 + length;
    }
    true
}

/// The codec and record in a video configuration body: what follows an `avcC` or `hvcC` tag, or the whole body,
/// as H.264 when it reads as an avcC record and as H.265 otherwise.
pub fn detect_config(body: &[u8]) -> (VideoCodec, &[u8]) {
    for index in 4..body.len().saturating_sub(3) {
        match &body[index..index + 4] {
            b"hvcC" => return (VideoCodec::H265, &body[index + 4..]),
            b"avcC" => return (VideoCodec::H264, &body[index + 4..]),
            _ => {}
        }
    }
    let looks_like_avcc = body.len() >= 9
        && body[5] & 0x1f >= 1
        && 8 + usize::from(u16::from_be_bytes([body[6], body[7]])) <= body.len()
        && body[8] & 0x1f == 7;
    if looks_like_avcc {
        (VideoCodec::H264, body)
    } else {
        (VideoCodec::H265, body)
    }
}

/// Opens a screen stream's messages in order.
pub struct ScreenReader {
    key: [u8; 32],
    frames: u64,
}

impl ScreenReader {
    pub fn new(key: [u8; 32]) -> ScreenReader {
        ScreenReader { key, frames: 0 }
    }

    /// The message of one header and body; `None` for an opcode not used, or a frame too short to hold a tag.
    pub fn message(
        &mut self,
        header: &[u8; HEADER_BYTES],
        mut body: Vec<u8>,
    ) -> Result<Option<ScreenMessage>, AuthenticationFailed> {
        match header[OPCODE] {
            FRAME if body.len() >= TAG_BYTES => {
                crypto::open_in_place(
                    &self.key,
                    &crypto::counter_nonce(self.frames),
                    header,
                    &mut body,
                )?;
                self.frames += 1;
                to_annex_b(&mut body);
                Ok(Some(ScreenMessage::Frame {
                    data: body,
                    sender_nanos: sender_nanos(header),
                }))
            }
            CONFIG => {
                let (codec, record) = detect_config(&body);
                Ok(Some(ScreenMessage::Config {
                    codec,
                    record: record.to_vec(),
                }))
            }
            _ => Ok(None),
        }
    }
}

/// Reads a screen stream until it ends or `stop` says so, giving each message to `deliver`.
pub fn run(
    stream: &dyn Stream,
    key: [u8; 32],
    stop: &dyn Fn() -> bool,
    deliver: &mut dyn FnMut(ScreenMessage),
) -> io::Result<()> {
    let mut reader = ScreenReader::new(key);
    let mut header = [0; HEADER_BYTES];
    loop {
        if !read_exact(stream, &mut header, stop)? {
            return Ok(());
        }
        let size = u32::from_le_bytes(header[..4].try_into().expect("four bytes")) as usize;
        if size > MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("screen message of {size} bytes"),
            ));
        }
        let mut body = vec![0; size];
        if !read_exact(stream, &mut body, stop)? {
            return Ok(());
        }
        if let Some(message) = reader.message(&header, body)? {
            deliver(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_length_prefixes_become_annex_b() {
        let mut payload = vec![0, 0, 0, 2, 0x40, 0x01, 0, 0, 0, 3, 0x42, 0x01, 0x02];
        assert!(to_annex_b(&mut payload));
        assert_eq!(
            payload,
            [0, 0, 0, 1, 0x40, 0x01, 0, 0, 0, 1, 0x42, 0x01, 0x02]
        );
    }

    #[test]
    fn malformed_lengths_leave_the_payload() {
        let mut payload = vec![0, 0, 0, 5, 0x40, 0x01];
        assert!(!to_annex_b(&mut payload));
        assert_eq!(payload, [0, 0, 0, 5, 0x40, 0x01]);
        let mut annex_b = vec![0, 0, 0, 1, 0x65];
        assert!(!to_annex_b(&mut annex_b));
    }

    #[test]
    fn reads_the_frame_time() {
        let mut header = [0u8; HEADER_BYTES];
        let raw: u64 = (3 << 32) | 0x4000_0000;
        header[8..16].copy_from_slice(&raw.to_le_bytes());
        assert_eq!(sender_nanos(&header), 3_250_000_000);
        header[8..16].copy_from_slice(&(raw + 0x0444_4444).to_le_bytes());
        assert_eq!(sender_nanos(&header) - 3_250_000_000, 16_666_666);
        assert_eq!(sender_nanos(&[0; 12]), 0);
    }

    #[test]
    fn configuration_codecs() {
        let mut body = vec![0; 8];
        body.extend_from_slice(b"hvcC");
        body.extend_from_slice(&[1, 2, 3]);
        assert_eq!(detect_config(&body), (VideoCodec::H265, &[1u8, 2, 3][..]));
        let avcc = [1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 1, 0x67];
        assert_eq!(detect_config(&avcc), (VideoCodec::H264, &avcc[..]));
        assert_eq!(detect_config(&[1, 2, 3]).0, VideoCodec::H265);
    }

    #[test]
    fn frames_are_opened_with_counted_nonces() {
        let key = [9; 32];
        let mut reader = ScreenReader::new(key);
        for counter in 0..3u64 {
            let mut header = [0u8; HEADER_BYTES];
            header[OPCODE] = FRAME;
            let plain = [0, 0, 0, 2, 0x65, counter as u8];
            // The body's size is in the header, which is the associated data.
            header[..4].copy_from_slice(&((plain.len() + TAG_BYTES) as u32).to_le_bytes());
            let sealed = crypto::seal(&key, &crypto::counter_nonce(counter), &header, &plain);
            let message = reader.message(&header, sealed).unwrap().unwrap();
            assert_eq!(
                message,
                ScreenMessage::Frame {
                    data: vec![0, 0, 0, 1, 0x65, counter as u8],
                    sender_nanos: 0
                }
            );
        }
        let mut header = [0u8; HEADER_BYTES];
        header[OPCODE] = FRAME;
        assert!(reader.message(&header, vec![0; 20]).is_err());
        assert_eq!(reader.message(&header, vec![0; 5]).unwrap(), None);
    }
}
