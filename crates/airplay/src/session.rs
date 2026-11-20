// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/AirPlaySession.kt and
// airplay/CarPlayMediaEngine.kt (DiPlay commit 615b32c); main screen, audio, microphone and the iAP2 tunnel only.

//! One CarPlay session: the control connection an iPhone opens, and everything it sets up.
//!
//! The control connection carries pairing, /info, auth-setup and SETUP in the clear until pair-verify, then
//! encrypted. SETUP opens the timing port, the event channel (which carries the receiver's commands and HID
//! reports) and the streams: the main screen, audio with its microphone uplink, and the iAP2 tunnel. The host
//! hears of all of it through [`Host`].
//!
//! A connection of the session that fails ends the session. One whose peer stopped answering, which the host's
//! sockets report as a timed-out error, ends it as [`Ending::PeerUnreachable`].

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use carbridge_iap2::mfi::Authenticator;

use crate::audio::{open_packet, AudioFormat};
use crate::bplist::{self, Dict, Value};
use crate::control_cipher::{self, Opener, Sealer};
use crate::crypto;
use crate::hid::{self, Contact, Knob};
use crate::identity::{Identity, Pairings};
use crate::info::{self, Config};
use crate::microphone::{MicrophoneFormat, Packetizer, SILENT_START_MS};
use crate::net::{same_host, Datagram, Listener, Received, Sockets, Stream};
use crate::ntp;
use crate::pairing::{PairSetup, PairVerify};
use crate::rtsp::{self, Request, Response};
use crate::sap;
use crate::screen::{self, ScreenMessage};
use crate::tunnel::{Tunnel, Unpacker};

const PLIST: &str = "application/x-apple-binary-plist";
const PAIRING: &str = "application/pairing+tlv8";
const OCTETS: &str = "application/octet-stream";

const MAIN_SCREEN: i64 = 110;
const MAIN_AUDIO: i64 = 100;
const ALT_AUDIO: i64 = 101;
const MAIN_HIGH_AUDIO: i64 = 102;
const DATA_STREAM: i64 = 130;
const IAP2_DATA_STREAM: &str = "E9459FD0-BCAD-4C45-820F-1E72447EF2F2";

const POLL: Duration = Duration::from_millis(200);
const READ_BYTES: usize = 16 * 1024;
const AUDIO_RECEIVE_BUFFER: usize = 512 * 1024;
/// How long an iAP2 message waits for the event channel.
const EVENT_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// What a session is set up with.
#[derive(Clone)]
pub struct Setup {
    pub config: Arc<Config>,
    pub identity: Arc<Identity>,
    pub pairings: Arc<dyn Pairings>,
    pub authenticator: Arc<dyn Authenticator>,
    pub sockets: Arc<dyn Sockets>,
}

/// Why a session ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// The iPhone closed it.
    Closed,
    /// The host ended it.
    Ended,
    /// The iPhone stopped answering on one of the session's connections.
    PeerUnreachable { connection: &'static str },
    /// A connection failed otherwise.
    Failed(String),
}

impl fmt::Display for Ending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ending::Closed => f.write_str("the iPhone closed the session"),
            Ending::Ended => f.write_str("the session was ended"),
            Ending::PeerUnreachable { connection } => {
                write!(
                    f,
                    "the iPhone stopped answering on the {connection} connection"
                )
            }
            Ending::Failed(reason) => f.write_str(reason),
        }
    }
}

fn ending(connection: &'static str, error: &io::Error) -> Ending {
    if error.kind() == io::ErrorKind::TimedOut {
        Ending::PeerUnreachable { connection }
    } else {
        Ending::Failed(format!("{connection} connection: {error}"))
    }
}

/// What the iPhone says of itself in its session SETUP.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub device_id: String,
    pub mac_address: String,
    pub model: String,
    pub os_version: String,
}

/// One audio stream: its stream type, and the kind of audio it carries (`media`, `telephony`, ...).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AudioStreamId {
    pub kind: u16,
    pub audio_type: String,
}

/// What a session tells its host. Every method has a default that does nothing. They run on the session's
/// threads and should return quickly.
pub trait Host: Send + Sync {
    /// A line worth logging.
    fn log(&self, _session: &Session, _message: &str) {}

    /// The iPhone started the session (RECORD).
    fn active(&self, _session: &Arc<Session>) {}

    /// The session ended; it is called once.
    fn ended(&self, _session: &Arc<Session>, _ending: &Ending) {}

    /// Pair-setup or auth-setup failed for the iPhone at `peer`.
    fn pairing_refused(&self, _peer: IpAddr) {}

    fn device_info(&self, _session: &Session, _info: &DeviceInfo) {}

    /// A command from the iPhone, such as `requestUI` or `duckAudio`.
    fn command(&self, _session: &Session, _kind: &str, _params: &Dict) {}

    /// The main screen stream started or stopped.
    fn screen(&self, _session: &Session, _active: bool) {}

    fn video(&self, _session: &Session, _message: ScreenMessage) {}

    fn audio_started(&self, _session: &Session, _stream: &AudioStreamId, _format: &AudioFormat) {}

    /// One RTP packet, header and clear payload, with its timestamp.
    fn audio(
        &self,
        _session: &Session,
        _stream: &AudioStreamId,
        _format: &AudioFormat,
        _rtp: &[u8],
        _timestamp: u32,
    ) {
    }

    fn audio_stopped(&self, _session: &Session, _stream: &AudioStreamId) {}

    /// The iPhone wants the microphone for `stream`; send it with [`Session::send_microphone`].
    fn microphone_started(
        &self,
        _session: &Session,
        _stream: &AudioStreamId,
        _format: &MicrophoneFormat,
    ) {
    }

    fn microphone_stopped(&self, _session: &Session, _stream: &AudioStreamId) {}

    /// The iPhone set up the iAP2 tunnel: an iAP2 session runs on `tunnel`.
    fn iap2_tunnel(&self, _session: &Arc<Session>, _tunnel: Arc<dyn Stream>) {}
}

struct Microphone {
    format: MicrophoneFormat,
    destination: SocketAddr,
    socket: Arc<dyn Datagram>,
    packetizer: Mutex<Packetizer>,
    started: AtomicBool,
}

struct AudioEntry {
    format: AudioFormat,
    data: Arc<dyn Datagram>,
    control: Arc<dyn Datagram>,
    stop: Arc<AtomicBool>,
    connection_id: Value,
    latency_ms: i64,
    /// The first timestamp and when it arrived.
    started: Arc<Mutex<Option<(u32, Instant)>>>,
    microphone: Option<Arc<Microphone>>,
}

struct Connection {
    listener: Arc<dyn Listener>,
    stop: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<Arc<dyn Stream>>>>,
}

impl Connection {
    fn new(listener: Arc<dyn Listener>) -> Connection {
        Connection {
            listener,
            stop: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(Mutex::new(None)),
        }
    }

    fn close(&self) {
        self.stop.store(true, Ordering::Release);
        self.listener.close();
        if let Some(stream) = self.stream.lock().unwrap_or_else(|p| p.into_inner()).take() {
            stream.shutdown();
        }
    }
}

#[derive(Default)]
struct State {
    shared_secret: Option<[u8; 32]>,
    clock: Option<Arc<ntp::Clock>>,
    event_listener: Option<Arc<dyn Listener>>,
    keep_alive: Option<Arc<dyn Datagram>>,
    screen: Option<Connection>,
    audio: HashMap<AudioStreamId, AudioEntry>,
    tunnel: Option<(Connection, Arc<Tunnel>)>,
    /// Tunnels to hand to the host once the SETUP response is sent.
    new_tunnels: Vec<Arc<Tunnel>>,
}

#[derive(Default)]
struct Events {
    stream: Option<Arc<dyn Stream>>,
    sealer: Option<Sealer>,
    sequence: u32,
    night_mode: Option<bool>,
}

/// One CarPlay session.
pub struct Session {
    me: Weak<Session>,
    peer: IpAddr,
    control: Arc<dyn Stream>,
    setup: Setup,
    host: Arc<dyn Host>,
    ended: AtomicBool,
    pairing: Mutex<(PairSetup, PairVerify)>,
    state: Mutex<State>,
    events: Mutex<Events>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(body);
}

fn int(value: Option<&Value>) -> Option<i64> {
    value?.as_i64()
}

fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

impl Session {
    /// Runs a session on the control connection `control` from the iPhone at `peer`.
    pub fn start(
        control: Arc<dyn Stream>,
        peer: IpAddr,
        setup: Setup,
        host: Arc<dyn Host>,
    ) -> Arc<Session> {
        let pairing = (
            PairSetup::new(setup.identity.clone(), setup.pairings.clone()),
            PairVerify::new(setup.identity.clone(), setup.pairings.clone()),
        );
        let session = Arc::new_cyclic(|me| Session {
            me: me.clone(),
            peer,
            control,
            setup,
            host,
            ended: AtomicBool::new(false),
            pairing: Mutex::new(pairing),
            state: Mutex::new(State::default()),
            events: Mutex::new(Events::default()),
        });
        let running = session.clone();
        spawn("airplay-control", move || running.run_control());
        session
    }

    /// The iPhone's address.
    pub fn peer(&self) -> IpAddr {
        self.peer
    }

    pub fn is_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// Ends the session.
    pub fn end(&self) {
        self.finish(Ending::Ended);
    }

    fn log(&self, message: &str) {
        self.host.log(self, message);
    }

    fn finish(&self, ending: Ending) {
        if self.ended.swap(true, Ordering::AcqRel) {
            return;
        }
        self.control.shutdown();
        let state = std::mem::take(&mut *lock(&self.state));
        if let Some(clock) = &state.clock {
            clock.close();
        }
        if let Some(listener) = &state.event_listener {
            listener.close();
        }
        if let Some(socket) = &state.keep_alive {
            socket.close();
        }
        if let Some(screen) = &state.screen {
            screen.close();
            self.host.screen(self, false);
        }
        for (stream, entry) in state.audio {
            self.stop_audio(&stream, entry);
        }
        if let Some((connection, tunnel)) = &state.tunnel {
            connection.close();
            tunnel.shutdown();
        }
        let events = std::mem::take(&mut *lock(&self.events));
        if let Some(stream) = events.stream {
            stream.shutdown();
        }
        self.log(&format!("AirPlay session ended: {ending}"));
        if let Some(me) = self.me.upgrade() {
            self.host.ended(&me, &ending);
        }
    }

    fn run_control(&self) {
        let mut buffer = vec![0; READ_BYTES];
        let mut sealed = Vec::new();
        let mut plain = Vec::new();
        let mut cipher: Option<(Opener, Sealer)> = None;
        while !self.is_ended() {
            let count = match self.control.receive(&mut buffer, POLL) {
                Ok(Received::Bytes(count)) => count,
                Ok(Received::Nothing) => continue,
                Ok(Received::End) => return self.finish(Ending::Closed),
                Err(error) => return self.finish(ending("control", &error)),
            };
            match &mut cipher {
                Some((opener, _)) => {
                    sealed.extend_from_slice(&buffer[..count]);
                    match opener.open(&mut sealed) {
                        Ok(opened) => plain.extend_from_slice(&opened),
                        Err(error) => {
                            return self.finish(Ending::Failed(format!("control channel: {error}")))
                        }
                    }
                }
                None => plain.extend_from_slice(&buffer[..count]),
            }
            let requests = match rtsp::take_requests(&mut plain) {
                Ok(requests) => requests,
                Err(error) => {
                    return self.finish(Ending::Failed(format!("control channel: {error}")))
                }
            };
            for request in requests {
                let wire = self.handle(&request).encode(&request);
                let wire = match &mut cipher {
                    Some((_, sealer)) => sealer.seal(&wire),
                    None => wire,
                };
                if let Err(error) = self.control.send(&wire) {
                    return self.finish(ending("control", &error));
                }
                if cipher.is_none() {
                    if let Some(verified) = lock(&self.pairing).1.verified() {
                        cipher = Some(control_cipher::split(&verified.control_keys));
                    }
                }
            }
            let tunnels = std::mem::take(&mut lock(&self.state).new_tunnels);
            if let Some(me) = self.me.upgrade() {
                for tunnel in tunnels {
                    self.host.iap2_tunnel(&me, tunnel);
                }
            }
        }
    }

    fn plist(value: &Value) -> Response {
        Response::ok().body(PLIST, bplist::encode(value))
    }

    fn handle(&self, request: &Request) -> Response {
        let path = request.path.to_ascii_lowercase();
        match request.method.as_str() {
            "SETUP" => return self.setup(&request.body),
            "SET_PARAMETER" => return Response::ok(),
            "RECORD" => {
                if let Some(me) = self.me.upgrade() {
                    self.host.active(&me);
                }
                return Response::ok();
            }
            "TEARDOWN" => return self.teardown(&request.body),
            _ => {}
        }
        if path.ends_with("/pair-setup") {
            let reply = lock(&self.pairing).0.handle(&request.body);
            if let Some(failure) = &reply.failure {
                self.log(&format!("pair-setup failed: {failure}"));
                self.host.pairing_refused(self.peer);
            }
            Response::ok().body(PAIRING, reply.body)
        } else if path.ends_with("/pair-verify") {
            let reply = {
                let mut pairing = lock(&self.pairing);
                let reply = pairing.1.handle(&request.body);
                if let Some(verified) = pairing.1.verified() {
                    lock(&self.state).shared_secret = Some(verified.shared_secret);
                }
                reply
            };
            if let Some(failure) = &reply.failure {
                self.log(&format!("pair-verify failed: {failure}"));
            }
            Response::ok().body(PAIRING, reply.body)
        } else if path.ends_with("/auth-setup") {
            match sap::auth_setup(&request.body, self.setup.authenticator.as_ref()) {
                Ok(body) => Response::ok().body(OCTETS, body),
                Err(failure) => {
                    self.log(&format!("auth-setup failed: {failure}"));
                    self.host.pairing_refused(self.peer);
                    Response::new(400)
                }
            }
        } else if path.ends_with("/info") {
            Session::plist(&info::info(&self.setup.config))
        } else if request.method == "POST" && path.ends_with("/command") {
            self.command(&request.body);
            Response::ok()
        } else if request.method == "POST" && path.ends_with("/feedback") {
            match self.feedback() {
                Some(feedback) => Session::plist(&feedback),
                None => Response::ok(),
            }
        } else {
            Response::ok()
        }
    }

    fn command(&self, body: &[u8]) {
        let body = bplist::decode(body).unwrap_or(Value::Null);
        let kind = text(body.get("type"));
        let params = body
            .get("params")
            .and_then(Value::as_dict)
            .cloned()
            .unwrap_or_default();
        self.host.command(self, &kind, &params);
    }

    fn setup(&self, body: &[u8]) -> Response {
        let Ok(Value::Dict(request)) = bplist::decode(body) else {
            return Response::new(400);
        };
        if let Some(streams) = request.get("streams").and_then(Value::as_array) {
            let mut accepted = Vec::new();
            for stream in streams {
                let Some(stream) = stream.as_dict() else {
                    continue;
                };
                match self.setup_stream(stream) {
                    Ok(Some(response)) => accepted.push(Value::Dict(response)),
                    Ok(None) => {}
                    Err(error) => self.log(&format!("stream setup failed: {error}")),
                }
            }
            return Session::plist(&Value::Dict(Dict::new().with("streams", accepted)));
        }
        let info = DeviceInfo {
            name: text(request.get("name")),
            device_id: text(request.get("deviceID")),
            mac_address: text(request.get("macAddress")).to_lowercase(),
            model: text(request.get("model")),
            os_version: text(request.get("osVersion")),
        };
        self.host.device_info(self, &info);
        match self.setup_session(&request) {
            Ok(response) => Session::plist(&Value::Dict(response)),
            Err(error) => {
                self.log(&format!("session setup failed: {error}"));
                Response::new(500)
            }
        }
    }

    fn setup_session(&self, request: &Dict) -> io::Result<Dict> {
        let sockets = &self.setup.sockets;
        let clock = Arc::new(ntp::Clock::new(sockets.datagram(self.peer, None)?));
        if let Some(port) = int(request.get("timingPort")).filter(|p| (1..=0xffff).contains(p)) {
            clock.sync_with(self.peer, port as u16);
        }
        let events = sockets.listen(self.peer)?;
        let mut response = Dict::new()
            .with("timingPort", clock.port())
            .with("eventPort", events.port());
        let low_power = request
            .get("keepAliveLowPower")
            .is_some_and(|v| v.as_bool() == Some(true) || v.as_i64() == Some(1));
        let keep_alive = if low_power {
            let socket = sockets.datagram(self.peer, None)?;
            response.insert("keepAlivePort", socket.port());
            Some(socket)
        } else {
            None
        };
        let mut features = Vec::new();
        if self.setup.config.hevc {
            features.push(Value::from("hevc"));
        }
        features.extend([Value::from("iAPChannel"), Value::from("viewAreas")]);
        response.insert("enabledFeatures", features);
        {
            let mut state = lock(&self.state);
            for old in [state.event_listener.take()].into_iter().flatten() {
                old.close();
            }
            if let Some(old) = state.clock.replace(clock) {
                old.close();
            }
            state.event_listener = Some(events.clone());
            state.keep_alive = keep_alive.clone();
        }
        if let Some(socket) = keep_alive {
            spawn("airplay-keep-alive", move || {
                let mut buffer = [0; 512];
                while socket.receive_from(&mut buffer, POLL).is_ok() {}
            });
        }
        let me = self.me.clone();
        spawn("airplay-events", move || {
            if let Some(session) = me.upgrade() {
                session.run_events(events);
            }
        });
        Ok(response)
    }

    /// The key a data stream is sealed with, from its connection identifier or seed.
    fn stream_key(&self, salt: &Value, label: &str) -> Option<[u8; 32]> {
        let shared = lock(&self.state).shared_secret?;
        let id = salt.as_u64()?;
        Some(crypto::hkdf_sha512(
            &shared,
            format!("DataStream-Salt{id}").as_bytes(),
            label.as_bytes(),
        ))
    }

    fn setup_stream(&self, stream: &Dict) -> io::Result<Option<Dict>> {
        match int(stream.get("type")) {
            Some(MAIN_SCREEN) => self.setup_screen(stream),
            Some(kind @ (MAIN_AUDIO | ALT_AUDIO | MAIN_HIGH_AUDIO)) => {
                self.setup_audio(kind as u16, stream)
            }
            Some(DATA_STREAM) => self.setup_tunnel(stream),
            other => {
                self.log(&format!("stream type {other:?} not set up"));
                Ok(None)
            }
        }
    }

    fn setup_screen(&self, stream: &Dict) -> io::Result<Option<Dict>> {
        let Some(key) = stream
            .get("streamConnectionID")
            .and_then(|id| self.stream_key(id, "DataStream-Output-Encryption-Key"))
        else {
            return Ok(None);
        };
        let connection = Connection::new(self.setup.sockets.listen(self.peer)?);
        let port = connection.listener.port();
        let (listener, stop, slot) = (
            connection.listener.clone(),
            connection.stop.clone(),
            connection.stream.clone(),
        );
        if let Some(old) = lock(&self.state).screen.replace(connection) {
            old.close();
        }
        self.host.screen(self, true);
        let me = self.me.clone();
        spawn("airplay-screen", move || {
            let Some(stream) = accept(listener.as_ref(), &stop, &me) else {
                return;
            };
            *lock(&slot) = Some(stream.clone());
            let stopped = || stop.load(Ordering::Acquire);
            let result = {
                let Some(session) = me.upgrade() else { return };
                screen::run(
                    stream.as_ref(),
                    key,
                    &|| stopped() || session.is_ended(),
                    &mut |message| session.host.video(&session, message),
                )
            };
            if stopped() {
                return;
            }
            if let Some(session) = me.upgrade() {
                session.finish(match result {
                    Ok(()) => Ending::Closed,
                    Err(error) => ending("screen", &error),
                });
            }
        });
        Ok(Some(
            Dict::new().with("type", MAIN_SCREEN).with("dataPort", port),
        ))
    }

    fn setup_audio(&self, kind: u16, stream: &Dict) -> io::Result<Option<Dict>> {
        let audio_type = stream
            .get("audioType")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_ascii_lowercase();
        let id = AudioStreamId { kind, audio_type };
        let previous = lock(&self.state).audio.remove(&id);
        if let Some(previous) = previous {
            self.stop_audio(&id, previous);
        }
        let connection_id = stream
            .get("streamConnectionID")
            .cloned()
            .unwrap_or(Value::Integer(0));
        let Some(key) = self.stream_key(&connection_id, "DataStream-Output-Encryption-Key") else {
            return Ok(None);
        };
        let bits = stream
            .get("audioFormat")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let format = AudioFormat::from_bits(bits);
        let data = self
            .setup
            .sockets
            .datagram(self.peer, Some(AUDIO_RECEIVE_BUFFER))?;
        let control = self.setup.sockets.datagram(self.peer, None)?;
        let microphone = self.microphone(&id, stream, &connection_id, bits)?;
        let entry = AudioEntry {
            format,
            data: data.clone(),
            control: control.clone(),
            stop: Arc::new(AtomicBool::new(false)),
            connection_id: connection_id.clone(),
            latency_ms: int(stream.get("audioLatencyMs")).unwrap_or(0),
            started: Arc::new(Mutex::new(None)),
            microphone: microphone.clone(),
        };
        let (stop, started) = (entry.stop.clone(), entry.started.clone());
        let (data_port, control_port) = (data.port(), control.port());
        lock(&self.state).audio.insert(id.clone(), entry);
        self.log(&format!(
            "audio {} on stream {kind}: {:?}, {} Hz, {} channel(s), format 0x{bits:x}",
            id.audio_type, format.codec, format.sample_rate, format.channels
        ));

        let me = self.me.clone();
        let (peer, stream_id) = (self.peer, id.clone());
        let receive_stop = stop.clone();
        let receive_microphone = microphone.clone();
        spawn("airplay-audio", move || {
            let mut buffer = vec![0; 4096];
            while !receive_stop.load(Ordering::Acquire) {
                let Ok(received) = data.receive_from(&mut buffer, POLL) else {
                    break;
                };
                let Some((count, from)) = received else {
                    continue;
                };
                let Some(session) = me.upgrade() else { break };
                if session.is_ended() || !same_host(from.ip(), peer) {
                    continue;
                }
                let Ok((rtp, timestamp)) = open_packet(&key, &buffer[..count]) else {
                    continue;
                };
                let first = {
                    let mut started = lock(&started);
                    let first = started.is_none();
                    if first {
                        *started = Some((timestamp, Instant::now()));
                    }
                    first
                };
                if first {
                    session.host.audio_started(&session, &stream_id, &format);
                    if let Some(microphone) = &receive_microphone {
                        session.start_microphone(&stream_id, microphone);
                    }
                }
                session
                    .host
                    .audio(&session, &stream_id, &format, &rtp, timestamp);
            }
        });
        let drain_stop = stop;
        spawn("airplay-rtcp", move || {
            let mut buffer = [0; 2048];
            while !drain_stop.load(Ordering::Acquire)
                && control.receive_from(&mut buffer, POLL).is_ok()
            {}
        });
        if let Some(microphone) = microphone.filter(|_| id.audio_type != "telephony") {
            let me = self.me.clone();
            spawn("airplay-microphone-start", move || {
                std::thread::sleep(Duration::from_millis(SILENT_START_MS));
                if let Some(session) = me.upgrade() {
                    let current = lock(&session.state)
                        .audio
                        .get(&id)
                        .and_then(|entry| entry.microphone.clone())
                        .is_some_and(|current| Arc::ptr_eq(&current, &microphone));
                    if current {
                        session.start_microphone(&id, &microphone);
                    }
                }
            });
        }
        Ok(Some(
            Dict::new()
                .with("type", i64::from(kind))
                .with("dataPort", data_port)
                .with("controlPort", control_port)
                .with("streamConnectionID", connection_id),
        ))
    }

    /// The uplink of an audio stream that asks for the microphone.
    fn microphone(
        &self,
        id: &AudioStreamId,
        stream: &Dict,
        connection_id: &Value,
        bits: u64,
    ) -> io::Result<Option<Arc<Microphone>>> {
        if !self.setup.config.microphone
            || i64::from(id.kind) != MAIN_AUDIO
            || !wants_microphone(&id.audio_type, stream)
        {
            return Ok(None);
        }
        let Some(port) = int(stream.get("dataPort")).filter(|p| (1..=0xffff).contains(p)) else {
            return Ok(None);
        };
        let Some(key) = self.stream_key(connection_id, "DataStream-Input-Encryption-Key") else {
            return Ok(None);
        };
        let frames = int(stream.get("framesPerPacket")).and_then(|f| u32::try_from(f).ok());
        Ok(Some(Arc::new(Microphone {
            format: MicrophoneFormat::new(bits, frames),
            destination: SocketAddr::new(self.peer, port as u16),
            socket: self.setup.sockets.datagram(self.peer, None)?,
            packetizer: Mutex::new(Packetizer::new(key, MAIN_AUDIO as u8)),
            started: AtomicBool::new(false),
        })))
    }

    fn start_microphone(&self, id: &AudioStreamId, microphone: &Microphone) {
        if !microphone.started.swap(true, Ordering::AcqRel) {
            self.host.microphone_started(self, id, &microphone.format);
        }
    }

    /// Sends one packet of microphone audio for `stream`: PCM as captured, little-endian, or one Opus packet.
    /// False when the stream has no microphone running.
    pub fn send_microphone(&self, stream: &AudioStreamId, payload: &[u8]) -> io::Result<bool> {
        let microphone = lock(&self.state)
            .audio
            .get(stream)
            .and_then(|entry| entry.microphone.clone());
        let Some(microphone) = microphone.filter(|m| m.started.load(Ordering::Acquire)) else {
            return Ok(false);
        };
        let body = match microphone.format.codec {
            crate::audio::AudioCodec::Pcm => crate::microphone::to_wire_pcm(payload),
            _ => payload.to_vec(),
        };
        let packet =
            lock(&microphone.packetizer).seal(&body, microphone.format.rtp_samples_per_packet());
        microphone.socket.send_to(&packet, microphone.destination)?;
        Ok(true)
    }

    fn stop_audio(&self, id: &AudioStreamId, entry: AudioEntry) {
        entry.stop.store(true, Ordering::Release);
        entry.data.close();
        entry.control.close();
        if let Some(microphone) = &entry.microphone {
            microphone.socket.close();
            if microphone.started.load(Ordering::Acquire) {
                self.host.microphone_stopped(self, id);
            }
        }
        if lock(&entry.started).is_some() {
            self.host.audio_stopped(self, id);
        }
    }

    fn setup_tunnel(&self, stream: &Dict) -> io::Result<Option<Dict>> {
        let uuid = text(stream.get("clientTypeUUID")).to_ascii_uppercase();
        if uuid != IAP2_DATA_STREAM {
            self.log(&format!("data stream {uuid} not set up"));
            return Ok(None);
        }
        let Some(key) = stream
            .get("seed")
            .and_then(|seed| self.stream_key(seed, "DataStream-Output-Encryption-Key"))
        else {
            return Ok(None);
        };
        let connection = Connection::new(self.setup.sockets.listen(self.peer)?);
        let port = connection.listener.port();
        let me = self.me.clone();
        let tunnel = Tunnel::new(move |bytes| match me.upgrade() {
            Some(session) if session.send_iap2(bytes) => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the event channel did not take the iAP2 message",
            )),
        });
        let (listener, stop, slot) = (
            connection.listener.clone(),
            connection.stop.clone(),
            connection.stream.clone(),
        );
        {
            let mut state = lock(&self.state);
            if let Some((old, old_tunnel)) = state.tunnel.replace((connection, tunnel.clone())) {
                old.close();
                old_tunnel.shutdown();
            }
            state.new_tunnels.push(tunnel.clone());
        }
        let me = self.me.clone();
        spawn("airplay-iap2-tunnel", move || {
            let Some(stream) = accept(listener.as_ref(), &stop, &me) else {
                return;
            };
            *lock(&slot) = Some(stream.clone());
            tunnel.opened();
            let mut unpacker = Unpacker::new(key);
            let mut buffer = vec![0; READ_BYTES];
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let failure = match stream.receive(&mut buffer, POLL) {
                    Ok(Received::Bytes(count)) => match unpacker.push(&buffer[..count]) {
                        Ok(iap2) => {
                            tunnel.received(&iap2);
                            continue;
                        }
                        Err(error) => error,
                    },
                    Ok(Received::Nothing) => continue,
                    Ok(Received::End) => {
                        tunnel.ended(None);
                        return;
                    }
                    Err(error) => error,
                };
                tunnel.ended(Some(&failure));
                if let Some(session) = me.upgrade() {
                    session.finish(ending("iAP2 tunnel", &failure));
                }
                return;
            }
        });
        let mut response = Dict::new()
            .with("type", DATA_STREAM)
            .with("streamID", 1)
            .with("dataPort", port);
        if let Some(id) = stream.get("streamConnectionID") {
            response.insert("streamConnectionID", id.clone());
        }
        Ok(Some(response))
    }

    fn teardown(&self, body: &[u8]) -> Response {
        let body = bplist::decode(body).ok();
        let types: Option<Vec<i64>> = body
            .as_ref()
            .and_then(|b| b.get("streams"))
            .and_then(Value::as_array)
            .map(|streams| {
                streams
                    .iter()
                    .filter_map(|s| {
                        let kind = int(s.get("type"))?;
                        // Only the iAP2 tunnel is stream 1 of its type.
                        let other_data =
                            kind == DATA_STREAM && int(s.get("streamID")).is_some_and(|id| id != 1);
                        (!other_data).then_some(kind)
                    })
                    .collect()
            })
            .filter(|types: &Vec<i64>| !types.is_empty());
        let wants = |kind: i64| types.as_ref().is_none_or(|types| types.contains(&kind));
        let (screen, audio, tunnel) = {
            let mut state = lock(&self.state);
            let screen = if wants(MAIN_SCREEN) {
                state.screen.take()
            } else {
                None
            };
            let ids: Vec<AudioStreamId> = state
                .audio
                .keys()
                .filter(|id| wants(i64::from(id.kind)))
                .cloned()
                .collect();
            let audio: Vec<(AudioStreamId, AudioEntry)> = ids
                .into_iter()
                .filter_map(|id| state.audio.remove_entry(&id))
                .collect();
            let tunnel = if wants(DATA_STREAM) {
                state.tunnel.take()
            } else {
                None
            };
            (screen, audio, tunnel)
        };
        if let Some(screen) = screen {
            screen.close();
            self.host.screen(self, false);
        }
        for (id, entry) in audio {
            self.stop_audio(&id, entry);
        }
        if let Some((connection, tunnel)) = tunnel {
            connection.close();
            tunnel.shutdown();
        }
        Response::ok()
    }

    fn feedback(&self) -> Option<Value> {
        let state = lock(&self.state);
        if state.audio.is_empty() {
            return None;
        }
        let now = state.clock.as_ref().map_or(0, |clock| clock.now());
        let streams: Vec<Value> = state
            .audio
            .iter()
            .map(|(id, entry)| {
                let clock_rate = entry.format.rtp_clock_rate;
                let mut stream = Dict::new()
                    .with("type", i64::from(id.kind))
                    .with("sampleRate", clock_rate);
                if let Some((first, at)) = *lock(&entry.started) {
                    let elapsed =
                        (at.elapsed().as_secs_f64() - entry.latency_ms as f64 / 1000.0).max(0.0);
                    let sample =
                        first.wrapping_add((elapsed * f64::from(clock_rate)).round() as u32);
                    stream.insert("streamConnectionID", entry.connection_id.clone());
                    stream.insert("timestamp", now);
                    stream.insert("timestampRawNs", monotonic_nanos());
                    stream.insert("sampleTime", sample);
                }
                Value::Dict(stream)
            })
            .collect();
        Some(Value::Dict(Dict::new().with("streams", streams)))
    }

    fn run_events(&self, listener: Arc<dyn Listener>) {
        let stop = AtomicBool::new(false);
        let Some(stream) = accept(listener.as_ref(), &stop, &self.me.clone()) else {
            return;
        };
        let Some(shared) = lock(&self.state).shared_secret else {
            stream.shutdown();
            return self.finish(Ending::Failed("event channel before pair-verify".into()));
        };
        let key = |info: &str| crypto::hkdf_sha512(&shared, b"Events-Salt", info.as_bytes());
        let mut opener = Opener::new(key("Events-Read-Encryption-Key"));
        {
            let mut events = lock(&self.events);
            events.stream = Some(stream.clone());
            events.sealer = Some(Sealer::new(key("Events-Write-Encryption-Key")));
            if let Some(night) = events.night_mode {
                if Session::send_locked(&mut events, &night_mode(night)).is_ok() {
                    events.night_mode = None;
                }
            }
        }
        let mut buffer = vec![0; READ_BYTES];
        let (mut sealed, mut plain) = (Vec::new(), Vec::new());
        while !self.is_ended() {
            let result = match stream.receive(&mut buffer, POLL) {
                Ok(Received::Bytes(count)) => {
                    sealed.extend_from_slice(&buffer[..count]);
                    opener
                        .open(&mut sealed)
                        .map_err(io::Error::from)
                        .and_then(|opened| {
                            plain.extend_from_slice(&opened);
                            rtsp::take_requests(&mut plain).map_err(io::Error::from)
                        })
                        .and_then(|requests| self.answer_events(&requests))
                }
                Ok(Received::Nothing) => Ok(()),
                Ok(Received::End) => return self.finish(Ending::Closed),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                return self.finish(ending("event", &error));
            }
        }
    }

    /// Answers the iPhone's requests on the event channel; its responses to the receiver's commands are skipped.
    fn answer_events(&self, requests: &[Request]) -> io::Result<()> {
        for request in requests {
            if request.method.starts_with("RTSP/") || request.method.starts_with("HTTP/") {
                continue;
            }
            let wire = Response::ok().encode(request);
            let mut events = lock(&self.events);
            let (Some(stream), Some(sealer)) = (events.stream.clone(), events.sealer.as_mut())
            else {
                return Ok(());
            };
            stream.send(&sealer.seal(&wire))?;
        }
        Ok(())
    }

    fn send_locked(events: &mut Events, command: &Value) -> io::Result<()> {
        let (Some(stream), Some(sealer)) = (events.stream.clone(), events.sealer.as_mut()) else {
            return Err(io::ErrorKind::NotConnected.into());
        };
        events.sequence += 1;
        let body = bplist::encode(command);
        let mut wire = format!(
            "POST /command RTSP/1.0\r\nContent-Type: {PLIST}\r\nContent-Length: {}\r\nCSeq: {}\r\n\r\n",
            body.len(),
            events.sequence
        )
        .into_bytes();
        wire.extend_from_slice(&body);
        stream.send(&sealer.seal(&wire))
    }

    /// Sends a command on the event channel; false when the channel is not up, or the send failed, which ends
    /// the session.
    pub fn send_command(&self, command: &Value) -> bool {
        let result = Session::send_locked(&mut lock(&self.events), command);
        match result {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::NotConnected => false,
            Err(error) => {
                self.finish(ending("event", &error));
                false
            }
        }
    }

    fn send_iap2(&self, bytes: &[u8]) -> bool {
        let command = Value::Dict(
            Dict::new()
                .with("type", "iAPSendMessage")
                .with("params", Dict::new().with("data", bytes)),
        );
        let deadline = Instant::now() + EVENT_READY_TIMEOUT;
        loop {
            if self.send_command(&command) {
                return true;
            }
            if self.is_ended() || Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn hid(&self, device: u32, report: Vec<u8>) -> bool {
        self.send_command(&Value::Dict(
            Dict::new()
                .with("type", "hidSendReport")
                .with("uuid", format!("{device:x}"))
                .with("hidReport", report),
        ))
    }

    /// Reports the fingers on the screen; an empty list releases every finger.
    pub fn touch(&self, contacts: &[Contact]) -> bool {
        let display = &self.setup.config.display;
        self.hid(
            hid::TOUCH,
            hid::touch_report(contacts, display.width, display.height),
        )
    }

    /// Reports the knob; a momentary one is released again at once.
    pub fn knob(&self, knob: &Knob, momentary: bool) -> bool {
        let sent = self.hid(hid::KNOB, hid::knob_report(knob));
        if momentary {
            self.hid(hid::KNOB, hid::knob_report(&Knob::default()));
        }
        sent
    }

    /// Presses and releases a media key, one of [`hid::media`].
    pub fn media_key(&self, key: u8) -> bool {
        let sent = self.hid(hid::MEDIA, hid::key_report(key));
        self.hid(hid::MEDIA, hid::key_report(0));
        sent
    }

    /// Presses and releases a telephony key, one of [`hid::telephony`].
    pub fn telephony_key(&self, key: u8) -> bool {
        let sent = self.hid(hid::TELEPHONY, hid::key_report(key));
        self.hid(hid::TELEPHONY, hid::key_report(0));
        sent
    }

    /// Asks for Siri, as a press and release of its button.
    pub fn siri(&self) -> bool {
        let action = |action: i64| {
            Value::Dict(
                Dict::new()
                    .with("type", "requestSiri")
                    .with("params", Dict::new().with("siriAction", action)),
            )
        };
        self.send_command(&action(2)) && self.send_command(&action(3))
    }

    /// Asks the iPhone for a key frame of the main screen.
    pub fn request_key_frame(&self) -> bool {
        self.send_command(&Value::Dict(Dict::new().with("type", "forceKeyFrame")))
    }

    /// Sets CarPlay to night (true) or day: sent now when the event channel is up, else as soon as it is. True
    /// when it was sent now.
    pub fn set_night_mode(&self, night: bool) -> bool {
        let mut events = lock(&self.events);
        events.night_mode = Some(night);
        if Session::send_locked(&mut events, &night_mode(night)).is_ok() {
            events.night_mode = None;
            true
        } else {
            false
        }
    }
}

fn night_mode(night: bool) -> Value {
    Value::Dict(
        Dict::new()
            .with("type", "setNightMode")
            .with("params", Dict::new().with("nightMode", night)),
    )
}

/// Waits for the listener's connection while the session runs and the connection is wanted.
fn accept(
    listener: &dyn Listener,
    stop: &AtomicBool,
    session: &Weak<Session>,
) -> Option<Arc<dyn Stream>> {
    loop {
        if stop.load(Ordering::Acquire) || session.upgrade().is_none_or(|s| s.is_ended()) {
            return None;
        }
        match listener.accept(POLL) {
            Ok(Some(stream)) => return Some(stream),
            Ok(None) => continue,
            Err(error) => {
                if let Some(session) = session.upgrade() {
                    session.log(&format!("accept failed: {error}"));
                }
                return None;
            }
        }
    }
}

/// Whether an audio stream records: calls and Siri always; app audio when the iPhone asks for input, by an
/// `input` flag or, without one, by an uplink port.
pub fn wants_microphone(audio_type: &str, stream: &Dict) -> bool {
    match audio_type {
        "telephony" | "speechrecognition" => true,
        "default" | "compatibility" => match stream.get("input") {
            Some(Value::Bool(input)) => *input,
            Some(Value::Integer(input)) => *input != 0,
            _ => int(stream.get("dataPort")).is_some_and(|p| (1..=0xffff).contains(&p)),
        },
        _ => false,
    }
}

/// Nanoseconds on a monotonic clock.
fn monotonic_nanos() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests;
