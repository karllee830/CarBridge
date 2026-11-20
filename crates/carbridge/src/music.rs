// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What plays on the iPhone, for the head unit: `NotifyMusicInfo` on the control channel.
//!
//! Head units with the UCar SDK show the phone's music in the capsule their cast window shrinks to, with its title,
//! artist, cover and play, pause, next and previous buttons, but only for music info that carries a cover and names
//! the app the window casts (`id`); its play or pause button follows `isPlaying`. The SDK takes a song as the same
//! while its title, album, artist and duration stay, and keeps the cover it was given for it, so the cover goes once
//! per song and connection, and again when it changes. It decodes the cover as an encoded image (PNG or JPEG). `id` -1
//! says nothing plays any more.

use std::sync::Arc;
use std::time::Instant;

use carbridge_carlink_proto::command;
use carbridge_carlink_proto::frame::Message;
use carbridge_carlink_proto::header::{Category, Kind};
use carbridge_carlink_proto::pb::NotifyMusicInfo;
use carbridge_carlink_session::display::{WindowMode, MIRROR_WINDOW_APP, SIMPLE_WINDOW_APP};
use carbridge_carplay::now_playing::NowPlaying;

/// The cover of a song without artwork: a grey music note.
pub const FALLBACK_COVER: &[u8] = include_bytes!("../assets/music-cover.png");
/// The largest artwork sent; a larger one is replaced by the fallback cover, to keep the control channel moving.
pub const MAX_COVER: usize = 1024 * 1024;
/// The `id` that says nothing plays any more.
const ENDED: i32 = -1;

/// The app a head unit's window casts in `mode`; the mirror window's when it is not known.
pub fn window_app(mode: Option<WindowMode>) -> i32 {
    match mode {
        Some(WindowMode::Simple) => SIMPLE_WINDOW_APP,
        _ => MIRROR_WINDOW_APP,
    }
}

/// The title, album, artist and duration, by which the head unit tells songs apart.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Song {
    title: String,
    album: String,
    artist: String,
    duration_ms: u64,
}

impl Song {
    fn of(now: &NowPlaying) -> Song {
        Song {
            title: now.title.clone(),
            album: now.album.clone().unwrap_or_default(),
            artist: now.artist.clone().unwrap_or_default(),
            duration_ms: now.duration_ms.map_or(0, u64::from),
        }
    }
}

/// The song's cover: its artwork, unless it has none or the artwork is too large, then the fallback cover.
fn cover(now: &NowPlaying) -> Option<&Arc<[u8]>> {
    now.artwork
        .as_ref()
        .filter(|artwork| !artwork.is_empty() && artwork.len() <= MAX_COVER)
}

/// What one connection to a head unit was told of the music.
#[derive(Debug)]
pub struct Music {
    /// The app the head unit's window casts, which the music info names.
    app: i32,
    /// The song last told, with the artwork its cover was made of (`None` for the fallback cover).
    sent: Option<(Song, Option<Arc<[u8]>>)>,
}

impl Music {
    /// Nothing told yet, for a head unit whose window casts `app`.
    pub fn new(app: i32) -> Music {
        Music { app, sent: None }
    }

    /// The head unit's window casts `app` from now on; whether it changed.
    pub fn set_app(&mut self, app: i32) -> bool {
        std::mem::replace(&mut self.app, app) != app
    }

    /// The music info to send for `now`, what plays, at `at`: none when nothing plays and nothing was told. The cover
    /// goes with it only when the song or its cover differs from what was told last.
    pub fn info(&mut self, now: Option<&NowPlaying>, at: Instant) -> Option<NotifyMusicInfo> {
        let Some(now) = now else {
            self.sent.take()?;
            return Some(NotifyMusicInfo {
                is_playing: false,
                id: Some(ENDED),
                ..NotifyMusicInfo::default()
            });
        };
        let song = Song::of(now);
        let artwork = cover(now).cloned();
        let told = Some((song.clone(), artwork.clone()));
        let cover_art_bitmap = (self.sent != told).then(|| {
            artwork
                .as_deref()
                .map_or_else(|| FALLBACK_COVER.to_vec(), <[u8]>::to_vec)
        });
        self.sent = told;
        Some(NotifyMusicInfo {
            artist_name: song.artist,
            album_name: song.album,
            title: song.title,
            playing_times_ms: song.duration_ms,
            playing_current_time_ms: now
                .position_ms(at)
                .map_or(0, |ms| ms.min(i32::MAX as u32) as i32),
            is_playing: now.playing,
            cover_art_bitmap,
            id: Some(self.app),
            ..NotifyMusicInfo::default()
        })
    }

    /// Forgets what was told, as for music info that did not go out: the next carries the cover again.
    pub fn forget(&mut self) {
        self.sent = None;
    }
}

/// The control channel message of `info`.
pub fn message(info: &NotifyMusicInfo) -> Message {
    Message::protobuf(
        Category::CONTROL,
        command::control::NOTIFY_MUSIC_INFO,
        Kind::Send,
        info,
    )
}

/// A line for the log about `info`, without the cover's bytes.
pub fn describe(info: &NotifyMusicInfo) -> String {
    if info.id == Some(ENDED) {
        return "music info: nothing plays".into();
    }
    let cover = info
        .cover_art_bitmap
        .as_ref()
        .map_or_else(String::new, |cover| {
            if cover.as_slice() == FALLBACK_COVER {
                ", fallback cover".into()
            } else {
                format!(", cover of {} bytes", cover.len())
            }
        });
    format!(
        "music info: {}, app {}{cover}",
        if info.is_playing {
            "playing"
        } else {
            "not playing"
        },
        info.id.unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use std::time::Duration;

    fn playing(title: &str, artwork: Option<&[u8]>) -> NowPlaying {
        NowPlaying {
            title: title.into(),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            duration_ms: Some(180_000),
            elapsed: None,
            playing: true,
            artwork: artwork.map(Into::into),
        }
    }

    #[test]
    fn the_fallback_cover_is_a_png() {
        assert!(FALLBACK_COVER.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(FALLBACK_COVER.len() < MAX_COVER);
    }

    #[test]
    fn a_song_carries_its_cover_once() {
        let mut music = Music::new(MIRROR_WINDOW_APP);
        let at = Instant::now();
        let song = playing("A", Some(&[0xff, 0xd8, 1]));
        let info = music.info(Some(&song), at).unwrap();
        assert_eq!(info.title, "A");
        assert_eq!(info.artist_name, "Artist");
        assert_eq!(info.album_name, "Album");
        assert_eq!(info.playing_times_ms, 180_000);
        assert!(info.is_playing);
        assert_eq!(info.id, Some(10));
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(&[0xff, 0xd8, 1][..]));
        // The same song again, paused: no cover.
        let mut paused = song.clone();
        paused.playing = false;
        let info = music.info(Some(&paused), at).unwrap();
        assert!(!info.is_playing);
        assert_eq!(info.cover_art_bitmap, None);
        // Its artwork changes: the new cover.
        let changed = playing("A", Some(&[0x89, b'P']));
        let info = music.info(Some(&changed), at).unwrap();
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(&[0x89, b'P'][..]));
    }

    #[test]
    fn a_song_without_artwork_gets_the_fallback_cover() {
        let mut music = Music::new(MIRROR_WINDOW_APP);
        let at = Instant::now();
        let info = music.info(Some(&playing("A", None)), at).unwrap();
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(FALLBACK_COVER));
        assert_eq!(
            music
                .info(Some(&playing("A", None)), at)
                .unwrap()
                .cover_art_bitmap,
            None
        );
        // Its artwork arrives.
        let info = music.info(Some(&playing("A", Some(&[1]))), at).unwrap();
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(&[1][..]));
        // Artwork that is too large.
        let large = vec![7; MAX_COVER + 1];
        let info = music.info(Some(&playing("B", Some(&large))), at).unwrap();
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(FALLBACK_COVER));
    }

    #[test]
    fn another_song_carries_its_cover_again() {
        let mut music = Music::new(MIRROR_WINDOW_APP);
        let at = Instant::now();
        music.info(Some(&playing("A", None)), at);
        let info = music.info(Some(&playing("B", None)), at).unwrap();
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(FALLBACK_COVER));
        // The head unit tells songs apart by their duration too.
        let mut longer = playing("B", None);
        longer.duration_ms = Some(1);
        assert!(music
            .info(Some(&longer), at)
            .unwrap()
            .cover_art_bitmap
            .is_some());
    }

    #[test]
    fn the_end_is_told_once_and_only_after_music() {
        let mut music = Music::new(MIRROR_WINDOW_APP);
        let at = Instant::now();
        assert_eq!(music.info(None, at), None);
        music.info(Some(&playing("A", None)), at);
        let info = music.info(None, at).unwrap();
        assert_eq!(info.id, Some(-1));
        assert!(!info.is_playing);
        assert_eq!(music.info(None, at), None);
        // The next song carries its cover again.
        let info = music.info(Some(&playing("A", None)), at).unwrap();
        assert!(info.cover_art_bitmap.is_some());
    }

    #[test]
    fn music_info_that_did_not_go_out_is_told_again_with_its_cover() {
        let mut music = Music::new(10);
        let at = Instant::now();
        music.info(Some(&playing("A", None)), at);
        music.forget();
        let info = music.info(Some(&playing("A", None)), at).unwrap();
        assert!(info.cover_art_bitmap.is_some());
    }

    #[test]
    fn the_info_names_the_window_app() {
        assert_eq!(window_app(None), 10);
        assert_eq!(window_app(Some(WindowMode::Mirror)), 10);
        assert_eq!(window_app(Some(WindowMode::Simple)), 11);
        let mut music = Music::new(10);
        let at = Instant::now();
        assert!(!music.set_app(10));
        assert!(music.set_app(11));
        assert_eq!(
            music.info(Some(&playing("A", None)), at).unwrap().id,
            Some(11)
        );
    }

    #[test]
    fn the_position_advances_while_playing() {
        let mut music = Music::new(10);
        let at = Instant::now();
        let mut song = playing("A", None);
        song.elapsed = Some((1000, at));
        let info = music
            .info(Some(&song), at + Duration::from_millis(2500))
            .unwrap();
        assert_eq!(info.playing_current_time_ms, 3500);
    }

    #[test]
    fn the_message_is_notify_music_info_on_the_control_channel() {
        let mut music = Music::new(10);
        let message = message(
            &music
                .info(Some(&playing("A", None)), Instant::now())
                .unwrap(),
        );
        assert_eq!(message.category, Category::CONTROL);
        assert_eq!(message.command, command::control::NOTIFY_MUSIC_INFO);
        let info = NotifyMusicInfo::decode(&message.body[..]).unwrap();
        assert_eq!(info.title, "A");
        assert_eq!(info.cover_art_bitmap.as_deref(), Some(FALLBACK_COVER));
        // The end's id is -1 on the wire.
        let ended = super::message(&music.info(None, Instant::now()).unwrap());
        let info = NotifyMusicInfo::decode(&ended.body[..]).unwrap();
        assert_eq!(info.id, Some(-1));
    }

    #[test]
    fn descriptions_leave_the_cover_out() {
        let mut music = Music::new(10);
        let at = Instant::now();
        let info = music.info(Some(&playing("A", None)), at).unwrap();
        assert_eq!(
            describe(&info),
            "music info: playing, app 10, fallback cover"
        );
        let info = music.info(Some(&playing("A", Some(&[1, 2]))), at).unwrap();
        assert_eq!(
            describe(&info),
            "music info: playing, app 10, cover of 2 bytes"
        );
        assert_eq!(
            describe(&music.info(None, at).unwrap()),
            "music info: nothing plays"
        );
    }
}
