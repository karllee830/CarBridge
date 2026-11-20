// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The AirPlay side of CarPlay: the control listener the iPhone connects to, a session per connection, and the media
//! that comes out of it.

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use carbridge_airplay::audio::{AudioCodec, AudioFormat};
use carbridge_airplay::bplist::{Dict, Value};
use carbridge_airplay::hid::{self, Contact, Knob};
use carbridge_airplay::identity::Identity;
use carbridge_airplay::info::{Config, Display, Icon};
use carbridge_airplay::microphone::MicrophoneFormat;
use carbridge_airplay::net::Listener as _;
use carbridge_airplay::screen::ScreenMessage;
use carbridge_airplay::session::{AudioStreamId, DeviceInfo, Ending, Host, Session, Setup};
use carbridge_iap2::file_transfer::Artwork;
use carbridge_iap2::mfi::{Authenticator, LocalAuthenticator};
use carbridge_iap2::now_playing::NowPlayingUpdate;
use carbridge_iap2::Stream;

use crate::audio::{self, AudioKind};
use crate::input::Key;
use crate::net::{Listener, Sockets, Tcp, PEER_TIMEOUT};
use crate::night::NightMode;
use crate::now_playing::{NowPlaying, NowPlayingState};
use crate::opus::{self, Codecs};
use crate::picture::Picture;
use crate::store::Store;
use crate::tile::CarTile;
use crate::video::{access_unit, unit_kinds, ParameterSets, VideoCodec, VideoFrame};

/// Stream type of the main screen.
const MAIN_SCREEN: u16 = 110;
/// How long one accept waits before it looks whether the receiver stopped.
const ACCEPT_WAIT: Duration = Duration::from_millis(500);
/// How soon a session that could not take the day or night mode yet is told again.
const NIGHT_MODE_RETRY: Duration = Duration::from_millis(250);
/// The AirPlay source version the receiver announces.
const SOURCE_VERSION: &str = "950.7.1";

/// How the receiver presents itself and where it keeps its state.
#[derive(Clone, Debug)]
pub struct ReceiverConfig {
    /// The address the iPhone reaches the receiver at.
    pub local: IpAddr,
    /// The AirPlay control port.
    pub port: u16,
    /// The name the iPhone shows.
    pub name: String,
    /// The AirPlay device ID, an address in `AA:BB:CC:DD:EE:FF` form.
    pub device_id: String,
    /// The Bluetooth address the iPhone paired with.
    pub bluetooth_address: String,
    /// The picture iPhones that connect render, until [`Receiver::set_picture`] changes it, and why it is this one,
    /// for the log.
    pub picture: Picture,
    pub picture_reason: String,
    /// Whether iPhones that connect are offered H.265, until [`Receiver::set_hevc`] changes it.
    pub hevc: bool,
    /// The car tile iPhones that connect are given, until [`Receiver::set_car_tile`] changes it.
    pub tile: CarTile,
    /// Where the AirPlay identity and the iPhone pairings are kept.
    pub state_dir: PathBuf,
    /// The MFi identity: the private key (PKCS #8) and the certificate (PKCS #7).
    pub mfi_key: PathBuf,
    pub mfi_certificate: PathBuf,
}

impl ReceiverConfig {
    fn air_play(&self) -> Config {
        let picture = self.picture.fitted();
        let mut display = Display::new(picture.width, picture.height);
        picture.apply(&mut display);
        let mut config = Config::new(
            &self.name,
            &self.device_id,
            &self.bluetooth_address,
            SOURCE_VERSION,
            display,
        );
        config.manufacturer = "CarBridge".into();
        config.model = "CarBridge".into();
        set_tile(&mut config, &self.tile);
        config.hevc = false;
        // The iPhone sets up CarPlay audio only when Opus is offered beside PCM, for its microphone too. It then sends
        // everything but music in Opus, which is decoded for the head unit, and the head unit's microphone is
        // encoded to Opus when the iPhone asks for that.
        config.microphone = true;
        config.microphone_opus = true;
        config
    }
}

/// Gives the iPhone the car tile in /info: its label and its icon.
fn set_tile(config: &mut Config, tile: &CarTile) {
    config.icon_label = tile.label.clone();
    config.icons = vec![Icon {
        width: tile.icon.side,
        height: tile.icon.side,
        png: tile.icon.png.to_vec(),
    }];
}

/// What happens on the AirPlay side.
#[derive(Debug)]
pub enum ReceiverEvent {
    /// An iPhone's session is up.
    SessionActive {
        peer: Option<IpAddr>,
        /// The codec the session's screen is passed on in: H.265 when the session was offered it, H.264 otherwise.
        codec: VideoCodec,
        /// The picture the session was given, which the iPhone renders.
        picture: Picture,
    },
    SessionEnded,
    /// An iPhone failed AirPlay authentication: pair-setup or MFi auth-setup. Its session gives up next.
    AuthenticationFailed {
        peer: Option<IpAddr>,
    },
    /// The screen stream started or stopped.
    ScreenActive(bool),
    Video(VideoFrame),
    Audio(AudioEvent),
    /// The iPhone runs the session over Wi-Fi and asks the accessory to give up its Bluetooth connections to it
    /// (`disableBluetooth`); `device_id` is its Bluetooth address.
    BluetoothHandoff {
        device_id: String,
    },
    /// The session of the moment took the head unit's day or night mode ([`Receiver::set_night_mode`]): night when
    /// true.
    NightMode(bool),
    /// What plays on the iPhone looks otherwise: another song, artwork, playback status or position; `None` once
    /// nothing is known any more, as when the iPhone's session ended.
    NowPlaying(Option<NowPlaying>),
    /// Something worth logging.
    Log(String),
}

/// One of the iPhone's audio streams: the AirPlay stream type and what it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AudioStream {
    pub stream_type: u16,
    pub kind: AudioKind,
}

/// The iPhone's sound.
#[derive(Clone, Debug)]
pub enum AudioEvent {
    /// A stream starts: AAC (`music`) or 16-bit PCM, which Opus is decoded to.
    Started {
        stream: AudioStream,
        music: bool,
        sample_rate: u32,
        channels: u8,
    },
    /// An AAC frame with its ADTS header.
    Music {
        stream: AudioStream,
        adts: Vec<u8>,
    },
    /// 16-bit little-endian PCM.
    Pcm {
        stream: AudioStream,
        samples: Vec<u8>,
    },
    Stopped {
        stream: AudioStream,
    },
    /// The iPhone wants the car's microphone for a kind of sound: 16-bit PCM at this rate and channel count, given
    /// to [`Receiver::microphone`] in frames of `frame_bytes`.
    MicrophoneStarted {
        stream_type: u16,
        kind: AudioKind,
        sample_rate: u32,
        channels: u8,
        frame_bytes: usize,
    },
    MicrophoneStopped {
        stream_type: u16,
    },
    /// The iPhone's session ended, and with it all its streams and its microphone.
    SessionEnded,
}

type Events = Arc<dyn Fn(ReceiverEvent) + Send + Sync>;

/// What became of a frame of the car's microphone ([`Receiver::microphone`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicrophoneSent {
    /// Encoded to Opus and sent.
    Opus,
    /// Sent as PCM, as the iPhone asked for.
    Pcm,
    /// No microphone stream of the iPhone's is open for it.
    NoStream,
}

/// Why a frame of the car's microphone did not reach the iPhone.
#[derive(Debug)]
pub enum MicrophoneError {
    /// The Opus encoder did not take it.
    Encode,
    /// Sending it failed.
    Send(io::Error),
}

impl std::fmt::Display for MicrophoneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MicrophoneError::Encode => f.write_str("microphone frame not encoded to Opus"),
            MicrophoneError::Send(err) => write!(f, "{err}"),
        }
    }
}

/// What plays on the iPhone, from the now-playing updates and artwork transfers of its iAP2 links, each link known by
/// a number of its own. Every change is told as [`ReceiverEvent::NowPlaying`], in the order it happens; it is
/// forgotten when the iPhone's session ends.
#[derive(Clone)]
pub struct NowPlayingFeed {
    state: Arc<Mutex<NowPlayingState>>,
    events: Events,
}

impl NowPlayingFeed {
    fn new(events: Events) -> NowPlayingFeed {
        NowPlayingFeed {
            state: Arc::default(),
            events,
        }
    }

    /// Takes a now-playing update from link `link`.
    pub fn change(&self, link: u64, change: &NowPlayingUpdate) {
        self.apply(|state| state.change(link, change, Instant::now()));
    }

    /// Takes an artwork transfer that ended on link `link`.
    pub fn transfer(&self, link: u64, transfer: Artwork) {
        self.apply(|state| state.transfer(link, transfer.id, transfer.bytes));
    }

    fn clear(&self) {
        self.apply(NowPlayingState::clear);
    }

    fn apply(&self, change: impl FnOnce(&mut NowPlayingState) -> bool) {
        let mut state = self.state.lock().unwrap();
        if change(&mut state) {
            // Told under the lock, so that changes from different links are told in the order they happened.
            (self.events)(ReceiverEvent::NowPlaying(state.current()));
        }
    }
}

/// What a session was given when the iPhone connected, and keeps until it connects again.
#[derive(Clone, Debug)]
pub struct Given {
    pub tile: Arc<CarTile>,
    pub picture: Picture,
    /// Whether it was offered H.265.
    pub hevc: bool,
}

/// The session of the moment, and the codec its screen is passed on in.
struct Current {
    session: Option<Weak<Session>>,
    /// The session's number, counted from 1 by the receiver.
    id: u64,
    /// H.265 when the session was offered it, H.264 otherwise.
    codec: VideoCodec,
    /// The codec last logged for each of the session's screens.
    logged: Vec<(u16, VideoCodec)>,
    /// The car tile and the picture the session was given.
    given: Given,
}

type Shared = Arc<Mutex<Current>>;

/// The session accepted last, while it may still be starting, and what it was given.
type Latest = Arc<Mutex<Option<(Weak<Session>, Given)>>>;

/// The picture iPhones that connect from now on render, and why it is this one.
type Chosen = Arc<Mutex<(Picture, Arc<str>)>>;

/// The head unit's day or night mode for the sessions, and what wakes the thread that tells them.
#[derive(Default)]
struct Night {
    mode: Mutex<NightMode>,
    changed: Condvar,
}

/// Tells each session the head unit's day or night mode once it can take it, and again whenever the mode changes,
/// until the receiver stops.
fn tell_night_mode(night: Arc<Night>, current: Shared, events: Events, stopped: Arc<AtomicBool>) {
    let mut mode = night.mode.lock().unwrap();
    while !stopped.load(Ordering::Acquire) {
        let Some((id, value)) = mode.due() else {
            mode = night.changed.wait(mode).unwrap();
            continue;
        };
        drop(mode);
        let session = {
            let current = current.lock().unwrap();
            current
                .session
                .as_ref()
                .filter(|_| current.id == id)
                .and_then(Weak::upgrade)
        };
        // The session takes it once its event channel is up.
        let taken = session.is_some_and(|session| session.set_night_mode(value));
        mode = night.mode.lock().unwrap();
        if taken {
            mode.told(id, value);
            drop(mode);
            events(ReceiverEvent::NightMode(value));
            mode = night.mode.lock().unwrap();
        } else {
            mode = night
                .changed
                .wait_timeout(mode, NIGHT_MODE_RETRY)
                .unwrap()
                .0;
        }
    }
}

/// The main screen's parameter sets, and what of them has been logged since its stream started.
#[derive(Default)]
struct VideoState {
    sets: ParameterSets,
    config_logged: bool,
    first_key_logged: bool,
    incomplete_logged: bool,
}

/// Takes the media out of the AirPlay sessions.
struct Bridge {
    events: Events,
    current: Shared,
    /// The main screen's parameter sets, and what of them has been logged for its stream.
    video: Mutex<VideoState>,
    /// The main screen's codec, while it is the one its session's screen is passed on in.
    passed: Mutex<Option<VideoCodec>>,
    codecs: Arc<Codecs>,
}

impl Bridge {
    fn on_video_codec(&self, kind: u16, codec: VideoCodec) -> io::Result<()> {
        let mut current = self.current.lock().unwrap();
        let passed = codec == current.codec;
        if kind == MAIN_SCREEN {
            *self.passed.lock().unwrap() = passed.then_some(codec);
        }
        // The iPhone reports the codec again with every configuration; it is logged when it changes.
        let logged = current.logged.iter_mut().find(|(k, _)| *k == kind);
        let changed = match logged {
            Some((_, last)) if *last == codec => false,
            Some((_, last)) => {
                *last = codec;
                true
            }
            None => {
                current.logged.push((kind, codec));
                true
            }
        };
        drop(current);
        if changed {
            let line = if passed {
                format!("screen {kind} in {}", codec_name(codec))
            } else {
                format!("screen {kind}: {} is not cast", codec_name(codec))
            };
            (self.events)(ReceiverEvent::Log(line));
        }
        Ok(())
    }

    fn on_video_config(&self, kind: u16, codec_data: &[u8]) -> io::Result<()> {
        if kind != MAIN_SCREEN {
            return Ok(());
        }
        let Some(codec) = *self.passed.lock().unwrap() else {
            return Ok(());
        };
        let mut video = self.video.lock().unwrap();
        if !video.sets.config(codec, codec_data) && !video.config_logged {
            video.config_logged = true;
            let head: Vec<String> = codec_data
                .iter()
                .take(64)
                .map(|b| format!("{b:02x}"))
                .collect();
            (self.events)(ReceiverEvent::Log(format!(
                "screen {kind}: no parameter sets in the {} configuration ({} bytes: {}); key pictures take their own",
                codec_name(codec),
                codec_data.len(),
                head.join("")
            )));
        }
        Ok(())
    }

    fn on_video_frame(&self, kind: u16, nalu_bytes: &[u8], sender_nanos: u64) -> io::Result<()> {
        if kind != MAIN_SCREEN {
            return Ok(());
        }
        let Some(codec) = *self.passed.lock().unwrap() else {
            return Ok(());
        };
        let mut video = self.video.lock().unwrap();
        let frame = access_unit(codec, nalu_bytes, &mut video.sets, sender_nanos);
        if frame.key && !video.first_key_logged {
            video.first_key_logged = true;
            let source = match video.sets.get(codec) {
                Some((_, crate::video::SetsSource::Config)) => {
                    "parameter sets from the configuration"
                }
                Some((_, crate::video::SetsSource::Stream)) => "parameter sets from the stream",
                None => "no complete parameter sets",
            };
            (self.events)(ReceiverEvent::Log(format!(
                "screen {kind}: first key picture in {}: {} as received, {} as passed on; {source}",
                codec_name(codec),
                unit_kinds(codec, nalu_bytes),
                unit_kinds(codec, &frame.data)
            )));
        }
        if frame.key && !frame.complete && !video.incomplete_logged {
            video.incomplete_logged = true;
            (self.events)(ReceiverEvent::Log(format!(
                "screen {kind}: a key picture without a complete group of parameter sets; the head unit may not decode it"
            )));
        }
        drop(video);
        (self.events)(ReceiverEvent::Video(frame));
        Ok(())
    }

    fn on_screen_stream_active(&self, kind: u16, active: bool) {
        if kind == MAIN_SCREEN {
            // The parameter sets belong to the stream: a new one gives its own.
            *self.video.lock().unwrap() = VideoState::default();
            (self.events)(ReceiverEvent::ScreenActive(active));
        }
    }

    fn on_audio_started(&self, id: &AudioStreamId, format: &AudioFormat) -> io::Result<()> {
        let stream = audio_stream(id);
        (self.events)(ReceiverEvent::Log(format!(
            "audio {} on stream {}: {:?}, {} Hz, {} channel(s)",
            id.audio_type, id.kind, format.codec, format.sample_rate, format.channels
        )));
        let (music, sample_rate, channels) = match format.codec {
            AudioCodec::AacLc => (true, format.sample_rate, format.channels),
            AudioCodec::Pcm => (false, format.sample_rate, format.channels),
            AudioCodec::Opus => {
                let rate = opus::decode_rate(stream.kind);
                if !self.codecs.start_decoding(id.kind, &id.audio_type, rate) {
                    (self.events)(ReceiverEvent::Log(format!(
                        "audio {}: no Opus decoder at {rate} Hz",
                        id.audio_type
                    )));
                    return Ok(());
                }
                (false, rate, 1)
            }
        };
        (self.events)(ReceiverEvent::Audio(AudioEvent::Started {
            stream,
            music,
            sample_rate,
            channels,
        }));
        Ok(())
    }

    fn on_audio_rtp(
        &self,
        id: &AudioStreamId,
        format: &AudioFormat,
        rtp: &[u8],
        _sample: u32,
    ) -> io::Result<()> {
        let Some(payload) = audio::rtp_payload(rtp).filter(|p| !p.is_empty()) else {
            return Ok(());
        };
        let stream = audio_stream(id);
        let event = match format.codec {
            AudioCodec::AacLc => match audio::adts(payload, format.sample_rate, format.channels) {
                Some(adts) => AudioEvent::Music { stream, adts },
                None => return Ok(()),
            },
            AudioCodec::Pcm => AudioEvent::Pcm {
                stream,
                samples: audio::pcm_to_little_endian(payload),
            },
            AudioCodec::Opus => {
                let rate = opus::decode_rate(stream.kind);
                match self.codecs.decode(id.kind, &id.audio_type, rate, payload) {
                    Some(samples) if !samples.is_empty() => AudioEvent::Pcm { stream, samples },
                    _ => return Ok(()),
                }
            }
        };
        (self.events)(ReceiverEvent::Audio(event));
        Ok(())
    }

    fn on_audio_stopped(&self, id: &AudioStreamId) -> io::Result<()> {
        self.codecs.stop_decoding(id.kind, &id.audio_type);
        (self.events)(ReceiverEvent::Audio(AudioEvent::Stopped {
            stream: audio_stream(id),
        }));
        Ok(())
    }

    fn on_microphone_started(
        &self,
        id: &AudioStreamId,
        config: &MicrophoneFormat,
    ) -> io::Result<()> {
        (self.events)(ReceiverEvent::Log(format!(
            "microphone for {} on stream {}: {:?}, {} Hz, {} channel(s), {} ms frames",
            id.audio_type,
            id.kind,
            config.codec,
            config.sample_rate,
            config.channels,
            config.frame_ms
        )));
        match config.codec {
            AudioCodec::Opus => {
                if !self.codecs.start_encoding(
                    id.kind,
                    config.sample_rate,
                    config.channels,
                    config.bitrate,
                ) {
                    (self.events)(ReceiverEvent::Log(format!(
                        "microphone for {}: no Opus encoder at {} Hz",
                        id.audio_type, config.sample_rate
                    )));
                    return Ok(());
                }
            }
            _ => self.codecs.stop_encoding(id.kind),
        }
        (self.events)(ReceiverEvent::Audio(AudioEvent::MicrophoneStarted {
            stream_type: id.kind,
            kind: AudioKind::from_audio_type(&id.audio_type),
            sample_rate: config.sample_rate,
            channels: config.channels,
            frame_bytes: config.frame_bytes(),
        }));
        Ok(())
    }

    fn on_microphone_stopped(&self, id: &AudioStreamId) -> io::Result<()> {
        self.codecs.stop_encoding(id.kind);
        (self.events)(ReceiverEvent::Audio(AudioEvent::MicrophoneStopped {
            stream_type: id.kind,
        }));
        Ok(())
    }
}

fn codec_name(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::H265 => "H.265",
    }
}

fn audio_stream(id: &AudioStreamId) -> AudioStream {
    AudioStream {
        stream_type: id.kind,
        kind: AudioKind::from_audio_type(&id.audio_type),
    }
}

/// The microphones the iPhones' sessions asked for, by stream type: the session and the stream.
type Microphones = Arc<Mutex<HashMap<u16, (Weak<Session>, AudioStreamId)>>>;

/// What runs iAP2 on each tunnel an iPhone opens inside its session.
type TunnelHandler = Arc<Mutex<Option<Arc<dyn Fn(Arc<dyn Stream>) + Send + Sync>>>>;

/// Follows one session's lifecycle and passes its media on.
struct Observer {
    events: Events,
    bridge: Arc<Bridge>,
    microphones: Microphones,
    tunnel: TunnelHandler,
    /// The session of the moment, for the input that goes through it and for the codec its screen is passed on in.
    current: Shared,
    /// The session's number.
    id: u64,
    /// The codec this session's screen is passed on in.
    codec: VideoCodec,
    /// The car tile and the picture this session was given.
    given: Given,
    night: Arc<Night>,
    now_playing: NowPlayingFeed,
    /// The session it follows, once started.
    session: Mutex<Weak<Session>>,
}

impl Host for Observer {
    fn log(&self, _session: &Session, message: &str) {
        (self.events)(ReceiverEvent::Log(message.to_owned()));
    }

    fn pairing_refused(&self, peer: IpAddr) {
        (self.events)(ReceiverEvent::AuthenticationFailed { peer: Some(peer) });
    }

    fn active(&self, session: &Arc<Session>) {
        *self.current.lock().unwrap() = Current {
            session: Some(Arc::downgrade(session)),
            id: self.id,
            codec: self.codec,
            logged: Vec::new(),
            given: self.given.clone(),
        };
        (self.events)(ReceiverEvent::SessionActive {
            peer: Some(session.peer()),
            codec: self.codec,
            picture: self.given.picture,
        });
        self.night.mode.lock().unwrap().session_started(self.id);
        self.night.changed.notify_all();
    }

    fn ended(&self, session: &Arc<Session>, ending: &Ending) {
        if let Ending::PeerUnreachable { connection } = ending {
            (self.events)(ReceiverEvent::Log(format!(
                "iPhone {} stopped answering: no reply on the {connection} connection for {} s; ending its session",
                session.peer(),
                PEER_TIMEOUT.as_secs()
            )));
        }
        self.microphones.lock().unwrap().retain(|_, (owner, _)| {
            !owner
                .upgrade()
                .is_some_and(|owner| Arc::ptr_eq(&owner, session))
        });
        let mut current = self.current.lock().unwrap();
        let was_current = current
            .session
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|c| Arc::ptr_eq(&c, session));
        if was_current {
            current.session = None;
        }
        drop(current);
        if was_current {
            self.now_playing.clear();
        }
        self.night.mode.lock().unwrap().session_ended(self.id);
        (self.events)(ReceiverEvent::Audio(AudioEvent::SessionEnded));
        (self.events)(ReceiverEvent::SessionEnded);
    }

    fn device_info(&self, _session: &Session, info: &DeviceInfo) {
        (self.events)(ReceiverEvent::Log(format!(
            "iPhone {} ({}), model {}",
            info.name, info.device_id, info.model
        )));
    }

    fn command(&self, _session: &Session, kind: &str, params: &Dict) {
        if kind == "requestUI" {
            (self.events)(ReceiverEvent::Log(
                "iPhone asks for the car's own screen".into(),
            ));
        }
        (self.events)(ReceiverEvent::Log(format!("command {kind}: {params:?}")));
        if kind == "disableBluetooth" {
            if let Some(device_id) = params.get("deviceID").and_then(Value::as_str) {
                (self.events)(ReceiverEvent::BluetoothHandoff {
                    device_id: device_id.to_owned(),
                });
            }
        }
    }

    fn screen(&self, _session: &Session, active: bool) {
        self.bridge.on_screen_stream_active(MAIN_SCREEN, active);
    }

    fn video(&self, _session: &Session, message: ScreenMessage) {
        let _ = match message {
            ScreenMessage::Config { codec, record } => self
                .bridge
                .on_video_codec(MAIN_SCREEN, codec)
                .and_then(|()| self.bridge.on_video_config(MAIN_SCREEN, &record)),
            ScreenMessage::Frame { data, sender_nanos } => {
                self.bridge.on_video_frame(MAIN_SCREEN, &data, sender_nanos)
            }
        };
    }

    fn audio_started(&self, _session: &Session, stream: &AudioStreamId, format: &AudioFormat) {
        let _ = self.bridge.on_audio_started(stream, format);
    }

    fn audio(
        &self,
        _session: &Session,
        stream: &AudioStreamId,
        format: &AudioFormat,
        rtp: &[u8],
        timestamp: u32,
    ) {
        let _ = self.bridge.on_audio_rtp(stream, format, rtp, timestamp);
    }

    fn audio_stopped(&self, _session: &Session, stream: &AudioStreamId) {
        let _ = self.bridge.on_audio_stopped(stream);
    }

    fn microphone_started(
        &self,
        _session: &Session,
        stream: &AudioStreamId,
        format: &MicrophoneFormat,
    ) {
        let owner = self.session.lock().unwrap().clone();
        self.microphones
            .lock()
            .unwrap()
            .insert(stream.kind, (owner, stream.clone()));
        let _ = self.bridge.on_microphone_started(stream, format);
    }

    fn microphone_stopped(&self, _session: &Session, stream: &AudioStreamId) {
        self.microphones
            .lock()
            .unwrap()
            .retain(|_, (_, id)| id != stream);
        let _ = self.bridge.on_microphone_stopped(stream);
    }

    fn iap2_tunnel(&self, _session: &Arc<Session>, tunnel: Arc<dyn Stream>) {
        let handler = self.tunnel.lock().unwrap().clone();
        match handler {
            Some(handler) => handler(tunnel),
            None => tunnel.shutdown(),
        }
    }
}

/// A running receiver. Dropping it does not stop it; call [`Receiver::stop`].
pub struct Receiver {
    current: Shared,
    /// Whether iPhones that connect from now on are offered H.265.
    hevc: Arc<AtomicBool>,
    /// The car tile iPhones that connect from now on are given.
    tile: Arc<Mutex<Arc<CarTile>>>,
    picture: Chosen,
    latest: Latest,
    /// Every session started, while it may still run.
    sessions: Arc<Mutex<Vec<Weak<Session>>>>,
    microphones: Microphones,
    tunnel: TunnelHandler,
    mfi: Arc<dyn Authenticator>,
    identity: Arc<Identity>,
    air_play: Config,
    port: u16,
    listener: Arc<Listener>,
    stopped: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    night: Arc<Night>,
    codecs: Arc<Codecs>,
    now_playing: NowPlayingFeed,
}

impl Receiver {
    /// Starts listening for iPhones; `events` is called from the receiver's threads and must return quickly.
    pub fn start(
        config: ReceiverConfig,
        events: impl Fn(ReceiverEvent) + Send + Sync + 'static,
    ) -> io::Result<Receiver> {
        let events: Events = Arc::new(events);
        let store = Arc::new(Store::open(&config.state_dir)?);
        let identity = Arc::new(store.identity()?);
        let mfi: Arc<dyn Authenticator> = Arc::new(LocalAuthenticator::new(
            &std::fs::read(&config.mfi_key)?,
            &std::fs::read(&config.mfi_certificate)?,
        )?);
        let sockets = Arc::new(Sockets::new(config.local));
        let current: Shared = Arc::new(Mutex::new(Current {
            session: None,
            id: 0,
            codec: VideoCodec::H264,
            logged: Vec::new(),
            given: Given {
                tile: Arc::new(config.tile.clone()),
                picture: config.picture.fitted(),
                hevc: config.hevc,
            },
        }));
        let hevc = Arc::new(AtomicBool::new(config.hevc));
        let tile = Arc::new(Mutex::new(Arc::new(config.tile.clone())));
        let picture: Chosen = Arc::new(Mutex::new((
            config.picture.fitted(),
            config.picture_reason.as_str().into(),
        )));
        let latest: Latest = Arc::default();
        let sessions: Arc<Mutex<Vec<Weak<Session>>>> = Arc::default();
        let microphones: Microphones = Arc::default();
        let tunnel: TunnelHandler = Arc::default();
        let codecs = Arc::new(Codecs::default());
        let bridge = Arc::new(Bridge {
            events: events.clone(),
            current: current.clone(),
            video: Mutex::new(VideoState::default()),
            passed: Mutex::new(None),
            codecs: codecs.clone(),
        });
        let listener = Arc::new(Listener::bind(
            config.local,
            config.port,
            IpAddr::from([0, 0, 0, 0]),
        )?);
        let stopped = Arc::new(AtomicBool::new(false));
        let night = Arc::new(Night::default());
        let now_playing = NowPlayingFeed::new(events.clone());
        {
            let (night, current, events, stopped) = (
                night.clone(),
                current.clone(),
                events.clone(),
                stopped.clone(),
            );
            std::thread::Builder::new()
                .name("carplay-night".into())
                .spawn(move || tell_night_mode(night, current, events, stopped))?;
        }
        let air_play = config.air_play();
        let (mfi_shared, identity_shared, air_play_shared) =
            (mfi.clone(), identity.clone(), air_play.clone());

        let thread = {
            let (listener, stopped) = (listener.clone(), stopped.clone());
            let (current, hevc, night, tile, picture, latest, now_playing) = (
                current.clone(),
                hevc.clone(),
                night.clone(),
                tile.clone(),
                picture.clone(),
                latest.clone(),
                now_playing.clone(),
            );
            let (sessions, microphones, tunnel) =
                (sessions.clone(), microphones.clone(), tunnel.clone());
            let session_count = AtomicU64::new(0);
            std::thread::Builder::new()
                .name("airplay-accept".into())
                .spawn(move || {
                    while !stopped.load(Ordering::Acquire) {
                        let (stream, peer) = match listener.accept_tcp(ACCEPT_WAIT) {
                            Ok(Some(accepted)) => accepted,
                            Ok(None) => continue,
                            Err(err) => {
                                events(ReceiverEvent::Log(format!("accept: {err}")));
                                std::thread::sleep(ACCEPT_WAIT);
                                continue;
                            }
                        };
                        // Each session is offered the codecs of the moment and given the car tile and the picture of
                        // the moment.
                        let mut session_config = air_play.clone();
                        session_config.hevc = hevc.load(Ordering::Acquire);
                        let session_tile = tile.lock().unwrap().clone();
                        set_tile(&mut session_config, &session_tile);
                        let (session_picture, reason) = picture.lock().unwrap().clone();
                        session_picture.apply(&mut session_config.display);
                        let id = session_count.fetch_add(1, Ordering::Relaxed) + 1;
                        events(ReceiverEvent::Log(format!(
                            "session {id} from {}: picture {session_picture} ({reason})",
                            peer.ip()
                        )));
                        let given = Given {
                            tile: session_tile,
                            picture: session_picture,
                            hevc: session_config.hevc,
                        };
                        let observer = Arc::new(Observer {
                            events: events.clone(),
                            bridge: bridge.clone(),
                            microphones: microphones.clone(),
                            tunnel: tunnel.clone(),
                            current: current.clone(),
                            id,
                            codec: if session_config.hevc {
                                VideoCodec::H265
                            } else {
                                VideoCodec::H264
                            },
                            given: given.clone(),
                            night: night.clone(),
                            now_playing: now_playing.clone(),
                            session: Mutex::new(Weak::new()),
                        });
                        let control: Arc<dyn Stream> = match Tcp::new(stream) {
                            Ok(control) => Arc::new(control),
                            Err(err) => {
                                events(ReceiverEvent::Log(format!("session from {peer}: {err}")));
                                continue;
                            }
                        };
                        let setup = Setup {
                            config: Arc::new(session_config),
                            identity: identity.clone(),
                            pairings: store.clone(),
                            authenticator: mfi.clone(),
                            sockets: sockets.clone(),
                        };
                        let session = Session::start(control, peer.ip(), setup, observer.clone());
                        *observer.session.lock().unwrap() = Arc::downgrade(&session);
                        *latest.lock().unwrap() = Some((Arc::downgrade(&session), given));
                        let mut sessions = sessions.lock().unwrap();
                        sessions.retain(|session| session.upgrade().is_some_and(|s| !s.is_ended()));
                        sessions.push(Arc::downgrade(&session));
                    }
                })?
        };
        Ok(Receiver {
            current,
            hevc,
            tile,
            picture,
            latest,
            sessions,
            microphones,
            tunnel,
            mfi: mfi_shared,
            identity: identity_shared,
            air_play: air_play_shared,
            port: config.port,
            listener,
            stopped,
            thread: Mutex::new(Some(thread)),
            night,
            codecs,
            now_playing,
        })
    }

    /// Where the iPhone's iAP2 links pass what plays on it.
    pub fn now_playing(&self) -> NowPlayingFeed {
        self.now_playing.clone()
    }

    /// Takes the head unit's day or night mode, night when true, or `None` once no head unit is connected. The
    /// session of the moment is told a mode at once, or as soon as it can take it, and each session that starts is
    /// told the mode kept; with no head unit, sessions are told nothing.
    pub fn set_night_mode(&self, night: Option<bool>) {
        self.night.mode.lock().unwrap().set_head_unit(night);
        self.night.changed.notify_all();
    }

    /// Offers H.265 as well as H.264 to the iPhones that connect from now on, or only H.264. A session keeps the
    /// codecs it was offered: the iPhone has to connect again to change.
    pub fn set_hevc(&self, on: bool) {
        self.hevc.store(on, Ordering::Release);
    }

    /// Gives the iPhones that connect from now on this car tile; returns whether it differs from the one before. A
    /// session keeps the tile it was given: the iPhone shows a new one when it connects again
    /// ([`Receiver::end_session_if`]).
    pub fn set_car_tile(&self, tile: CarTile) -> bool {
        let mut kept = self.tile.lock().unwrap();
        let changed = **kept != tile;
        *kept = Arc::new(tile);
        changed
    }

    /// Gives the iPhones that connect from now on this picture, fitted to what AirPlay takes, and why it is this one
    /// for the log; returns whether it differs from the one before. A session keeps the picture it was given: the
    /// iPhone renders a new one when it connects again ([`Receiver::end_session_if`]).
    pub fn set_picture(&self, picture: Picture, reason: &str) -> bool {
        let picture = picture.fitted();
        let mut kept = self.picture.lock().unwrap();
        let changed = kept.0 != picture;
        *kept = (picture, reason.into());
        changed
    }

    /// The picture the session of the moment was given, if an iPhone is connected.
    pub fn session_picture(&self) -> Option<Picture> {
        let current = self.current.lock().unwrap();
        current
            .session
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|_| current.given.picture)
    }

    /// Ends the iPhone's session, the one up or else one still starting, when `ends` says so of what it was given;
    /// returns whether it ends. It ends in the background, as when the iPhone closes it.
    pub fn end_session_if(&self, ends: impl FnOnce(&Given) -> bool) -> bool {
        let up = {
            let current = self.current.lock().unwrap();
            current
                .session
                .as_ref()
                .and_then(Weak::upgrade)
                .map(|session| (session, current.given.clone()))
        };
        let starting = || {
            self.latest
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|(session, given)| Some((session.upgrade()?, given.clone())))
        };
        let Some((session, given)) = up
            .or_else(starting)
            .filter(|(session, _)| !session.is_ended())
        else {
            return false;
        };
        if !ends(&given) {
            return false;
        }
        // Ending tells the session's listeners, which may take the receiver's locks.
        std::thread::spawn(move || session.end());
        true
    }

    /// The codec the screen of the session of the moment is passed on in, if an iPhone is connected.
    pub fn session_codec(&self) -> Option<VideoCodec> {
        let current = self.current.lock().unwrap();
        current
            .session
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|_| current.codec)
    }

    /// The MFi identity the receiver authenticates with, for the iAP2 link too.
    pub fn mfi(&self) -> Arc<dyn Authenticator> {
        self.mfi.clone()
    }

    /// The AirPlay identity's public key, in hex.
    pub fn public_key(&self) -> String {
        self.identity.public_key_hex()
    }

    /// The AirPlay device ID.
    pub fn device_id(&self) -> &str {
        &self.air_play.device_id
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn source_version(&self) -> &str {
        &self.air_play.source_version
    }

    /// Gives the iPhone one frame of the car's microphone: 16-bit little-endian PCM, the frame size its
    /// [`AudioEvent::MicrophoneStarted`] gave. It goes in Opus when the iPhone asked for that.
    pub fn microphone(
        &self,
        stream_type: u16,
        frame: &[u8],
    ) -> Result<MicrophoneSent, MicrophoneError> {
        let target = self.microphones.lock().unwrap().get(&stream_type).cloned();
        let send = |payload: &[u8]| -> io::Result<bool> {
            match &target {
                Some((session, stream)) => match session.upgrade() {
                    Some(session) => session.send_microphone(stream, payload),
                    None => Ok(false),
                },
                None => Ok(false),
            }
        };
        let (sent, opus) = match self.codecs.encode(stream_type, frame) {
            Some(Some(packet)) => (send(&packet), true),
            Some(None) => return Err(MicrophoneError::Encode),
            None => (send(frame), false),
        };
        match sent {
            Ok(true) if opus => Ok(MicrophoneSent::Opus),
            Ok(true) => Ok(MicrophoneSent::Pcm),
            Ok(false) => Ok(MicrophoneSent::NoStream),
            Err(err) => Err(MicrophoneError::Send(err)),
        }
    }

    /// Hands each iAP2 tunnel an iPhone opens inside its session to `on_tunnel`, which runs iAP2 on it without
    /// blocking.
    pub fn set_iap_tunnel(&self, on_tunnel: impl Fn(Arc<dyn Stream>) + Send + Sync + 'static) {
        *self.tunnel.lock().unwrap() = Some(Arc::new(on_tunnel));
    }

    /// The session of the moment.
    fn session(&self) -> Option<Arc<Session>> {
        self.current
            .lock()
            .unwrap()
            .session
            .as_ref()
            .and_then(Weak::upgrade)
    }

    /// Asks the iPhone for a key frame.
    pub fn request_key_frame(&self) {
        if let Some(session) = self.session() {
            session.request_key_frame();
        }
    }

    /// Passes the contacts of CarPlay's touch screen, as fractions of the screen, to the iPhone.
    pub fn touch(&self, contacts: &[Contact]) -> io::Result<bool> {
        Ok(self
            .session()
            .is_some_and(|session| session.touch(contacts)))
    }

    /// Presses and releases a key on the iPhone.
    pub fn key(&self, key: Key) -> io::Result<()> {
        let Some(session) = self.session() else {
            return Ok(());
        };
        let knob = |knob: Knob| session.knob(&knob, true);
        let _ = match key {
            Key::Back => knob(Knob {
                back: true,
                ..Knob::default()
            }),
            Key::Home => knob(Knob {
                home: true,
                ..Knob::default()
            }),
            Key::Select => knob(Knob {
                select: true,
                ..Knob::default()
            }),
            Key::Play => session.media_key(hid::media::PLAY),
            Key::Pause => session.media_key(hid::media::PAUSE),
            Key::PlayPause => session.media_key(hid::media::PLAY_PAUSE),
            Key::Next => session.media_key(hid::media::NEXT),
            Key::Previous => session.media_key(hid::media::PREVIOUS),
            Key::Answer => session.telephony_key(hid::telephony::HOOK_SWITCH),
            Key::HangUp => session.telephony_key(hid::telephony::DROP),
            Key::Siri => session.siri(),
        };
        Ok(())
    }

    /// Stops listening and ends the sessions.
    pub fn stop(&self) {
        {
            // Under the lock, for the thread that tells the day or night mode to see it before it waits again.
            let _mode = self.night.mode.lock().unwrap();
            self.stopped.store(true, Ordering::Release);
            self.night.changed.notify_all();
        }
        self.listener.close();
        let sessions: Vec<Arc<Session>> = self
            .sessions
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|session| session.upgrade())
            .collect();
        for session in sessions {
            session.end();
        }
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}
