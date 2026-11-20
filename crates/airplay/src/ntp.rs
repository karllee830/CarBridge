// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/NtpClock.kt (DiPlay commit
// 615b32c).

//! The timing port: the receiver keeps a clock on the iPhone's time, which /feedback reports.
//!
//! Once a second the receiver sends a request (payload type 210) with its transmit time; the iPhone's response
//! (211) adds its receive and transmit times. Of every two exchanges the one with the shorter round trip counts,
//! and only when its round trip is no longer than the last eight: an offset above 128 ms steps the clock, a
//! smaller one moves it an eighth of the way. The receiver also answers the iPhone's own requests.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::net::{same_host, Datagram};

const REQUEST: u8 = 210;
const RESPONSE: u8 = 211;
const PACKET_BYTES: usize = 32;
const ORIGINATE: usize = 8;
const RECEIVE: usize = 16;
const TRANSMIT: usize = 24;
const REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(200);
const STEP_THRESHOLD_SECONDS: f64 = 0.128;
const SLEW_GAIN: f64 = 1.0 / 8.0;
const DELAY_WINDOW: usize = 8;
const PICK_COUNT: u32 = 2;
/// Seconds from 1900, the NTP epoch, to 1970.
const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const TWO_32: f64 = 4_294_967_296.0;

/// Nanoseconds as NTP 32.32 fixed point.
fn ntp_from_nanos(nanos: i128) -> u64 {
    let seconds = nanos.div_euclid(1_000_000_000);
    let fraction = nanos.rem_euclid(1_000_000_000);
    ((seconds as u64) << 32) | ((fraction << 32) / 1_000_000_000) as u64
}

/// NTP 32.32 fixed point as nanoseconds.
pub fn nanos_from_ntp(ntp: u64) -> u64 {
    (ntp >> 32) * 1_000_000_000 + (((ntp & 0xffff_ffff) * 1_000_000_000) >> 32)
}

/// The clock's arithmetic, apart from its sockets.
struct State {
    started: Instant,
    /// Nanoseconds from `started` to the iPhone's clock, as nanoseconds since 1900.
    offset: i128,
    delays: [f64; DELAY_WINDOW],
    next_delay: usize,
    picks_left: u32,
    pick_round_trip: f64,
    pick_offset: f64,
    pending: Option<u64>,
    synced: bool,
}

impl State {
    fn new() -> State {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        State {
            started: Instant::now(),
            offset: unix.as_nanos() as i128 + i128::from(NTP_UNIX_OFFSET) * 1_000_000_000,
            delays: [f64::INFINITY; DELAY_WINDOW],
            next_delay: 0,
            picks_left: PICK_COUNT,
            pick_round_trip: f64::INFINITY,
            pick_offset: 0.0,
            pending: None,
            synced: false,
        }
    }

    fn now(&self) -> u64 {
        ntp_from_nanos(self.started.elapsed().as_nanos() as i128 + self.offset)
    }

    /// Takes one exchange: our transmit time `t1`, the iPhone's receive and transmit times, and our receive time.
    fn exchange(&mut self, t1: u64, t2: u64, t3: u64, t4: u64) {
        if self.pending != Some(t1) {
            return;
        }
        self.pending = None;
        let diff = |a: u64, b: u64| a.wrapping_sub(b) as i64 as f64;
        let offset = (diff(t2, t1) + diff(t3, t4)) * 0.5 / TWO_32;
        let round_trip = (diff(t4, t1) - diff(t3, t2)) / TWO_32;
        if round_trip < 0.0 {
            return;
        }
        if round_trip < self.pick_round_trip {
            self.pick_round_trip = round_trip;
            self.pick_offset = offset;
        }
        self.picks_left -= 1;
        if self.picks_left > 0 {
            return;
        }
        let (round_trip, offset) = (self.pick_round_trip, self.pick_offset);
        self.picks_left = PICK_COUNT;
        self.pick_round_trip = f64::INFINITY;
        let shortest = self.delays.iter().all(|&delay| round_trip <= delay);
        self.delays[self.next_delay] = round_trip;
        self.next_delay = (self.next_delay + 1) % DELAY_WINDOW;
        if shortest {
            self.apply(offset);
        }
    }

    fn apply(&mut self, offset: f64) {
        let step = !self.synced || offset.abs() > STEP_THRESHOLD_SECONDS;
        let applied = if step { offset } else { offset * SLEW_GAIN };
        self.offset += (applied * 1e9).round() as i128;
        if step {
            self.delays = [f64::INFINITY; DELAY_WINDOW];
            self.next_delay = 0;
            self.pending = None;
            self.synced = true;
        }
    }
}

fn packet(kind: u8) -> [u8; PACKET_BYTES] {
    let mut packet = [0; PACKET_BYTES];
    packet[0] = 0x80;
    packet[1] = kind;
    packet[2..4].copy_from_slice(&7u16.to_be_bytes());
    packet
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
}

struct Shared {
    socket: Arc<dyn Datagram>,
    peer: Mutex<Option<SocketAddr>>,
    state: Mutex<State>,
    running: AtomicBool,
}

/// A clock on the iPhone's time, synchronized over its own UDP socket.
pub struct Clock {
    shared: Arc<Shared>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Clock {
    /// Starts answering requests on `socket`; [`Clock::sync_with`] starts asking.
    pub fn new(socket: Arc<dyn Datagram>) -> Clock {
        let shared = Arc::new(Shared {
            socket,
            peer: Mutex::new(None),
            state: Mutex::new(State::new()),
            running: AtomicBool::new(true),
        });
        let receiver = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("airplay-ntp".into())
                .spawn(move || shared.receive())
        };
        Clock {
            shared,
            threads: Mutex::new(receiver.into_iter().collect()),
        }
    }

    /// The port the iPhone sends its timing packets to.
    pub fn port(&self) -> u16 {
        self.shared.socket.port()
    }

    /// Asks the iPhone at `peer` for its time once a second.
    pub fn sync_with(&self, peer: IpAddr, port: u16) {
        *self.shared.peer.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(SocketAddr::new(peer, port));
        let shared = self.shared.clone();
        if let Ok(sender) = std::thread::Builder::new()
            .name("airplay-ntp-requests".into())
            .spawn(move || shared.request())
        {
            self.threads
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(sender);
        }
    }

    /// Now on the iPhone's clock, as NTP 32.32.
    pub fn now(&self) -> u64 {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .now()
    }

    pub fn close(&self) {
        self.shared.running.store(false, Ordering::Release);
        self.shared.socket.close();
        for thread in self
            .threads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            let _ = thread.join();
        }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.close();
    }
}

impl Shared {
    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    fn request(&self) {
        let mut next = Instant::now();
        while self.running() {
            if Instant::now() >= next {
                next += REQUEST_INTERVAL;
                let peer = *self.peer.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(peer) = peer {
                    let mut request = packet(REQUEST);
                    let t1 = {
                        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                        let t1 = state.now();
                        state.pending = Some(t1);
                        t1
                    };
                    request[TRANSMIT..].copy_from_slice(&t1.to_be_bytes());
                    let _ = self.socket.send_to(&request, peer);
                }
            }
            std::thread::sleep(
                POLL.min(next.saturating_duration_since(Instant::now()))
                    .max(Duration::from_millis(1)),
            );
        }
    }

    fn receive(&self) {
        let mut buffer = [0; 2048];
        while self.running() {
            let Ok(Some((count, from))) = self.socket.receive_from(&mut buffer, POLL) else {
                continue;
            };
            if count < PACKET_BYTES {
                continue;
            }
            let message = &buffer[..PACKET_BYTES];
            match message[1] {
                REQUEST => {
                    let mut response = packet(RESPONSE);
                    response[ORIGINATE..RECEIVE].copy_from_slice(&message[TRANSMIT..]);
                    let now = self.state.lock().unwrap_or_else(|p| p.into_inner()).now();
                    response[RECEIVE..TRANSMIT].copy_from_slice(&now.to_be_bytes());
                    response[TRANSMIT..].copy_from_slice(&now.to_be_bytes());
                    let _ = self.socket.send_to(&response, from);
                }
                RESPONSE => {
                    let peer = *self.peer.lock().unwrap_or_else(|p| p.into_inner());
                    if !peer.is_some_and(|peer| same_host(peer.ip(), from.ip())) {
                        continue;
                    }
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let t4 = state.now();
                    let (t1, t2, t3) = (
                        read_u64(message, ORIGINATE),
                        read_u64(message, RECEIVE),
                        read_u64(message, TRANSMIT),
                    );
                    state.exchange(t1, t2, t3, t4);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{Sockets, StdSockets};
    use std::net::UdpSocket;

    const SKEW: u64 = 3600 << 32;

    fn wall_ntp_seconds() -> i64 {
        (SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + NTP_UNIX_OFFSET) as i64
    }

    /// Answers two of the clock's requests an hour ahead, from `reply_from`; gives the clock's drift in seconds.
    fn exchange(reply_from: &str) -> i64 {
        let socket = StdSockets
            .datagram(IpAddr::from([127, 0, 0, 1]), None)
            .unwrap();
        let clock = Clock::new(socket);
        let phone = UdpSocket::bind("127.0.0.1:0").unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let replier = UdpSocket::bind(format!("{reply_from}:0")).unwrap();
        clock.sync_with(
            IpAddr::from([127, 0, 0, 1]),
            phone.local_addr().unwrap().port(),
        );
        for _ in 0..2 {
            let mut request = [0; 64];
            let (count, _) = phone.recv_from(&mut request).unwrap();
            assert_eq!(count, 32);
            assert_eq!(request[1], REQUEST);
            let t1 = read_u64(&request, TRANSMIT);
            let mut response = packet(RESPONSE);
            response[ORIGINATE..RECEIVE].copy_from_slice(&t1.to_be_bytes());
            response[RECEIVE..TRANSMIT].copy_from_slice(&(t1 + SKEW).to_be_bytes());
            response[TRANSMIT..].copy_from_slice(&(t1 + SKEW).to_be_bytes());
            replier
                .send_to(&response, ("127.0.0.1", clock.port()))
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(300));
        let drift = (clock.now() >> 32) as i64 - wall_ntp_seconds();
        clock.close();
        drift
    }

    #[test]
    fn the_phones_response_moves_the_clock() {
        assert!(exchange("127.0.0.1") > 3000);
    }

    #[test]
    fn a_strangers_response_cannot_move_the_clock() {
        // 127.0.0.2 is loopback on Linux; elsewhere the test cannot bind it and has nothing to show.
        if UdpSocket::bind("127.0.0.2:0").is_err() {
            return;
        }
        assert!(exchange("127.0.0.2").abs() < 60);
    }

    #[test]
    fn the_receiver_answers_requests() {
        let socket = StdSockets
            .datagram(IpAddr::from([127, 0, 0, 1]), None)
            .unwrap();
        let clock = Clock::new(socket);
        let phone = UdpSocket::bind("127.0.0.1:0").unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = packet(REQUEST);
        request[TRANSMIT..].copy_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
        phone
            .send_to(&request, ("127.0.0.1", clock.port()))
            .unwrap();
        let mut response = [0; 64];
        let (count, _) = phone.recv_from(&mut response).unwrap();
        assert_eq!(count, 32);
        assert_eq!(response[1], RESPONSE);
        assert_eq!(read_u64(&response, ORIGINATE), 0x1122_3344_5566_7788);
        assert!(((read_u64(&response, RECEIVE) >> 32) as i64 - wall_ntp_seconds()).abs() < 5);
    }

    #[test]
    fn fixed_point_conversions() {
        assert_eq!(nanos_from_ntp((3 << 32) | 0x4000_0000), 3_250_000_000);
        assert_eq!(ntp_from_nanos(3_250_000_000), (3 << 32) | 0x4000_0000);
        assert!(nanos_from_ntp(ntp_from_nanos(1_234_567_890_123)).abs_diff(1_234_567_890_123) <= 1);
    }

    #[test]
    fn small_offsets_slew_and_large_ones_step() {
        let mut state = State::new();
        state.synced = true;
        let before = state.offset;
        state.apply(0.08);
        assert_eq!(state.offset - before, 10_000_000);
        state.apply(1.0);
        assert_eq!(state.offset - before, 1_010_000_000);
    }
}
