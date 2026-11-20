// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/MicrophonePacketizer.kt and
// airplay/MicrophoneStart.kt (DiPlay commit 615b32c).

//! The microphone uplink: the receiver's captured audio, sent to the iPhone in the layout of its own audio
//! streams, sealed under the stream's input key.

use crate::audio::{opus_clock_rate, AudioCodec, AudioFormat, RTP_HEADER_BYTES};
use crate::crypto::{self, TAG_BYTES};

const OPUS_SAMPLES_PER_PACKET: u32 = 960;
const OPUS_FRAME_MS: u32 = 20;
/// What a sealed packet adds to its samples.
pub const SEAL_OVERHEAD: usize = RTP_HEADER_BYTES + TAG_BYTES + 8;
/// A 1,500-byte link MTU less the IPv6 and UDP headers.
const MAX_UDP_PAYLOAD: usize = 1500 - 40 - 8;
/// How long a stream that asks for input but plays nothing waits before the microphone starts anyway; calls
/// wait for their downlink, which echo cancellation needs.
pub const SILENT_START_MS: u64 = 500;

/// What the iPhone asked the microphone for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MicrophoneFormat {
    pub codec: AudioCodec,
    /// The capture rate; Opus captures at 48 kHz.
    pub sample_rate: u32,
    pub channels: u8,
    /// Milliseconds of audio in a packet.
    pub frame_ms: u32,
    /// The rate the RTP timestamps count at.
    pub rtp_clock_rate: u32,
    /// The Opus bit rate to encode at.
    pub bitrate: Option<u32>,
}

impl MicrophoneFormat {
    /// The format for a stream's audioFormat bits and its `framesPerPacket`, if any.
    pub fn new(bits: u64, frames_per_packet: Option<u32>) -> MicrophoneFormat {
        let format = AudioFormat::from_bits(bits);
        let frame_ms = match (format.codec, frames_per_packet) {
            (AudioCodec::Opus, _) => OPUS_FRAME_MS,
            (_, Some(frames)) if frames > 0 => {
                ((f64::from(frames) * 1000.0 / f64::from(format.sample_rate)).round() as u32)
                    .clamp(5, 60)
            }
            _ => pcm_frame_ms(format.sample_rate, format.channels),
        };
        let bitrate = (format.codec == AudioCodec::Opus).then_some({
            if bits & 0x4000_0000 != 0 {
                96_000
            } else if bits & 0x2000_0000 != 0 {
                64_000
            } else {
                48_000
            }
        });
        MicrophoneFormat {
            codec: format.codec,
            sample_rate: format.sample_rate,
            channels: format.channels,
            frame_ms,
            rtp_clock_rate: if format.codec == AudioCodec::Opus {
                opus_clock_rate(bits)
            } else {
                format.sample_rate
            },
            bitrate,
        }
    }

    /// Samples captured for one packet.
    pub fn samples_per_packet(&self) -> u32 {
        if self.codec == AudioCodec::Opus {
            OPUS_SAMPLES_PER_PACKET
        } else {
            (self.sample_rate * self.frame_ms / 1000).max(1)
        }
    }

    /// How far one packet moves the RTP timestamp.
    pub fn rtp_samples_per_packet(&self) -> u32 {
        if self.codec == AudioCodec::Opus {
            self.rtp_clock_rate * OPUS_FRAME_MS / 1000
        } else {
            self.samples_per_packet()
        }
    }

    /// Bytes of 16-bit PCM captured for one packet.
    pub fn frame_bytes(&self) -> usize {
        self.samples_per_packet() as usize * usize::from(self.channels) * 2
    }
}

/// The longest of 20, 10 and 5 ms of PCM whose sealed packet fits one link MTU, so that it is not fragmented.
pub fn pcm_frame_ms(sample_rate: u32, channels: u8) -> u32 {
    [20, 10, 5]
        .into_iter()
        .find(|ms| {
            SEAL_OVERHEAD + (sample_rate * ms / 1000) as usize * usize::from(channels) * 2
                <= MAX_UDP_PAYLOAD
        })
        .unwrap_or(5)
}

/// Captured PCM is little-endian; the uplink sends it big-endian.
pub fn to_wire_pcm(little_endian: &[u8]) -> Vec<u8> {
    let mut out = little_endian.to_vec();
    for pair in out.chunks_exact_mut(2) {
        pair.swap(0, 1);
    }
    out
}

/// Seals one stream's uplink packets, counting sequence, timestamp and nonce.
#[derive(Clone, Debug)]
pub struct Packetizer {
    key: [u8; 32],
    payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub nonce: u64,
}

impl Packetizer {
    pub fn new(key: [u8; 32], payload_type: u8) -> Packetizer {
        Packetizer {
            key,
            payload_type,
            sequence: 0,
            timestamp: 0,
            nonce: 0,
        }
    }

    /// One packet of `body`, which moves the timestamp by `samples`.
    pub fn seal(&mut self, body: &[u8], samples: u32) -> Vec<u8> {
        let mut packet = Vec::with_capacity(SEAL_OVERHEAD + body.len());
        packet.extend_from_slice(&[0x80, self.payload_type & 0x7f]);
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        packet.extend_from_slice(&self.timestamp.to_be_bytes());
        packet.extend_from_slice(&[0; 4]);
        let mut sealed = body.to_vec();
        crypto::seal_in_place(
            &self.key,
            &crypto::counter_nonce(self.nonce),
            &packet[4..],
            &mut sealed,
        );
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&self.nonce.to_le_bytes());
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples);
        self.nonce += 1;
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::open_packet;

    #[test]
    fn packets_carry_the_rtp_header_and_open_with_the_key() {
        let key: [u8; 32] = std::array::from_fn(|i| i as u8 + 1);
        let mut packetizer = Packetizer::new(key, 100);
        let packet = packetizer.seal(&[0x28, 1, 2, 3], 480);
        assert_eq!(&packet[..8], [0x80, 100, 0, 0, 0, 0, 0, 0]);
        let (rtp, _) = open_packet(&key, &packet).unwrap();
        assert_eq!(&rtp[12..], [0x28, 1, 2, 3]);
        assert_eq!(
            (packetizer.sequence, packetizer.timestamp, packetizer.nonce),
            (1, 480, 1)
        );
    }

    #[test]
    fn pcm_goes_big_endian() {
        assert_eq!(
            to_wire_pcm(&[0x34, 0x12, 0x78, 0x56]),
            [0x12, 0x34, 0x56, 0x78]
        );
    }

    #[test]
    fn opus_timestamps_count_in_the_chosen_clock() {
        let siri = MicrophoneFormat::new(0x2000_0000, None);
        assert_eq!(siri.samples_per_packet(), 960);
        assert_eq!(siri.frame_bytes(), 1920);
        assert_eq!(siri.rtp_samples_per_packet(), 480);
        assert_eq!(siri.bitrate, Some(64_000));
        assert_eq!(
            MicrophoneFormat::new(0x4000_0000, None).rtp_samples_per_packet(),
            960
        );
        assert_eq!(
            MicrophoneFormat::new(0x1000_0000, None).rtp_samples_per_packet(),
            320
        );
        assert_eq!(
            MicrophoneFormat::new(0x10, None).rtp_samples_per_packet(),
            320
        );
        let wired_siri = MicrophoneFormat::new(0x40, None);
        assert_eq!(wired_siri.rtp_clock_rate, 24_000);
        assert_eq!(wired_siri.rtp_samples_per_packet(), 480);
    }

    #[test]
    fn packet_sequences() {
        for (bits, step) in [
            (0x2000_0000u64, 480u32),
            (0x4000_0000, 960),
            (0x1000_0000, 320),
        ] {
            let format = MicrophoneFormat::new(bits, None);
            let mut packetizer = Packetizer::new([1; 32], 100);
            for index in 0..3u32 {
                let packet = packetizer.seal(&[0xf8, index as u8], format.rtp_samples_per_packet());
                assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), index as u16);
                assert_eq!(
                    u32::from_be_bytes(packet[4..8].try_into().unwrap()),
                    index * step
                );
                let tail = &packet[packet.len() - 8..];
                assert_eq!(
                    u64::from_le_bytes(tail.try_into().unwrap()),
                    u64::from(index)
                );
            }
            assert_eq!(packetizer.timestamp, 3 * step);
        }
    }

    #[test]
    fn pcm_packets_fit_one_mtu() {
        assert_eq!(pcm_frame_ms(48_000, 1), 10);
        assert_eq!(pcm_frame_ms(24_000, 1), 20);
        assert_eq!(pcm_frame_ms(16_000, 1), 20);
        assert_eq!(pcm_frame_ms(48_000, 2), 5);
        for (rate, channels, bits) in [
            (48_000, 1, 0x4000),
            (48_000, 2, 0x8000),
            (44_100, 1, 0x400),
            (24_000, 1, 0x40),
        ] {
            let format = MicrophoneFormat::new(bits, None);
            assert_eq!((format.sample_rate, format.channels), (rate, channels));
            let packet = Packetizer::new([0; 32], 100)
                .seal(&vec![0; format.frame_bytes()], format.samples_per_packet());
            assert!(
                40 + 8 + packet.len() <= 1500,
                "{rate} Hz x{channels}: {} bytes",
                packet.len()
            );
        }
        assert_eq!(MicrophoneFormat::new(0x10, Some(480)).frame_ms, 30);
    }
}
