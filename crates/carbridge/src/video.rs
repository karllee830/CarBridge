// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Where the cast's pictures come from.

use std::io;
use std::path::Path;
use std::time::Duration;

use carbridge_carlink_wfd::h264::AccessUnit;
use carbridge_carlink_wfd::Codec;
use tokio::time::{interval, Interval, MissedTickBehavior};

use crate::options::Hevc;

/// The codec of a cast, as chosen at M4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CastCodec {
    pub codec: Codec,
    /// The iPhone's session of the moment is in H.265, which the head unit does not take: the cast shows the waiting
    /// clip until CarPlay connects again, in H.264.
    pub session_left_out: bool,
}

/// The codec of a cast to a head unit that `offered` H.265 or not, while the iPhone's session of the moment, if any,
/// is in `session`. A session goes on in its codec when the head unit takes it.
pub fn cast_codec(hevc: Hevc, offered: bool, session: Option<Codec>) -> CastCodec {
    let takes_hevc = match hevc {
        Hevc::Off => false,
        Hevc::Auto => offered,
        Hevc::Force => true,
    };
    let codec = match session {
        Some(Codec::H264) => Codec::H264,
        Some(Codec::H265) | None if takes_hevc => Codec::H265,
        _ => Codec::H264,
    };
    CastCodec {
        codec,
        session_left_out: session.is_some_and(|session| session != codec),
    }
}

/// Whether iPhones that connect are offered H.265, while the cast of the moment, if any, is in `cast`.
pub fn offer_hevc(hevc: Hevc, cast: Option<Codec>) -> bool {
    match hevc {
        Hevc::Off => false,
        Hevc::Auto => cast == Some(Codec::H265),
        Hevc::Force => true,
    }
}

/// One picture to send.
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub key: bool,
}

/// An H.264 or H.265 clip cast in a loop at a fixed frame rate.
pub struct Clip {
    units: Vec<AccessUnit>,
    next: usize,
    tick: Interval,
}

impl Clip {
    pub async fn open(path: &Path, codec: Codec, fps: u32) -> io::Result<Clip> {
        let stream = tokio::fs::read(path).await?;
        Clip::from_stream(&stream, codec, fps)
    }

    pub fn from_stream(stream: &[u8], codec: Codec, fps: u32) -> io::Result<Clip> {
        let units = codec.access_units(stream);
        if !units.first().is_some_and(|unit| unit.key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "clip does not start with a key frame",
            ));
        }
        let fps = fps.max(1);
        let mut tick = interval(Duration::from_secs(1) / fps);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Ok(Clip {
            units,
            next: 0,
            tick,
        })
    }

    /// Waits for the next frame's time and returns it.
    pub async fn next(&mut self) -> Frame<'_> {
        self.tick.tick().await;
        let unit = &self.units[self.next];
        self.next = (self.next + 1) % self.units.len();
        Frame {
            data: &unit.data,
            key: unit.key,
        }
    }

    /// Goes back to the last key frame, so that the next frame is one.
    pub fn key_frame(&mut self) {
        let current = self.next.checked_sub(1).unwrap_or(self.units.len() - 1);
        self.next = (0..=current)
            .rev()
            .find(|&i| self.units[i].key)
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> Vec<u8> {
        let mut stream = Vec::new();
        for unit in [
            &[0x67u8, 1][..],
            &[0x68, 2],
            &[0x65, 0x88],
            &[0x41, 0x9A],
            &[0x65, 0x88],
            &[0x41, 0x9A],
        ] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(unit);
        }
        stream
    }

    #[tokio::test(start_paused = true)]
    async fn loops() {
        let mut clip = Clip::from_stream(&clip(), Codec::H264, 30).unwrap();
        let mut seen = Vec::new();
        for _ in 0..5 {
            seen.push(clip.next().await.key);
        }
        assert_eq!(seen, [true, false, true, false, true]);
    }

    #[tokio::test(start_paused = true)]
    async fn key_frame_request_goes_back() {
        let mut clip = Clip::from_stream(&clip(), Codec::H264, 30).unwrap();
        clip.next().await;
        clip.next().await;
        clip.key_frame();
        assert!(clip.next().await.key);
    }

    #[test]
    fn needs_a_key_frame_first() {
        let stream = [0, 0, 0, 1, 0x41, 0x9A];
        assert!(Clip::from_stream(&stream, Codec::H264, 30).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn hevc_clip() {
        let mut stream = Vec::new();
        // VPS, SPS, PPS, IDR_N_LP, then two TRAIL_R pictures.
        for unit in [
            &[0x40u8, 0x01, 0x0C][..],
            &[0x42, 0x01, 0x01],
            &[0x44, 0x01, 0xC1],
            &[0x28, 0x01, 0xAF],
            &[0x02, 0x01, 0xD0],
            &[0x02, 0x01, 0xD1],
        ] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(unit);
        }
        let mut clip = Clip::from_stream(&stream, Codec::H265, 30).unwrap();
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(clip.next().await.key);
        }
        assert_eq!(seen, [true, false, false, true]);
        assert!(Clip::from_stream(&stream, Codec::H264, 30).is_err());
    }

    #[test]
    fn packaged_clips_start_with_a_key_frame() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../openwrt/carbridge/files");
        for (name, codec) in [("waiting.h264", Codec::H264), ("waiting.h265", Codec::H265)] {
            let stream = std::fs::read(dir.join(name)).unwrap();
            let units = codec.access_units(&stream);
            assert_eq!(units.len(), 30, "{name}");
            assert!(units[0].key, "{name}");
        }
    }

    #[test]
    fn packaged_state_screens_start_with_a_key_frame() {
        use crate::screens::StateScreen;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../openwrt/carbridge/files");
        for screen in StateScreen::NAMED {
            let name = screen.clip_name().unwrap();
            for (shape, codec) in [
                ("", Codec::H264),
                ("", Codec::H265),
                ("-portrait", Codec::H264),
                ("-portrait", Codec::H265),
            ] {
                let file = format!(
                    "{name}{shape}.{}",
                    if codec == Codec::H264 { "h264" } else { "h265" }
                );
                let stream = std::fs::read(dir.join(&file)).unwrap();
                let units = codec.access_units(&stream);
                assert_eq!(units.len(), 30, "{file}");
                assert!(units[0].key, "{file}");
            }
        }
    }

    #[test]
    fn codec_of_a_cast() {
        let cast = |hevc, offered, session| {
            let choice = cast_codec(hevc, offered, session);
            (choice.codec, choice.session_left_out)
        };
        use Codec::{H264, H265};
        // No iPhone yet: the mode and the head unit decide.
        assert_eq!(cast(Hevc::Off, true, None), (H264, false));
        assert_eq!(cast(Hevc::Auto, false, None), (H264, false));
        assert_eq!(cast(Hevc::Auto, true, None), (H265, false));
        assert_eq!(cast(Hevc::Force, false, None), (H265, false));
        // An H.264 session goes on in H.264 with any head unit.
        assert_eq!(cast(Hevc::Auto, true, Some(H264)), (H264, false));
        assert_eq!(cast(Hevc::Force, false, Some(H264)), (H264, false));
        // An H.265 session goes on where the head unit takes H.265, and is left out where it does not.
        assert_eq!(cast(Hevc::Auto, true, Some(H265)), (H265, false));
        assert_eq!(cast(Hevc::Force, false, Some(H265)), (H265, false));
        assert_eq!(cast(Hevc::Auto, false, Some(H265)), (H264, true));
        assert_eq!(cast(Hevc::Off, true, Some(H265)), (H264, true));
    }

    #[test]
    fn hevc_offered_to_iphones() {
        use Codec::{H264, H265};
        for cast in [None, Some(H264), Some(H265)] {
            assert!(!offer_hevc(Hevc::Off, cast));
            assert!(offer_hevc(Hevc::Force, cast));
        }
        assert!(!offer_hevc(Hevc::Auto, None));
        assert!(!offer_hevc(Hevc::Auto, Some(H264)));
        assert!(offer_hevc(Hevc::Auto, Some(H265)));
    }
}
