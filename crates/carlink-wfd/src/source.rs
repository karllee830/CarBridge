// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The phone's side of Wi-Fi Display: the RTSP exchange with the head unit's sink, and the stream it sets up.
//!
//! The phone casts an MPEG-2 transport stream over RTP on TCP: it does not offer `ovm_control`, so the sink does not
//! read `wfd_av_raw_packet` and expects a transport stream.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{interval, timeout, Instant, MissedTickBehavior};

use crate::rtp::Packetizer;
use crate::rtsp::{parameters, Message};
use crate::ts::Muxer;
use crate::Codec;

/// The port the phone serves RTSP on.
pub const RTSP_PORT: u16 = 7236;
/// The port the phone listens on for the input back channel.
pub const UIBC_PORT: u16 = 4321;
/// How long the RTSP exchange may take, from the head unit's connection to `PLAY`.
pub const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the phone sends a keep-alive `GET_PARAMETER`.
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(20);

const PUBLIC: &str = "org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER";
const CONTROL_URI: &str = "rtsp://localhost/wfd1.0";
const SESSION_ID: &str = "4d7a1c2e";
/// The Carlink parameter by which a head unit offers H.265 in M3, and by which the phone chooses it in M4.
pub const HEVC_PARAMETER: &str = "iccoa_support_hevc";
/// Parameters asked for in M3.
const M3_PARAMETERS: &[&str] = &[
    "wfd_video_formats",
    "wfd_audio_codecs",
    "wfd_client_rtp_ports",
    "wfd_uibc_capability",
    "wfd_car_display_mode",
    "wfd_audio_latency",
    "wfd_content_protection",
];

/// H.264 levels as `wfd_video_formats` names them: the level's bit, its name, and the largest frame (in macroblocks)
/// and macroblock rate it allows. Levels 5 to 5.2 are the bits Wi-Fi Display R2 adds.
const H264_LEVELS: &[(u8, &str, u32, u32)] = &[
    (0x01, "3.1", 3_600, 108_000),
    (0x02, "3.2", 5_120, 216_000),
    (0x04, "4", 8_192, 245_760),
    (0x08, "4.1", 8_192, 245_760),
    (0x10, "4.2", 8_704, 522_240),
    (0x20, "5", 22_080, 589_824),
    (0x40, "5.1", 36_864, 983_040),
    (0x80, "5.2", 36_864, 2_073_600),
];

/// The video the phone claims in M4's `wfd_video_formats`: Constrained Baseline in the 1280x720 mode, at 30 frames a
/// second for a picture of up to 30 and at 60 otherwise, and the smallest H.264 level that holds the picture the
/// phone really sends. Head units name a small level in M3 but decode a larger picture, whose size the decoder takes
/// from the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl VideoMode {
    /// The mode's frame rate: 30 or 60.
    pub fn timing(&self) -> u32 {
        if self.fps <= 30 {
            30
        } else {
            60
        }
    }

    /// The level's bit in `wfd_video_formats` and its name.
    pub fn level(&self) -> (u8, &'static str) {
        let (columns, rows) = (self.width.div_ceil(16), self.height.div_ceil(16));
        let frame = columns * rows;
        let rate = frame.saturating_mul(self.fps.max(1));
        let level = H264_LEVELS.iter().find(|&&(_, _, max_frame, max_rate)| {
            // A side may not be longer than √(8 × the largest frame) macroblocks.
            let longest = columns.max(rows);
            frame <= max_frame && rate <= max_rate && longest * longest <= 8 * max_frame
        });
        let &(bit, name, _, _) = level.unwrap_or(&H264_LEVELS[H264_LEVELS.len() - 1]);
        (bit, name)
    }

    /// The `wfd_video_formats` line.
    pub fn format(&self) -> String {
        // CEA modes 5 and 6: 1280x720p30 and 1280x720p60. The native mode names the table (0, CEA) in its low three
        // bits and the mode above them.
        let cea = if self.timing() == 30 { 5 } else { 6 };
        format!(
            "{:02x} 00 01 {:02x} {:08x} 00000000 00000000 00 0000 0000 00 none none",
            cea << 3,
            self.level().0,
            1u32 << cea
        )
    }
}

impl fmt::Display for VideoMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "1280x720p{}, H.264 level {} for {}x{} at {} fps",
            self.timing(),
            self.level().1,
            self.width,
            self.height,
            self.fps
        )
    }
}

/// Why the cast could not start or ended.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A message that is not RTSP.
    Rtsp(String),
    /// The sink answered a request with an error status.
    Status {
        request: &'static str,
        status: u16,
    },
    /// The sink's `SETUP` had no usable `client_port`.
    Transport(String),
    /// The exchange took longer than [`NEGOTIATION_TIMEOUT`].
    Timeout,
    /// The sink closed the RTSP connection.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "{err}"),
            Error::Rtsp(err) => write!(f, "RTSP: {err}"),
            Error::Status { request, status } => write!(f, "sink answered {request} with {status}"),
            Error::Transport(transport) => write!(f, "unusable transport {transport:?}"),
            Error::Timeout => f.write_str("RTSP exchange timed out"),
            Error::Closed => f.write_str("sink closed the RTSP connection"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

/// The phone's choices for the cast.
#[derive(Clone, Debug)]
pub struct Choices {
    /// The phone's address on the head unit's network.
    pub phone: Ipv4Addr,
    /// Where the phone listens for the input back channel.
    pub uibc_port: u16,
    /// `wfd_video_formats` the phone sends in M4.
    pub video_format: String,
    /// `wfd_audio_codecs` the phone sends in M4.
    pub audio_codec: String,
    /// The IPv4 type of service of the RTP connection, which carries the picture and the sound.
    pub tos: Option<u32>,
    /// The density the head unit gives the phone to lay out with, which an H.265 cast names in M4.
    pub head_unit_dpi: u32,
}

impl Choices {
    pub fn new(phone: Ipv4Addr) -> Choices {
        Choices {
            phone,
            uibc_port: UIBC_PORT,
            video_format: VideoMode {
                width: 1280,
                height: 720,
                fps: 30,
            }
            .format(),
            // AAC, 48 kHz stereo.
            audio_codec: "AAC 00000001 00".into(),
            tos: None,
            head_unit_dpi: 320,
        }
    }

    /// The M4 parameters for a cast in `codec`. With H.265, they tell the head unit to decode H.265, whether or not it
    /// offered it, and name the car display mode and standby capability as well: head units that take H.265 only so
    /// set up their decoder for it.
    fn m4(&self, codec: Codec) -> String {
        let hevc = match codec {
            Codec::H264 => String::new(),
            Codec::H265 => format!(
                "{HEVC_PARAMETER}: supported\r\n\
                 wfd_standby_resume_capability: none\r\n\
                 wfd_car_display_mode: mode=0;display=1280:720;dpi={};fps=30\r\n",
                self.head_unit_dpi
            ),
        };
        format!(
            "wfd_video_formats: {}\r\n\
             {hevc}\
             wfd_audio_codecs: {}\r\n\
             wfd_client_rtp_ports: RTP/AVP/TCP;unicast 19000 0 mode=play\r\n\
             wfd_presentation_URL: rtsp://{}:{RTSP_PORT}/wfd1.0/streamid=0 none\r\n\
             wfd_uibc_capability: input_category_list=GENERIC;generic_cap_list=Keyboard, MultiTouch;hidc_cap_list=none;port={};uibc_encrypted=true\r\n\
             wfd_uibc_setting: enable\r\n\
             wfd_content_protection: none\r\n",
            self.video_format, self.audio_codec, self.phone, self.uibc_port
        )
    }
}

/// Connects to `address`, sending with `tos` as the IPv4 type of service.
async fn connect(address: SocketAddrV4, tos: Option<u32>) -> io::Result<TcpStream> {
    let socket = TcpSocket::new_v4()?;
    if let Some(tos) = tos {
        socket.set_tos_v4(tos)?;
    }
    socket.connect(address.into()).await
}

/// What the sink asks of the phone during the cast.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinkEvent {
    /// The sink's decoder needs a key frame.
    IdrRequest,
    /// The sink ended the cast.
    Teardown,
    /// The RTSP connection closed.
    Closed,
}

/// A cast set up and playing.
pub struct Cast {
    /// The sink's answer to M3.
    pub sink_parameters: Vec<(String, String)>,
    /// The video codec chosen in M4.
    pub codec: Codec,
    /// The connection to the sink's RTP port.
    pub rtp: TcpStream,
    /// The RTSP connection, to keep alive with [`Cast::serve_rtsp`].
    pub rtsp: Rtsp,
}

/// The RTSP connection with its read buffer and the phone's sequence numbers.
pub struct Rtsp {
    stream: TcpStream,
    buf: Vec<u8>,
    cseq: u32,
}

impl Rtsp {
    fn new(stream: TcpStream) -> Rtsp {
        let _ = stream.set_nodelay(true);
        Rtsp {
            stream,
            buf: Vec::new(),
            cseq: 0,
        }
    }

    async fn recv(&mut self) -> Result<Message, Error> {
        loop {
            if let Some((message, used)) = Message::parse(&self.buf).map_err(Error::Rtsp)? {
                self.buf.drain(..used);
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Err(Error::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn send(&mut self, message: &Message) -> Result<(), Error> {
        self.stream.write_all(&message.encode()).await?;
        Ok(())
    }

    /// Sends a request with the next sequence number and returns that number.
    async fn request(
        &mut self,
        method: &str,
        uri: &str,
        parameters: Option<String>,
    ) -> Result<u32, Error> {
        self.cseq += 1;
        let mut message = Message::request(method, uri, self.cseq);
        if method == "OPTIONS" {
            message.headers_mut().push("Require", "org.wfa.wfd1.0");
        }
        if let Some(text) = parameters {
            message = message.with_parameters(text);
        }
        self.send(&message).await?;
        Ok(self.cseq)
    }

    /// Answers a request from the sink with `200 OK`, plus `extra` headers.
    async fn ok(&mut self, request: &Message, extra: &[(&str, String)]) -> Result<(), Error> {
        let mut response = Message::ok(request.headers().cseq().unwrap_or(0));
        for (name, value) in extra {
            response.headers_mut().push(name, value.clone());
        }
        self.send(&response).await
    }

    /// Ends the cast from the phone's side.
    pub async fn teardown(&mut self) -> Result<(), Error> {
        self.request(
            "SET_PARAMETER",
            CONTROL_URI,
            Some("wfd_trigger_method: TEARDOWN\r\n".into()),
        )
        .await
        .map(drop)
    }
}

/// Runs the exchange on the connection the head unit opened to the phone's RTSP port, up to `PLAY`. Between the
/// sink's answer to M3 and M4, `choose_codec` picks the video codec, told whether the sink offered H.265.
pub async fn negotiate(
    stream: TcpStream,
    choices: &Choices,
    choose_codec: impl FnOnce(bool) -> Codec,
) -> Result<Cast, Error> {
    let head_unit = match stream.peer_addr()? {
        SocketAddr::V4(addr) => *addr.ip(),
        SocketAddr::V6(addr) => addr
            .ip()
            .to_ipv4_mapped()
            .ok_or_else(|| Error::Rtsp("IPv6 sink".into()))?,
    };
    timeout(
        NEGOTIATION_TIMEOUT,
        exchange(Rtsp::new(stream), head_unit, choices, choose_codec),
    )
    .await
    .map_err(|_| Error::Timeout)?
}

async fn exchange(
    mut rtsp: Rtsp,
    head_unit: Ipv4Addr,
    choices: &Choices,
    choose_codec: impl FnOnce(bool) -> Codec,
) -> Result<Cast, Error> {
    let mut choose_codec = Some(choose_codec);
    let mut codec = Codec::H264;
    let m1 = rtsp.request("OPTIONS", "*", None).await?;
    let (mut m1_done, mut m2_done) = (false, false);
    let (mut m3, mut m4, mut m5) = (None, None, None);
    let mut sink_parameters = Vec::new();
    let mut rtp = None;
    loop {
        let message = rtsp.recv().await?;
        match &message {
            Message::Response {
                status,
                headers,
                body,
                ..
            } => {
                let cseq = headers.cseq();
                let request = match cseq {
                    c if c == Some(m1) => "M1",
                    c if c.is_some() && c == m3 => "M3",
                    c if c.is_some() && c == m4 => "M4",
                    c if c.is_some() && c == m5 => "M5",
                    _ => continue,
                };
                if *status != 200 {
                    return Err(Error::Status {
                        request,
                        status: *status,
                    });
                }
                match request {
                    "M1" => m1_done = true,
                    "M3" => {
                        sink_parameters = parameters(body);
                        if let Some(choose) = choose_codec.take() {
                            codec = choose(offers_hevc(&sink_parameters));
                        }
                        m4 = Some(
                            rtsp.request("SET_PARAMETER", CONTROL_URI, Some(choices.m4(codec)))
                                .await?,
                        );
                    }
                    "M4" => {
                        let trigger = "wfd_trigger_method: SETUP\r\n".to_owned();
                        m5 = Some(
                            rtsp.request("SET_PARAMETER", CONTROL_URI, Some(trigger))
                                .await?,
                        );
                    }
                    _ => {}
                }
            }
            Message::Request {
                method, headers, ..
            } => match method.as_str() {
                "OPTIONS" => {
                    rtsp.ok(&message, &[("Public", PUBLIC.into())]).await?;
                    m2_done = true;
                }
                "SETUP" => {
                    let transport = headers.get("Transport").unwrap_or_default().to_owned();
                    let port = client_port(&transport)
                        .ok_or_else(|| Error::Transport(transport.clone()))?;
                    // The sink listens on its client port; the phone connects before it answers.
                    let stream = connect(SocketAddrV4::new(head_unit, port), choices.tos).await?;
                    let _ = stream.set_nodelay(true);
                    rtp = Some(stream);
                    let answer =
                        format!("RTP/AVP/TCP;unicast;client_port={port};server_port=19000-19001");
                    rtsp.ok(
                        &message,
                        &[
                            ("Session", format!("{SESSION_ID};timeout=30")),
                            ("Transport", answer),
                        ],
                    )
                    .await?;
                }
                "PLAY" => {
                    rtsp.ok(&message, &[("Session", SESSION_ID.into())]).await?;
                    let rtp = rtp.ok_or_else(|| Error::Rtsp("PLAY before SETUP".into()))?;
                    return Ok(Cast {
                        sink_parameters,
                        codec,
                        rtp,
                        rtsp,
                    });
                }
                _ => rtsp.ok(&message, &[]).await?,
            },
        }
        if m1_done && m2_done && m3.is_none() {
            let names: String = M3_PARAMETERS
                .iter()
                .map(|name| format!("{name}\r\n"))
                .collect();
            m3 = Some(
                rtsp.request("GET_PARAMETER", CONTROL_URI, Some(names))
                    .await?,
            );
        }
    }
}

fn offers_hevc(sink_parameters: &[(String, String)]) -> bool {
    sink_parameters
        .iter()
        .any(|(name, value)| name == HEVC_PARAMETER && value == "supported")
}

/// The RTP port in a `Transport` header's `client_port`.
fn client_port(transport: &str) -> Option<u16> {
    let value = transport
        .split(';')
        .find_map(|part| part.trim().strip_prefix("client_port="))?;
    value.split('-').next()?.trim().parse().ok()
}

impl Cast {
    /// Whether the sink offered H.265 in its answer to M3.
    pub fn offers_hevc(&self) -> bool {
        offers_hevc(&self.sink_parameters)
    }

    /// Serves the RTSP connection until the cast ends: sends keep-alives, answers the sink's requests and reports
    /// them. Returns why it ended.
    pub async fn serve_rtsp(
        mut rtsp: Rtsp,
        events: mpsc::Sender<SinkEvent>,
    ) -> Result<SinkEvent, Error> {
        let mut keep_alive = interval(KEEP_ALIVE_INTERVAL);
        keep_alive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        keep_alive.reset_at(Instant::now() + KEEP_ALIVE_INTERVAL);
        loop {
            tokio::select! {
                _ = keep_alive.tick() => { rtsp.request("GET_PARAMETER", CONTROL_URI, None).await?; }
                received = rtsp.recv() => {
                    let message = match received {
                        Ok(message) => message,
                        Err(Error::Closed) => {
                            let _ = events.send(SinkEvent::Closed).await;
                            return Ok(SinkEvent::Closed);
                        }
                        Err(err) => return Err(err),
                    };
                    let Message::Request { method, body, .. } = &message else { continue };
                    rtsp.ok(&message, &[]).await?;
                    let event = if method == "TEARDOWN" || body.contains("wfd_trigger_method: TEARDOWN") {
                        Some(SinkEvent::Teardown)
                    } else if body.contains("wfd_idr_request") {
                        Some(SinkEvent::IdrRequest)
                    } else {
                        None
                    };
                    if let Some(event) = event {
                        let _ = events.send(event).await;
                        if event == SinkEvent::Teardown {
                            return Ok(event);
                        }
                    }
                }
            }
        }
    }
}

/// The lower bounds of the lengths [`Stalls`] counts writes by, in milliseconds.
pub const STALL_BOUNDS_MS: [u32; 4] = [50, 100, 200, 500];

/// Writes to the sink that waited for room in the connection, counted by how long they took: from 50 ms, 100 ms,
/// 200 ms and 500 ms on ([`STALL_BOUNDS_MS`]), each write in the last of these it reaches.
#[derive(Debug, Default)]
pub struct Stalls {
    counts: [AtomicU32; 4],
    longest_ms: AtomicU32,
}

/// What [`Stalls::take`] gives: the writes in each length and the longest write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StallCounts {
    pub counts: [u32; 4],
    pub longest_ms: u32,
}

impl StallCounts {
    /// Whether no write stalled.
    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|&count| count == 0)
    }
}

impl Stalls {
    /// Counts a write that took `took`, if it took long enough to be a stall.
    pub fn add(&self, took: Duration) {
        let ms = u32::try_from(took.as_millis()).unwrap_or(u32::MAX);
        if let Some(bucket) = STALL_BOUNDS_MS.iter().rposition(|&bound| ms >= bound) {
            self.counts[bucket].fetch_add(1, Ordering::Relaxed);
            self.longest_ms.fetch_max(ms, Ordering::Relaxed);
        }
    }

    /// The writes counted since the last call, and starts again from none.
    pub fn take(&self) -> StallCounts {
        StallCounts {
            counts: std::array::from_fn(|i| self.counts[i].swap(0, Ordering::Relaxed)),
            longest_ms: self.longest_ms.swap(0, Ordering::Relaxed),
        }
    }
}

/// Muxes H.264 or H.265 (and AAC) into a transport stream and sends it to the sink over RTP.
pub struct Streamer {
    rtp: TcpStream,
    codec: Codec,
    muxer: Muxer,
    packetizer: Packetizer,
    ts: Vec<u8>,
    out: Vec<u8>,
    stalls: Option<Arc<Stalls>>,
}

impl Streamer {
    pub fn new(rtp: TcpStream, codec: Codec, audio: bool) -> Streamer {
        Streamer {
            rtp,
            codec,
            muxer: Muxer::new(codec, audio),
            packetizer: Packetizer::new(0x4341_524C),
            ts: Vec::new(),
            out: Vec::new(),
            stalls: None,
        }
    }

    /// Counts the writes that wait for room in the connection in `stalls`.
    pub fn count_stalls(mut self, stalls: Arc<Stalls>) -> Streamer {
        self.stalls = Some(stalls);
        self
    }

    /// The RTP connection to the sink.
    pub fn socket(&self) -> &TcpStream {
        &self.rtp
    }

    /// Sends one access unit in Annex B form, presented at `time` (90 kHz). Access unit delimiters are left out: the
    /// head unit's player tells a key frame by the first NAL unit of the access unit, which must be a parameter set
    /// or the key picture's slice.
    pub async fn video(
        &mut self,
        access_unit: &[u8],
        time: u64,
        key_frame: bool,
    ) -> io::Result<()> {
        self.ts.clear();
        let access_unit = self.codec.without_delimiters(access_unit);
        self.muxer
            .video(&access_unit, time, key_frame, &mut self.ts);
        self.flush(time).await
    }

    /// Sends one ADTS AAC frame presented at `time` (90 kHz).
    pub async fn audio(&mut self, adts_frame: &[u8], time: u64) -> io::Result<()> {
        self.ts.clear();
        self.muxer.audio(adts_frame, time, &mut self.ts);
        self.flush(time).await
    }

    async fn flush(&mut self, time: u64) -> io::Result<()> {
        self.out.clear();
        self.packetizer.packetize(&self.ts, time, &mut self.out);
        let Some(stalls) = &self.stalls else {
            return self.rtp.write_all(&self.out).await;
        };
        let started = std::time::Instant::now();
        let written = self.rtp.write_all(&self.out).await;
        stalls.add(started.elapsed());
        written
    }
}

#[cfg(test)]
mod tests;
