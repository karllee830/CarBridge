// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Carlink screen cast: Wi-Fi Display source (RTSP, MPEG-TS over RTP) and the input back channel.
//!
//! - [`source`]: the RTSP exchange with the head unit's sink, and sending the stream.
//! - [`ts`]: the transport stream.
//! - [`rtp`]: RTP packets framed for the sink's TCP session.
//! - [`h264`], [`h265`]: NAL units and access units of an Annex B stream.
//! - [`uibc`]: touches and keys from the head unit.
//! - [`rtsp`]: RTSP messages.

pub mod h264;
pub mod h265;
pub mod rtp;
pub mod rtsp;
pub mod source;
pub mod ts;
pub mod uibc;

/// The video codec of a cast.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Codec {
    #[default]
    H264,
    H265,
}

impl Codec {
    /// The access units of an Annex B stream.
    pub fn access_units(self, stream: &[u8]) -> Vec<h264::AccessUnit> {
        match self {
            Codec::H264 => h264::access_units(stream),
            Codec::H265 => h265::access_units(stream),
        }
    }

    /// The access unit without its access unit delimiters, in Annex B form with four-byte start codes.
    pub fn without_delimiters(self, access_unit: &[u8]) -> Vec<u8> {
        match self {
            Codec::H264 => h264::without_delimiters(access_unit),
            Codec::H265 => h265::without_delimiters(access_unit),
        }
    }
}

impl std::fmt::Display for Codec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
        })
    }
}
