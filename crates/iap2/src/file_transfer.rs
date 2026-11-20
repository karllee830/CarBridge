// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's
// transport/Iap2FileTransferReceiver.kt (DiPlay commit 615b32c).

//! The receiving side of the file transfer session, which brings now-playing artwork.
//!
//! A datagram is a transfer identifier, a byte of first/last flags and an opcode in the low nibble — data (0),
//! start (1), cancel (2), setup (4), success (5) — and what the opcode carries. A version-2 setup carries the size
//! in eight bytes and the file type in two, big-endian. Only now-playing artwork is taken.

/// A finished artwork transfer. Empty bytes clear the artwork: a cancelled, refused or empty transfer gives one,
/// so the previous song's cover does not stay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artwork {
    /// The transfer identifier a NowPlayingUpdate names.
    pub id: u8,
    pub bytes: Vec<u8>,
}

/// What one datagram gives: datagrams to send back, and a finished transfer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub replies: Vec<[u8; 2]>,
    pub finished: Option<Artwork>,
}

const HEADER_BYTES: usize = 2;
const SETUP_BYTES: usize = 12;
const OPCODE_MASK: u8 = 0x0f;
const LAST: u8 = 0x40;
const DATA: u8 = 0;
const START: u8 = 1;
const CANCEL: u8 = 2;
const SETUP: u8 = 4;
const SUCCESS: u8 = 5;
const NOW_PLAYING_ARTWORK: u16 = 2;
const MAX_PENDING: usize = 4;
/// The largest artwork taken by default.
pub const DEFAULT_MAX_ARTWORK_BYTES: usize = 2 * 1024 * 1024;

struct Pending {
    id: u8,
    expected: usize,
    bytes: Vec<u8>,
}

/// Collects artwork transfers. At most a few are pending at once, each no larger than the limit.
pub struct Receiver {
    max_bytes: usize,
    pending: Vec<Pending>,
}

impl Default for Receiver {
    fn default() -> Receiver {
        Receiver::new(DEFAULT_MAX_ARTWORK_BYTES)
    }
}

impl Receiver {
    pub fn new(max_bytes: usize) -> Receiver {
        Receiver {
            max_bytes,
            pending: Vec::new(),
        }
    }

    /// Takes one datagram of the file transfer session.
    pub fn accept(&mut self, datagram: &[u8]) -> Outcome {
        let [id, flags, ..] = *datagram else {
            return Outcome::default();
        };
        match flags & OPCODE_MASK {
            SETUP => self.setup(id, datagram),
            DATA => self.data(id, flags, &datagram[HEADER_BYTES..]),
            CANCEL => match self.take(id) {
                Some(_) => Outcome {
                    replies: Vec::new(),
                    finished: Some(cleared(id)),
                },
                None => Outcome::default(),
            },
            _ => Outcome::default(),
        }
    }

    /// Drops every pending transfer.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    fn take(&mut self, id: u8) -> Option<Pending> {
        let index = self.pending.iter().position(|p| p.id == id)?;
        Some(self.pending.remove(index))
    }

    fn setup(&mut self, id: u8, datagram: &[u8]) -> Outcome {
        self.take(id);
        if datagram.len() < SETUP_BYTES {
            return reply(id, CANCEL, None);
        }
        let size = u64::from_be_bytes(datagram[2..10].try_into().expect("eight bytes"));
        let kind = u16::from_be_bytes([datagram[10], datagram[11]]);
        if kind != NOW_PLAYING_ARTWORK {
            return reply(id, CANCEL, None);
        }
        if size > self.max_bytes as u64 {
            return reply(id, CANCEL, Some(cleared(id)));
        }
        if size == 0 {
            return Outcome {
                replies: vec![[id, START], [id, SUCCESS]],
                finished: Some(cleared(id)),
            };
        }
        let expected = size as usize;
        self.pending.push(Pending {
            id,
            expected,
            bytes: Vec::with_capacity(expected),
        });
        if self.pending.len() > MAX_PENDING {
            self.pending.remove(0);
        }
        reply(id, START, None)
    }

    fn data(&mut self, id: u8, flags: u8, bytes: &[u8]) -> Outcome {
        let Some(index) = self.pending.iter().position(|p| p.id == id) else {
            return Outcome::default();
        };
        let transfer = &mut self.pending[index];
        if bytes.len() > transfer.expected - transfer.bytes.len() {
            self.pending.remove(index);
            return reply(id, CANCEL, Some(cleared(id)));
        }
        transfer.bytes.extend_from_slice(bytes);
        let complete = transfer.bytes.len() == transfer.expected;
        if flags & LAST == 0 && !complete {
            return Outcome::default();
        }
        let transfer = self.pending.remove(index);
        if !complete {
            return reply(id, CANCEL, Some(cleared(id)));
        }
        reply(
            id,
            SUCCESS,
            Some(Artwork {
                id,
                bytes: transfer.bytes,
            }),
        )
    }
}

fn cleared(id: u8) -> Artwork {
    Artwork {
        id,
        bytes: Vec::new(),
    }
}

fn reply(id: u8, opcode: u8, finished: Option<Artwork>) -> Outcome {
    Outcome {
        replies: vec![[id, opcode]],
        finished,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(id: u8, size: u64, kind: u16) -> Vec<u8> {
        let mut datagram = vec![id, SETUP];
        datagram.extend_from_slice(&size.to_be_bytes());
        datagram.extend_from_slice(&kind.to_be_bytes());
        datagram
    }

    fn data(id: u8, flags: u8, bytes: &[u8]) -> Vec<u8> {
        [&[id, flags][..], bytes].concat()
    }

    #[test]
    fn artwork_is_reassembled() {
        let mut receiver = Receiver::default();
        assert_eq!(receiver.accept(&setup(0x81, 5, 2)).replies, [[0x81, 0x01]]);
        assert_eq!(
            receiver.accept(&data(0x81, 0x80, &[1, 2, 3])).finished,
            None
        );
        let done = receiver.accept(&data(0x81, 0x40, &[4, 5]));
        assert_eq!(done.replies, [[0x81, 0x05]]);
        assert_eq!(
            done.finished,
            Some(Artwork {
                id: 0x81,
                bytes: vec![1, 2, 3, 4, 5]
            })
        );
    }

    #[test]
    fn oversized_unsupported_and_truncated_transfers_are_refused() {
        let mut receiver = Receiver::new(4);
        let oversized = receiver.accept(&setup(1, 5, 2));
        assert_eq!(oversized.replies, [[1, 2]]);
        assert_eq!(oversized.finished, Some(cleared(1)));
        let unsupported = receiver.accept(&setup(2, 3, 7));
        assert_eq!(unsupported.replies, [[2, 2]]);
        assert_eq!(unsupported.finished, None);
        assert_eq!(receiver.accept(&[4, 4]).finished, None);
        receiver.accept(&setup(3, 4, 2));
        let truncated = receiver.accept(&data(3, 0x40, &[1, 2]));
        assert_eq!(truncated.replies, [[3, 2]]);
        assert_eq!(truncated.finished, Some(cleared(3)));
        receiver.accept(&setup(4, 2, 2));
        let overrun = receiver.accept(&data(4, 0x00, &[1, 2, 3]));
        assert_eq!(overrun.replies, [[4, 2]]);
    }

    #[test]
    fn a_cancel_clears_only_a_pending_transfer() {
        let mut receiver = Receiver::default();
        receiver.accept(&setup(5, 4, 2));
        let cancelled = receiver.accept(&data(5, 0x02, &[]));
        assert!(cancelled.replies.is_empty());
        assert_eq!(cancelled.finished, Some(cleared(5)));
        assert_eq!(receiver.accept(&data(5, 0x02, &[])).finished, None);
    }

    #[test]
    fn empty_artwork_clears_the_image() {
        let outcome = Receiver::default().accept(&setup(0x80, 0, 2));
        assert_eq!(outcome.replies, [[0x80, 1], [0x80, 5]]);
        assert_eq!(outcome.finished, Some(cleared(0x80)));
    }

    #[test]
    fn at_most_four_transfers_are_pending() {
        let mut receiver = Receiver::default();
        for id in 0..5 {
            receiver.accept(&setup(id, 1, 2));
        }
        assert_eq!(receiver.accept(&data(0, 0x40, &[9])).finished, None);
        assert_eq!(
            receiver
                .accept(&data(4, 0x40, &[9]))
                .finished
                .unwrap()
                .bytes,
            [9]
        );
        assert_eq!(receiver.accept(&[7]), Outcome::default());
        let short = receiver.accept(&[8, SETUP, 0, 0]);
        assert_eq!(short.replies, [[8, CANCEL]]);
    }
}
