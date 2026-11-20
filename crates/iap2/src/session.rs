// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's transport/Iap2LinkChannel.kt,
// transport/Iap2CsmChannel.kt and iap2/session/Iap2Session.kt (DiPlay commit 615b32c).

//! An iAP2 link on a byte stream, carrying whole control session messages.
//!
//! A worker thread receives from the stream, runs the link's timers and answers the file transfer session, whose
//! finished artwork goes to a callback. Any thread may send messages; one thread at a time receives them. Once the
//! link ends, for any reason, every call fails with the reason, and the stream is shut down.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::csm::{split_for_link, Deframer, MAX_LINK_CHUNK_BYTES};
use crate::error::Error;
use crate::file_transfer::{self, Artwork};
use crate::link::{self, Engine, Event, FILE_TRANSFER_SESSION};
use crate::stream::{Received, Stream};
use crate::wire::Frame;

/// How long one receive waits before the worker looks at its timers and whether the session closed.
const RECEIVE_POLL: Duration = Duration::from_millis(100);
const RECEIVE_CHUNK_BYTES: usize = 8 * 1024;
/// Received messages kept for a slow reader before the link is given up.
const MAX_PENDING_FRAMES: usize = 64;
const MAX_PENDING_FRAME_BYTES: usize = 1024 * 1024;

/// Which link the stream carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// Bluetooth RFCOMM: the iPhone starts the link, and packets are acknowledged.
    Bluetooth,
    /// The tunnel inside a CarPlay session over Wi-Fi: the accessory starts the link, without acknowledgements.
    Tunnel,
}

/// Why the link ended.
#[derive(Clone, Debug)]
enum Ending {
    /// [`Session::close`].
    Closed,
    /// The link layer ended it, with its reason; `None` when the stream ended.
    Link(Option<String>),
    /// The stream failed.
    Stream(io::ErrorKind, String),
}

impl Ending {
    fn error(&self) -> Error {
        match self {
            Ending::Closed => Error::Ended(Some("the session was closed".into())),
            Ending::Link(reason) => Error::Ended(reason.clone()),
            Ending::Stream(kind, message) => Error::Io(io::Error::new(*kind, message.clone())),
        }
    }
}

struct Link {
    engine: Engine,
    deframer: Deframer,
    files: file_transfer::Receiver,
    incoming: VecDeque<Frame>,
    incoming_bytes: usize,
    ready: bool,
    ended: Option<Ending>,
}

struct Shared {
    stream: Arc<dyn Stream>,
    started: Instant,
    link: Mutex<Link>,
    changed: Condvar,
    /// Held while output is taken from the engine and written, so writes keep their order.
    writer: Mutex<()>,
    /// Held while one message is split into packets, so messages do not interleave.
    sender: Mutex<()>,
    on_artwork: Box<dyn Fn(Artwork) + Send + Sync>,
}

/// An iAP2 session on a stream.
pub struct Session {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Session {
    /// Starts the link on `stream`; `on_artwork` gets each finished artwork transfer, on the worker thread.
    pub fn open(
        stream: Arc<dyn Stream>,
        transport: Transport,
        on_artwork: impl Fn(Artwork) + Send + Sync + 'static,
    ) -> Session {
        let (config, initiator) = match transport {
            Transport::Bluetooth => (link::Config::bluetooth(), false),
            Transport::Tunnel => (link::Config::tunnel(), true),
        };
        let shared = Arc::new(Shared {
            stream,
            started: Instant::now(),
            link: Mutex::new(Link {
                engine: Engine::new(config),
                deframer: Deframer::new(),
                files: file_transfer::Receiver::default(),
                incoming: VecDeque::new(),
                incoming_bytes: 0,
                ready: false,
                ended: None,
            }),
            changed: Condvar::new(),
            writer: Mutex::new(()),
            sender: Mutex::new(()),
            on_artwork: Box::new(on_artwork),
        });
        let now = shared.now();
        shared.lock().engine.start(initiator, now);
        shared.flush();
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("iap2-link".into())
                .spawn(move || shared.run())
        };
        let worker = match worker {
            Ok(worker) => Some(worker),
            Err(error) => {
                shared.end(Ending::Stream(error.kind(), error.to_string()));
                None
            }
        };
        Session {
            shared,
            worker: Mutex::new(worker),
        }
    }

    /// Waits until messages can be sent: false when `timeout` passed first, an error when the link ended.
    pub fn await_ready(&self, timeout: Duration) -> Result<bool, Error> {
        let deadline = Instant::now() + timeout;
        let link = self
            .shared
            .wait_until(deadline, |link| link.ready || link.ended.is_some());
        match &link.ended {
            Some(ending) => Err(ending.error()),
            None => Ok(link.ready),
        }
    }

    /// Sends one message, split to the peer's packet size, within `timeout`. A message that could only be sent in
    /// part ends the link, as the rest of the stream would no longer frame.
    pub fn send(&self, frame: &Frame, timeout: Duration) -> Result<(), Error> {
        let deadline = Instant::now() + timeout;
        let _sending = self.shared.sender.lock().unwrap_or_else(|p| p.into_inner());
        let encoded = frame.encode();
        let chunk = {
            let link = self
                .shared
                .wait_until(deadline, |link| link.ready || link.ended.is_some());
            if let Some(ending) = &link.ended {
                return Err(ending.error());
            }
            if !link.ready {
                return Err(Error::TimedOut("waiting for the link to send"));
            }
            link.engine.peer_max_payload().min(MAX_LINK_CHUNK_BYTES)
        };
        for (index, piece) in split_for_link(&encoded, chunk)?.enumerate() {
            {
                let mut link = self.shared.wait_until(deadline, |link| {
                    link.engine.writable() || link.ended.is_some()
                });
                if let Some(ending) = &link.ended {
                    return Err(ending.error());
                }
                if !link.engine.writable() {
                    drop(link);
                    if index > 0 {
                        self.shared.end(Ending::Link(Some(
                            "a message could not be sent whole in time".into(),
                        )));
                    }
                    return Err(Error::TimedOut("sending a message"));
                }
                let now = self.shared.now();
                link.engine.send_control(piece, now)?;
            }
            self.shared.flush();
        }
        Ok(())
    }

    /// The next message: `None` when `timeout` passed first, an error once the link ended.
    pub fn recv(&self, timeout: Duration) -> Result<Option<Frame>, Error> {
        let deadline = Instant::now() + timeout;
        let mut link = self.shared.wait_until(deadline, |link| {
            !link.incoming.is_empty() || link.ended.is_some()
        });
        if let Some(frame) = link.incoming.pop_front() {
            link.incoming_bytes -= frame.body().len();
            return Ok(Some(frame));
        }
        match &link.ended {
            Some(ending) => Err(ending.error()),
            None => Ok(None),
        }
    }

    /// Whether the link ended.
    pub fn is_ended(&self) -> bool {
        self.shared.lock().ended.is_some()
    }

    /// Ends the link, shuts the stream down and waits for the worker.
    pub fn close(&self) {
        self.shared.end(Ending::Closed);
        let worker = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(worker) = worker {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn send_on(&self, session: u8, payload: &[u8]) {
        let now = self.shared.now();
        self.shared
            .lock()
            .engine
            .send_session(session, payload, now)
            .unwrap();
        self.shared.flush();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Link> {
        self.link.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn wait_until(&self, deadline: Instant, done: impl Fn(&Link) -> bool) -> MutexGuard<'_, Link> {
        let mut link = self.lock();
        loop {
            if done(&link) {
                return link;
            }
            let now = Instant::now();
            if now >= deadline {
                return link;
            }
            link = self
                .changed
                .wait_timeout(link, deadline - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Writes what the engine has to send.
    fn flush(&self) {
        let _writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let output = self.lock().engine.take_output();
        if output.is_empty() {
            return;
        }
        if let Err(error) = self.stream.send(&output) {
            self.end(Ending::Stream(error.kind(), error.to_string()));
        }
    }

    fn end(&self, ending: Ending) {
        {
            let mut link = self.lock();
            if link.ended.is_none() {
                link.ended = Some(ending);
            }
            link.ready = false;
            link.files.clear();
            self.changed.notify_all();
        }
        self.stream.shutdown();
    }

    fn run(&self) {
        let mut buffer = vec![0; RECEIVE_CHUNK_BYTES];
        loop {
            let wait = {
                let mut link = self.lock();
                if link.ended.is_some() {
                    return;
                }
                let now = self.now();
                link.engine.advance_time(now);
                let finished = self.drain(&mut link);
                let wait = link
                    .engine
                    .next_deadline()
                    .map_or(RECEIVE_POLL, |at| {
                        Duration::from_millis(at.saturating_sub(now))
                    })
                    .clamp(Duration::from_millis(1), RECEIVE_POLL);
                drop(link);
                self.deliver(finished);
                wait
            };
            self.flush();
            let received = self.stream.receive(&mut buffer, wait);
            let finished = {
                let mut link = self.lock();
                if link.ended.is_some() {
                    return;
                }
                match received {
                    Ok(Received::Bytes(count)) => {
                        let now = self.now();
                        link.engine.feed(&buffer[..count], now);
                    }
                    Ok(Received::Nothing) => {}
                    Ok(Received::End) => link.engine.feed_end(),
                    Err(error) => {
                        drop(link);
                        self.end(Ending::Stream(error.kind(), error.to_string()));
                        return;
                    }
                }
                self.drain(&mut link)
            };
            self.deliver(finished);
            self.flush();
        }
    }

    /// Handles the engine's events; gives the artwork transfers that finished.
    fn drain(&self, link: &mut Link) -> Vec<Artwork> {
        let mut finished = Vec::new();
        while let Some(event) = link.engine.poll_event() {
            match event {
                Event::Writable(writable) => {
                    link.ready = writable && link.ended.is_none();
                    self.changed.notify_all();
                }
                Event::Control(payload) => {
                    for frame in link.deframer.push(&payload) {
                        if link.incoming.len() >= MAX_PENDING_FRAMES
                            || frame.body().len() > MAX_PENDING_FRAME_BYTES - link.incoming_bytes
                        {
                            self.end_locked(
                                link,
                                Ending::Link(Some("received messages are not read".into())),
                            );
                            return finished;
                        }
                        link.incoming_bytes += frame.body().len();
                        link.incoming.push_back(frame);
                    }
                    self.changed.notify_all();
                }
                Event::Session { id, payload } if id == FILE_TRANSFER_SESSION => {
                    let outcome = link.files.accept(&payload);
                    let now = self.now();
                    for reply in outcome.replies {
                        // A reply the link cannot take is dropped; the iPhone then gives the transfer up.
                        let _ = link.engine.send_session(FILE_TRANSFER_SESSION, &reply, now);
                    }
                    finished.extend(outcome.finished);
                }
                Event::Session { .. } => {}
                Event::Dead(reason) => self.end_locked(link, Ending::Link(reason)),
            }
        }
        finished
    }

    fn end_locked(&self, link: &mut Link, ending: Ending) {
        if link.ended.is_none() {
            link.ended = Some(ending);
        }
        link.ready = false;
        link.files.clear();
        self.changed.notify_all();
        self.stream.shutdown();
    }

    fn deliver(&self, finished: Vec<Artwork>) {
        for artwork in finished {
            (self.on_artwork)(artwork);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::testing::pipe;
    use crate::wire::Parameters;

    const WAIT: Duration = Duration::from_secs(5);

    /// An accessory's session on Bluetooth and, standing in for the iPhone, an initiating session on the other end.
    fn connected() -> (Session, Session, Arc<Mutex<Vec<Artwork>>>) {
        let (a, b) = pipe();
        let artwork = Arc::new(Mutex::new(Vec::new()));
        let seen = artwork.clone();
        let accessory = Session::open(a, Transport::Bluetooth, move |art| {
            seen.lock().unwrap().push(art)
        });
        let phone = Session::open(b, Transport::Tunnel, |_| {});
        (accessory, phone, artwork)
    }

    #[test]
    fn messages_pass_both_ways() {
        let (accessory, phone, _) = connected();
        assert!(accessory.await_ready(WAIT).unwrap());
        assert!(phone.await_ready(WAIT).unwrap());
        let hello = Frame::with_parameters(0x1d00, &Parameters::new().string(0, "hello")).unwrap();
        phone.send(&hello, WAIT).unwrap();
        assert_eq!(accessory.recv(WAIT).unwrap(), Some(hello.clone()));
        accessory.send(&hello, WAIT).unwrap();
        assert_eq!(phone.recv(WAIT).unwrap(), Some(hello));
        assert_eq!(accessory.recv(Duration::from_millis(50)).unwrap(), None);
    }

    #[test]
    fn large_messages_are_split_and_joined() {
        let (accessory, phone, _) = connected();
        accessory.await_ready(WAIT).unwrap();
        phone.await_ready(WAIT).unwrap();
        let big =
            Frame::with_parameters(0x5001, &Parameters::new().bytes(0, &vec![7; 60_000])).unwrap();
        for _ in 0..3 {
            phone.send(&big, WAIT).unwrap();
        }
        for _ in 0..3 {
            assert_eq!(accessory.recv(WAIT).unwrap(), Some(big.clone()));
        }
    }

    #[test]
    fn artwork_is_answered_and_delivered() {
        let (accessory, phone, artwork) = connected();
        accessory.await_ready(WAIT).unwrap();
        phone.await_ready(WAIT).unwrap();
        let mut setup = vec![0x81, 0x04];
        setup.extend_from_slice(&3u64.to_be_bytes());
        setup.extend_from_slice(&2u16.to_be_bytes());
        phone.send_on(FILE_TRANSFER_SESSION, &setup);
        phone.send_on(FILE_TRANSFER_SESSION, &[0x81, 0x40, 1, 2, 3]);
        let deadline = Instant::now() + WAIT;
        while artwork.lock().unwrap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            artwork.lock().unwrap().as_slice(),
            [Artwork {
                id: 0x81,
                bytes: vec![1, 2, 3]
            }]
        );
    }

    #[test]
    fn the_end_of_the_stream_ends_the_session() {
        let (accessory, phone, _) = connected();
        accessory.await_ready(WAIT).unwrap();
        phone.close();
        assert!(matches!(accessory.recv(WAIT), Err(Error::Ended(_))));
        assert!(accessory.is_ended());
        assert!(accessory
            .send(&Frame::new(1, vec![]).unwrap(), WAIT)
            .is_err());
    }

    /// A stream whose receive fails, as one does when the peer stops answering.
    struct Silent;

    impl Stream for Silent {
        fn send(&self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }

        fn receive(&self, _: &mut [u8], _: Duration) -> io::Result<Received> {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "peer stopped answering",
            ))
        }

        fn shutdown(&self) {}
    }

    #[test]
    fn a_peer_that_stops_answering_ends_the_session() {
        let session = Session::open(Arc::new(Silent), Transport::Tunnel, |_| {});
        match session.await_ready(WAIT) {
            Err(Error::Io(error)) => assert_eq!(error.kind(), io::ErrorKind::TimedOut),
            other => panic!("expected the stream's error, got {other:?}"),
        }
    }

    #[test]
    fn nothing_ready_times_out() {
        let (a, _b) = pipe();
        let session = Session::open(a, Transport::Bluetooth, |_| {});
        assert!(!session.await_ready(Duration::from_millis(50)).unwrap());
        assert!(matches!(
            session.send(&Frame::new(1, vec![]).unwrap(), Duration::from_millis(50)),
            Err(Error::TimedOut(_))
        ));
        session.close();
        assert!(session.is_ended());
    }
}
