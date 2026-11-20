// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's
// transport/BlockingDuplexByteStream.kt (DiPlay commit 615b32c).

//! The byte stream an iAP2 link runs on: a Bluetooth RFCOMM connection, or the tunnel inside a CarPlay session.

use std::io;
use std::time::Duration;

/// What one receive gave.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// This many bytes were read into the buffer.
    Bytes(usize),
    /// Nothing arrived within the timeout; the stream is still up.
    Nothing,
    /// The peer ended its side of the stream.
    End,
}

/// A blocking, ordered, full-duplex byte stream that one thread receives from while others send.
///
/// A receive that finds nothing within its timeout is [`Received::Nothing`]. An error, including one that says
/// the peer stopped answering (such as a keep-alive that went unanswered), ends the link.
pub trait Stream: Send + Sync {
    /// Sends all of `bytes`.
    fn send(&self, bytes: &[u8]) -> io::Result<()>;

    /// Waits up to `timeout` for bytes and reads what has arrived into `buffer`.
    fn receive(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Received>;

    /// Ends the stream in both directions, so that a receive waiting on it returns.
    fn shutdown(&self);
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Instant;

    use super::*;

    #[derive(Default)]
    struct Direction {
        bytes: Mutex<(VecDeque<u8>, bool)>,
        changed: Condvar,
    }

    /// One end of an in-memory stream.
    pub(crate) struct PipeEnd {
        incoming: Arc<Direction>,
        outgoing: Arc<Direction>,
    }

    /// Two connected ends.
    pub(crate) fn pipe() -> (Arc<PipeEnd>, Arc<PipeEnd>) {
        let (a, b) = (
            Arc::new(Direction::default()),
            Arc::new(Direction::default()),
        );
        (
            Arc::new(PipeEnd {
                incoming: a.clone(),
                outgoing: b.clone(),
            }),
            Arc::new(PipeEnd {
                incoming: b,
                outgoing: a,
            }),
        )
    }

    impl Stream for PipeEnd {
        fn send(&self, bytes: &[u8]) -> io::Result<()> {
            let mut state = self.outgoing.bytes.lock().unwrap();
            if state.1 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            state.0.extend(bytes);
            self.outgoing.changed.notify_all();
            Ok(())
        }

        fn receive(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Received> {
            let deadline = Instant::now() + timeout;
            let mut state = self.incoming.bytes.lock().unwrap();
            loop {
                if !state.0.is_empty() {
                    let count = buffer.len().min(state.0.len());
                    for (slot, byte) in buffer.iter_mut().zip(state.0.drain(..count)) {
                        *slot = byte;
                    }
                    return Ok(Received::Bytes(count));
                }
                if state.1 {
                    return Ok(Received::End);
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok(Received::Nothing);
                }
                state = self
                    .incoming
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap()
                    .0;
            }
        }

        fn shutdown(&self) {
            for direction in [&self.incoming, &self.outgoing] {
                direction.bytes.lock().unwrap().1 = true;
                direction.changed.notify_all();
            }
        }
    }
}
