// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Wireless CarPlay's start over Bluetooth: iAP2 on the RFCOMM connection the iPhone opens, which identifies the
//! accessory, authenticates it with the MFi identity, gives the iPhone the access point to join and the AirPlay
//! receiver to reach, and starts the CarPlay session. The link stays up while the session lasts. Each link passes
//! what plays on the iPhone, its now-playing updates and artwork transfers, to the receiver.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use carbridge_iap2::identification::{self, parse_bluetooth_address};
use carbridge_iap2::messages::{self, Security};
use carbridge_iap2::wireless::{self as control, Event, Host, Outcome, Setup};
use carbridge_iap2::{now_playing, Received, Session, Stream, Transport};

use crate::receiver::{NowPlayingFeed, Receiver};

/// How long the iAP2 link may take to come up after the iPhone connected.
const LINK_TIMEOUT: Duration = Duration::from_secs(10);
/// Accessory protocol named in identification; no app uses it.
const ACCESSORY_PROTOCOL: &str = "org.carbridge.accessory";
/// The links run so far, which number each link's artwork transfers apart.
static LINKS: AtomicU64 = AtomicU64::new(0);
/// How long a link waits for the iPhone to try AirPlay again after its authentication failed.
pub const AUTHENTICATION_GRACE: Duration = Duration::from_secs(5);
/// How long a link waits for a CarPlay session after the iPhone was given the access point and asked to start
/// CarPlay, or after its last session ended. Long enough for a slow join of the access point.
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(60);

/// The access point the iPhone joins.
#[derive(Clone)]
pub struct Network {
    pub ssid: String,
    pub passphrase: String,
    pub security: Security,
    pub channel: u8,
    pub bssid: Option<[u8; 6]>,
    /// The receiver's address on it.
    pub address: IpAddr,
}

/// How the accessory identifies itself over iAP2.
#[derive(Clone, Debug)]
pub struct Accessory {
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub firmware_version: String,
    pub hardware_version: String,
    /// The adapter the iPhone paired with.
    pub bluetooth_address: String,
}

/// An RFCOMM socket as a blocking duplex stream.
pub struct Rfcomm {
    reader: Mutex<UnixStream>,
    writer: Mutex<UnixStream>,
}

impl Rfcomm {
    pub fn new(socket: OwnedFd) -> io::Result<Rfcomm> {
        // Only stream reads, writes, timeouts and shutdown are used, which work on any stream socket.
        let stream = UnixStream::from(socket);
        stream.set_nonblocking(false)?;
        Ok(Rfcomm {
            writer: Mutex::new(stream.try_clone()?),
            reader: Mutex::new(stream),
        })
    }
}

impl Stream for Rfcomm {
    fn send(&self, data: &[u8]) -> io::Result<()> {
        self.writer.lock().unwrap().write_all(data)
    }

    fn receive(&self, buffer: &mut [u8], timeout: Duration) -> io::Result<Received> {
        let mut reader = self.reader.lock().unwrap();
        // Some systems refuse socket options once the peer has shut the connection down.
        match reader.set_read_timeout(Some(timeout.max(Duration::from_millis(1)))) {
            Err(err) if err.kind() == io::ErrorKind::InvalidInput => return Ok(Received::End),
            other => other?,
        }
        match reader.read(buffer) {
            Ok(0) => Ok(Received::End),
            Ok(n) => Ok(Received::Bytes(n)),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(Received::Nothing)
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
                ) =>
            {
                Ok(Received::End)
            }
            Err(err) => Err(err),
        }
    }

    fn shutdown(&self) {
        let _ = self.writer.lock().unwrap().shutdown(Shutdown::Both);
    }
}

/// The security an access point's uci `encryption` stands for.
pub fn security(encryption: &str) -> Security {
    let encryption = encryption.split('+').next().unwrap_or_default();
    match encryption {
        "none" | "owe" | "" => Security::None,
        "sae" => Security::Wpa3Only,
        "sae-mixed" => Security::Wpa3Transition,
        "wep" | "wep-open" | "wep-shared" => Security::Wep,
        _ => Security::WpaWpa2,
    }
}

/// A step of a link's bring-up, as it happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// iAP2 is up: the accessory identified and authenticated, and the subscriptions sent.
    Ready,
    /// The iPhone was given the access point to join (0x5703).
    WifiConfigurationSent,
    /// The iPhone was asked to start CarPlay (0x4301), in answer to its CarPlay availability.
    StartSessionSent,
}

/// Why a link over Bluetooth is started afresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stall {
    /// AirPlay authentication failed and the iPhone did not try again.
    AuthenticationFailed,
    /// No CarPlay session came.
    NoSession,
}

/// Follows a link over Bluetooth that started CarPlay over Wi-Fi, for a session that does not come: one whose
/// iPhone failed AirPlay authentication, or never reached the receiver, is better called afresh than left up, as the
/// iPhone tries no more over a link that stays. The wait starts once the iPhone was both given the access point and
/// asked to start CarPlay; an iPhone that has not yet said CarPlay is available, as when it waits for its user to
/// allow CarPlay, is not timed.
#[derive(Clone, Debug, Default)]
pub struct SessionWatch {
    wifi_sent: bool,
    start_sent: bool,
    /// Since when the link waits for a session.
    waiting: Option<Instant>,
    /// When AirPlay authentication failed while the link waited.
    failed: Option<Instant>,
}

impl SessionWatch {
    pub fn step(&mut self, step: Step, now: Instant) {
        match step {
            Step::Ready => return,
            Step::WifiConfigurationSent => self.wifi_sent = true,
            Step::StartSessionSent => self.start_sent = true,
        }
        if self.wifi_sent && self.start_sent && self.waiting.is_none() {
            self.waiting = Some(now);
        }
    }

    /// An iPhone failed AirPlay authentication.
    pub fn authentication_failed(&mut self, now: Instant) {
        if self.waiting.is_some() && self.failed.is_none() {
            self.failed = Some(now);
        }
    }

    /// A CarPlay session is up.
    pub fn session_started(&mut self) {
        self.waiting = None;
        self.failed = None;
    }

    /// A CarPlay session ended, or one that was starting gave up: a link that started CarPlay waits for the next.
    pub fn session_ended(&mut self, now: Instant) {
        if self.wifi_sent && self.start_sent && self.waiting.is_none() {
            self.waiting = Some(now);
        }
    }

    /// Whether the link should be started afresh, at `now`; never while a CarPlay session runs.
    pub fn stalled(&self, now: Instant, session_running: bool) -> Option<Stall> {
        let waiting = self.waiting.filter(|_| !session_running)?;
        if self
            .failed
            .is_some_and(|at| now.saturating_duration_since(at) >= AUTHENTICATION_GRACE)
        {
            return Some(Stall::AuthenticationFailed);
        }
        (now.saturating_duration_since(waiting) >= SESSION_TIMEOUT).then_some(Stall::NoSession)
    }
}

/// How a link's run ended, and what it did.
#[derive(Debug)]
pub struct LinkEnd(Outcome);

impl std::fmt::Display for LinkEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let outcome = &self.0;
        match &outcome.link_ended {
            Some(err) => write!(f, "{err}")?,
            None => f.write_str("stopped")?,
        }
        write!(
            f,
            "; Wi-Fi configuration sent {} time(s), CarPlay asked to start {} time(s)",
            outcome.wifi_configurations_sent, outcome.start_sessions_sent
        )
    }
}

struct Events<'a> {
    stop: &'a AtomicBool,
    log: &'a (dyn Fn(String) + Send + Sync),
    step: &'a (dyn Fn(Step) + Send + Sync),
    now_playing: &'a NowPlayingFeed,
    /// The link's number for `now_playing`.
    link: u64,
}

impl Host for Events<'_> {
    fn event(&self, event: Event<'_>) {
        match event {
            Event::Ready => (self.step)(Step::Ready),
            Event::WifiConfigurationSent => {
                (self.log)("iAP2 Wi-Fi configuration sent".into());
                (self.step)(Step::WifiConfigurationSent);
            }
            Event::StartSessionSent => {
                (self.log)("iAP2 CarPlay session started".into());
                (self.step)(Step::StartSessionSent);
            }
            Event::Message(frame) => {
                if let Some(change) = now_playing::now_playing(frame) {
                    self.now_playing.change(self.link, &change);
                }
            }
        }
    }

    fn log(&self, message: &str) {
        (self.log)(message.to_owned());
    }

    fn keep_running(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
    }
}

/// Runs iAP2 on an RFCOMM connection from an iPhone until the link closes or `stop` is set; `step` runs at each
/// step of the bring-up. Blocks.
pub fn run(
    socket: OwnedFd,
    accessory: &Accessory,
    network: &Network,
    receiver: &Receiver,
    stop: &AtomicBool,
    log: &(dyn Fn(String) + Send + Sync),
    step: &(dyn Fn(Step) + Send + Sync),
) -> io::Result<LinkEnd> {
    let stream: Arc<dyn Stream> = Arc::new(Rfcomm::new(socket)?);
    control_link(
        stream,
        Transport::Bluetooth,
        accessory,
        network,
        receiver,
        stop,
        log,
        step,
    )
}

/// Runs iAP2 on the tunnel the iPhone opens inside its CarPlay session, once the session runs over Wi-Fi: the
/// iPhone identifies and authenticates the accessory there again, and finishes setting up CarPlay only then.
/// `ready` runs once iAP2 is up on the tunnel. Blocks until the tunnel closes or `stop` is set.
pub fn run_tunnel(
    stream: Arc<dyn Stream>,
    accessory: &Accessory,
    network: &Network,
    receiver: &Receiver,
    stop: &AtomicBool,
    log: &(dyn Fn(String) + Send + Sync),
    ready: &(dyn Fn() + Send + Sync),
) -> io::Result<LinkEnd> {
    let step = |step| {
        if step == Step::Ready {
            ready();
        }
    };
    control_link(
        stream,
        Transport::Tunnel,
        accessory,
        network,
        receiver,
        stop,
        log,
        &step,
    )
}

#[allow(clippy::too_many_arguments)]
fn control_link(
    stream: Arc<dyn Stream>,
    transport: Transport,
    accessory: &Accessory,
    network: &Network,
    receiver: &Receiver,
    stop: &AtomicBool,
    log: &(dyn Fn(String) + Send + Sync),
    step: &(dyn Fn(Step) + Send + Sync),
) -> io::Result<LinkEnd> {
    let bluetooth_address =
        parse_bluetooth_address(&accessory.bluetooth_address).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Bluetooth address {:?} is not six hex bytes",
                    accessory.bluetooth_address
                ),
            )
        })?;
    let link = LINKS.fetch_add(1, Ordering::Relaxed) + 1;
    let now_playing = receiver.now_playing();
    let session = {
        let now_playing = now_playing.clone();
        Session::open(stream, transport, move |transfer| {
            now_playing.transfer(link, transfer)
        })
    };
    let result = (|| {
        if !session.await_ready(LINK_TIMEOUT)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "iAP2 link did not come up",
            ));
        }
        let setup = Setup {
            accessory: identification::Accessory {
                name: accessory.name.clone(),
                model: accessory.model.clone(),
                manufacturer: accessory.manufacturer.clone(),
                serial_number: accessory.serial_number.clone(),
                firmware_version: accessory.firmware_version.clone(),
                hardware_version: accessory.hardware_version.clone(),
                language: "en".into(),
                accessory_protocol: ACCESSORY_PROTOCOL.into(),
            },
            bluetooth_address,
            network: messages::Network {
                ssid: network.ssid.clone(),
                passphrase: network.passphrase.clone(),
                channel: network.channel,
                security: network.security,
                bssid: network.bssid,
            },
            receiver: messages::Receiver {
                addresses: vec![network.address.to_string()],
                port: receiver.port(),
                device_id: receiver.device_id().to_owned(),
                public_key: receiver.public_key(),
                source_version: receiver.source_version().to_owned(),
            },
        };
        let events = Events {
            stop,
            log,
            step,
            now_playing: &now_playing,
            link,
        };
        Ok(LinkEnd(control::run(
            &session,
            &setup,
            receiver.mfi().as_ref(),
            &events,
        )?))
    })();
    session.close();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn securities() {
        assert_eq!(security("psk2"), Security::WpaWpa2);
        assert_eq!(security("psk2+ccmp"), Security::WpaWpa2);
        assert_eq!(security("psk-mixed"), Security::WpaWpa2);
        assert_eq!(security("sae"), Security::Wpa3Only);
        assert_eq!(security("sae-mixed"), Security::Wpa3Transition);
        assert_eq!(security("none"), Security::None);
    }

    /// A link that gave the iPhone the access point and asked it to start CarPlay at `start`.
    fn started(start: Instant) -> SessionWatch {
        let mut watch = SessionWatch::default();
        watch.step(Step::Ready, start);
        watch.step(Step::WifiConfigurationSent, start);
        watch.step(Step::StartSessionSent, start);
        watch
    }

    #[test]
    fn a_link_that_has_not_started_carplay_is_not_timed() {
        let start = Instant::now();
        let later = start + SESSION_TIMEOUT * 10;
        let mut watch = SessionWatch::default();
        watch.step(Step::Ready, start);
        assert_eq!(watch.stalled(later, false), None);
        // Given the access point, the iPhone has not yet said CarPlay is available.
        watch.step(Step::WifiConfigurationSent, start);
        watch.authentication_failed(start);
        watch.session_ended(start);
        assert_eq!(watch.stalled(later, false), None);
        let mut watch = SessionWatch::default();
        watch.step(Step::StartSessionSent, start);
        assert_eq!(watch.stalled(later, false), None);
    }

    #[test]
    fn a_failed_authentication_restarts_the_link_after_the_grace() {
        let start = Instant::now();
        let mut watch = started(start);
        let failed = start + Duration::from_millis(300);
        watch.authentication_failed(failed);
        assert_eq!(watch.stalled(failed, false), None);
        let due = failed + AUTHENTICATION_GRACE;
        assert_eq!(watch.stalled(due - Duration::from_millis(1), false), None);
        assert_eq!(watch.stalled(due, false), Some(Stall::AuthenticationFailed));
        // The starting session that failed ends; the failure stands.
        watch.session_ended(failed);
        assert_eq!(watch.stalled(due, false), Some(Stall::AuthenticationFailed));
        assert_eq!(watch.stalled(due, true), None);
    }

    #[test]
    fn a_retry_that_succeeds_keeps_the_link() {
        let start = Instant::now();
        let mut watch = started(start);
        watch.authentication_failed(start);
        watch.session_started();
        assert_eq!(watch.stalled(start + SESSION_TIMEOUT * 2, false), None);
    }

    #[test]
    fn no_session_restarts_the_link_after_the_timeout() {
        let start = Instant::now();
        let watch = started(start);
        let due = start + SESSION_TIMEOUT;
        assert_eq!(watch.stalled(due - Duration::from_secs(1), false), None);
        assert_eq!(watch.stalled(due, false), Some(Stall::NoSession));
        assert_eq!(watch.stalled(due, true), None);
    }

    #[test]
    fn a_link_kept_after_its_session_ended_waits_for_the_next() {
        let start = Instant::now();
        let mut watch = started(start);
        watch.session_started();
        let ended = start + Duration::from_secs(600);
        assert_eq!(watch.stalled(ended, false), None);
        watch.session_ended(ended);
        // The iPhone asks again over the same link: the wait does not start over.
        watch.step(Step::StartSessionSent, ended + Duration::from_secs(1));
        assert_eq!(
            watch.stalled(ended + SESSION_TIMEOUT, false),
            Some(Stall::NoSession)
        );
        // Only a failure while waiting counts.
        let mut watch = started(start);
        watch.session_started();
        watch.authentication_failed(ended);
        assert_eq!(watch.stalled(ended + SESSION_TIMEOUT, false), None);
    }

    #[test]
    fn rfcomm_stream_over_a_socket_pair() {
        let (a, b) = UnixStream::pair().unwrap();
        let stream = Rfcomm::new(OwnedFd::from(a)).unwrap();
        let mut peer = b;
        peer.write_all(b"iap2").unwrap();
        let mut buf = [0u8; 16];
        let Received::Bytes(n) = stream.receive(&mut buf, Duration::from_secs(5)).unwrap() else {
            panic!("data")
        };
        assert_eq!(&buf[..n], b"iap2");
        stream.send(b"ack").unwrap();
        let mut buf = [0u8; 3];
        peer.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ack");
        assert!(matches!(
            stream.receive(&mut buf, Duration::from_millis(20)).unwrap(),
            Received::Nothing
        ));
        drop(peer);
        assert!(matches!(
            stream.receive(&mut buf, Duration::from_secs(5)).unwrap(),
            Received::End
        ));
    }
}
