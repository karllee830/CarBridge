// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! What the detailed log adds while it is on: statistics of the car's microphone, of sound dropped on its way to the
//! head unit, of the cast's writes that waited for room, of the TCP connections to the head unit and of the Wi-Fi link
//! to it. Counting is cheap; the lines are formatted only for the detailed log.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use carbridge_carlink_proto::pb::AudioType;
use carbridge_carlink_wfd::source::{StallCounts, STALL_BOUNDS_MS};

use crate::sound::Format;

/// A sample this close to zero or closer, in a whole message of the head unit's recording, makes it silent: about
/// -60 dBFS.
pub const SILENT_PEAK: u16 = 33;

/// The peak and the power of 16-bit samples.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level {
    /// The largest magnitude of a sample.
    pub peak: u16,
    sum_squares: f64,
    samples: u64,
}

impl Level {
    /// The level of `pcm`, 16-bit little-endian samples; a byte left over is ignored.
    pub fn of(pcm: &[u8]) -> Level {
        let mut level = Level::default();
        for sample in pcm.chunks_exact(2) {
            let sample = i16::from_le_bytes([sample[0], sample[1]]);
            level.peak = level.peak.max(sample.unsigned_abs());
            level.sum_squares += f64::from(sample) * f64::from(sample);
            level.samples += 1;
        }
        level
    }

    /// Adds the samples of `other`.
    pub fn add(&mut self, other: &Level) {
        self.peak = self.peak.max(other.peak);
        self.sum_squares += other.sum_squares;
        self.samples += other.samples;
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// The root mean square of the samples, 0 for none.
    pub fn rms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            (self.sum_squares / self.samples as f64).sqrt()
        }
    }

    /// The peak in decibels of full scale.
    pub fn peak_dbfs(&self) -> f64 {
        dbfs(f64::from(self.peak))
    }

    /// The root mean square in decibels of full scale.
    pub fn rms_dbfs(&self) -> f64 {
        dbfs(self.rms())
    }
}

/// `magnitude` of a 16-bit sample in decibels of full scale; minus infinity for 0.
pub fn dbfs(magnitude: f64) -> f64 {
    20.0 * (magnitude / 32768.0).log10()
}

/// A level in dBFS as the log shows it.
fn show_dbfs(db: f64) -> String {
    if db.is_finite() {
        format!("{db:.1} dBFS")
    } else {
        "silence".to_owned()
    }
}

/// What became of a frame of the car's microphone on its way to the iPhone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicOutcome {
    /// Encoded to Opus and sent.
    Opus,
    /// Sent as PCM.
    Pcm,
    /// No microphone stream of the iPhone's took it.
    NoStream,
    /// The Opus encoder failed on it.
    EncodeFailed,
    /// Sending it failed.
    SendFailed,
}

/// The formats of the microphone: what the iPhone asked for and what the head unit was asked to record in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MicFormats {
    iphone: Format,
    frame_bytes: usize,
    head_unit: Format,
}

/// What the microphone did since the last report.
#[derive(Clone, Copy, Debug)]
struct MicWindow {
    since: Instant,
    messages: u64,
    bytes: u64,
    opus: u64,
    pcm: u64,
    no_stream: u64,
    encode_failed: u64,
    send_failed: u64,
    longest_gap: Duration,
    level: Level,
    /// Samples in messages whose every sample was quiet ([`SILENT_PEAK`]), and in those all zero.
    silent_samples: u64,
    zero_samples: u64,
}

impl MicWindow {
    fn new(since: Instant) -> MicWindow {
        MicWindow {
            since,
            messages: 0,
            bytes: 0,
            opus: 0,
            pcm: 0,
            no_stream: 0,
            encode_failed: 0,
            send_failed: 0,
            longest_gap: Duration::ZERO,
            level: Level::default(),
            silent_samples: 0,
            zero_samples: 0,
        }
    }
}

/// Statistics of the car's microphone while it is on: what the head unit sends, its level and silence, the gaps
/// between its messages, and what goes on to the iPhone.
#[derive(Debug)]
pub struct MicStats {
    /// The format the iPhone asked for last, and the size of its frames.
    iphone: Option<(Format, usize)>,
    /// The formats while the microphone is on.
    on: Option<MicFormats>,
    window: MicWindow,
    /// When the last message came from the head unit, or the microphone started.
    last_message: Option<Instant>,
}

impl MicStats {
    pub fn new(now: Instant) -> MicStats {
        MicStats {
            iphone: None,
            on: None,
            window: MicWindow::new(now),
            last_message: None,
        }
    }

    /// The iPhone asked for the microphone in `format`, in frames of `frame_bytes`.
    pub fn iphone_asked(&mut self, format: Format, frame_bytes: usize) {
        self.iphone = Some((format, frame_bytes));
    }

    /// The head unit was asked to start recording in `format`, or to stop. Gives the line for what the microphone did
    /// since the last report when it stops.
    pub fn head_unit_asked(&mut self, on: bool, format: Format, now: Instant) -> Option<String> {
        if on {
            let (iphone, frame_bytes) = self.iphone.unwrap_or((format, 0));
            if self.on.is_none() {
                self.window = MicWindow::new(now);
                self.last_message = Some(now);
            }
            self.on = Some(MicFormats {
                iphone,
                frame_bytes,
                head_unit: format,
            });
            None
        } else {
            let last = self.report(now);
            self.on = None;
            self.last_message = None;
            last
        }
    }

    /// A message of the head unit's recording came: 16-bit PCM in the format it was asked for.
    pub fn received(&mut self, pcm: &[u8], now: Instant) {
        let window = &mut self.window;
        window.messages += 1;
        window.bytes += pcm.len() as u64;
        if let Some(last) = self.last_message {
            window.longest_gap = window.longest_gap.max(now.saturating_duration_since(last));
        }
        self.last_message = Some(now);
        let level = Level::of(pcm);
        if level.peak <= SILENT_PEAK {
            window.silent_samples += level.samples();
            if level.peak == 0 {
                window.zero_samples += level.samples();
            }
        }
        window.level.add(&level);
    }

    /// A frame went on to the iPhone, or did not.
    pub fn sent(&mut self, outcome: MicOutcome) {
        let window = &mut self.window;
        match outcome {
            MicOutcome::Opus => window.opus += 1,
            MicOutcome::Pcm => window.pcm += 1,
            MicOutcome::NoStream => window.no_stream += 1,
            MicOutcome::EncodeFailed => window.encode_failed += 1,
            MicOutcome::SendFailed => window.send_failed += 1,
        }
    }

    /// The line for what the microphone did since the last report, while it is on, and starts the next report. The
    /// gap the head unit leaves until `now` counts with the gaps between its messages.
    pub fn report(&mut self, now: Instant) -> Option<String> {
        let formats = self.on?;
        let window = self.window;
        self.window = MicWindow::new(now);
        let elapsed = now.saturating_duration_since(window.since).as_secs_f64();
        let longest_gap = match self.last_message {
            Some(last) => window.longest_gap.max(now.saturating_duration_since(last)),
            None => window.longest_gap,
        };
        let head_unit = formats.head_unit;
        let samples_per_second =
            f64::from(head_unit.sample_rate) * f64::from(head_unit.channels.max(1));
        let seconds = |samples: u64| {
            if samples_per_second > 0.0 {
                samples as f64 / samples_per_second
            } else {
                0.0
            }
        };
        let received_rate = if elapsed > 0.0 {
            window.bytes as f64 / 2.0 / f64::from(head_unit.channels.max(1)) / elapsed
        } else {
            0.0
        };
        let mut line = format!(
            "microphone in {elapsed:.1} s: {} messages, {} bytes from the head unit (about {received_rate:.0} Hz), \
             longest gap {} ms",
            window.messages,
            window.bytes,
            longest_gap.as_millis()
        );
        if window.level.samples() > 0 {
            let _ = write!(
                line,
                ", peak {}, RMS {}",
                show_dbfs(window.level.peak_dbfs()),
                show_dbfs(window.level.rms_dbfs())
            );
        }
        let _ = write!(
            line,
            ", silent {:.2} s (all zero {:.2} s); to the iPhone: {} in Opus, {} in PCM, {} with no stream, {} not \
             encoded, {} not sent; iPhone asked {}, {}-byte frames; head unit asked {}",
            seconds(window.silent_samples),
            seconds(window.zero_samples),
            window.opus,
            window.pcm,
            window.no_stream,
            window.encode_failed,
            window.send_failed,
            show_format(formats.iphone),
            formats.frame_bytes,
            show_format(head_unit)
        );
        Some(line)
    }
}

fn show_format(format: Format) -> String {
    format!("{} Hz {} ch", format.sample_rate, format.channels)
}

/// Lets a line through at most once every `every`, counting those held back meanwhile.
#[derive(Debug)]
pub struct Throttle {
    every: Duration,
    last: Option<Instant>,
    held: u64,
}

impl Throttle {
    pub const fn new(every: Duration) -> Throttle {
        Throttle {
            every,
            last: None,
            held: 0,
        }
    }

    /// Whether a line may go out at `now`: with how many were held back since the last that did.
    pub fn pass(&mut self, now: Instant) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < self.every)
        {
            self.held += 1;
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.held))
    }
}

/// The name of the head unit's audio type `command`.
fn audio_type_name(command: u16) -> String {
    match AudioType::try_from(i32::from(command)) {
        Ok(audio_type) => format!("{audio_type:?}"),
        Err(_) => format!("audio type {command}"),
    }
}

/// The line for sound dropped, from counts by where it was dropped and by audio type; `None` for none.
pub fn describe_drops(drops: &[(&'static str, u16, u64)], period: &str) -> Option<String> {
    if drops.is_empty() {
        return None;
    }
    let mut stages: Vec<&str> = Vec::new();
    for (stage, _, _) in drops {
        if !stages.contains(stage) {
            stages.push(stage);
        }
    }
    let parts: Vec<String> = stages
        .iter()
        .map(|stage| {
            let counts: Vec<String> = drops
                .iter()
                .filter(|(s, _, _)| s == stage)
                .map(|(_, command, count)| format!("{} {count}", audio_type_name(*command)))
                .collect();
            format!("{stage}: {}", counts.join(", "))
        })
        .collect();
    Some(format!("sound dropped in {period}: {}", parts.join("; ")))
}

/// The line for the cast's writes that waited for room in its connection; `None` for none.
pub fn describe_stalls(stalls: StallCounts, period: &str) -> Option<String> {
    if stalls.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for (i, count) in stalls.counts.iter().enumerate() {
        let name = match STALL_BOUNDS_MS.get(i + 1) {
            Some(next) => format!("{}-{next} ms", STALL_BOUNDS_MS[i]),
            None => format!("{} ms and more", STALL_BOUNDS_MS[i]),
        };
        parts.push(format!("{name} {count}"));
    }
    Some(format!(
        "cast writes stalled in {period}: {}, longest {} ms",
        parts.join(", "),
        stalls.longest_ms
    ))
}

/// What `TCP_INFO` tells of a connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpHealth {
    pub rtt_us: u32,
    pub rttvar_us: u32,
    pub rto_us: u32,
    pub cwnd: u32,
    pub ssthresh: u32,
    pub unacked: u32,
    pub lost: u32,
    /// Retransmissions of the moment, unacknowledged.
    pub retrans: u32,
    pub total_retrans: u32,
}

/// The line for the TCP connection `name`, with the retransmissions since `before` when it is the same connection.
pub fn describe_tcp(name: &str, before: Option<&TcpHealth>, now: &TcpHealth) -> String {
    let ssthresh = if now.ssthresh >= i32::MAX as u32 {
        "-".to_owned()
    } else {
        now.ssthresh.to_string()
    };
    let retransmits = match before {
        Some(before) if now.total_retrans >= before.total_retrans => format!(
            "+{} ({} in all)",
            now.total_retrans - before.total_retrans,
            now.total_retrans
        ),
        _ => format!("{} in all", now.total_retrans),
    };
    format!(
        "tcp {name}: rtt {:.1} ms (var {:.1}), rto {} ms, cwnd {}, ssthresh {ssthresh}, unacked {}, lost {}, \
         retransmits {retransmits}, {} outstanding",
        f64::from(now.rtt_us) / 1000.0,
        f64::from(now.rttvar_us) / 1000.0,
        now.rto_us / 1000,
        now.cwnd,
        now.unacked,
        now.lost,
        now.retrans
    )
}

/// The TCP state of the connection on `socket`.
#[cfg(target_os = "linux")]
pub fn tcp_health(socket: std::os::fd::RawFd) -> std::io::Result<TcpHealth> {
    // SAFETY: tcp_info is plain data; getsockopt fills at most `len` bytes of it and says how many in `len`.
    let info = unsafe {
        let mut info: libc::tcp_info = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
        if libc::getsockopt(
            socket,
            libc::IPPROTO_TCP,
            libc::TCP_INFO,
            (&mut info as *mut libc::tcp_info).cast(),
            &mut len,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        info
    };
    Ok(TcpHealth {
        rtt_us: info.tcpi_rtt,
        rttvar_us: info.tcpi_rttvar,
        rto_us: info.tcpi_rto,
        cwnd: info.tcpi_snd_cwnd,
        ssthresh: info.tcpi_snd_ssthresh,
        unacked: info.tcpi_unacked,
        lost: info.tcpi_lost,
        retrans: info.tcpi_retrans,
        total_retrans: info.tcpi_total_retrans,
    })
}

/// What `iw dev IFACE station dump`, or `iw dev IFACE link` for drivers that dump no stations, tells of the access
/// point a station is associated with, and what the interface counted of what it sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiStation {
    pub signal_dbm: Option<i32>,
    pub signal_avg_dbm: Option<i32>,
    pub tx_bitrate: Option<String>,
    pub rx_bitrate: Option<String>,
    pub tx_packets: Option<u64>,
    pub tx_retries: Option<u64>,
    pub tx_failed: Option<u64>,
    /// The interface's own counts of what it could not send.
    pub tx_errors: Option<u64>,
    pub tx_dropped: Option<u64>,
}

impl WifiStation {
    /// The first station of `iw`'s station dump, or the access point of its link report; `None` when there is none.
    pub fn parse(report: &str) -> Option<WifiStation> {
        let header = |line: &str| line.starts_with("Station ") || line.starts_with("Connected to ");
        let mut lines = report.lines();
        lines.find(|line| header(line))?;
        let mut station = WifiStation::default();
        for line in lines {
            if header(line) {
                break;
            }
            let Some((key, value)) = line.trim().split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key {
                "signal" => station.signal_dbm = first_number(value),
                "signal avg" => station.signal_avg_dbm = first_number(value),
                "tx bitrate" => station.tx_bitrate = Some(value.to_owned()),
                "rx bitrate" => station.rx_bitrate = Some(value.to_owned()),
                "tx packets" => station.tx_packets = first_number(value),
                "tx retries" => station.tx_retries = first_number(value),
                "tx failed" => station.tx_failed = first_number(value),
                _ => {}
            }
        }
        Some(station)
    }
}

/// The interface's counter `name` (`tx_packets`, `tx_errors`, `tx_dropped`…) from `/sys/class/net`.
pub fn interface_counter(iface: &str, name: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{name}")).ok()?;
    text.trim().parse().ok()
}

/// The number `value` starts with.
fn first_number<T: std::str::FromStr>(value: &str) -> Option<T> {
    value.split_whitespace().next()?.parse().ok()
}

/// The line for the Wi-Fi link of `iface`, with the counters since `before` when they went on from it.
pub fn describe_wifi(iface: &str, before: Option<&WifiStation>, now: &WifiStation) -> String {
    let mut line = format!("wifi {iface}:");
    let mut parts = Vec::new();
    if let Some(signal) = now.signal_dbm {
        parts.push(match now.signal_avg_dbm {
            Some(avg) => format!("signal {signal} dBm (avg {avg})"),
            None => format!("signal {signal} dBm"),
        });
    }
    if let Some(rate) = &now.tx_bitrate {
        parts.push(format!("tx {rate}"));
    }
    if let Some(rate) = &now.rx_bitrate {
        parts.push(format!("rx {rate}"));
    }
    let counter = |name: &str, now: Option<u64>, before: Option<u64>| {
        now.map(|now| match before {
            Some(before) if now >= before => format!("{name} +{}", now - before),
            _ => format!("{name} {now} in all"),
        })
    };
    let continues = before.is_some_and(|b| match (b.tx_packets, now.tx_packets) {
        (Some(b), Some(n)) => n >= b,
        _ => true,
    });
    let before = before.filter(|_| continues);
    parts.extend(counter(
        "tx packets",
        now.tx_packets,
        before.and_then(|b| b.tx_packets),
    ));
    parts.extend(counter(
        "tx retries",
        now.tx_retries,
        before.and_then(|b| b.tx_retries),
    ));
    parts.extend(counter(
        "tx failed",
        now.tx_failed,
        before.and_then(|b| b.tx_failed),
    ));
    parts.extend(counter(
        "tx errors",
        now.tx_errors,
        before.and_then(|b| b.tx_errors),
    ));
    parts.extend(counter(
        "tx dropped",
        now.tx_dropped,
        before.and_then(|b| b.tx_dropped),
    ));
    if parts.is_empty() {
        line.push_str(" nothing reported");
    } else {
        line.push(' ');
        line.push_str(&parts.join(", "));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    const MONO_16K: Format = Format {
        sample_rate: 16000,
        channels: 1,
    };

    #[test]
    fn level_peak_and_rms() {
        let level = Level::of(&pcm(&[0, 1000, -2000, 0]));
        assert_eq!(level.peak, 2000);
        assert_eq!(level.samples(), 4);
        // sqrt((1000² + 2000²) / 4)
        assert!((level.rms() - 1118.034).abs() < 0.01, "{}", level.rms());
        let full = Level::of(&pcm(&[i16::MIN, i16::MAX]));
        assert_eq!(full.peak, 32768);
        assert!(full.peak_dbfs().abs() < 1e-9);
        assert!((dbfs(16384.0) + 6.0206).abs() < 0.001);
        assert_eq!(Level::of(&[]).rms(), 0.0);
        assert!(Level::of(&pcm(&[0, 0])).rms_dbfs().is_infinite());
        // A byte left over is no sample.
        assert_eq!(Level::of(&[1, 0, 7]).samples(), 1);
        let mut sum = Level::of(&pcm(&[3, 4]));
        sum.add(&Level::of(&pcm(&[-5])));
        assert_eq!((sum.peak, sum.samples()), (5, 3));
        // sqrt((9 + 16 + 25) / 3)
        assert!((sum.rms() - (50.0f64 / 3.0).sqrt()).abs() < 1e-9);
    }

    #[test]
    fn microphone_gaps_silence_and_level() {
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let mut mic = MicStats::new(start);
        assert_eq!(mic.report(ms(1000)), None, "off: nothing to report");
        mic.iphone_asked(MONO_16K, 640);
        assert_eq!(mic.head_unit_asked(true, MONO_16K, start), None);
        // 20 ms messages of 320 samples: one all zero, one quiet, one loud, the last after a 90 ms gap.
        mic.received(&pcm(&[0; 320]), ms(20));
        mic.received(&pcm(&[SILENT_PEAK as i16; 320]), ms(40));
        mic.received(&pcm(&[8000; 320]), ms(60));
        mic.received(&pcm(&[-100; 320]), ms(150));
        for outcome in [MicOutcome::Opus, MicOutcome::Opus, MicOutcome::SendFailed] {
            mic.sent(outcome);
        }
        let line = mic.report(ms(160)).unwrap();
        assert!(
            line.starts_with("microphone in 0.2 s: 4 messages, 2560 bytes"),
            "{line}"
        );
        assert!(line.contains("longest gap 90 ms"), "{line}");
        // 640 of the 1280 samples were silent, 320 of them zero: 40 and 20 ms at 16 kHz.
        assert!(line.contains("silent 0.04 s (all zero 0.02 s)"), "{line}");
        // A peak of 8000; the RMS of the four messages is about 4000.
        assert!(line.contains("peak -12.2 dBFS, RMS -18.3 dBFS"), "{line}");
        assert!(
            line.contains("2 in Opus, 0 in PCM, 0 with no stream, 0 not encoded, 1 not sent"),
            "{line}"
        );
        assert!(
            line.contains(
                "iPhone asked 16000 Hz 1 ch, 640-byte frames; head unit asked 16000 Hz 1 ch"
            ),
            "{line}"
        );

        // Nothing came since: the gap until the report counts, and the level is left out.
        let line = mic.report(ms(1160)).unwrap();
        assert!(line.contains("0 messages, 0 bytes"), "{line}");
        assert!(line.contains("longest gap 1010 ms"), "{line}");
        assert!(!line.contains("peak"), "{line}");

        // Stopping gives the last report, then nothing until it starts again.
        mic.received(&pcm(&[1; 160]), ms(1170));
        let last = mic.head_unit_asked(false, MONO_16K, ms(1200)).unwrap();
        assert!(last.contains("1 messages, 320 bytes"), "{last}");
        assert_eq!(mic.report(ms(2000)), None);
    }

    #[test]
    fn microphone_rate_received() {
        let start = Instant::now();
        let mut mic = MicStats::new(start);
        let stereo = Format {
            sample_rate: 48000,
            channels: 2,
        };
        mic.iphone_asked(stereo, 1920);
        mic.head_unit_asked(true, stereo, start);
        // A second of 48 kHz stereo is 192000 bytes.
        for n in 1..=50 {
            mic.received(&[0; 3840], start + Duration::from_millis(20 * n));
        }
        let line = mic.report(start + Duration::from_secs(1)).unwrap();
        assert!(
            line.contains("192000 bytes from the head unit (about 48000 Hz)"),
            "{line}"
        );
        assert!(line.contains("silent 1.00 s (all zero 1.00 s)"), "{line}");
        // Starting again while on keeps the report going.
        mic.head_unit_asked(true, stereo, start + Duration::from_secs(1));
        assert!(mic.report(start + Duration::from_secs(2)).is_some());
    }

    #[test]
    fn throttle_counts_what_it_holds_back() {
        let start = Instant::now();
        let mut throttle = Throttle::new(Duration::from_secs(10));
        assert_eq!(throttle.pass(start), Some(0));
        assert_eq!(throttle.pass(start + Duration::from_secs(1)), None);
        assert_eq!(throttle.pass(start + Duration::from_secs(9)), None);
        assert_eq!(throttle.pass(start + Duration::from_secs(10)), Some(2));
        assert_eq!(throttle.pass(start + Duration::from_secs(11)), None);
    }

    #[test]
    fn drops_by_stage_and_type() {
        assert_eq!(describe_drops(&[], "1 s"), None);
        let tts = AudioType::AudioTts as u16;
        let call = AudioType::AudioIpCall as u16;
        assert_eq!(
            describe_drops(
                &[("session queue", tts, 3), ("media queue", tts, 2), ("session queue", call, 1)],
                "1 s"
            )
            .unwrap(),
            "sound dropped in 1 s: session queue: AudioTts 3, AudioIpCall 1; media queue: AudioTts 2"
        );
        assert_eq!(
            describe_drops(&[("x", 999, 1)], "1 s").unwrap(),
            "sound dropped in 1 s: x: audio type 999 1"
        );
    }

    #[test]
    fn stalls_line() {
        assert_eq!(describe_stalls(StallCounts::default(), "5 s"), None);
        let stalls = StallCounts {
            counts: [3, 1, 0, 2],
            longest_ms: 900,
        };
        assert_eq!(
            describe_stalls(stalls, "5 s").unwrap(),
            "cast writes stalled in 5 s: 50-100 ms 3, 100-200 ms 1, 200-500 ms 0, 500 ms and more 2, longest 900 ms"
        );
    }

    #[test]
    fn tcp_line() {
        let before = TcpHealth {
            total_retrans: 10,
            ..TcpHealth::default()
        };
        let now = TcpHealth {
            rtt_us: 4200,
            rttvar_us: 1100,
            rto_us: 204_000,
            cwnd: 10,
            ssthresh: i32::MAX as u32,
            unacked: 3,
            lost: 0,
            retrans: 1,
            total_retrans: 15,
        };
        assert_eq!(
            describe_tcp("cast", Some(&before), &now),
            "tcp cast: rtt 4.2 ms (var 1.1), rto 204 ms, cwnd 10, ssthresh -, unacked 3, lost 0, retransmits +5 \
             (15 in all), 1 outstanding"
        );
        // Another connection: no difference to the one before.
        let fresh = TcpHealth {
            ssthresh: 7,
            total_retrans: 2,
            ..TcpHealth::default()
        };
        let line = describe_tcp("media", Some(&now), &fresh);
        assert!(line.contains("ssthresh 7"), "{line}");
        assert!(line.contains("retransmits 2 in all"), "{line}");
    }

    const DUMP: &str = "\
Station 02:11:22:33:44:55 (on carlink0)
\tinactive time:\t10 ms
\trx bytes:\t123456
\ttx packets:\t2000
\ttx retries:\t150
\ttx failed:\t3
\tsignal:  \t-55 [-57, -58] dBm
\tsignal avg:\t-56 [-58, -59] dBm
\ttx bitrate:\t433.3 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 1
\trx bitrate:\t390.0 MBit/s VHT-MCS 8 80MHz short GI VHT-NSS 1
\tauthorized:\tyes
";

    #[test]
    fn station_dump_parsed() {
        assert_eq!(WifiStation::parse(""), None);
        let station = WifiStation::parse(DUMP).unwrap();
        assert_eq!(
            station,
            WifiStation {
                signal_dbm: Some(-55),
                signal_avg_dbm: Some(-56),
                tx_bitrate: Some("433.3 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 1".into()),
                rx_bitrate: Some("390.0 MBit/s VHT-MCS 8 80MHz short GI VHT-NSS 1".into()),
                tx_packets: Some(2000),
                tx_retries: Some(150),
                tx_failed: Some(3),
                ..WifiStation::default()
            }
        );
        assert_eq!(
            describe_wifi("carlink0", None, &station),
            "wifi carlink0: signal -55 dBm (avg -56), tx 433.3 MBit/s VHT-MCS 9 80MHz short GI VHT-NSS 1, rx 390.0 \
             MBit/s VHT-MCS 8 80MHz short GI VHT-NSS 1, tx packets 2000 in all, tx retries 150 in all, tx failed 3 in all"
        );
        let later = WifiStation {
            tx_packets: Some(2500),
            tx_retries: Some(190),
            tx_failed: Some(3),
            tx_bitrate: None,
            rx_bitrate: None,
            signal_avg_dbm: None,
            ..station.clone()
        };
        assert_eq!(
            describe_wifi("carlink0", Some(&station), &later),
            "wifi carlink0: signal -55 dBm, tx packets +500, tx retries +40, tx failed +0"
        );
        // Counters that went back belong to a new association.
        let anew = WifiStation {
            tx_packets: Some(10),
            ..later.clone()
        };
        assert!(describe_wifi("carlink0", Some(&later), &anew)
            .contains("tx packets 10 in all, tx retries 190 in all"));
        assert_eq!(
            describe_wifi("carlink0", None, &WifiStation::default()),
            "wifi carlink0: nothing reported"
        );
    }

    #[test]
    fn link_report_parsed() {
        let link = "\
Connected to 2a:4a:b8:00:00:01 (on carlink0)
\tSSID: DIRECT-xy
\tfreq: 5200.0
\tsignal: -26 dBm
\ttx bitrate: 434.0 MBit/s
";
        assert_eq!(WifiStation::parse("Not connected.\n"), None);
        let mut station = WifiStation::parse(link).unwrap();
        assert_eq!(station.signal_dbm, Some(-26));
        assert_eq!(station.tx_bitrate.as_deref(), Some("434.0 MBit/s"));
        station.tx_packets = Some(100);
        station.tx_dropped = Some(15);
        station.tx_errors = Some(0);
        let later = WifiStation {
            tx_packets: Some(400),
            tx_dropped: Some(18),
            ..station.clone()
        };
        assert_eq!(
            describe_wifi("carlink0", Some(&station), &later),
            "wifi carlink0: signal -26 dBm, tx 434.0 MBit/s, tx packets +300, tx errors +0, tx dropped +3"
        );
    }
}
