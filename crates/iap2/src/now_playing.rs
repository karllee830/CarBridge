// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from the message decoding of DiPlay's
// media/CarPlayPlaybackStatus.kt, hud/BydClusterSong.kt and hud/BydCarPlayCall.kt (DiPlay commit 615b32c).

//! What plays on the iPhone, from NowPlayingUpdate (0x5001), and its calls, from CallStateUpdate (0x4155).
//!
//! An update carries only what changed: a field it leaves out keeps its value, and a string it carries empty
//! clears it. A field that cannot be read is treated as left out, so one bad field does not lose the rest.

use crate::messages::id;
use crate::wire::{Frame, Parameters};

/// The media item group and its fields.
const MEDIA_ITEM: u16 = 0;
const TITLE: u16 = 1;
const DURATION: u16 = 4;
const ALBUM: u16 = 6;
const ARTIST: u16 = 12;
const ARTWORK_TRANSFER: u16 = 26;
/// The playback group and its fields.
const PLAYBACK: u16 = 1;
const STATUS: u16 = 0;
const ELAPSED: u16 = 1;
const APP: u16 = 7;

/// Playback statuses.
pub mod status {
    pub const STOPPED: u8 = 0;
    pub const PLAYING: u8 = 1;
    pub const PAUSED: u8 = 2;
    pub const SEEKING_FORWARD: u8 = 3;
    pub const SEEKING_BACKWARD: u8 = 4;
}

/// The fields one NowPlayingUpdate changes. Strings are trimmed; `Some("")` clears the field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NowPlayingUpdate {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    /// The file transfer that brings the item's artwork.
    pub artwork_transfer: Option<u8>,
    /// One of [`status`].
    pub status: Option<u8>,
    pub elapsed_ms: Option<u32>,
    /// The app that plays.
    pub app: Option<String>,
}

fn group(body: &Parameters, id: u16) -> Option<Parameters> {
    body.first(id)?.as_group().ok()
}

fn string(group: &Option<Parameters>, id: u16) -> Option<String> {
    let value = group.as_ref()?.first(id)?.as_string().ok()?;
    Some(value.trim().to_owned())
}

fn u32_field(group: &Option<Parameters>, id: u16) -> Option<u32> {
    group.as_ref()?.first(id)?.as_u32().ok()
}

fn u8_field(group: &Option<Parameters>, id: u16) -> Option<u8> {
    group.as_ref()?.first(id)?.as_u8().ok()
}

/// The changes in a NowPlayingUpdate; `None` for another message or a body that cannot be read.
pub fn now_playing(frame: &Frame) -> Option<NowPlayingUpdate> {
    if frame.message_id() != id::NOW_PLAYING_UPDATE {
        return None;
    }
    let body = frame.parameters().ok()?;
    let media = group(&body, MEDIA_ITEM);
    let playback = group(&body, PLAYBACK);
    Some(NowPlayingUpdate {
        title: string(&media, TITLE),
        artist: string(&media, ARTIST),
        album: string(&media, ALBUM),
        duration_ms: u32_field(&media, DURATION),
        artwork_transfer: u8_field(&media, ARTWORK_TRANSFER),
        status: u8_field(&playback, STATUS),
        elapsed_ms: u32_field(&playback, ELAPSED),
        app: string(&playback, APP),
    })
}

/// Call statuses.
pub mod call_status {
    pub const DISCONNECTED: u8 = 0;
    pub const SENDING: u8 = 1;
    pub const RINGING: u8 = 2;
    pub const CONNECTING: u8 = 3;
    pub const ACTIVE: u8 = 4;
    pub const HELD: u8 = 5;
    pub const DISCONNECTING: u8 = 6;
}

/// One call's state. Several calls can exist at once; `call` tells them apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallUpdate {
    /// The call's identifier: its UUID, or the remote party when there is none.
    pub call: String,
    pub remote: String,
    pub display_name: String,
    /// One of [`call_status`].
    pub status: u8,
}

/// The call a CallStateUpdate describes; `None` for another message, or one without a status.
pub fn call_state(frame: &Frame) -> Option<CallUpdate> {
    if frame.message_id() != id::CALL_STATE_UPDATE {
        return None;
    }
    let body = Some(frame.parameters().ok()?);
    let status = u8_field(&body, 2)?;
    let remote = string(&body, 0).unwrap_or_default();
    let display_name = string(&body, 1).unwrap_or_default();
    let call = string(&body, 4)
        .filter(|uuid| !uuid.is_empty())
        .unwrap_or_else(|| remote.clone());
    Some(CallUpdate {
        call,
        remote,
        display_name,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(body: Parameters) -> Frame {
        Frame::with_parameters(id::NOW_PLAYING_UPDATE, &body).unwrap()
    }

    #[test]
    fn media_item_fields() {
        let frame = update(
            Parameters::new().group(
                0,
                Parameters::new()
                    .string(1, "Dreams")
                    .u32(4, 257_000)
                    .string(6, "Rumours")
                    .string(12, "Fleetwood Mac"),
            ),
        );
        assert_eq!(
            now_playing(&frame),
            Some(NowPlayingUpdate {
                title: Some("Dreams".into()),
                album: Some("Rumours".into()),
                artist: Some("Fleetwood Mac".into()),
                duration_ms: Some(257_000),
                ..NowPlayingUpdate::default()
            })
        );
    }

    #[test]
    fn playback_fields() {
        let frame = update(Parameters::new().group(
            1,
            Parameters::new().u8(0, 1).u32(1, 12_500).string(7, "Music"),
        ));
        assert_eq!(
            now_playing(&frame),
            Some(NowPlayingUpdate {
                status: Some(status::PLAYING),
                elapsed_ms: Some(12_500),
                app: Some("Music".into()),
                ..NowPlayingUpdate::default()
            })
        );
        // The position alone, as the iPhone sends it twice a second.
        let elapsed = update(Parameters::new().group(1, Parameters::new().u32(1, 120_706)));
        assert_eq!(now_playing(&elapsed).unwrap().status, None);
    }

    #[test]
    fn empty_strings_clear_and_artwork_ids_are_read() {
        let frame =
            update(Parameters::new().group(0, Parameters::new().string(1, "  ").u8(26, 0x81)));
        let update = now_playing(&frame).unwrap();
        assert_eq!(update.title.as_deref(), Some(""));
        assert_eq!(update.artwork_transfer, Some(0x81));
    }

    #[test]
    fn bad_fields_are_left_out_and_other_messages_ignored() {
        let frame = update(
            Parameters::new()
                .group(0, Parameters::new().u8(4, 1).string(1, "Song"))
                .raw(1, vec![0, 1]),
        );
        let update = now_playing(&frame).unwrap();
        assert_eq!(update.duration_ms, None);
        assert_eq!(update.title.as_deref(), Some("Song"));
        assert_eq!(update.status, None);
        let route = Frame::with_parameters(
            0x5201,
            &Parameters::new().group(1, Parameters::new().u8(0, 1)),
        )
        .unwrap();
        assert_eq!(now_playing(&route), None);
        assert_eq!(
            now_playing(&Frame::new(id::NOW_PLAYING_UPDATE, vec![0]).unwrap()),
            None
        );
    }

    #[test]
    fn calls() {
        let frame = Frame::with_parameters(
            id::CALL_STATE_UPDATE,
            &Parameters::new()
                .string(0, "+15551234")
                .string(1, " Jane ")
                .u8(2, call_status::RINGING)
                .string(4, "C0FFEE"),
        )
        .unwrap();
        assert_eq!(
            call_state(&frame),
            Some(CallUpdate {
                call: "C0FFEE".into(),
                remote: "+15551234".into(),
                display_name: "Jane".into(),
                status: call_status::RINGING,
            })
        );
        let without_uuid = Frame::with_parameters(
            id::CALL_STATE_UPDATE,
            &Parameters::new().string(0, "+1").u8(2, 4),
        )
        .unwrap();
        assert_eq!(call_state(&without_uuid).unwrap().call, "+1");
        let without_status =
            Frame::with_parameters(id::CALL_STATE_UPDATE, &Parameters::new().string(0, "+1"))
                .unwrap();
        assert_eq!(call_state(&without_status), None);
    }
}
