// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Opus, in which the iPhone sends everything but music and wants the car's microphone, to and from the 16-bit
//! little-endian PCM the head unit plays and records.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::audio::AudioKind;

/// The longest Opus packet, 120 ms, in samples per channel at 48 kHz.
const MAX_FRAME: usize = 5760;
/// The largest encoded packet the encoder may write.
const MAX_PACKET: usize = 4000;
/// Encoder complexity, 0 to 10: speech stays clear at a fraction of the work of the highest setting.
const COMPLEXITY: i32 = 5;

/// The rate the iPhone's Opus of a kind of sound is decoded at: calls at 16 kHz, as the head unit plays a VoIP
/// call, the rest at Opus's own 48 kHz.
pub fn decode_rate(kind: AudioKind) -> u32 {
    match kind {
        AudioKind::Call => 16000,
        _ => 48000,
    }
}

/// An Opus decoder with 16-bit output.
pub struct Decoder {
    state: *mut unsafe_libopus::OpusDecoder,
    channels: u8,
}

// The decoder's state is owned by it alone and used through `&mut self`.
unsafe impl Send for Decoder {}

impl Decoder {
    /// A decoder to `rate` (8, 12, 16, 24 or 48 kHz) and one or two channels; `None` for others.
    pub fn new(rate: u32, channels: u8) -> Option<Decoder> {
        let mut error = 0;
        // SAFETY: the arguments are plain values and `error` outlives the call.
        let state = unsafe {
            unsafe_libopus::opus_decoder_create(rate as i32, i32::from(channels), &mut error)
        };
        (error == unsafe_libopus::OPUS_OK && !state.is_null())
            .then_some(Decoder { state, channels })
    }

    /// One packet as little-endian PCM; `None` for a packet that does not decode.
    pub fn decode(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        let channels = usize::from(self.channels);
        let mut pcm = vec![0i16; MAX_FRAME * channels];
        // SAFETY: `pcm` holds `MAX_FRAME` samples per channel, the frame size passed; `packet` is read for its
        // length only.
        let samples = unsafe {
            unsafe_libopus::opus_decode(
                self.state,
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                MAX_FRAME as i32,
                0,
            )
        };
        let samples = usize::try_from(samples).ok()?;
        Some(
            pcm[..samples * channels]
                .iter()
                .flat_map(|s| s.to_le_bytes())
                .collect(),
        )
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the state came from `opus_decoder_create` and is freed once.
        unsafe { unsafe_libopus::opus_decoder_destroy(self.state) }
    }
}

/// An Opus encoder for speech, from 16-bit input.
pub struct Encoder {
    state: *mut unsafe_libopus::OpusEncoder,
    channels: u8,
}

// The encoder's state is owned by it alone and used through `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    /// An encoder from `rate` (8, 12, 16, 24 or 48 kHz) and one or two channels, at `bitrate` bits a second or
    /// Opus's own choice; `None` for others.
    pub fn new(rate: u32, channels: u8, bitrate: Option<u32>) -> Option<Encoder> {
        let mut error = 0;
        // SAFETY: the arguments are plain values and `error` outlives the call.
        let state = unsafe {
            unsafe_libopus::opus_encoder_create(
                rate as i32,
                i32::from(channels),
                unsafe_libopus::OPUS_APPLICATION_VOIP,
                &mut error,
            )
        };
        if error != unsafe_libopus::OPUS_OK || state.is_null() {
            return None;
        }
        let encoder = Encoder { state, channels };
        // SAFETY: the state is a live encoder; each request takes one i32.
        unsafe {
            unsafe_libopus::opus_encoder_ctl!(
                state,
                unsafe_libopus::OPUS_SET_COMPLEXITY_REQUEST,
                COMPLEXITY
            );
            unsafe_libopus::opus_encoder_ctl!(
                state,
                unsafe_libopus::OPUS_SET_SIGNAL_REQUEST,
                unsafe_libopus::OPUS_SIGNAL_VOICE
            );
            if let Some(bitrate) = bitrate {
                unsafe_libopus::opus_encoder_ctl!(
                    state,
                    unsafe_libopus::OPUS_SET_BITRATE_REQUEST,
                    bitrate as i32
                );
            }
        }
        Some(encoder)
    }

    /// One frame of little-endian PCM, 2.5 to 60 ms of it, as a packet; `None` for a frame of another length.
    pub fn encode(&mut self, pcm: &[u8]) -> Option<Vec<u8>> {
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]))
            .collect();
        let frame = samples.len() / usize::from(self.channels);
        let mut packet = vec![0u8; MAX_PACKET];
        // SAFETY: `samples` holds `frame` samples per channel and `packet` `MAX_PACKET` bytes, as passed.
        let len = unsafe {
            unsafe_libopus::opus_encode(
                self.state,
                samples.as_ptr(),
                frame as i32,
                packet.as_mut_ptr(),
                MAX_PACKET as i32,
            )
        };
        packet.truncate(usize::try_from(len).ok()?);
        Some(packet)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the state came from `opus_encoder_create` and is freed once.
        unsafe { unsafe_libopus::opus_encoder_destroy(self.state) }
    }
}

/// The decoders of the iPhone's Opus streams, by stream type and audio type, and the encoders of the microphones
/// it wants in Opus, by stream type.
#[derive(Default)]
pub struct Codecs {
    decoders: Mutex<HashMap<(u16, String), Decoder>>,
    encoders: Mutex<HashMap<u16, Encoder>>,
}

impl Codecs {
    /// Starts decoding a stream to `rate` and one channel; returns whether it can be.
    pub fn start_decoding(&self, stream_type: u16, audio_type: &str, rate: u32) -> bool {
        let Some(decoder) = Decoder::new(rate, 1) else {
            return false;
        };
        self.decoders
            .lock()
            .unwrap()
            .insert((stream_type, audio_type.to_owned()), decoder);
        true
    }

    /// One packet of a stream as PCM, starting its decoder at `rate` if it has none yet.
    pub fn decode(
        &self,
        stream_type: u16,
        audio_type: &str,
        rate: u32,
        packet: &[u8],
    ) -> Option<Vec<u8>> {
        let mut decoders = self.decoders.lock().unwrap();
        let key = (stream_type, audio_type.to_owned());
        if !decoders.contains_key(&key) {
            decoders.insert(key.clone(), Decoder::new(rate, 1)?);
        }
        decoders.get_mut(&key)?.decode(packet)
    }

    pub fn stop_decoding(&self, stream_type: u16, audio_type: &str) {
        self.decoders
            .lock()
            .unwrap()
            .remove(&(stream_type, audio_type.to_owned()));
    }

    /// Starts encoding a microphone; returns whether it can be.
    pub fn start_encoding(
        &self,
        stream_type: u16,
        rate: u32,
        channels: u8,
        bitrate: Option<u32>,
    ) -> bool {
        let Some(encoder) = Encoder::new(rate, channels, bitrate) else {
            return false;
        };
        self.encoders.lock().unwrap().insert(stream_type, encoder);
        true
    }

    /// A frame of a microphone encoded, or `None` when the microphone is not encoded.
    pub fn encode(&self, stream_type: u16, pcm: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut encoders = self.encoders.lock().unwrap();
        encoders
            .get_mut(&stream_type)
            .map(|encoder| encoder.encode(pcm))
    }

    pub fn stop_encoding(&self, stream_type: u16) {
        self.encoders.lock().unwrap().remove(&stream_type);
    }

    /// Drops every decoder and encoder.
    pub fn clear(&self) {
        self.decoders.lock().unwrap().clear();
        self.encoders.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 ms of a 440 Hz tone at `rate`, mono, as little-endian PCM.
    fn tone(rate: u32, offset: usize) -> Vec<u8> {
        let samples = rate as usize / 50;
        (offset..offset + samples)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                ((t * 440.0 * std::f64::consts::TAU).sin() * 8000.0) as i16
            })
            .flat_map(i16::to_le_bytes)
            .collect()
    }

    fn energy(pcm: &[u8]) -> f64 {
        pcm.chunks_exact(2)
            .map(|s| f64::from(i16::from_le_bytes([s[0], s[1]])).powi(2))
            .sum::<f64>()
            / (pcm.len() / 2) as f64
    }

    #[test]
    fn round_trip() {
        let mut encoder = Encoder::new(48000, 1, Some(96000)).unwrap();
        let mut decoder = Decoder::new(48000, 1).unwrap();
        let mut last = Vec::new();
        for frame in 0..10 {
            let packet = encoder.encode(&tone(48000, frame * 960)).unwrap();
            assert!(!packet.is_empty() && packet.len() < MAX_PACKET);
            last = decoder.decode(&packet).unwrap();
            assert_eq!(last.len(), 960 * 2);
        }
        // The tone comes back near its own level once the codec has settled.
        let (sent, received) = (energy(&tone(48000, 0)), energy(&last));
        assert!(
            received > sent / 4.0 && received < sent * 4.0,
            "{sent} {received}"
        );
    }

    #[test]
    fn decodes_at_the_rate_asked_for() {
        let mut encoder = Encoder::new(48000, 1, None).unwrap();
        let mut decoder = Decoder::new(16000, 1).unwrap();
        let pcm = decoder
            .decode(&encoder.encode(&tone(48000, 0)).unwrap())
            .unwrap();
        assert_eq!(pcm.len(), 320 * 2);
    }

    #[test]
    fn refuses_what_opus_cannot_do() {
        assert!(Decoder::new(44100, 1).is_none());
        assert!(Encoder::new(48000, 3, None).is_none());
        let mut encoder = Encoder::new(48000, 1, None).unwrap();
        // 7 ms is not an Opus frame length.
        assert!(encoder.encode(&vec![0; 336 * 2]).is_none());
        assert!(Decoder::new(48000, 1).unwrap().decode(&[0xFF; 3]).is_none());
    }

    #[test]
    fn codecs_by_stream() {
        let codecs = Codecs::default();
        assert!(codecs.encode(100, &tone(48000, 0)).is_none());
        assert!(codecs.start_encoding(100, 48000, 1, Some(48000)));
        let packet = codecs.encode(100, &tone(48000, 0)).unwrap().unwrap();
        assert!(codecs.start_decoding(100, "telephony", decode_rate(AudioKind::Call)));
        assert_eq!(
            codecs
                .decode(100, "telephony", 16000, &packet)
                .unwrap()
                .len(),
            640
        );
        // A stream whose start was missed gets a decoder with its first packet.
        assert_eq!(
            codecs.decode(101, "default", 48000, &packet).unwrap().len(),
            1920
        );
        codecs.stop_encoding(100);
        assert!(codecs.encode(100, &tone(48000, 0)).is_none());
        codecs.clear();
    }

    #[test]
    fn rates() {
        assert_eq!(decode_rate(AudioKind::Call), 16000);
        assert_eq!(decode_rate(AudioKind::Assistant), 48000);
        assert_eq!(decode_rate(AudioKind::Speech), 48000);
    }
}
