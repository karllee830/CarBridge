// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The iPhone's sound on the head unit: the head unit's audio type each CarPlay stream plays as, the head unit's
//! player for each type, the car's microphone, and the call or Siri the head unit is told of.
//!
//! The head unit keeps one player per audio type, created by the first `NotifyAudioPlayerState` start and kept until
//! a stop; a start for a type that has a player is ignored, so a new format needs a stop first. Its microphone
//! records with the voice-communication source, which cancels the echo of its speakers, when `NotifyPhoneState` has
//! said a call or the voice assistant is on at the moment the microphone starts; the microphone is started again
//! when that changes while it records.
//!
//! The iPhone's navigation prompts play as the head unit's navigation speech, or as another kind of sound the head
//! unit's settings choose ([`NavAudio`]), which the head unit then treats as such: its volume, how other sound is
//! lowered, and audio focus. A stream that comes while another plays in the player of its audio type is converted to
//! that player's format and mixed with it, rather than starting the player again; it goes on in that format once the
//! other ends. What the head unit is told of calls and Siri follows their own streams only.

use std::collections::HashMap;

use carbridge_carlink_proto::command;
use carbridge_carlink_proto::frame::Message;
use carbridge_carlink_proto::header::{Category, Kind};
use carbridge_carlink_proto::pb::{
    AudioPlayerState, AudioType, CallState, ChannelMask, EncodingFormat, NotifyAudioPlayerState,
    NotifyMicrophoneState, NotifyPhoneState,
};
use carbridge_carplay::audio::{AudioKind, Framer};
use carbridge_carplay::receiver::{AudioEvent, AudioStream};

use crate::mixing::{Converter, Pending};

/// The head unit's audio type for a kind of sound. Calls play as a VoIP call; speech, the voice assistant and the
/// ring tone play over the music, which the head unit lowers meanwhile.
pub fn audio_type(kind: AudioKind) -> AudioType {
    match kind {
        AudioKind::Media => AudioType::AudioCastMusic,
        AudioKind::Call => AudioType::AudioIpCall,
        AudioKind::Assistant => AudioType::AudioAiAssistant,
        AudioKind::Alert => AudioType::AudioRing,
        AudioKind::Speech => AudioType::AudioTts,
        AudioKind::System => AudioType::AudioSystem,
    }
}

/// The kind of sound the iPhone's navigation prompts play as on the head unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavAudio {
    /// Navigation speech.
    #[default]
    Tts,
    /// Media, in a player of its own next to the cast's music: the head unit's media type for PCM, as the cast's
    /// music type is the music decoded from the cast, which a second stream in its player would garble.
    Media,
    /// A phone call.
    Call,
    /// The voice assistant.
    Assistant,
}

impl NavAudio {
    /// The head unit's audio type navigation plays as.
    pub fn audio_type(self) -> AudioType {
        match self {
            NavAudio::Tts => AudioType::AudioTts,
            NavAudio::Media => AudioType::AudioUndefined,
            NavAudio::Call => AudioType::AudioIpCall,
            NavAudio::Assistant => AudioType::AudioAiAssistant,
        }
    }
}

/// The kind of sound navigation plays as: `tts`, `media`, `call` or `assistant`.
pub fn parse_nav_audio(text: &str) -> Option<NavAudio> {
    match text {
        "tts" => Some(NavAudio::Tts),
        "media" => Some(NavAudio::Media),
        "call" => Some(NavAudio::Call),
        "assistant" => Some(NavAudio::Assistant),
        _ => None,
    }
}

/// The name of a kind of sound for navigation, as [`parse_nav_audio`] takes it.
pub fn nav_audio_name(nav: NavAudio) -> &'static str {
    match nav {
        NavAudio::Tts => "tts",
        NavAudio::Media => "media",
        NavAudio::Call => "call",
        NavAudio::Assistant => "assistant",
    }
}

/// 16-bit PCM at a rate and channel count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u8,
}

impl Format {
    fn channel_mask(self) -> ChannelMask {
        if self.channels == 1 {
            ChannelMask::ChannelMono
        } else {
            ChannelMask::ChannelStereo
        }
    }
}

/// What the head unit is told the phone is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phone {
    Idle,
    Call,
    Assistant,
}

impl Phone {
    fn describe(self) -> &'static str {
        match self {
            Phone::Idle => "no call or Siri",
            Phone::Call => "call on",
            Phone::Assistant => "Siri on",
        }
    }
}

/// What goes to the head unit.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// A player starts or stops.
    Player {
        audio_type: AudioType,
        state: AudioPlayerState,
        format: Format,
    },
    /// Whether a call or Siri is on.
    Phone(Phone),
    /// The microphone starts or stops.
    Microphone { on: bool, format: Format },
    /// PCM on the media channel.
    Pcm {
        audio_type: AudioType,
        samples: Vec<u8>,
    },
    /// An AAC frame for the cast.
    Music(Vec<u8>),
}

impl Out {
    /// The control channel message, for a player, the phone state or the microphone.
    pub fn control(&self) -> Option<Message> {
        match self {
            Out::Player {
                audio_type,
                state,
                format,
            } => {
                let notice = NotifyAudioPlayerState {
                    r#type: *audio_type as i32,
                    state: *state as i32,
                    sample_rate: format.sample_rate as i32,
                    channel_mask: format.channel_mask() as i32,
                    encoding_format: EncodingFormat::EncodingPcm16bit as i32,
                    mime_type: None,
                };
                Some(Message::protobuf(
                    Category::CONTROL,
                    command::control::NOTIFY_AUDIO_PLAYER_STATE,
                    Kind::Send,
                    &notice,
                ))
            }
            Out::Phone(phone) => {
                let notice = phone_state(*phone);
                Some(Message::protobuf(
                    Category::CONTROL,
                    command::control::NOTIFY_PHONE_STATE,
                    Kind::Send,
                    &notice,
                ))
            }
            Out::Microphone { on, format } => {
                let notice = NotifyMicrophoneState {
                    state: *on,
                    sample_rate: format.sample_rate as i32,
                    channel_mask: format.channel_mask() as i32,
                    encoding_format: EncodingFormat::EncodingPcm16bit as i32,
                };
                Some(Message::protobuf(
                    Category::CONTROL,
                    command::control::NOTIFY_MICROPHONE_STATE,
                    Kind::Send,
                    &notice,
                ))
            }
            Out::Pcm { .. } | Out::Music(_) => None,
        }
    }
}

/// The head unit's phone state for a call, Siri, or neither. A call is in progress (`cs` other than idle) and over
/// the network (`isWechatQqCall`), as the call's sound plays as a VoIP call; Siri is the voice assistant.
fn phone_state(phone: Phone) -> NotifyPhoneState {
    NotifyPhoneState {
        cs: if phone == Phone::Call {
            CallState::CallActive
        } else {
            CallState::CallIdle
        } as i32,
        is_screen_locked: false,
        is_wechat_qq_call: phone == Phone::Call,
        is_voice_assistant_active: phone == Phone::Assistant,
        is_low_power: None,
        is_overheat: None,
    }
}

/// The car's microphone while the iPhone records from it.
struct Microphone {
    stream_type: u16,
    kind: AudioKind,
    framer: Framer,
    format: Format,
}

/// A PCM stream playing.
struct Playing {
    /// The head unit's audio type it plays as.
    audio_type: AudioType,
    /// Its order among the streams that started, the earliest first: of the streams in one player, the earliest
    /// leads, and the others are mixed into it.
    order: u64,
    /// To the format of its player, when that is another.
    converter: Option<Converter>,
    /// What waits to be mixed into the stream that leads its player.
    pending: Pending,
}

/// The iPhone's sound for one connection to a head unit.
pub struct Sound {
    /// The PCM streams playing.
    streams: HashMap<AudioStream, Playing>,
    /// The head unit's players, by audio type, and the format each was started with.
    players: HashMap<AudioType, Format>,
    microphone: Option<Microphone>,
    /// What the head unit was told last.
    phone: Phone,
    /// The kind of sound navigation plays as, from its next prompt on.
    nav: NavAudio,
    /// How many streams started.
    started: u64,
}

impl Default for Sound {
    fn default() -> Sound {
        Sound::new()
    }
}

impl Sound {
    pub fn new() -> Sound {
        Sound {
            streams: HashMap::new(),
            players: HashMap::new(),
            microphone: None,
            phone: Phone::Idle,
            nav: NavAudio::default(),
            started: 0,
        }
    }

    /// Plays the iPhone's navigation as `nav` from its next prompt on.
    pub fn set_nav(&mut self, nav: NavAudio) {
        if nav != self.nav {
            log!(
                "navigation plays as {:?} from its next prompt on",
                nav.audio_type()
            );
            self.nav = nav;
        }
    }

    /// The head unit's audio type for a stream of `kind`: navigation's as its setting has it.
    fn audio_type_of(&self, kind: AudioKind) -> AudioType {
        match kind {
            AudioKind::Speech => self.nav.audio_type(),
            kind => audio_type(kind),
        }
    }

    /// The streams other than `stream` that play in the player of `audio_type`.
    fn sharing(&self, stream: AudioStream, audio_type: AudioType) -> Vec<AudioStream> {
        let mut others: Vec<(u64, AudioStream)> = self
            .streams
            .iter()
            .filter(|(s, p)| **s != stream && p.audio_type == audio_type)
            .map(|(s, p)| (p.order, *s))
            .collect();
        others.sort_by_key(|(order, _)| *order);
        others.into_iter().map(|(_, s)| s).collect()
    }

    /// What the head unit gets for one of the iPhone's audio events.
    pub fn take(&mut self, event: AudioEvent) -> Vec<Out> {
        let mut out = Vec::new();
        match event {
            AudioEvent::Started {
                stream,
                music: true,
                sample_rate,
                channels,
            } => log!(
                "audio {:?} on stream {} as {:?} in the cast: AAC, {sample_rate} Hz, {channels} channel(s)",
                stream.kind,
                stream.stream_type,
                AudioType::AudioCastMusic
            ),
            AudioEvent::Started {
                stream,
                music: false,
                sample_rate,
                channels,
            } => {
                let format = Format {
                    sample_rate,
                    channels,
                };
                let audio_type = self.audio_type_of(stream.kind);
                self.started += 1;
                let order = self
                    .streams
                    .get(&stream)
                    .filter(|p| p.audio_type == audio_type)
                    .map_or(self.started, |p| p.order);
                if let Some(old) = self.streams.remove(&stream) {
                    if old.audio_type != audio_type {
                        self.stop_player_if_unused(&mut out, old.audio_type, stream);
                    }
                }
                let shared = !self.sharing(stream, audio_type).is_empty();
                let player = self.players.get(&audio_type).copied();
                let joins = match player {
                    Some(player) if shared => Some(player),
                    _ => None,
                };
                self.streams.insert(
                    stream,
                    Playing {
                        audio_type,
                        order,
                        converter: joins
                            .filter(|player| *player != format)
                            .map(|player| Converter::new(format, player)),
                        pending: Pending::default(),
                    },
                );
                if let Some(player) = joins {
                    log!(
                        "audio {:?} on stream {} as {audio_type:?}: PCM, {sample_rate} Hz, {channels} channel(s), \
                         mixed into the player another stream plays in, at {} Hz, {} channel(s)",
                        stream.kind,
                        stream.stream_type,
                        player.sample_rate,
                        player.channels
                    );
                    self.tell_phone(&mut out, true);
                    return out;
                }
                match self.players.insert(audio_type, format) {
                    Some(old) if old == format => {}
                    Some(old) => {
                        log!(
                            "audio {:?} on stream {} as {audio_type:?}: player started again for {sample_rate} Hz, \
                             {channels} channel(s), from {} Hz, {} channel(s)",
                            stream.kind,
                            stream.stream_type,
                            old.sample_rate,
                            old.channels
                        );
                        out.push(Out::Player {
                            audio_type,
                            state: AudioPlayerState::PlayerStop,
                            format: old,
                        });
                        out.push(Out::Player {
                            audio_type,
                            state: AudioPlayerState::PlayerStart,
                            format,
                        });
                    }
                    None => {
                        log!(
                            "audio {:?} on stream {} as {audio_type:?}: PCM, {sample_rate} Hz, {channels} channel(s)",
                            stream.kind,
                            stream.stream_type
                        );
                        out.push(Out::Player {
                            audio_type,
                            state: AudioPlayerState::PlayerStart,
                            format,
                        });
                    }
                }
                self.tell_phone(&mut out, true);
            }
            AudioEvent::Music { adts, .. } => out.push(Out::Music(adts)),
            AudioEvent::Pcm { stream, samples } => self.pcm(&mut out, stream, samples),
            AudioEvent::Stopped { stream } => {
                if let Some(playing) = self.streams.remove(&stream) {
                    // The stream that leads the player next sends what of it waits, rather than lose it.
                    if let Some(&next) = self.sharing(stream, playing.audio_type).first() {
                        let waiting = self
                            .streams
                            .get_mut(&next)
                            .map(|p| p.pending.take())
                            .unwrap_or_default();
                        if !waiting.is_empty() {
                            out.push(Out::Pcm {
                                audio_type: playing.audio_type,
                                samples: waiting,
                            });
                        }
                    }
                    self.stop_player_if_unused(&mut out, playing.audio_type, stream);
                    self.tell_phone(&mut out, true);
                }
            }
            AudioEvent::MicrophoneStarted {
                stream_type,
                kind,
                sample_rate,
                channels,
                frame_bytes,
            } => {
                let format = Format {
                    sample_rate,
                    channels,
                };
                self.microphone = Some(Microphone {
                    stream_type,
                    kind,
                    framer: Framer::new(frame_bytes),
                    format,
                });
                // The head unit picks its recording source by the phone state it has when the microphone starts.
                self.tell_phone(&mut out, false);
                log!("microphone on for {kind:?}: {sample_rate} Hz, {channels} channel(s)");
                out.push(self.microphone_out(true, format));
            }
            AudioEvent::MicrophoneStopped { stream_type } => {
                if let Some(microphone) = self
                    .microphone
                    .take_if(|m| m.stream_type == stream_type)
                {
                    log!("microphone off");
                    out.push(self.microphone_out(false, microphone.format));
                    self.tell_phone(&mut out, false);
                }
            }
            AudioEvent::SessionEnded => out = self.reset(),
        }
        out
    }

    /// What the head unit is told once its control channel is up: that no call or Siri is on, for a head unit that
    /// kept one from a connection that ended during it, and then a call or Siri that is on now, starting a recording
    /// microphone again for the head unit to pick its source anew.
    pub fn control_up(&mut self) -> Vec<Out> {
        log!(
            "head unit told: {}, as the session starts",
            Phone::Idle.describe()
        );
        self.phone = Phone::Idle;
        let mut out = vec![Out::Phone(Phone::Idle)];
        self.tell_phone(&mut out, true);
        out
    }

    /// A packet of a stream's PCM, in its player's format: as it is when the stream plays alone, mixed with what waits
    /// of the others when it leads its player, else kept to be mixed into the one that leads.
    fn pcm(&mut self, out: &mut Vec<Out>, stream: AudioStream, samples: Vec<u8>) {
        // The head unit drops the sound of a type it has no player for.
        let Some(playing) = self.streams.get_mut(&stream) else {
            return;
        };
        let audio_type = playing.audio_type;
        let Some(&player) = self.players.get(&audio_type) else {
            return;
        };
        let mut samples = match &mut playing.converter {
            Some(converter) => converter.convert(&samples),
            None => samples,
        };
        let others = self.sharing(stream, audio_type);
        let order = self.streams[&stream].order;
        let leads = others.iter().all(|other| self.streams[other].order > order);
        if !leads {
            if let Some(playing) = self.streams.get_mut(&stream) {
                playing.pending.push(&samples, player);
            }
            return;
        }
        for other in others {
            if let Some(playing) = self.streams.get_mut(&other) {
                playing.pending.mix_into(&mut samples);
            }
        }
        out.push(Out::Pcm {
            audio_type,
            samples,
        });
    }

    /// The head unit's recording, in the frames the iPhone takes, with the stream type they go to.
    pub fn microphone(&mut self, recording: &[u8]) -> Option<(u16, Vec<Vec<u8>>)> {
        let microphone = self.microphone.as_mut()?;
        Some((microphone.stream_type, microphone.framer.push(recording)))
    }

    /// Stops every player and the microphone and ends a call or Siri, as when the iPhone's session ends.
    pub fn reset(&mut self) -> Vec<Out> {
        let mut out = Vec::new();
        let mut players: Vec<_> = self.players.drain().collect();
        players.sort_by_key(|(audio_type, _)| *audio_type as i32);
        for (audio_type, format) in players {
            out.push(Out::Player {
                audio_type,
                state: AudioPlayerState::PlayerStop,
                format,
            });
        }
        self.streams.clear();
        if let Some(microphone) = self.microphone.take() {
            log!("microphone off");
            out.push(self.microphone_out(false, microphone.format));
        }
        self.tell_phone(&mut out, false);
        out
    }

    /// Stops the player of `audio_type` once no stream plays as it.
    fn stop_player_if_unused(
        &mut self,
        out: &mut Vec<Out>,
        audio_type: AudioType,
        stream: AudioStream,
    ) {
        if self.streams.values().any(|p| p.audio_type == audio_type) {
            return;
        }
        if let Some(format) = self.players.remove(&audio_type) {
            log!(
                "audio {:?} on stream {} as {audio_type:?} stopped",
                stream.kind,
                stream.stream_type
            );
            out.push(Out::Player {
                audio_type,
                state: AudioPlayerState::PlayerStop,
                format,
            });
        }
    }

    /// Tells the head unit to start or stop recording in `format`; the detailed log notes it with the phone state the
    /// head unit has then, which picks its recording source.
    fn microphone_out(&self, on: bool, format: Format) -> Out {
        detail!(
            "microphone {} to the head unit: {} Hz, {} channel(s), phone state: {}",
            if on { "on" } else { "off" },
            format.sample_rate,
            format.channels,
            self.phone.describe()
        );
        Out::Microphone { on, format }
    }

    /// A call while a call's sound or microphone goes on, else Siri while Siri's does.
    fn phone_now(&self) -> Phone {
        // By the streams' own kinds, not the audio types they play as: navigation played as a call is no call.
        let kinds = self
            .streams
            .keys()
            .map(|s| s.kind)
            .chain(self.microphone.as_ref().map(|m| m.kind));
        let mut phone = Phone::Idle;
        for kind in kinds {
            match kind {
                AudioKind::Call => return Phone::Call,
                AudioKind::Assistant => phone = Phone::Assistant,
                _ => {}
            }
        }
        phone
    }

    /// Tells the head unit when a call or Siri starts or ends, and, with `restart`, starts a recording microphone
    /// again for the head unit to pick its source anew.
    fn tell_phone(&mut self, out: &mut Vec<Out>, restart: bool) {
        let phone = self.phone_now();
        if phone == self.phone {
            return;
        }
        log!(
            "head unit told: {} (was {})",
            phone.describe(),
            self.phone.describe()
        );
        self.phone = phone;
        out.push(Out::Phone(phone));
        if let Some(microphone) = self.microphone.as_ref().filter(|_| restart) {
            log!("microphone started again for {}", phone.describe());
            out.push(self.microphone_out(false, microphone.format));
            out.push(self.microphone_out(true, microphone.format));
        }
    }
}

/// The streams and microphones of the iPhone that have started and not stopped, for a connection to a head unit that
/// begins while they go on.
#[derive(Default)]
pub struct Replay {
    started: Vec<AudioEvent>,
}

impl Replay {
    /// Notes an event on its way to the connection.
    pub fn note(&mut self, event: &AudioEvent) {
        match event {
            AudioEvent::Started { stream, .. } | AudioEvent::Stopped { stream } => {
                self.started
                    .retain(|e| !matches!(e, AudioEvent::Started { stream: s, .. } if s == stream));
                if matches!(event, AudioEvent::Started { .. }) {
                    self.started.push(event.clone());
                }
            }
            AudioEvent::MicrophoneStarted { stream_type, .. }
            | AudioEvent::MicrophoneStopped { stream_type } => {
                self.started.retain(|e| {
                    !matches!(e, AudioEvent::MicrophoneStarted { stream_type: s, .. } if s == stream_type)
                });
                if matches!(event, AudioEvent::MicrophoneStarted { .. }) {
                    self.started.push(event.clone());
                }
            }
            AudioEvent::SessionEnded => self.started.clear(),
            AudioEvent::Music { .. } | AudioEvent::Pcm { .. } => {}
        }
    }

    /// The starts of what goes on, in the order they came.
    pub fn started(&self) -> &[AudioEvent] {
        &self.started
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn stream(stream_type: u16, kind: AudioKind) -> AudioStream {
        AudioStream { stream_type, kind }
    }

    fn started(stream: AudioStream, sample_rate: u32, channels: u8) -> AudioEvent {
        AudioEvent::Started {
            stream,
            music: false,
            sample_rate,
            channels,
        }
    }

    fn microphone(kind: AudioKind) -> AudioEvent {
        AudioEvent::MicrophoneStarted {
            stream_type: 100,
            kind,
            sample_rate: 48000,
            channels: 1,
            frame_bytes: 1920,
        }
    }

    const MONO_48K: Format = Format {
        sample_rate: 48000,
        channels: 1,
    };
    const MONO_16K: Format = Format {
        sample_rate: 16000,
        channels: 1,
    };

    fn player(audio_type: AudioType, state: AudioPlayerState, format: Format) -> Out {
        Out::Player {
            audio_type,
            state,
            format,
        }
    }

    #[test]
    fn audio_types() {
        assert_eq!(audio_type(AudioKind::Media), AudioType::AudioCastMusic);
        assert_eq!(audio_type(AudioKind::Call), AudioType::AudioIpCall);
        assert_eq!(
            audio_type(AudioKind::Assistant),
            AudioType::AudioAiAssistant
        );
        assert_eq!(audio_type(AudioKind::Alert), AudioType::AudioRing);
        assert_eq!(audio_type(AudioKind::Speech), AudioType::AudioTts);
        assert_eq!(audio_type(AudioKind::System), AudioType::AudioSystem);
    }

    #[test]
    fn the_head_unit_starts_idle() {
        let mut sound = Sound::new();
        assert_eq!(sound.control_up(), [Out::Phone(Phone::Idle)]);
        // Siri goes on as the head unit connects: idle first, then Siri, its microphone started again.
        let siri = stream(100, AudioKind::Assistant);
        let mut out = sound.take(started(siri, 16000, 1));
        out.extend(sound.take(microphone(AudioKind::Assistant)));
        assert!(out.contains(&Out::Phone(Phone::Assistant)));
        assert_eq!(
            sound.control_up(),
            [
                Out::Phone(Phone::Idle),
                Out::Phone(Phone::Assistant),
                Out::Microphone {
                    on: false,
                    format: MONO_48K
                },
                Out::Microphone {
                    on: true,
                    format: MONO_48K
                },
            ]
        );
        // Its own bookkeeping follows: Siri ending tells the head unit once.
        assert_eq!(
            sound.take(AudioEvent::MicrophoneStopped { stream_type: 100 }),
            [Out::Microphone {
                on: false,
                format: MONO_48K
            }]
        );
        let ended = sound.take(AudioEvent::Stopped { stream: siri });
        assert_eq!(
            ended,
            [
                player(
                    AudioType::AudioAiAssistant,
                    AudioPlayerState::PlayerStop,
                    MONO_16K
                ),
                Out::Phone(Phone::Idle),
            ]
        );
        assert_eq!(sound.control_up(), [Out::Phone(Phone::Idle)]);
    }

    #[test]
    fn a_call() {
        let mut sound = Sound::new();
        let call = stream(100, AudioKind::Call);
        // The call's sound comes first, then its microphone, once the downlink runs.
        assert_eq!(
            sound.take(started(call, 16000, 1)),
            [
                player(
                    AudioType::AudioIpCall,
                    AudioPlayerState::PlayerStart,
                    MONO_16K
                ),
                Out::Phone(Phone::Call),
            ]
        );
        assert_eq!(
            sound.take(microphone(AudioKind::Call)),
            [Out::Microphone {
                on: true,
                format: MONO_48K
            }]
        );
        assert_eq!(
            sound.take(AudioEvent::Pcm {
                stream: call,
                samples: vec![1, 2]
            }),
            [Out::Pcm {
                audio_type: AudioType::AudioIpCall,
                samples: vec![1, 2]
            }]
        );
        let (stream_type, frames) = sound.microphone(&[0; 4000]).unwrap();
        assert_eq!((stream_type, frames.len()), (100, 2));
        // The microphone goes first; the call is over once its sound stops too.
        assert_eq!(
            sound.take(AudioEvent::MicrophoneStopped { stream_type: 100 }),
            [Out::Microphone {
                on: false,
                format: MONO_48K
            }]
        );
        assert_eq!(
            sound.take(AudioEvent::Stopped { stream: call }),
            [
                player(
                    AudioType::AudioIpCall,
                    AudioPlayerState::PlayerStop,
                    MONO_16K
                ),
                Out::Phone(Phone::Idle),
            ]
        );
        assert!(sound.microphone(&[0; 4]).is_none());
    }

    #[test]
    fn siri_with_its_microphone_first() {
        let mut sound = Sound::new();
        assert_eq!(
            sound.take(microphone(AudioKind::Assistant)),
            [
                Out::Phone(Phone::Assistant),
                Out::Microphone {
                    on: true,
                    format: MONO_48K
                },
            ]
        );
        let siri = stream(100, AudioKind::Assistant);
        assert_eq!(
            sound.take(started(siri, 48000, 1)),
            [player(
                AudioType::AudioAiAssistant,
                AudioPlayerState::PlayerStart,
                MONO_48K
            )]
        );
        assert_eq!(
            sound.take(AudioEvent::Stopped { stream: siri }),
            [player(
                AudioType::AudioAiAssistant,
                AudioPlayerState::PlayerStop,
                MONO_48K
            )]
        );
        assert_eq!(
            sound.take(AudioEvent::MicrophoneStopped { stream_type: 100 }),
            [
                Out::Microphone {
                    on: false,
                    format: MONO_48K
                },
                Out::Phone(Phone::Idle),
            ]
        );
    }

    #[test]
    fn microphone_starts_again_when_a_call_comes_while_it_records() {
        let mut sound = Sound::new();
        sound.take(microphone(AudioKind::Speech));
        assert_eq!(
            sound.take(started(stream(100, AudioKind::Call), 16000, 1)),
            [
                player(
                    AudioType::AudioIpCall,
                    AudioPlayerState::PlayerStart,
                    MONO_16K
                ),
                Out::Phone(Phone::Call),
                Out::Microphone {
                    on: false,
                    format: MONO_48K
                },
                Out::Microphone {
                    on: true,
                    format: MONO_48K
                },
            ]
        );
    }

    #[test]
    fn a_call_outranks_siri() {
        let mut sound = Sound::new();
        sound.take(started(stream(100, AudioKind::Assistant), 48000, 1));
        let out = sound.take(started(stream(101, AudioKind::Call), 16000, 1));
        assert!(out.contains(&Out::Phone(Phone::Call)));
        let out = sound.take(AudioEvent::Stopped {
            stream: stream(101, AudioKind::Call),
        });
        assert!(out.contains(&Out::Phone(Phone::Assistant)));
    }

    #[test]
    fn a_new_format_starts_the_player_again() {
        let mut sound = Sound::new();
        let speech = stream(100, AudioKind::Speech);
        sound.take(started(speech, 48000, 1));
        // The same format again leaves the player as it is.
        assert!(sound.take(started(speech, 48000, 1)).is_empty());
        let stereo = Format {
            sample_rate: 44100,
            channels: 2,
        };
        assert_eq!(
            sound.take(started(speech, 44100, 2)),
            [
                player(AudioType::AudioTts, AudioPlayerState::PlayerStop, MONO_48K),
                player(AudioType::AudioTts, AudioPlayerState::PlayerStart, stereo),
            ]
        );
    }

    #[test]
    fn streams_of_one_type_share_its_player() {
        let mut sound = Sound::new();
        let (main, alt) = (
            stream(100, AudioKind::Speech),
            stream(101, AudioKind::Speech),
        );
        assert_eq!(sound.take(started(main, 48000, 1)).len(), 1);
        assert!(sound.take(started(alt, 48000, 1)).is_empty());
        assert!(sound.take(AudioEvent::Stopped { stream: main }).is_empty());
        assert_eq!(
            sound.take(AudioEvent::Stopped { stream: alt }),
            [player(
                AudioType::AudioTts,
                AudioPlayerState::PlayerStop,
                MONO_48K
            )]
        );
    }

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn sent(out: &[Out]) -> Vec<(AudioType, Vec<u8>)> {
        out.iter()
            .filter_map(|out| match out {
                Out::Pcm {
                    audio_type,
                    samples,
                } => Some((*audio_type, samples.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn navigation_plays_as_its_setting_has_it() {
        for (nav, audio_type) in [
            (NavAudio::Tts, AudioType::AudioTts),
            (NavAudio::Media, AudioType::AudioUndefined),
            (NavAudio::Call, AudioType::AudioIpCall),
            (NavAudio::Assistant, AudioType::AudioAiAssistant),
        ] {
            assert_eq!(parse_nav_audio(nav_audio_name(nav)), Some(nav));
            let mut sound = Sound::new();
            sound.set_nav(nav);
            let nav_stream = stream(100, AudioKind::Speech);
            // Navigation played as a call or as Siri is neither to the head unit.
            assert_eq!(
                sound.take(started(nav_stream, 48000, 1)),
                [player(audio_type, AudioPlayerState::PlayerStart, MONO_48K)]
            );
            assert_eq!(
                sent(&sound.take(AudioEvent::Pcm {
                    stream: nav_stream,
                    samples: vec![1, 2]
                })),
                [(audio_type, vec![1, 2])]
            );
        }
        assert_eq!(parse_nav_audio("music"), None);
        // Other streams keep their types.
        let mut sound = Sound::new();
        sound.set_nav(NavAudio::Call);
        assert_eq!(sound.audio_type_of(AudioKind::Alert), AudioType::AudioRing);
    }

    #[test]
    fn a_new_setting_applies_from_the_next_prompt() {
        let mut sound = Sound::new();
        let nav = stream(100, AudioKind::Speech);
        sound.take(started(nav, 48000, 1));
        sound.set_nav(NavAudio::Media);
        // The prompt that plays goes on as it started.
        assert_eq!(
            sent(&sound.take(AudioEvent::Pcm {
                stream: nav,
                samples: vec![1, 2]
            }))[0]
                .0,
            AudioType::AudioTts
        );
        let ended = sound.take(AudioEvent::Stopped { stream: nav });
        assert_eq!(
            ended,
            [player(
                AudioType::AudioTts,
                AudioPlayerState::PlayerStop,
                MONO_48K
            )]
        );
        assert_eq!(
            sound.take(started(nav, 48000, 1)),
            [player(
                AudioType::AudioUndefined,
                AudioPlayerState::PlayerStart,
                MONO_48K
            )]
        );
    }

    #[test]
    fn navigation_in_the_player_of_a_call() {
        let mut sound = Sound::new();
        sound.set_nav(NavAudio::Call);
        let (call, nav) = (stream(101, AudioKind::Call), stream(100, AudioKind::Speech));
        assert!(sound
            .take(started(call, 16000, 1))
            .contains(&Out::Phone(Phone::Call)));
        // Navigation joins the call's player at 16 kHz: no player starts again, and the phone state stays.
        assert!(sound.take(started(nav, 48000, 1)).is_empty());
        // Its prompt, three times the rate, waits to be mixed into the call's next packet.
        assert!(sound
            .take(AudioEvent::Pcm {
                stream: nav,
                samples: pcm(&[300, 300, 300, 600, 600, 600])
            })
            .is_empty());
        assert_eq!(
            sent(&sound.take(AudioEvent::Pcm {
                stream: call,
                samples: pcm(&[10, 10, 10])
            })),
            [(AudioType::AudioIpCall, pcm(&[310, 610, 10]))]
        );
        // The call ends: its player stays, and navigation goes on in its format, alone.
        let ended = sound.take(AudioEvent::Stopped { stream: call });
        assert_eq!(ended, [Out::Phone(Phone::Idle)]);
        assert_eq!(
            sent(&sound.take(AudioEvent::Pcm {
                stream: nav,
                samples: pcm(&[900, 900, 900])
            })),
            [(AudioType::AudioIpCall, pcm(&[900]))]
        );
        assert_eq!(
            sound.take(AudioEvent::Stopped { stream: nav }),
            [player(
                AudioType::AudioIpCall,
                AudioPlayerState::PlayerStop,
                MONO_16K
            )]
        );
    }

    #[test]
    fn a_call_into_the_player_navigation_started() {
        let mut sound = Sound::new();
        sound.set_nav(NavAudio::Call);
        let (call, nav) = (stream(101, AudioKind::Call), stream(100, AudioKind::Speech));
        assert_eq!(
            sound.take(started(nav, 48000, 1)),
            [player(
                AudioType::AudioIpCall,
                AudioPlayerState::PlayerStart,
                MONO_48K
            )]
        );
        // The call joins navigation's player at 48 kHz; the head unit is told of the call all the same.
        assert_eq!(
            sound.take(started(call, 16000, 1)),
            [Out::Phone(Phone::Call)]
        );
        assert!(sound
            .take(AudioEvent::Pcm {
                stream: call,
                samples: pcm(&[0, 300])
            })
            .is_empty());
        let mixed = sent(&sound.take(AudioEvent::Pcm {
            stream: nav,
            samples: pcm(&[1, 1, 1, 1, 1]),
        }));
        assert_eq!(
            mixed,
            [(AudioType::AudioIpCall, pcm(&[1, 101, 201, 301, 1]))]
        );
        // Navigation ends: the call leads, and what of it waited goes out first.
        assert!(sound
            .take(AudioEvent::Pcm {
                stream: call,
                samples: pcm(&[600])
            })
            .is_empty());
        assert_eq!(
            sound.take(AudioEvent::Stopped { stream: nav }),
            [Out::Pcm {
                audio_type: AudioType::AudioIpCall,
                samples: pcm(&[400, 500, 600])
            }]
        );
        assert_eq!(
            sent(&sound.take(AudioEvent::Pcm {
                stream: call,
                samples: pcm(&[900])
            })),
            [(AudioType::AudioIpCall, pcm(&[700, 800, 900]))]
        );
        assert!(sound
            .take(AudioEvent::Stopped { stream: call })
            .contains(&Out::Phone(Phone::Idle)));
    }

    #[test]
    fn unknown_streams_and_music() {
        let mut sound = Sound::new();
        assert!(sound
            .take(AudioEvent::Pcm {
                stream: stream(100, AudioKind::Alert),
                samples: vec![0; 4]
            })
            .is_empty());
        assert!(sound
            .take(AudioEvent::Stopped {
                stream: stream(100, AudioKind::Alert)
            })
            .is_empty());
        let media = stream(102, AudioKind::Media);
        assert!(sound
            .take(AudioEvent::Started {
                stream: media,
                music: true,
                sample_rate: 48000,
                channels: 2
            })
            .is_empty());
        assert_eq!(
            sound.take(AudioEvent::Music {
                stream: media,
                adts: vec![0xFF]
            }),
            [Out::Music(vec![0xFF])]
        );
        assert!(sound
            .take(AudioEvent::MicrophoneStopped { stream_type: 100 })
            .is_empty());
    }

    #[test]
    fn the_end_of_the_session_ends_everything() {
        let mut sound = Sound::new();
        sound.take(started(stream(100, AudioKind::Call), 16000, 1));
        sound.take(started(stream(101, AudioKind::Alert), 48000, 1));
        sound.take(microphone(AudioKind::Call));
        assert_eq!(
            sound.take(AudioEvent::SessionEnded),
            [
                player(
                    AudioType::AudioIpCall,
                    AudioPlayerState::PlayerStop,
                    MONO_16K
                ),
                player(AudioType::AudioRing, AudioPlayerState::PlayerStop, MONO_48K),
                Out::Microphone {
                    on: false,
                    format: MONO_48K
                },
                Out::Phone(Phone::Idle),
            ]
        );
        assert!(sound.reset().is_empty());
    }

    #[test]
    fn messages() {
        let message = Out::Phone(Phone::Call).control().unwrap();
        assert_eq!(message.category, Category::CONTROL);
        assert_eq!(message.command, command::control::NOTIFY_PHONE_STATE);
        let state = NotifyPhoneState::decode(&message.body[..]).unwrap();
        assert_eq!(state.cs, CallState::CallActive as i32);
        assert!(state.is_wechat_qq_call && !state.is_voice_assistant_active);

        let state =
            NotifyPhoneState::decode(&Out::Phone(Phone::Assistant).control().unwrap().body[..])
                .unwrap();
        assert_eq!(state.cs, CallState::CallIdle as i32);
        assert!(!state.is_wechat_qq_call && state.is_voice_assistant_active);

        let state =
            NotifyPhoneState::decode(&Out::Phone(Phone::Idle).control().unwrap().body[..]).unwrap();
        assert_eq!(state.cs, CallState::CallIdle as i32);
        assert!(!state.is_wechat_qq_call && !state.is_voice_assistant_active);

        let message = player(
            AudioType::AudioIpCall,
            AudioPlayerState::PlayerStart,
            MONO_16K,
        )
        .control()
        .unwrap();
        assert_eq!(message.command, command::control::NOTIFY_AUDIO_PLAYER_STATE);
        let notice = NotifyAudioPlayerState::decode(&message.body[..]).unwrap();
        assert_eq!(notice.r#type, AudioType::AudioIpCall as i32);
        assert_eq!(notice.state, AudioPlayerState::PlayerStart as i32);
        assert_eq!(notice.sample_rate, 16000);
        assert_eq!(notice.channel_mask, ChannelMask::ChannelMono as i32);

        let message = Out::Microphone {
            on: true,
            format: MONO_48K,
        }
        .control()
        .unwrap();
        assert_eq!(message.command, command::control::NOTIFY_MICROPHONE_STATE);
        let notice = NotifyMicrophoneState::decode(&message.body[..]).unwrap();
        assert!(notice.state);
        assert_eq!(notice.sample_rate, 48000);

        assert!(Out::Music(Vec::new()).control().is_none());
    }

    #[test]
    fn replay() {
        let mut replay = Replay::default();
        let (call, alert) = (stream(100, AudioKind::Call), stream(101, AudioKind::Alert));
        replay.note(&started(call, 16000, 1));
        replay.note(&started(alert, 48000, 1));
        replay.note(&microphone(AudioKind::Call));
        replay.note(&AudioEvent::Pcm {
            stream: call,
            samples: vec![0; 2],
        });
        replay.note(&AudioEvent::Stopped { stream: alert });
        assert_eq!(replay.started().len(), 2);
        // A connection that begins now hears the call and records for it.
        let mut sound = Sound::new();
        let out: Vec<_> = replay
            .started()
            .iter()
            .flat_map(|e| sound.take(e.clone()))
            .collect();
        assert!(out.contains(&Out::Phone(Phone::Call)));
        assert!(out.contains(&Out::Microphone {
            on: true,
            format: MONO_48K
        }));
        replay.note(&AudioEvent::MicrophoneStopped { stream_type: 100 });
        assert_eq!(replay.started().len(), 1);
        replay.note(&AudioEvent::SessionEnded);
        assert!(replay.started().is_empty());
    }
}
