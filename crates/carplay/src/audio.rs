// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The iPhone's sound as the head unit plays it: music as AAC frames with ADTS headers, everything else as 16-bit
//! little-endian PCM tagged with what it is.

/// What a sound is, from the AirPlay audio type of its stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AudioKind {
    /// Music and other media.
    Media,
    /// A phone call.
    Call,
    /// Siri.
    Assistant,
    /// Alerts and ring tones.
    Alert,
    /// Navigation prompts and other speech.
    Speech,
    /// Sounds of apps that do not say.
    System,
}

impl AudioKind {
    /// The kind of an AirPlay audio type, in any case: the AirPlay crate gives it in lower case.
    pub fn from_audio_type(audio_type: &str) -> AudioKind {
        match audio_type.to_ascii_lowercase().as_str() {
            "media" => AudioKind::Media,
            "telephony" => AudioKind::Call,
            "speechrecognition" => AudioKind::Assistant,
            "alert" => AudioKind::Alert,
            "default" => AudioKind::Speech,
            _ => AudioKind::System,
        }
    }
}

/// The payload of an RTP packet: after the fixed header, the contributing sources and any extension, and without
/// padding. `None` for a packet too short or not version 2.
pub fn rtp_payload(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let mut start = 12 + 4 * usize::from(packet[0] & 0x0F);
    if packet[0] & 0x10 != 0 {
        let words = usize::from(u16::from_be_bytes([
            *packet.get(start + 2)?,
            *packet.get(start + 3)?,
        ]));
        start += 4 + 4 * words;
    }
    let mut end = packet.len();
    if packet[0] & 0x20 != 0 {
        end = end.checked_sub(usize::from(*packet.last()?))?;
    }
    packet.get(start..end)
}

/// An AAC-LC frame with its ADTS header, which the head unit's decoder and the transport stream take. `None` for a
/// sample rate ADTS cannot say or a frame too long for it.
pub fn adts(raw: &[u8], sample_rate: u32, channels: u8) -> Option<Vec<u8>> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let rate = RATES.iter().position(|&r| r == sample_rate)? as u8;
    let len = raw.len() + 7;
    if len > 0x1FFF || channels == 0 || channels > 7 {
        return None;
    }
    let profile = 1; // AAC LC, minus one
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&[
        0xFF,
        0xF1, // MPEG-4, no CRC
        (profile << 6) | (rate << 2) | (channels >> 2),
        ((channels & 3) << 6) | (len >> 11) as u8,
        (len >> 3) as u8,
        (((len & 7) << 5) as u8) | 0x1F,
        0xFC,
    ]);
    out.extend_from_slice(raw);
    Some(out)
}

/// 16-bit samples from network byte order to little-endian. A trailing odd byte is dropped.
pub fn pcm_to_little_endian(big_endian: &[u8]) -> Vec<u8> {
    big_endian
        .chunks_exact(2)
        .flat_map(|s| [s[1], s[0]])
        .collect()
}

/// Cuts a stream of PCM into frames of a fixed size, keeping what is left for the next push.
pub struct Framer {
    frame_bytes: usize,
    pending: Vec<u8>,
}

impl Framer {
    pub fn new(frame_bytes: usize) -> Framer {
        Framer {
            frame_bytes: frame_bytes.max(2),
            pending: Vec::new(),
        }
    }

    /// Adds `data` and returns the frames now complete.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(data);
        let whole = self.pending.len() / self.frame_bytes * self.frame_bytes;
        let frames = self.pending[..whole]
            .chunks(self.frame_bytes)
            .map(<[u8]>::to_vec)
            .collect();
        self.pending.drain(..whole);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(AudioKind::from_audio_type("media"), AudioKind::Media);
        assert_eq!(AudioKind::from_audio_type("telephony"), AudioKind::Call);
        assert_eq!(
            AudioKind::from_audio_type("speechRecognition"),
            AudioKind::Assistant
        );
        assert_eq!(
            AudioKind::from_audio_type("speechrecognition"),
            AudioKind::Assistant
        );
        assert_eq!(AudioKind::from_audio_type("alert"), AudioKind::Alert);
        assert_eq!(AudioKind::from_audio_type("default"), AudioKind::Speech);
        assert_eq!(
            AudioKind::from_audio_type("compatibility"),
            AudioKind::System
        );
    }

    #[test]
    fn payloads() {
        let mut packet = vec![0x80, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        packet.extend_from_slice(b"abc");
        assert_eq!(rtp_payload(&packet), Some(&b"abc"[..]));
        // One CSRC, an extension of one word, two bytes of padding.
        let mut packet = vec![
            0xB1, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 3, 4, 0xBE, 0xDE, 0, 1, 9, 9, 9, 9,
        ];
        packet.extend_from_slice(b"xy\0\x02");
        assert_eq!(rtp_payload(&packet), Some(&b"xy"[..]));
        assert_eq!(rtp_payload(&[0x40; 12]), None);
        assert_eq!(rtp_payload(&[0x80; 5]), None);
    }

    #[test]
    fn adts_header() {
        let frame = adts(&[0xAA; 100], 48000, 2).unwrap();
        assert_eq!(frame.len(), 107);
        assert_eq!(&frame[..2], &[0xFF, 0xF1]);
        // LC, 48 kHz (index 3), two channels.
        assert_eq!(frame[2], 0x4C);
        assert_eq!(frame[3] >> 6, 2);
        let len = (usize::from(frame[3] & 3) << 11)
            | (usize::from(frame[4]) << 3)
            | usize::from(frame[5] >> 5);
        assert_eq!(len, 107);
        assert_eq!(adts(&[0; 10], 12345, 2), None);
    }

    #[test]
    fn frames() {
        let mut framer = Framer::new(4);
        assert!(framer.push(&[1, 2, 3]).is_empty());
        assert_eq!(
            framer.push(&[4, 5, 6, 7, 8, 9]),
            [vec![1, 2, 3, 4], vec![5, 6, 7, 8]]
        );
        assert_eq!(framer.push(&[10, 11, 12]), [vec![9, 10, 11, 12]]);
    }

    #[test]
    fn byte_order() {
        assert_eq!(
            pcm_to_little_endian(&[0x12, 0x34, 0x56, 0x78, 0x9A]),
            [0x34, 0x12, 0x78, 0x56]
        );
    }
}
