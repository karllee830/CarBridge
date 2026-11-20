// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/AudioStream.kt (DiPlay
// commit 615b32c).

//! An audio stream from the iPhone: one RTP packet per UDP datagram, a 12-byte header in the clear, the payload
//! sealed with ChaCha20-Poly1305, its tag, and the 8-byte little-endian nonce. The header's timestamp and SSRC
//! are the associated data.

use std::fmt;

use crate::crypto::{self, TAG_BYTES};

pub const RTP_HEADER_BYTES: usize = 12;
pub const NONCE_TAIL_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioCodec {
    AacLc,
    Opus,
    /// Big-endian 16-bit PCM.
    Pcm,
}

/// How a stream's audio is coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: AudioCodec,
    /// The rate it decodes at; Opus always decodes at 48 kHz.
    pub sample_rate: u32,
    pub channels: u8,
    /// The rate of its RTP timestamps: an Opus stream counts at the rate its format names.
    pub rtp_clock_rate: u32,
}

/// The rate an Opus format's timestamps count at: 48, 24 or 16 kHz, and 48 kHz without an Opus bit.
pub fn opus_clock_rate(bits: u64) -> u32 {
    if bits & 0x4000_0000 != 0 {
        48_000
    } else if bits & 0x2000_0000 != 0 {
        24_000
    } else if bits & 0x1000_0000 != 0 {
        16_000
    } else {
        48_000
    }
}

impl AudioFormat {
    /// The format of the audioFormat bits the iPhone chose; PCM at 44.1 kHz stereo when they name none known.
    pub fn from_bits(bits: u64) -> AudioFormat {
        let pcm = |sample_rate, channels| AudioFormat {
            codec: AudioCodec::Pcm,
            sample_rate,
            channels,
            rtp_clock_rate: sample_rate,
        };
        if bits & 0x7000_0000 != 0 {
            return AudioFormat {
                codec: AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 1,
                rtp_clock_rate: opus_clock_rate(bits),
            };
        }
        if bits & 0xc0_0000 != 0 {
            let sample_rate = if bits & 0x80_0000 != 0 {
                48_000
            } else {
                44_100
            };
            return AudioFormat {
                codec: AudioCodec::AacLc,
                sample_rate,
                channels: 2,
                rtp_clock_rate: sample_rate,
            };
        }
        match bits {
            0x4 => pcm(8_000, 1),
            0x8 => pcm(8_000, 2),
            0x10 => pcm(16_000, 1),
            0x20 => pcm(16_000, 2),
            0x40 => pcm(24_000, 1),
            0x80 => pcm(24_000, 2),
            0x100 => pcm(32_000, 1),
            0x200 => pcm(32_000, 2),
            0x400 => pcm(44_100, 1),
            0x800 => pcm(44_100, 2),
            0x4000 => pcm(48_000, 1),
            0x8000 => pcm(48_000, 2),
            _ => pcm(44_100, 2),
        }
    }
}

/// A datagram that is not a packet of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketError {
    TooShort,
    Authentication,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PacketError::TooShort => f.write_str("audio packet shorter than its header and tail"),
            PacketError::Authentication => f.write_str("audio packet did not decrypt"),
        }
    }
}

impl std::error::Error for PacketError {}

/// An opened packet: the RTP header followed by the clear payload, and its timestamp.
pub fn open_packet(key: &[u8; 32], wire: &[u8]) -> Result<(Vec<u8>, u32), PacketError> {
    if wire.len() < RTP_HEADER_BYTES + TAG_BYTES + NONCE_TAIL_BYTES {
        return Err(PacketError::TooShort);
    }
    let sealed_end = wire.len() - NONCE_TAIL_BYTES;
    let mut nonce = [0; crypto::NONCE_BYTES];
    nonce[4..].copy_from_slice(&wire[sealed_end..]);
    let timestamp = u32::from_be_bytes(wire[4..8].try_into().expect("four bytes"));
    let mut rtp = wire[..sealed_end].to_vec();
    let mut payload = rtp.split_off(RTP_HEADER_BYTES);
    crypto::open_in_place(key, &nonce, &wire[4..RTP_HEADER_BYTES], &mut payload)
        .map_err(|_| PacketError::Authentication)?;
    rtp.extend_from_slice(&payload);
    Ok((rtp, timestamp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_decodes_at_48k_but_keeps_its_clock() {
        for (bits, clock) in [
            (0x1000_0000, 16_000),
            (0x2000_0000, 24_000),
            (0x4000_0000, 48_000),
        ] {
            let format = AudioFormat::from_bits(bits);
            assert_eq!(format.codec, AudioCodec::Opus);
            assert_eq!(format.sample_rate, 48_000);
            assert_eq!(format.rtp_clock_rate, clock);
        }
    }

    #[test]
    fn other_formats_count_at_their_sample_rate() {
        assert_eq!(AudioFormat::from_bits(0x80_0000).rtp_clock_rate, 48_000);
        assert_eq!(AudioFormat::from_bits(0x40_0000).rtp_clock_rate, 44_100);
        assert_eq!(AudioFormat::from_bits(0x10).rtp_clock_rate, 16_000);
        assert_eq!(AudioFormat::from_bits(0x8000).channels, 2);
        assert_eq!(AudioFormat::from_bits(0).codec, AudioCodec::Pcm);
    }

    #[test]
    fn packets_open() {
        let key = [3; 32];
        let header = [0x80, 0x60, 0, 1, 0, 0, 0x12, 0x34, 9, 9, 9, 9];
        let nonce_counter = 7u64;
        let mut nonce = [0; 12];
        nonce[4..].copy_from_slice(&nonce_counter.to_le_bytes());
        let sealed = crypto::seal(&key, &nonce, &header[4..], b"audio");
        let wire = [&header[..], &sealed, &nonce_counter.to_le_bytes()].concat();
        let (rtp, timestamp) = open_packet(&key, &wire).unwrap();
        assert_eq!(timestamp, 0x1234);
        assert_eq!(&rtp[..12], header);
        assert_eq!(&rtp[12..], b"audio");
        let mut tampered = wire.clone();
        tampered[5] ^= 1;
        assert_eq!(
            open_packet(&key, &tampered),
            Err(PacketError::Authentication)
        );
        assert_eq!(open_packet(&key, &wire[..30]), Err(PacketError::TooShort));
    }
}
