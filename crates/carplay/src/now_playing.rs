// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What plays on the iPhone, as its iAP2 links tell it: the now-playing updates (0x5001) give the song, its
//! duration, the playback status and the position, each update only what changed; the song's artwork comes as a
//! file transfer on the link's file-transfer session, which the update names by its transfer identifier. The update
//! and its transfer may arrive in either order, so the last few transfers of each link are kept.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use carbridge_iap2::now_playing::NowPlayingUpdate;

/// Playback statuses that count as playing: playing, seeking forward and seeking backward. Stopped (0) and
/// paused (2) do not.
const PLAYING_STATUSES: [u8; 3] = [1, 3, 4];
/// Completed transfers kept for an update that names them later.
const KEPT_TRANSFERS: usize = 4;

/// The song that plays, or played last, on the iPhone.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u32>,
    /// The position the iPhone gave last, and when.
    pub elapsed: Option<(u32, Instant)>,
    pub playing: bool,
    /// The song's artwork as the iPhone sent it, an encoded image; `None` while the song has none or it has not
    /// arrived yet.
    pub artwork: Option<Arc<[u8]>>,
}

impl NowPlaying {
    /// The position at `now`: the one given, advanced while the song plays, and no further than its end.
    pub fn position_ms(&self, now: Instant) -> Option<u32> {
        let (elapsed, at) = self.elapsed?;
        let advanced = if self.playing {
            now.saturating_duration_since(at)
        } else {
            Duration::ZERO
        };
        let position = u64::from(elapsed).saturating_add(advanced.as_millis() as u64);
        let end = self.duration_ms.map_or(u64::from(u32::MAX), u64::from);
        Some(position.min(end).min(u64::from(u32::MAX)) as u32)
    }
}

/// What the iPhone's now-playing updates and artwork transfers said so far. Links are told apart by a number of
/// the caller's, as each link numbers its own transfers.
#[derive(Debug, Default)]
pub struct NowPlayingState {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u32>,
    elapsed: Option<(u32, Instant)>,
    status: Option<u8>,
    artwork: Option<Arc<[u8]>>,
    /// The transfer the song's latest update named for its artwork: the link and the transfer identifier.
    named: Option<(u64, u8)>,
    /// The latest completed transfers, oldest first, with no bytes for one that failed.
    transfers: VecDeque<(u64, u8, Arc<[u8]>)>,
}

impl NowPlayingState {
    /// Takes an update from link `link`, received at `now`; whether what plays looks otherwise now.
    pub fn change(&mut self, link: u64, change: &NowPlayingUpdate, now: Instant) -> bool {
        let before = self.current();
        if let Some(title) = &change.title {
            // Another song: its artist, album, length and artwork are the ones its update names, if any.
            if self.title.as_ref() != Some(title) {
                self.artist = None;
                self.album = None;
                self.duration_ms = None;
                self.artwork = None;
                self.named = None;
            }
            self.title = Some(title.clone());
        }
        // An empty artist or album clears it.
        if let Some(artist) = &change.artist {
            self.artist = Some(artist.clone()).filter(|artist| !artist.is_empty());
        }
        if let Some(album) = &change.album {
            self.album = Some(album.clone()).filter(|album| !album.is_empty());
        }
        if let Some(duration) = change.duration_ms {
            self.duration_ms = Some(duration);
        }
        if let Some(status) = change.status.filter(|status| Some(*status) != self.status) {
            // The position stops or starts advancing from here.
            if let Some(position) = self.current().and_then(|song| song.position_ms(now)) {
                self.elapsed = Some((position, now));
            }
            self.status = Some(status);
        }
        if let Some(elapsed) = change.elapsed_ms {
            if self.elapsed.map(|(given, _)| given) != Some(elapsed) {
                self.elapsed = Some((elapsed, now));
            }
        }
        if let Some(id) = change.artwork_transfer {
            if self.named != Some((link, id)) {
                self.named = Some((link, id));
                // Until it arrives, the song keeps the artwork it has.
                if let Some(bytes) = self.kept(link, id) {
                    self.artwork = artwork(bytes);
                }
            }
        }
        before != self.current()
    }

    /// Takes a transfer that ended on link `link`: its artwork, or no bytes when it failed. Whether what plays looks
    /// otherwise now.
    pub fn transfer(&mut self, link: u64, id: u8, bytes: Vec<u8>) -> bool {
        let bytes: Arc<[u8]> = bytes.into();
        self.transfers.retain(|(l, i, _)| (*l, *i) != (link, id));
        self.transfers.push_back((link, id, bytes.clone()));
        while self.transfers.len() > KEPT_TRANSFERS {
            self.transfers.pop_front();
        }
        if self.named != Some((link, id)) {
            return false;
        }
        let before = self.current();
        self.artwork = artwork(bytes);
        before != self.current()
    }

    /// Forgets everything, as when the iPhone's session ends; whether a song was known.
    pub fn clear(&mut self) -> bool {
        let known = self.current().is_some();
        *self = NowPlayingState::default();
        known
    }

    /// The song, once an update has given its title.
    pub fn current(&self) -> Option<NowPlaying> {
        let title = self.title.as_ref().filter(|title| !title.is_empty())?;
        Some(NowPlaying {
            title: title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            duration_ms: self.duration_ms,
            elapsed: self.elapsed,
            playing: self
                .status
                .is_some_and(|status| PLAYING_STATUSES.contains(&status)),
            artwork: self.artwork.clone(),
        })
    }

    fn kept(&self, link: u64, id: u8) -> Option<Arc<[u8]>> {
        self.transfers
            .iter()
            .find(|(l, i, _)| (*l, *i) == (link, id))
            .map(|(_, _, bytes)| bytes.clone())
    }
}

/// The artwork of a transfer's bytes, none for a transfer that failed.
fn artwork(bytes: Arc<[u8]>) -> Option<Arc<[u8]>> {
    (!bytes.is_empty()).then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(title: &str) -> NowPlayingUpdate {
        NowPlayingUpdate {
            title: Some(title.into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            duration_ms: Some(200_000),
            ..Default::default()
        }
    }

    fn named(id: u8) -> NowPlayingUpdate {
        NowPlayingUpdate {
            artwork_transfer: Some(id),
            ..Default::default()
        }
    }

    fn status(status: u8, elapsed: Option<u32>) -> NowPlayingUpdate {
        NowPlayingUpdate {
            status: Some(status),
            elapsed_ms: elapsed,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_plays_before_a_title() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        assert!(!state.change(1, &status(1, Some(5000)), now));
        assert_eq!(state.current(), None);
        assert!(!state.change(1, &song(""), now));
        assert!(state.change(1, &song("A"), now));
        let current = state.current().unwrap();
        assert_eq!(current.title, "A");
        assert_eq!(current.artist.as_deref(), Some("Artist"));
        assert_eq!(current.album.as_deref(), Some("Album"));
        assert_eq!(current.duration_ms, Some(200_000));
        assert!(current.playing);
    }

    #[test]
    fn updates_merge_and_repeats_change_nothing() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        assert!(state.change(1, &song("A"), now));
        assert!(!state.change(1, &song("A"), now));
        // The other link says the same.
        assert!(!state.change(2, &song("A"), now));
        let later = now + Duration::from_secs(1);
        assert!(state.change(
            1,
            &NowPlayingUpdate {
                artist: Some("Other".into()),
                ..Default::default()
            },
            later
        ));
        let current = state.current().unwrap();
        assert_eq!(
            (current.title.as_str(), current.artist.as_deref()),
            ("A", Some("Other"))
        );
    }

    #[test]
    fn another_song_keeps_nothing_of_the_one_before() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        state.change(1, &song("A"), now);
        assert!(state.change(
            1,
            &NowPlayingUpdate {
                title: Some("B".into()),
                ..Default::default()
            },
            now
        ));
        let current = state.current().unwrap();
        assert_eq!(current.title, "B");
        assert_eq!(
            (current.artist, current.album, current.duration_ms),
            (None, None, None)
        );
    }

    #[test]
    fn playing_follows_the_status() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        state.change(1, &song("A"), now);
        assert!(!state.current().unwrap().playing);
        for (value, playing) in [(1, true), (2, false), (3, true), (4, true), (0, false)] {
            state.change(1, &status(value, None), now);
            assert_eq!(state.current().unwrap().playing, playing, "status {value}");
        }
    }

    #[test]
    fn the_position_advances_only_while_playing() {
        let mut state = NowPlayingState::default();
        let start = Instant::now();
        state.change(1, &song("A"), start);
        assert_eq!(state.current().unwrap().position_ms(start), None);
        state.change(1, &status(1, Some(10_000)), start);
        let later = start + Duration::from_secs(5);
        assert_eq!(state.current().unwrap().position_ms(later), Some(15_000));
        // Paused at 15 s: it stays there.
        assert!(state.change(1, &status(2, None), later));
        let much_later = later + Duration::from_secs(60);
        assert_eq!(
            state.current().unwrap().position_ms(much_later),
            Some(15_000)
        );
        // Playing again, up to the end of the song.
        state.change(1, &status(1, None), much_later);
        let past_the_end = much_later + Duration::from_secs(1000);
        assert_eq!(
            state.current().unwrap().position_ms(past_the_end),
            Some(200_000)
        );
        // A seek.
        assert!(state.change(1, &status(1, Some(1000)), past_the_end));
        assert_eq!(
            state.current().unwrap().position_ms(past_the_end),
            Some(1000)
        );
    }

    #[test]
    fn artwork_follows_its_transfer_in_either_order() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        state.change(1, &song("A"), now);
        // The update first, its transfer later.
        assert!(!state.change(1, &named(0x81), now));
        assert_eq!(state.current().unwrap().artwork, None);
        assert!(!state.transfer(1, 0x80, vec![9]));
        assert!(state.transfer(1, 0x81, vec![1, 2, 3]));
        assert_eq!(
            state.current().unwrap().artwork.as_deref(),
            Some(&[1, 2, 3][..])
        );
        // Another song without artwork.
        assert!(state.change(1, &song("B"), now));
        assert_eq!(state.current().unwrap().artwork, None);
        // The transfer first, the update that names it later.
        assert!(!state.transfer(1, 0x82, vec![4, 5]));
        let mut c = song("C");
        c.artwork_transfer = Some(0x82);
        assert!(state.change(1, &c, now));
        assert_eq!(
            state.current().unwrap().artwork.as_deref(),
            Some(&[4, 5][..])
        );
    }

    #[test]
    fn transfers_are_told_apart_by_link() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        state.change(1, &song("A"), now);
        state.change(1, &named(0x81), now);
        assert!(!state.transfer(2, 0x81, vec![7]));
        assert_eq!(state.current().unwrap().artwork, None);
        assert!(state.transfer(1, 0x81, vec![8]));
        assert_eq!(state.current().unwrap().artwork.as_deref(), Some(&[8][..]));
    }

    #[test]
    fn new_artwork_of_the_same_song_replaces_the_old_once_it_arrives() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        state.change(1, &song("A"), now);
        state.change(1, &named(0x81), now);
        state.transfer(1, 0x81, vec![1]);
        assert!(!state.change(1, &named(0x83), now));
        assert_eq!(state.current().unwrap().artwork.as_deref(), Some(&[1][..]));
        // The same picture again changes nothing.
        assert!(!state.transfer(1, 0x83, vec![1]));
        state.change(1, &named(0x84), now);
        assert!(state.transfer(1, 0x84, vec![2]));
        assert_eq!(state.current().unwrap().artwork.as_deref(), Some(&[2][..]));
    }

    #[test]
    fn a_failed_transfer_leaves_the_song_without_artwork() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        let mut a = song("A");
        a.artwork_transfer = Some(0x81);
        state.change(1, &a, now);
        assert!(!state.transfer(1, 0x81, Vec::new()));
        assert_eq!(state.current().unwrap().artwork, None);
    }

    #[test]
    fn only_the_latest_transfers_are_kept() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        state.change(1, &song("A"), now);
        for id in 0..=KEPT_TRANSFERS as u8 {
            state.transfer(1, id, vec![id + 1]);
        }
        state.change(1, &named(0), now);
        assert_eq!(state.current().unwrap().artwork, None);
        state.change(1, &named(1), now);
        assert_eq!(state.current().unwrap().artwork.as_deref(), Some(&[2][..]));
    }

    #[test]
    fn clearing_forgets_the_song() {
        let mut state = NowPlayingState::default();
        let now = Instant::now();
        assert!(!state.clear());
        state.change(1, &song("A"), now);
        assert!(state.clear());
        assert_eq!(state.current(), None);
        assert!(!state.clear());
        // A position alone after it shows nothing.
        state.change(1, &status(1, Some(10)), now);
        assert_eq!(state.current(), None);
    }
}
