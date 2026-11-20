// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Written by CarBridge contributors on 2026-11-20 for the rewrite of DiPlay's airplay/AirPlaySession.kt (DiPlay commit
// 615b32c).

//! A session driven by a scripted iPhone over loopback.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};

use ed25519_dalek::{Signer, SigningKey};

use super::*;
use crate::identity::MemoryPairings;
use crate::info::Display;
use crate::net::StdSockets;
use crate::pairing::SETUP_CODE;
use crate::srp::tests::{client, Form, CLIENT_PRIVATE};
use crate::tlv8::{kind, Items, Writer};

const WAIT: Duration = Duration::from_secs(5);
const CONTROLLER: &str = "11111111-2222-4333-8444-555555555555";

struct Fake;

impl Authenticator for Fake {
    fn certificate(&self) -> io::Result<Vec<u8>> {
        Ok(b"certificate".to_vec())
    }

    fn sign(&self, digest: &[u8]) -> io::Result<Vec<u8>> {
        Ok(digest.to_vec())
    }
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<String>>,
    video: Mutex<Vec<ScreenMessage>>,
    tunnel: Mutex<Option<Arc<dyn Stream>>>,
}

impl Recorder {
    fn push(&self, event: String) {
        lock(&self.events).push(event);
    }

    fn has(&self, event: &str) -> bool {
        lock(&self.events).iter().any(|e| e == event)
    }

    fn wait_for(&self, event: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.has(event) {
            assert!(
                Instant::now() < deadline,
                "no {event:?} in {:?}",
                lock(&self.events)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Host for Recorder {
    fn active(&self, _: &Arc<Session>) {
        self.push("active".into());
    }

    fn ended(&self, _: &Arc<Session>, ending: &Ending) {
        self.push(format!("ended {ending:?}"));
    }

    fn pairing_refused(&self, peer: IpAddr) {
        self.push(format!("refused {peer}"));
    }

    fn device_info(&self, _: &Session, info: &DeviceInfo) {
        self.push(format!("device {}", info.name));
    }

    fn command(&self, _: &Session, kind: &str, _: &Dict) {
        self.push(format!("command {kind}"));
    }

    fn screen(&self, _: &Session, active: bool) {
        self.push(format!("screen {active}"));
    }

    fn video(&self, _: &Session, message: ScreenMessage) {
        lock(&self.video).push(message);
    }

    fn audio_started(&self, _: &Session, stream: &AudioStreamId, format: &AudioFormat) {
        self.push(format!(
            "audio started {} {:?}",
            stream.audio_type, format.codec
        ));
    }

    fn audio(&self, _: &Session, _: &AudioStreamId, _: &AudioFormat, rtp: &[u8], timestamp: u32) {
        self.push(format!("audio {timestamp} {:?}", &rtp[12..]));
    }

    fn audio_stopped(&self, _: &Session, stream: &AudioStreamId) {
        self.push(format!("audio stopped {}", stream.audio_type));
    }

    fn microphone_started(&self, _: &Session, stream: &AudioStreamId, format: &MicrophoneFormat) {
        self.push(format!(
            "microphone {} {:?} {}",
            stream.audio_type, format.codec, format.sample_rate
        ));
    }

    fn iap2_tunnel(&self, _: &Arc<Session>, tunnel: Arc<dyn Stream>) {
        *lock(&self.tunnel) = Some(tunnel);
        self.push("tunnel".into());
    }
}

/// The iPhone's end of the control connection.
struct Phone {
    control: TcpStream,
    cipher: Option<(Sealer, Opener)>,
    received: Vec<u8>,
    sequence: u32,
    shared: Option<[u8; 32]>,
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

impl Phone {
    fn read_some(&mut self) {
        let mut buffer = [0; 16 * 1024];
        let count = self.control.read(&mut buffer).unwrap();
        assert!(count > 0, "the receiver closed the control connection");
        match &mut self.cipher {
            Some((_, opener)) => {
                let mut sealed = buffer[..count].to_vec();
                // Frames may split across reads; keep what is not whole for the next read.
                let opened = opener.open(&mut sealed).unwrap();
                self.received.extend_from_slice(&opened);
                assert!(sealed.is_empty(), "partial frame");
            }
            None => self.received.extend_from_slice(&buffer[..count]),
        }
    }

    fn request(&mut self, method: &str, path: &str, body: &[u8]) -> Reply {
        self.sequence += 1;
        let mut wire = format!(
            "{method} {path} RTSP/1.0\r\nCSeq: {}\r\nContent-Length: {}\r\n\r\n",
            self.sequence,
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(body);
        let wire = match &mut self.cipher {
            Some((sealer, _)) => sealer.seal(&wire),
            None => wire,
        };
        self.control.write_all(&wire).unwrap();
        loop {
            if let Some(end) = self.received.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.received[..end]).into_owned();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .map_or(0, |n| n.trim().parse().unwrap());
                if self.received.len() >= end + 4 + length {
                    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
                    assert!(head.contains(&format!("CSeq: {}", self.sequence)));
                    let body = self.received[end + 4..end + 4 + length].to_vec();
                    self.received.drain(..end + 4 + length);
                    return Reply { status, body };
                }
            }
            self.read_some();
        }
    }

    fn plist(&mut self, method: &str, path: &str, value: &Value) -> Value {
        let reply = self.request(method, path, &bplist::encode(value));
        assert_eq!(reply.status, 200);
        bplist::decode(&reply.body).unwrap()
    }

    fn pair_setup(&mut self, key: &SigningKey, code: &str) -> Items {
        let m2 = Items::parse(
            &self
                .request(
                    "POST",
                    "/pair-setup",
                    &Writer::new().byte(kind::STATE, 1).into_bytes(),
                )
                .body,
        );
        let srp = client(
            m2.get(kind::SALT).unwrap(),
            m2.get(kind::PUBLIC_KEY).unwrap(),
            code,
            crate::crypto::from_hex(CLIENT_PRIVATE).try_into().unwrap(),
            Form::Padded,
            Form::Padded,
            Form::Padded,
        );
        let m3 = Writer::new()
            .byte(kind::STATE, 3)
            .item(kind::PUBLIC_KEY, &srp.public)
            .item(kind::PROOF, &srp.proof)
            .into_bytes();
        let m4 = Items::parse(&self.request("POST", "/pair-setup", &m3).body);
        if m4.byte(kind::ERROR).is_some() {
            return m4;
        }
        let encrypt = crypto::hkdf_sha512(
            &srp.session_key,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
        );
        let sign = crypto::hkdf_sha512(
            &srp.session_key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
        );
        let public = key.verifying_key().to_bytes();
        let signature = key
            .sign(&[&sign[..], CONTROLLER.as_bytes(), &public].concat())
            .to_bytes();
        let sub = Writer::new()
            .item(kind::IDENTIFIER, CONTROLLER.as_bytes())
            .item(kind::PUBLIC_KEY, &public)
            .item(kind::SIGNATURE, &signature)
            .into_bytes();
        let m5 = Writer::new()
            .byte(kind::STATE, 5)
            .item(
                kind::ENCRYPTED_DATA,
                &crypto::seal(&encrypt, &crypto::label_nonce("PS-Msg05"), &[], &sub),
            )
            .into_bytes();
        Items::parse(&self.request("POST", "/pair-setup", &m5).body)
    }

    fn pair_verify(&mut self, key: &SigningKey) {
        let ephemeral = crypto::X25519::generate();
        let m1 = Writer::new()
            .byte(kind::STATE, 1)
            .item(kind::PUBLIC_KEY, &ephemeral.public_key())
            .into_bytes();
        let m2 = Items::parse(&self.request("POST", "/pair-verify", &m1).body);
        let receiver = m2.get(kind::PUBLIC_KEY).unwrap().to_vec();
        let shared = ephemeral.shared_secret(&receiver).unwrap();
        let encrypt = crypto::hkdf_sha512(
            &shared,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
        );
        let signature = key
            .sign(
                &[
                    &ephemeral.public_key()[..],
                    CONTROLLER.as_bytes(),
                    &receiver,
                ]
                .concat(),
            )
            .to_bytes();
        let sub = Writer::new()
            .item(kind::IDENTIFIER, CONTROLLER.as_bytes())
            .item(kind::SIGNATURE, &signature)
            .into_bytes();
        let m3 = Writer::new()
            .byte(kind::STATE, 3)
            .item(
                kind::ENCRYPTED_DATA,
                &crypto::seal(&encrypt, &crypto::label_nonce("PV-Msg03"), &[], &sub),
            )
            .into_bytes();
        let m4 = Items::parse(&self.request("POST", "/pair-verify", &m3).body);
        assert_eq!(m4.byte(kind::STATE), Some(4));
        assert_eq!(m4.byte(kind::ERROR), None);
        let write = crypto::hkdf_sha512(&shared, b"Control-Salt", b"Control-Write-Encryption-Key");
        let read = crypto::hkdf_sha512(&shared, b"Control-Salt", b"Control-Read-Encryption-Key");
        self.cipher = Some((Sealer::new(write), Opener::new(read)));
        self.shared = Some(shared);
    }

    fn stream_key(&self, salt: u64) -> [u8; 32] {
        crypto::hkdf_sha512(
            &self.shared.unwrap(),
            format!("DataStream-Salt{salt}").as_bytes(),
            b"DataStream-Output-Encryption-Key",
        )
    }
}

fn start() -> (Arc<Session>, Phone, Arc<Recorder>) {
    start_with(|_| {})
}

fn start_with(adjust: impl FnOnce(&mut Config)) -> (Arc<Session>, Phone, Arc<Recorder>) {
    let listener = StdSockets.listen(IpAddr::from([127, 0, 0, 1])).unwrap();
    let control = TcpStream::connect(("127.0.0.1", listener.port())).unwrap();
    control.set_read_timeout(Some(WAIT)).unwrap();
    let accepted = listener.accept(WAIT).unwrap().unwrap();
    let recorder = Arc::new(Recorder::default());
    let mut config = Config::new(
        "Car",
        "02:00:00:00:00:02",
        "02:00:00:00:00:03",
        "366.0",
        Display::new(800, 480),
    );
    adjust(&mut config);
    let setup = Setup {
        config: Arc::new(config),
        identity: Arc::new(Identity::generate()),
        pairings: Arc::new(MemoryPairings::new()),
        authenticator: Arc::new(Fake),
        sockets: Arc::new(StdSockets),
    };
    let session = Session::start(
        accepted,
        IpAddr::from([127, 0, 0, 1]),
        setup,
        recorder.clone(),
    );
    let phone = Phone {
        control,
        cipher: None,
        received: Vec::new(),
        sequence: 0,
        shared: None,
    };
    (session, phone, recorder)
}

/// Reads one command the receiver sends on the event channel, and gives its plist.
fn read_command(events: &mut TcpStream, opener: &mut Opener) -> Value {
    let (mut sealed, mut plain) = (Vec::new(), Vec::new());
    loop {
        let mut buffer = [0; 4096];
        let count = events.read(&mut buffer).unwrap();
        assert!(count > 0, "the event channel closed");
        sealed.extend_from_slice(&buffer[..count]);
        plain.extend(opener.open(&mut sealed).unwrap());
        let mut requests = rtsp::take_requests(&mut plain).unwrap();
        if let Some(request) = requests.pop() {
            assert_eq!(
                (request.method.as_str(), request.path.as_str()),
                ("POST", "/command")
            );
            return bplist::decode(&request.body).unwrap();
        }
    }
}

fn port(value: &Value, key: &str) -> u16 {
    value.get(key).unwrap().as_i64().unwrap() as u16
}

#[test]
fn a_whole_session() {
    let (session, mut phone, recorder) = start();
    let key = SigningKey::from_bytes(&[8; 32]);
    let m6 = phone.pair_setup(&key, SETUP_CODE);
    assert_eq!(m6.byte(kind::STATE), Some(6));
    phone.pair_verify(&key);

    let info = phone.plist("GET", "/info", &Value::Dict(Dict::new()));
    assert_eq!(info.get("name").unwrap().as_str(), Some("Car"));

    let setup = phone.plist(
        "SETUP",
        "/session",
        &Value::Dict(Dict::new().with("name", "iPhone").with("deviceID", "AA:BB")),
    );
    recorder.wait_for("device iPhone");
    assert_eq!(
        setup.get("enabledFeatures").unwrap().as_array().unwrap(),
        [Value::from("iAPChannel"), Value::from("viewAreas")]
    );
    let mut events = TcpStream::connect(("127.0.0.1", port(&setup, "eventPort"))).unwrap();
    events.set_read_timeout(Some(WAIT)).unwrap();
    let shared = phone.shared.unwrap();
    let mut event_opener = Opener::new(crypto::hkdf_sha512(
        &shared,
        b"Events-Salt",
        b"Events-Write-Encryption-Key",
    ));
    let deadline = Instant::now() + WAIT;
    while !session.set_night_mode(true) {
        assert!(
            Instant::now() < deadline,
            "the event channel did not come up"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let night = read_command(&mut events, &mut event_opener);
    assert_eq!(night.get("type").unwrap().as_str(), Some("setNightMode"));

    let streams = phone.plist(
        "SETUP",
        "/session",
        &Value::Dict(Dict::new().with(
            "streams",
            vec![
                Value::Dict(Dict::new().with("type", 110).with("streamConnectionID", 12345)),
                Value::Dict(
                    Dict::new()
                        .with("type", 100)
                        .with("audioType", "media")
                        .with("audioFormat", 0x8000)
                        .with("streamConnectionID", u64::MAX - 1),
                ),
            ],
        )),
    );
    let streams = streams.get("streams").unwrap().as_array().unwrap().to_vec();
    assert_eq!(streams.len(), 2);
    recorder.wait_for("screen true");
    assert_eq!(
        streams[1].get("streamConnectionID").unwrap().as_u64(),
        Some(u64::MAX - 1)
    );

    // The screen: a configuration, then a sealed frame.
    let mut screen = TcpStream::connect(("127.0.0.1", port(&streams[0], "dataPort"))).unwrap();
    let mut header = [0u8; 128];
    header[4] = 1;
    let record = b"\x01\x64\x00\x1f\xff\xe1\x00\x01\x67";
    header[..4].copy_from_slice(&(record.len() as u32).to_le_bytes());
    screen.write_all(&header).unwrap();
    screen.write_all(record).unwrap();
    let mut header = [0u8; 128];
    let frame = [0, 0, 0, 2, 0x65, 0x88];
    header[..4].copy_from_slice(&((frame.len() + 16) as u32).to_le_bytes());
    let sealed_frame = crypto::seal(
        &phone.stream_key(12345),
        &crypto::counter_nonce(0),
        &header,
        &frame,
    );
    screen.write_all(&header).unwrap();
    screen.write_all(&sealed_frame).unwrap();
    let deadline = Instant::now() + WAIT;
    while lock(&recorder.video).len() < 2 {
        assert!(Instant::now() < deadline, "no video");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        lock(&recorder.video).as_slice(),
        [
            ScreenMessage::Config {
                codec: screen::VideoCodec::H264,
                record: record.to_vec()
            },
            ScreenMessage::Frame {
                data: vec![0, 0, 0, 1, 0x65, 0x88],
                sender_nanos: 0
            },
        ]
    );

    // Audio: one sealed RTP packet.
    let rtp_header = [0x80, 0x60, 0, 1, 0, 0, 0x10, 0x00, 0, 0, 0, 1];
    let mut nonce = [0; 12];
    nonce[4..].copy_from_slice(&5u64.to_le_bytes());
    let payload = crypto::seal(
        &phone.stream_key(u64::MAX - 1),
        &nonce,
        &rtp_header[4..],
        &[1, 2, 3, 4],
    );
    let packet = [&rtp_header[..], &payload, &5u64.to_le_bytes()].concat();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.send_to(&packet, ("127.0.0.1", port(&streams[1], "dataPort")))
        .unwrap();
    recorder.wait_for("audio started media Pcm");
    recorder.wait_for("audio 4096 [1, 2, 3, 4]");

    let feedback = phone.plist("POST", "/feedback", &Value::Dict(Dict::new()));
    let entry = &feedback.get("streams").unwrap().as_array().unwrap()[0];
    assert_eq!(entry.get("type").unwrap().as_i64(), Some(100));
    assert_eq!(entry.get("sampleRate").unwrap().as_i64(), Some(48_000));
    assert!(entry.get("sampleTime").unwrap().as_i64().unwrap() >= 4096);

    phone.request(
        "POST",
        "/command",
        &bplist::encode(&Value::Dict(Dict::new().with("type", "requestUI"))),
    );
    recorder.wait_for("command requestUI");
    assert_eq!(phone.request("RECORD", "/session", &[]).status, 200);
    recorder.wait_for("active");

    let teardown =
        Value::Dict(Dict::new().with("streams", vec![Value::Dict(Dict::new().with("type", 110))]));
    assert_eq!(
        phone
            .request("TEARDOWN", "/session", &bplist::encode(&teardown))
            .status,
        200
    );
    recorder.wait_for("screen false");
    assert!(!session.is_ended());

    drop(phone);
    recorder.wait_for("ended Closed");
    recorder.wait_for("audio stopped media");
    assert!(session.is_ended());
}

#[test]
fn a_wrong_setup_code_and_a_bad_auth_setup_are_refused() {
    let (_session, mut phone, recorder) = start();
    let m4 = phone.pair_setup(&SigningKey::from_bytes(&[8; 32]), "0000");
    assert_eq!(m4.byte(kind::ERROR), Some(2));
    recorder.wait_for("refused 127.0.0.1");
    lock(&recorder.events).clear();
    assert_eq!(phone.request("POST", "/auth-setup", &[1, 2, 3]).status, 400);
    recorder.wait_for("refused 127.0.0.1");
}

#[test]
fn auth_setup_answers_with_the_certificate() {
    let (_session, mut phone, _recorder) = start();
    let request = [&[1u8][..], &crypto::X25519::generate().public_key()].concat();
    let reply = phone.request("POST", "/auth-setup", &request);
    assert_eq!(reply.status, 200);
    assert_eq!(&reply.body[36..47], b"certificate");
}

#[test]
fn the_host_ends_a_session() {
    let (session, mut phone, recorder) = start();
    session.end();
    recorder.wait_for("ended Ended");
    let mut buffer = [0; 16];
    assert_eq!(phone.control.read(&mut buffer).unwrap_or(0), 0);
}

#[test]
fn which_streams_record() {
    let stream = |entries: Vec<(&str, Value)>| entries.into_iter().collect::<Dict>();
    assert!(wants_microphone("telephony", &Dict::new()));
    assert!(wants_microphone("speechrecognition", &Dict::new()));
    assert!(!wants_microphone(
        "media",
        &stream(vec![("input", true.into())])
    ));
    assert!(wants_microphone(
        "default",
        &stream(vec![("input", 1.into())])
    ));
    assert!(!wants_microphone(
        "default",
        &stream(vec![("input", false.into()), ("dataPort", 5000.into())])
    ));
    assert!(wants_microphone(
        "default",
        &stream(vec![("dataPort", 5000.into())])
    ));
    assert!(!wants_microphone("default", &Dict::new()));
}

/// Pairs, sets the session up and connects the event channel; gives the event socket and its opener.
fn connected(session: &Session, phone: &mut Phone) -> (TcpStream, Opener) {
    let key = SigningKey::from_bytes(&[8; 32]);
    phone.pair_setup(&key, SETUP_CODE);
    phone.pair_verify(&key);
    let setup = phone.plist("SETUP", "/session", &Value::Dict(Dict::new()));
    let events = TcpStream::connect(("127.0.0.1", port(&setup, "eventPort"))).unwrap();
    events.set_read_timeout(Some(WAIT)).unwrap();
    let deadline = Instant::now() + WAIT;
    while !session.request_key_frame() {
        assert!(
            Instant::now() < deadline,
            "the event channel did not come up"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let shared = phone.shared.unwrap();
    let mut opener = Opener::new(crypto::hkdf_sha512(
        &shared,
        b"Events-Salt",
        b"Events-Write-Encryption-Key",
    ));
    let mut events = events;
    let first = read_command(&mut events, &mut opener);
    assert_eq!(first.get("type").unwrap().as_str(), Some("forceKeyFrame"));
    (events, opener)
}

#[test]
fn the_iap2_tunnel_carries_both_directions() {
    let (session, mut phone, recorder) = start();
    let (mut events, mut opener) = connected(&session, &mut phone);
    let seed: u64 = 0x83ac_b4a2_ca36_c664;
    let streams = phone.plist(
        "SETUP",
        "/session",
        &Value::Dict(Dict::new().with(
            "streams",
            vec![Value::Dict(
                Dict::new()
                    .with("type", 130)
                    .with("seed", seed)
                    .with("clientTypeUUID", "E9459FD0-BCAD-4C45-820F-1E72447EF2F2")
                    .with("streamConnectionID", 7),
            )],
        )),
    );
    let stream = &streams.get("streams").unwrap().as_array().unwrap()[0];
    assert_eq!(stream.get("streamID").unwrap().as_i64(), Some(1));
    recorder.wait_for("tunnel");
    let tunnel = lock(&recorder.tunnel).clone().unwrap();

    let mut connection = TcpStream::connect(("127.0.0.1", port(stream, "dataPort"))).unwrap();
    let mut package = vec![0; 32];
    package[..4].copy_from_slice(&36u32.to_be_bytes());
    package[16..20].copy_from_slice(b"comm");
    package.extend_from_slice(&[0xff, 0x55, 0x02, 0x00]);
    connection
        .write_all(&Sealer::new(phone.stream_key(seed)).seal(&package))
        .unwrap();
    let mut buffer = [0; 16];
    let deadline = Instant::now() + WAIT;
    let received = loop {
        if let Received::Bytes(count) = tunnel.receive(&mut buffer, POLL).unwrap() {
            break count;
        }
        assert!(Instant::now() < deadline, "no tunnel bytes");
    };
    assert_eq!(&buffer[..received], [0xff, 0x55, 0x02, 0x00]);

    tunnel.send(b"\xff\x5a").unwrap();
    let message = read_command(&mut events, &mut opener);
    assert_eq!(
        message.get("type").unwrap().as_str(),
        Some("iAPSendMessage")
    );
    assert_eq!(
        message
            .get("params")
            .unwrap()
            .get("data")
            .unwrap()
            .as_data(),
        Some(&b"\xff\x5a"[..])
    );
}

#[test]
fn a_call_records_once_its_downlink_starts() {
    let (session, mut phone, recorder) = start_with(|config| config.microphone = true);
    let _events = connected(&session, &mut phone);
    let uplink = UdpSocket::bind("127.0.0.1:0").unwrap();
    uplink.set_read_timeout(Some(WAIT)).unwrap();
    let streams = phone.plist(
        "SETUP",
        "/session",
        &Value::Dict(Dict::new().with(
            "streams",
            vec![Value::Dict(
                Dict::new()
                    .with("type", 100)
                    .with("audioType", "telephony")
                    .with("audioFormat", 0x4000_0000)
                    .with("dataPort", uplink.local_addr().unwrap().port())
                    .with("streamConnectionID", 99),
            )],
        )),
    );
    let stream = &streams.get("streams").unwrap().as_array().unwrap()[0];
    let id = AudioStreamId {
        kind: 100,
        audio_type: "telephony".into(),
    };
    // A call waits for its downlink before it records.
    std::thread::sleep(Duration::from_millis(SILENT_START_MS + 200));
    assert!(!recorder.has("microphone telephony Opus 48000"));
    assert!(!session.send_microphone(&id, b"early").unwrap());

    let rtp_header = [0x80, 0x64, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
    let payload = crypto::seal(
        &phone.stream_key(99),
        &crypto::counter_nonce(0),
        &rtp_header[4..],
        b"far",
    );
    let packet = [&rtp_header[..], &payload, &0u64.to_le_bytes()].concat();
    UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .send_to(&packet, ("127.0.0.1", port(stream, "dataPort")))
        .unwrap();
    recorder.wait_for("microphone telephony Opus 48000");

    assert!(session.send_microphone(&id, b"opus").unwrap());
    let mut buffer = [0; 256];
    let (count, _) = uplink.recv_from(&mut buffer).unwrap();
    let input = crypto::hkdf_sha512(
        &phone.shared.unwrap(),
        b"DataStream-Salt99",
        b"DataStream-Input-Encryption-Key",
    );
    let (rtp, timestamp) = open_packet(&input, &buffer[..count]).unwrap();
    assert_eq!((&rtp[12..], timestamp), (&b"opus"[..], 0));
}

/// A control connection whose peer stopped answering, as the host's keep-alive reports it.
struct Unanswered;

impl Stream for Unanswered {
    fn send(&self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn receive(&self, _: &mut [u8], _: Duration) -> io::Result<Received> {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "keep-alive unanswered",
        ))
    }

    fn shutdown(&self) {}
}

#[test]
fn an_iphone_that_stops_answering_ends_the_session() {
    let recorder = Arc::new(Recorder::default());
    let setup = Setup {
        config: Arc::new(Config::new(
            "Car",
            "02:00:00:00:00:02",
            "02:00:00:00:00:03",
            "366.0",
            Display::new(800, 480),
        )),
        identity: Arc::new(Identity::generate()),
        pairings: Arc::new(MemoryPairings::new()),
        authenticator: Arc::new(Fake),
        sockets: Arc::new(StdSockets),
    };
    let session = Session::start(
        Arc::new(Unanswered),
        IpAddr::from([127, 0, 0, 1]),
        setup,
        recorder.clone(),
    );
    recorder.wait_for("ended PeerUnreachable { connection: \"control\" }");
    assert!(session.is_ended());
}
