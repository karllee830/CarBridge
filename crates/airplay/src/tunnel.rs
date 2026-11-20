// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/IapTunnel.kt and
// airplay/AirPlayIapTunnelStream.kt (DiPlay commit 615b32c).

//! The iAP2 tunnel inside a CarPlay session (stream type 130 for the iAP2 data stream).
//!
//! The iPhone sends its iAP2 bytes on a TCP connection of their own, in frames sealed as the control channel's
//! are and holding transport packages: a 32-byte header with the package size (big-endian, at 0) and its type
//! (at 16); a `comm` package carries iAP2 bytes. The receiver's iAP2 bytes go back as `iAPSendMessage` commands
//! on the event channel. [`Tunnel`] joins both directions into one [`Stream`] for an iAP2 session.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::control_cipher::Opener;
use crate::net::{Received, Stream};

const PACKAGE_HEADER_BYTES: usize = 32;
const PACKAGE_TYPE: usize = 16;
const COMM: u32 = u32::from_be_bytes(*b"comm");
const MAX_PACKAGE: usize = 4 * 1024 * 1024;
/// Received bytes kept for a slow reader before the tunnel is given up.
const MAX_PENDING_BYTES: usize = 1024 * 1024;
/// How long a send waits for the iPhone to connect to the tunnel.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Takes the tunnel's sealed bytes apart into iAP2 bytes.
pub struct Unpacker {
    opener: Opener,
    sealed: Vec<u8>,
    plain: Vec<u8>,
}

impl Unpacker {
    pub fn new(key: [u8; 32]) -> Unpacker {
        Unpacker {
            opener: Opener::new(key),
            sealed: Vec::new(),
            plain: Vec::new(),
        }
    }

    /// Adds received bytes and gives the iAP2 bytes of every package they complete.
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        self.sealed.extend_from_slice(bytes);
        let opened = self.opener.open(&mut self.sealed)?;
        self.plain.extend_from_slice(&opened);
        let mut iap2 = Vec::new();
        let mut offset = 0;
        while self.plain.len() - offset >= PACKAGE_HEADER_BYTES {
            let at = |index: usize| {
                u32::from_be_bytes(
                    self.plain[offset + index..offset + index + 4]
                        .try_into()
                        .expect("four bytes"),
                )
            };
            let size = at(0) as usize;
            if !(PACKAGE_HEADER_BYTES..=MAX_PACKAGE).contains(&size) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("iAP2 tunnel package of {size} bytes"),
                ));
            }
            if self.plain.len() - offset < size {
                break;
            }
            if at(PACKAGE_TYPE) == COMM {
                iap2.extend_from_slice(&self.plain[offset + PACKAGE_HEADER_BYTES..offset + size]);
            }
            offset += size;
        }
        self.plain.drain(..offset);
        Ok(iap2)
    }
}

/// Delivers the receiver's iAP2 bytes to the iPhone.
type Outgoing = Box<dyn Fn(&[u8]) -> io::Result<()> + Send + Sync>;

/// Where the tunnel stands.
#[derive(Debug)]
enum State {
    /// The iPhone has not connected yet.
    Waiting,
    Open,
    /// The iPhone ended its side.
    Ended,
    /// The tunnel failed, or was shut down.
    Failed(io::ErrorKind, String),
}

struct Inner {
    state: State,
    pending: VecDeque<u8>,
}

/// The tunnel as a byte stream: receives what the iPhone sends, and sends through `send`.
pub struct Tunnel {
    inner: Mutex<Inner>,
    changed: Condvar,
    send: Outgoing,
}

impl Tunnel {
    /// A tunnel whose outgoing bytes `send` delivers to the iPhone.
    pub fn new(send: impl Fn(&[u8]) -> io::Result<()> + Send + Sync + 'static) -> Arc<Tunnel> {
        Arc::new(Tunnel {
            inner: Mutex::new(Inner {
                state: State::Waiting,
                pending: VecDeque::new(),
            }),
            changed: Condvar::new(),
            send: Box::new(send),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The iPhone connected.
    pub fn opened(&self) {
        let mut inner = self.lock();
        if matches!(inner.state, State::Waiting) {
            inner.state = State::Open;
        }
        self.changed.notify_all();
    }

    /// iAP2 bytes from the iPhone.
    pub fn received(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        if !matches!(inner.state, State::Open) || bytes.is_empty() {
            return;
        }
        if inner.pending.len() + bytes.len() > MAX_PENDING_BYTES {
            inner.state = State::Failed(
                io::ErrorKind::OutOfMemory,
                "iAP2 tunnel bytes are not read".into(),
            );
        } else {
            inner.pending.extend(bytes);
        }
        self.changed.notify_all();
    }

    /// The iPhone ended its side, cleanly or not.
    pub fn ended(&self, error: Option<&io::Error>) {
        let mut inner = self.lock();
        if matches!(inner.state, State::Waiting | State::Open) {
            inner.state = match error {
                None => State::Ended,
                Some(error) => State::Failed(error.kind(), error.to_string()),
            };
        }
        self.changed.notify_all();
    }

    fn failure(state: &State) -> Option<io::Error> {
        match state {
            State::Failed(kind, message) => Some(io::Error::new(*kind, message.clone())),
            _ => None,
        }
    }
}

impl Stream for Tunnel {
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let mut inner = self.lock();
        while matches!(inner.state, State::Waiting) {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the iPhone did not open the iAP2 tunnel",
                ));
            }
            inner = self
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        if let Some(error) = Tunnel::failure(&inner.state) {
            return Err(error);
        }
        if matches!(inner.state, State::Ended) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        drop(inner);
        (self.send)(bytes)
    }

    fn receive(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Received> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            if !inner.pending.is_empty() {
                let count = buffer.len().min(inner.pending.len());
                for (slot, byte) in buffer.iter_mut().zip(inner.pending.drain(..count)) {
                    *slot = byte;
                }
                return Ok(Received::Bytes(count));
            }
            if let Some(error) = Tunnel::failure(&inner.state) {
                return Err(error);
            }
            if matches!(inner.state, State::Ended) {
                return Ok(Received::End);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Received::Nothing);
            }
            inner = self
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    fn shutdown(&self) {
        let mut inner = self.lock();
        if !matches!(inner.state, State::Failed(..)) {
            inner.state = State::Failed(
                io::ErrorKind::ConnectionAborted,
                "the iAP2 tunnel was shut down".into(),
            );
        }
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_cipher::Sealer;

    fn package(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut package = vec![0; PACKAGE_HEADER_BYTES];
        package[..4].copy_from_slice(&((PACKAGE_HEADER_BYTES + body.len()) as u32).to_be_bytes());
        package[PACKAGE_TYPE..PACKAGE_TYPE + 4].copy_from_slice(kind);
        package.extend_from_slice(body);
        package
    }

    #[test]
    fn comm_packages_carry_iap2_bytes() {
        let key = [4; 32];
        let mut sealer = Sealer::new(key);
        let plain = [
            package(b"comm", b"\xff\x5a\x00"),
            package(b"ping", b"x"),
            package(b"comm", b"\x09"),
        ]
        .concat();
        let sealed = [sealer.seal(&plain[..40]), sealer.seal(&plain[40..])].concat();
        let mut unpacker = Unpacker::new(key);
        let mut iap2 = Vec::new();
        for chunk in sealed.chunks(7) {
            iap2.extend(unpacker.push(chunk).unwrap());
        }
        assert_eq!(iap2, b"\xff\x5a\x00\x09");
    }

    #[test]
    fn bad_packages_and_frames_fail() {
        let key = [4; 32];
        let mut bad_size = vec![0; PACKAGE_HEADER_BYTES];
        bad_size[3] = 4;
        assert!(Unpacker::new(key)
            .push(&Sealer::new(key).seal(&bad_size))
            .is_err());
        assert!(Unpacker::new(key)
            .push(&Sealer::new([5; 32]).seal(b"hello"))
            .is_err());
    }

    #[test]
    fn the_tunnel_is_a_stream() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let tunnel = {
            let sent = sent.clone();
            Tunnel::new(move |bytes| {
                sent.lock().unwrap().extend_from_slice(bytes);
                Ok(())
            })
        };
        let mut buffer = [0; 8];
        assert_eq!(
            tunnel
                .receive(&mut buffer, Duration::from_millis(10))
                .unwrap(),
            Received::Nothing
        );
        {
            let tunnel = tunnel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                tunnel.opened();
                tunnel.received(b"abc");
            });
        }
        // A send waits for the iPhone to connect.
        tunnel.send(b"out").unwrap();
        assert_eq!(*sent.lock().unwrap(), b"out");
        assert_eq!(
            tunnel.receive(&mut buffer, Duration::from_secs(2)).unwrap(),
            Received::Bytes(3)
        );
        assert_eq!(&buffer[..3], b"abc");
        tunnel.ended(None);
        assert_eq!(
            tunnel.receive(&mut buffer, Duration::from_secs(1)).unwrap(),
            Received::End
        );
        tunnel.shutdown();
        assert!(tunnel.receive(&mut buffer, Duration::from_secs(1)).is_err());
    }
}
