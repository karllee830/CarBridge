// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The 20-byte header in front of every message. All fields are big-endian.
//!
//! | Offset | Size | Field |
//! | --- | --- | --- |
//! | 0 | 4 | length of header and body as sent |
//! | 4 | 4 | sequence number |
//! | 8 | 4 | Unix time in seconds |
//! | 12 | 1 | flags: source (bit 0), body format (bits 1–2), kind (bits 3–4) |
//! | 13 | 1 | category |
//! | 14 | 2 | command |
//! | 16 | 2 | reserved |
//! | 18 | 2 | CRC-16/ARC of bytes 0–17 |

use crate::{crc16_arc, Error};

/// Size of the header.
pub const HEADER_LEN: usize = 20;

/// Which side made a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    HeadUnit,
    Phone,
}

/// How the body is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyFormat {
    /// Raw bytes, encrypted on an encrypted channel.
    Raw,
    /// A proto3 message, encrypted on an encrypted channel.
    Protobuf,
    /// Raw bytes that are never encrypted.
    Plain,
}

/// What a message expects in return.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Nothing.
    Send,
    /// A response with the same sequence number.
    Request,
    /// Answers the request with the same sequence number.
    Response,
    /// An ack (category [`Category::ACK`]) with the same sequence number.
    SendAck,
}

/// The group a message's command belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Category(pub u8);

impl Category {
    pub const CONTROL: Category = Category(1);
    pub const SENSOR: Category = Category(2);
    pub const AUTH: Category = Category(3);
    pub const AUDIO: Category = Category(4);
    pub const VIDEO: Category = Category(5);
    pub const CERT: Category = Category(6);
    pub const ACK: Category = Category(7);
}

/// A decoded header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Header plus body as sent, after encryption.
    pub length: u32,
    pub sequence: u32,
    pub time: u32,
    pub source: Source,
    pub format: BodyFormat,
    pub kind: Kind,
    pub category: Category,
    pub command: u16,
    /// 0 in requests and sends; a response repeats its request's value.
    pub reserved: u16,
}

impl Header {
    /// Encodes the header and computes its checksum.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0..4].copy_from_slice(&self.length.to_be_bytes());
        out[4..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..12].copy_from_slice(&self.time.to_be_bytes());
        out[12] = self.flags();
        out[13] = self.category.0;
        out[14..16].copy_from_slice(&self.command.to_be_bytes());
        out[16..18].copy_from_slice(&self.reserved.to_be_bytes());
        let crc = crc16_arc(&out[..18]);
        out[18..20].copy_from_slice(&crc.to_be_bytes());
        out
    }

    /// Decodes a header from the first [`HEADER_LEN`] bytes of `bytes` and checks its checksum. Flag bits 5–7 are
    /// ignored.
    ///
    /// # Panics
    ///
    /// When `bytes` is shorter than [`HEADER_LEN`].
    pub fn decode(bytes: &[u8]) -> Result<Header, Error> {
        let b = &bytes[..HEADER_LEN];
        let actual = u16::from_be_bytes([b[18], b[19]]);
        let expected = crc16_arc(&b[..18]);
        if actual != expected {
            return Err(Error::Checksum { expected, actual });
        }
        let flags = b[12];
        let format = match (flags >> 1) & 0b11 {
            0 => BodyFormat::Raw,
            1 => BodyFormat::Protobuf,
            2 => BodyFormat::Plain,
            bits => return Err(Error::BodyFormat(bits)),
        };
        let kind = match (flags >> 3) & 0b11 {
            0 => Kind::Send,
            1 => Kind::Request,
            2 => Kind::Response,
            _ => Kind::SendAck,
        };
        Ok(Header {
            length: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            sequence: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            time: u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            source: if flags & 1 != 0 {
                Source::Phone
            } else {
                Source::HeadUnit
            },
            format,
            kind,
            category: Category(b[13]),
            command: u16::from_be_bytes([b[14], b[15]]),
            reserved: u16::from_be_bytes([b[16], b[17]]),
        })
    }

    fn flags(&self) -> u8 {
        let source = match self.source {
            Source::HeadUnit => 0,
            Source::Phone => 1,
        };
        let format = match self.format {
            BodyFormat::Raw => 0,
            BodyFormat::Protobuf => 1,
            BodyFormat::Plain => 2,
        };
        let kind = match self.kind {
            Kind::Send => 0,
            Kind::Request => 1,
            Kind::Response => 2,
            Kind::SendAck => 3,
        };
        source | format << 1 | kind << 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            length: 0x0102_0304,
            sequence: 7,
            time: 0x6000_0000,
            source: Source::Phone,
            format: BodyFormat::Protobuf,
            kind: Kind::SendAck,
            category: Category::CONTROL,
            command: 22,
            reserved: 0xBEEF,
        }
    }

    #[test]
    fn layout() {
        let bytes = sample().encode();
        assert_eq!(&bytes[0..4], &[1, 2, 3, 4]);
        assert_eq!(&bytes[4..8], &[0, 0, 0, 7]);
        assert_eq!(&bytes[8..12], &[0x60, 0, 0, 0]);
        assert_eq!(bytes[12], 0b0001_1011);
        assert_eq!(bytes[13], 1);
        assert_eq!(&bytes[14..16], &[0, 22]);
        assert_eq!(&bytes[16..18], &[0xBE, 0xEF]);
        assert_eq!(
            u16::from_be_bytes([bytes[18], bytes[19]]),
            crc16_arc(&bytes[..18])
        );
    }

    #[test]
    fn round_trip() {
        let header = sample();
        assert_eq!(Header::decode(&header.encode()), Ok(header));
    }

    #[test]
    fn every_flag_combination() {
        for source in [Source::HeadUnit, Source::Phone] {
            for format in [BodyFormat::Raw, BodyFormat::Protobuf, BodyFormat::Plain] {
                for kind in [Kind::Send, Kind::Request, Kind::Response, Kind::SendAck] {
                    let header = Header {
                        source,
                        format,
                        kind,
                        ..sample()
                    };
                    assert_eq!(Header::decode(&header.encode()), Ok(header));
                }
            }
        }
    }

    #[test]
    fn bad_checksum() {
        let mut bytes = sample().encode();
        bytes[5] ^= 1;
        assert!(matches!(
            Header::decode(&bytes),
            Err(Error::Checksum { .. })
        ));
    }

    #[test]
    fn unused_body_format() {
        let mut bytes = sample().encode();
        bytes[12] |= 0b110;
        let crc = crc16_arc(&bytes[..18]);
        bytes[18..20].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(Header::decode(&bytes), Err(Error::BodyFormat(3)));
    }
}
