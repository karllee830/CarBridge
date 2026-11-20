// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's transport/Iap2LinkEngine.kt (DiPlay
// commit 615b32c).

//! The iAP2 link layer, without I/O or threads.
//!
//! The link starts with a marker, then both sides send synchronization packets that set the window, timeouts and
//! sessions. After that, packets carry a session's payload with a sequence number and an acknowledgement; lost
//! packets are sent again on a timer or when the peer names them in an extended acknowledgement.
//!
//! A packet is `0xff5a | u16 length | control | sequence | acknowledgement | session | header checksum`, then the
//! payload and its checksum; a checksum makes its bytes sum to zero.
//!
//! The caller passes a monotonic clock in milliseconds, writes [`Engine::take_output`] to its byte stream, feeds
//! what it receives to [`Engine::feed`] and drains [`Engine::poll_event`]. Pending output, events and packets are
//! bounded by [`Config`], so neither the peer nor a slow caller can grow memory without limit.

use std::collections::VecDeque;

use crate::wire::ProtocolError;

/// The control session, which carries control session messages.
pub const CONTROL_SESSION: u8 = 10;
/// The external accessory session.
pub const EXTERNAL_ACCESSORY_SESSION: u8 = 11;
/// The file transfer session, which carries artwork.
pub const FILE_TRANSFER_SESSION: u8 = 12;

/// What both sides send first to say they speak iAP2.
pub const MARKER: [u8; 6] = [0xff, 0x55, 0x02, 0x00, 0xee, 0x10];
/// The largest packet, header and checksums included.
pub const MAX_PACKET_BYTES: usize = 0xffff;
const HEADER_BYTES: usize = 9;
const CHECKSUM_BYTES: usize = 1;
/// The largest payload of one packet.
pub const MAX_PAYLOAD_BYTES: usize = MAX_PACKET_BYTES - HEADER_BYTES - CHECKSUM_BYTES;

const START: [u8; 2] = [0xff, 0x5a];
const SYNCHRONIZE: u8 = 0x80;
const ACKNOWLEDGE: u8 = 0x40;
const EXTENDED_ACKNOWLEDGE: u8 = 0x20;
const RESET: u8 = 0x10;

const SYNCHRONIZATION_VERSION: u8 = 1;
const SYNCHRONIZATION_FIXED_BYTES: usize = 10;
const INITIAL_SEQUENCE: u8 = 99;
const MARKER_RESEND_MS: u64 = 1_000;
const SYNCHRONIZATION_RESEND_MS: u64 = 500;
const DEFAULT_RETRANSMISSION_TIMEOUT_MS: u16 = 4_000;
const DEFAULT_MAX_RETRANSMISSIONS: u8 = 4;
const DEFAULT_MAX_ACKNOWLEDGEMENTS: u8 = 3;
/// How far beyond the window a sequence number may be and still count as recent.
const SEQUENCE_SLACK: u8 = 10;

/// A session one side offers in its synchronization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionDescriptor {
    pub id: u8,
    /// 0 control, 1 file transfer, 2 external accessory.
    pub kind: u8,
    pub version: u8,
}

/// The parameters one side announces in its synchronization packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Synchronization {
    /// Packets that may be in flight unacknowledged.
    pub max_outgoing: u8,
    /// The largest packet it accepts.
    pub max_length: u16,
    pub retransmission_timeout_ms: u16,
    pub acknowledgement_timeout_ms: u16,
    pub max_retransmissions: u8,
    /// Packets it may receive before it must acknowledge; 0 for no acknowledgements.
    pub max_acknowledgements: u8,
    pub sessions: Vec<SessionDescriptor>,
}

impl Synchronization {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SYNCHRONIZATION_FIXED_BYTES + 3 * self.sessions.len());
        out.push(SYNCHRONIZATION_VERSION);
        out.push(self.max_outgoing);
        out.extend_from_slice(&self.max_length.to_be_bytes());
        out.extend_from_slice(&self.retransmission_timeout_ms.to_be_bytes());
        out.extend_from_slice(&self.acknowledgement_timeout_ms.to_be_bytes());
        out.push(self.max_retransmissions);
        out.push(self.max_acknowledgements);
        for session in &self.sessions {
            out.extend_from_slice(&[session.id, session.kind, session.version]);
        }
        out
    }

    /// Reads a synchronization payload; `None` for another version or a short one. Trailing bytes that make no
    /// whole session descriptor are ignored.
    pub fn decode(bytes: &[u8]) -> Option<Synchronization> {
        if bytes.len() < SYNCHRONIZATION_FIXED_BYTES || bytes[0] != SYNCHRONIZATION_VERSION {
            return None;
        }
        let u16_at = |offset: usize| u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        Some(Synchronization {
            max_outgoing: bytes[1],
            max_length: u16_at(2),
            retransmission_timeout_ms: u16_at(4),
            acknowledgement_timeout_ms: u16_at(6),
            max_retransmissions: bytes[8],
            max_acknowledgements: bytes[9],
            sessions: bytes[SYNCHRONIZATION_FIXED_BYTES..]
                .chunks_exact(3)
                .map(|d| SessionDescriptor {
                    id: d[0],
                    kind: d[1],
                    version: d[2],
                })
                .collect(),
        })
    }
}

/// What the local side announces, and the bounds it keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub max_outgoing: u8,
    /// How much sooner than the peer's window the local side acknowledges.
    pub max_outgoing_delta: u8,
    pub acknowledgement_timeout_ms: u16,
    /// Announce no retransmissions and no acknowledgements, as on links that cannot lose packets.
    pub zero_acknowledgements: bool,
    pub control_session_version: u8,
    pub max_queued_packets: usize,
    pub max_out_of_order_packets: usize,
    pub max_pending_output_bytes: usize,
    pub max_pending_events: usize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            max_outgoing: 30,
            max_outgoing_delta: 0,
            acknowledgement_timeout_ms: 500,
            zero_acknowledgements: false,
            control_session_version: 1,
            max_queued_packets: 64,
            max_out_of_order_packets: 64,
            max_pending_output_bytes: 1024 * 1024,
            max_pending_events: 256,
        }
    }
}

impl Config {
    /// Bluetooth RFCOMM: a window of four, with acknowledgements.
    pub fn bluetooth() -> Config {
        Config {
            max_outgoing: 4,
            control_session_version: 2,
            ..Config::default()
        }
    }

    /// The iAP2 tunnel inside a CarPlay session over Wi-Fi: a window of four, without acknowledgements.
    pub fn tunnel() -> Config {
        Config {
            zero_acknowledgements: true,
            ..Config::bluetooth()
        }
    }

    fn synchronization(&self) -> Synchronization {
        let acks = !self.zero_acknowledgements;
        Synchronization {
            max_outgoing: self.max_outgoing,
            max_length: MAX_PACKET_BYTES as u16,
            retransmission_timeout_ms: if acks {
                DEFAULT_RETRANSMISSION_TIMEOUT_MS
            } else {
                0
            },
            acknowledgement_timeout_ms: if acks {
                self.acknowledgement_timeout_ms
            } else {
                0
            },
            max_retransmissions: if acks { DEFAULT_MAX_RETRANSMISSIONS } else { 0 },
            max_acknowledgements: if acks {
                DEFAULT_MAX_ACKNOWLEDGEMENTS
            } else {
                0
            },
            sessions: vec![
                SessionDescriptor {
                    id: CONTROL_SESSION,
                    kind: 0,
                    version: self.control_session_version,
                },
                SessionDescriptor {
                    id: EXTERNAL_ACCESSORY_SESSION,
                    kind: 2,
                    version: 1,
                },
                SessionDescriptor {
                    id: FILE_TRANSFER_SESSION,
                    kind: 1,
                    version: 2,
                },
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    /// Waiting for the peer's marker.
    Detecting,
    /// Exchanging synchronization packets.
    Negotiating,
    Normal,
    Dead,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A control session payload, in order.
    Control(Vec<u8>),
    /// A payload of another session, in order.
    Session { id: u8, payload: Vec<u8> },
    /// Whether the link takes more to send.
    Writable(bool),
    /// The link ended; `None` when the stream ended.
    Dead(Option<String>),
}

#[derive(Clone, Copy, Debug)]
struct Header {
    length: usize,
    control: u8,
    sequence: u8,
    acknowledgement: u8,
    session: u8,
}

#[derive(Clone, Debug)]
struct Packet {
    sequence: u8,
    session: u8,
    payload: Vec<u8>,
    retransmissions: u8,
    deadline: u64,
}

/// How many sequence numbers `sequence` is after `previous`; 0 when there is no previous one.
fn distance(sequence: u8, previous: Option<u8>) -> u8 {
    previous.map_or(0, |previous| sequence.wrapping_sub(previous))
}

fn checksum(bytes: &[u8]) -> u8 {
    bytes
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg()
}

fn checksum_valid(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

fn parse_header(bytes: &[u8]) -> Option<Header> {
    if bytes.len() != HEADER_BYTES || bytes[..2] != START || !checksum_valid(bytes) {
        return None;
    }
    Some(Header {
        length: usize::from(u16::from_be_bytes([bytes[2], bytes[3]])),
        control: bytes[4],
        sequence: bytes[5],
        acknowledgement: bytes[6],
        session: bytes[7],
    })
}

/// One end of an iAP2 link.
pub struct Engine {
    config: Config,
    state: State,
    peer: Synchronization,
    peer_synchronized: bool,
    sent_sequence: u8,
    last_sent_acknowledged: Option<u8>,
    last_received_in_order: u8,
    last_acknowledged_received: Option<u8>,
    accumulated_acknowledgements: u32,
    writable: bool,
    unacknowledged: Vec<Packet>,
    queued: VecDeque<Packet>,
    out_of_order: Vec<Packet>,
    events: VecDeque<Event>,
    output: Vec<u8>,
    receive: Vec<u8>,
    awaiting_payload: Option<Header>,
    marker_deadline: Option<u64>,
    synchronization_deadline: Option<u64>,
    send_acknowledgement_deadline: Option<u64>,
    receive_acknowledgement_deadline: Option<u64>,
}

impl Engine {
    pub fn new(config: Config) -> Engine {
        let peer = config.synchronization();
        Engine {
            config,
            state: State::Idle,
            peer,
            peer_synchronized: false,
            sent_sequence: INITIAL_SEQUENCE,
            last_sent_acknowledged: None,
            last_received_in_order: 0,
            last_acknowledged_received: None,
            accumulated_acknowledgements: 0,
            writable: false,
            unacknowledged: Vec::new(),
            queued: VecDeque::new(),
            out_of_order: Vec::new(),
            events: VecDeque::new(),
            output: Vec::new(),
            receive: Vec::new(),
            awaiting_payload: None,
            marker_deadline: None,
            synchronization_deadline: None,
            send_acknowledgement_deadline: None,
            receive_acknowledgement_deadline: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    /// What the peer announced, once it has; the local announcement before.
    pub fn peer_synchronization(&self) -> &Synchronization {
        &self.peer
    }

    /// The largest payload the peer takes, once the link is up.
    pub fn peer_max_payload(&self) -> usize {
        usize::from(self.peer.max_length).saturating_sub(HEADER_BYTES + CHECKSUM_BYTES)
    }

    /// The bytes to write to the stream, taken.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// When [`Engine::advance_time`] next has something to do.
    pub fn next_deadline(&self) -> Option<u64> {
        [
            self.marker_deadline,
            self.synchronization_deadline,
            self.send_acknowledgement_deadline,
            self.receive_acknowledgement_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Starts the link: sends the marker, and when `initiator` also the first synchronization without waiting
    /// for the peer's marker.
    pub fn start(&mut self, initiator: bool, now: u64) {
        if self.state != State::Idle {
            return;
        }
        self.state = State::Detecting;
        if !self.append_output(&MARKER) {
            return;
        }
        self.marker_deadline = Some(now + MARKER_RESEND_MS);
        if initiator {
            self.enter_negotiating(now);
        }
    }

    /// Takes received bytes, in pieces of any size.
    pub fn feed(&mut self, mut bytes: &[u8], now: u64) {
        while !bytes.is_empty() && self.state != State::Dead {
            let room = MAX_PACKET_BYTES - self.receive.len();
            if room == 0 {
                if !self.parse_available(now) {
                    self.die(Some(format!("iAP2 packet over {MAX_PACKET_BYTES} bytes")));
                    return;
                }
                continue;
            }
            let (now_bytes, later) = bytes.split_at(room.min(bytes.len()));
            self.receive.extend_from_slice(now_bytes);
            bytes = later;
            while self.state != State::Dead && self.parse_available(now) {}
        }
    }

    /// The stream ended.
    pub fn feed_end(&mut self) {
        self.die(None);
    }

    /// Runs the timers that are due at `now`.
    pub fn advance_time(&mut self, now: u64) {
        while self.state != State::Dead {
            match self.next_deadline() {
                Some(deadline) if deadline <= now => {}
                _ => return,
            }
            if self.marker_deadline.is_some_and(|at| at <= now) {
                self.marker_deadline = None;
                if self.state == State::Detecting && self.append_output(&MARKER) {
                    self.marker_deadline = Some(now + MARKER_RESEND_MS);
                }
            } else if self.synchronization_deadline.is_some_and(|at| at <= now) {
                self.synchronization_deadline = None;
                if self.state == State::Negotiating {
                    self.send_synchronization();
                    if self.state != State::Dead {
                        self.synchronization_deadline = Some(now + SYNCHRONIZATION_RESEND_MS);
                    }
                }
            } else if self
                .send_acknowledgement_deadline
                .is_some_and(|at| at <= now)
            {
                self.send_acknowledgement_deadline = None;
                if self.state == State::Normal {
                    self.last_acknowledged_received = Some(self.last_received_in_order);
                    self.send_acknowledgement();
                }
            } else {
                self.receive_acknowledgement_deadline = None;
                self.retransmit_due(now);
            }
        }
    }

    /// Queues a control session payload.
    pub fn send_control(&mut self, payload: &[u8], now: u64) -> Result<(), ProtocolError> {
        self.send_session(CONTROL_SESSION, payload, now)
    }

    /// Queues a payload on a session.
    pub fn send_session(
        &mut self,
        session: u8,
        payload: &[u8],
        now: u64,
    ) -> Result<(), ProtocolError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(ProtocolError::new(format!(
                "iAP2 payload of {} bytes over {MAX_PAYLOAD_BYTES}",
                payload.len()
            )));
        }
        if (self.peer_synchronized || self.state == State::Normal)
            && !self.peer_accepts(payload.len())
        {
            return Err(ProtocolError::new(format!(
                "iAP2 payload of {} bytes over the peer's limit of {}",
                payload.len(),
                self.peer_max_payload()
            )));
        }
        self.send_packet(
            Packet {
                sequence: 0,
                session,
                payload: payload.to_vec(),
                retransmissions: 0,
                deadline: 0,
            },
            now,
        );
        Ok(())
    }

    /// Parses what it can from the receive buffer; false when nothing more can be parsed yet.
    fn parse_available(&mut self, now: u64) -> bool {
        match self.state {
            State::Dead => return false,
            State::Detecting => {
                if self.receive.len() < MARKER.len() {
                    return false;
                }
                if self.receive[..MARKER.len()] != MARKER {
                    self.die(Some("iAP2 marker was not received".into()));
                    return false;
                }
                self.receive.drain(..MARKER.len());
                self.enter_negotiating(now);
                return true;
            }
            _ => {}
        }

        if let Some(header) = self.awaiting_payload {
            let length = header.length - HEADER_BYTES;
            if self.receive.len() < length {
                return false;
            }
            let payload: Vec<u8> = self.receive.drain(..length).collect();
            self.awaiting_payload = None;
            if checksum_valid(&payload) {
                let body = &payload[..payload.len() - CHECKSUM_BYTES];
                self.process(header, Some(body), now);
            }
            return true;
        }

        let skip = self
            .receive
            .windows(2)
            .position(|pair| pair == START)
            .unwrap_or(self.receive.len().saturating_sub(1));
        self.receive.drain(..skip);
        if self.receive.len() < HEADER_BYTES {
            return skip > 0;
        }
        let raw: Vec<u8> = self.receive.drain(..HEADER_BYTES).collect();
        match parse_header(&raw) {
            Some(header) if header.length == HEADER_BYTES => self.process(header, None, now),
            Some(header) if header.length > HEADER_BYTES => self.awaiting_payload = Some(header),
            _ => {}
        }
        true
    }

    fn enter_negotiating(&mut self, now: u64) {
        self.state = State::Negotiating;
        self.marker_deadline = None;
        self.send_synchronization();
        if self.state != State::Dead {
            self.synchronization_deadline = Some(now + SYNCHRONIZATION_RESEND_MS);
        }
    }

    fn process(&mut self, header: Header, payload: Option<&[u8]>, now: u64) {
        if header.control & RESET != 0 {
            self.die(Some("peer reset the iAP2 link".into()));
            return;
        }
        if header.control & SYNCHRONIZE != 0 {
            if let Some(synchronization) = Synchronization::decode(payload.unwrap_or_default()) {
                self.synchronized(synchronization, header.sequence);
            }
        }
        if header.control & ACKNOWLEDGE != 0 {
            self.accumulated_acknowledgements += 1;
            self.acknowledged(header.acknowledgement, now);
        }
        if header.control & EXTENDED_ACKNOWLEDGE != 0 {
            if let Some(missing) = payload {
                self.extended_acknowledgement(missing);
            }
        }
        if header.control & !ACKNOWLEDGE == 0 {
            if let Some(payload) = payload {
                self.data(
                    Packet {
                        sequence: header.sequence,
                        session: header.session,
                        payload: payload.to_vec(),
                        retransmissions: 0,
                        deadline: 0,
                    },
                    now,
                );
            }
        }
        let max = u32::from(self.peer.max_acknowledgements);
        if self.state != State::Dead && max > 0 && self.accumulated_acknowledgements >= max {
            self.accumulated_acknowledgements = 0;
            self.last_acknowledged_received = Some(self.last_received_in_order);
            self.send_acknowledgement();
        }
    }

    fn synchronized(&mut self, synchronization: Synchronization, sequence: u8) {
        if self.state != State::Negotiating {
            return;
        }
        self.peer = synchronization;
        self.peer_synchronized = true;
        self.last_received_in_order = sequence;
        self.last_acknowledged_received = Some(sequence);
        self.send_acknowledgement();
    }

    fn acknowledged(&mut self, acknowledgement: u8, now: u64) {
        if self.state == State::Negotiating {
            let carriable = self.peer_synchronized
                && self.peer_limits_usable()
                && self
                    .queued
                    .iter()
                    .all(|packet| self.peer_accepts(packet.payload.len()));
            if !carriable {
                self.die(Some(
                    "the peer's iAP2 packet size cannot carry the pending packets".into(),
                ));
                return;
            }
            self.state = State::Normal;
            self.synchronization_deadline = None;
            self.set_writable(true);
        }
        self.last_sent_acknowledged = Some(acknowledgement);

        // Packets after the acknowledgement are still in flight; those up to it are done.
        let in_flight = self
            .peer
            .max_outgoing
            .max(self.peer.max_acknowledgements)
            .saturating_add(SEQUENCE_SLACK);
        self.receive_acknowledgement_deadline = None;
        while let Some(first) = self.unacknowledged.first() {
            let ahead = distance(first.sequence, self.last_sent_acknowledged);
            if ahead > 0 && ahead <= in_flight {
                self.receive_acknowledgement_deadline = Some(first.deadline);
                break;
            }
            self.unacknowledged.remove(0);
        }

        while distance(self.sent_sequence, self.last_sent_acknowledged) < self.peer.max_outgoing {
            let Some(packet) = self.queued.pop_front() else {
                break;
            };
            self.send_packet(packet, now);
            self.set_writable(true);
        }
    }

    fn extended_acknowledgement(&mut self, missing: &[u8]) {
        if self.state != State::Normal {
            return;
        }
        for index in 0..self.unacknowledged.len() {
            let sequence = self.unacknowledged[index].sequence;
            if !missing.contains(&sequence) {
                continue;
            }
            self.unacknowledged[index].retransmissions += 1;
            if self.unacknowledged[index].retransmissions == self.peer.max_retransmissions {
                self.die(Some(format!("iAP2 packet {sequence} was not acknowledged")));
                return;
            }
            let packet = self.unacknowledged[index].clone();
            self.send_data(packet.sequence, packet.session, &packet.payload);
            self.send_acknowledgement_deadline = None;
            self.receive_acknowledgement_deadline = Some(packet.deadline);
        }
    }

    fn data(&mut self, packet: Packet, now: u64) {
        let ahead = distance(packet.sequence, Some(self.last_received_in_order));
        if ahead == 0 || ahead > self.peer.max_outgoing.saturating_add(SEQUENCE_SLACK) {
            // A packet already received, or far out of the window: say again what was received.
            self.send_acknowledgement();
            return;
        }
        if self
            .out_of_order
            .iter()
            .any(|held| held.sequence == packet.sequence)
        {
            return;
        }
        if self.out_of_order.len() >= self.config.max_out_of_order_packets {
            self.die(Some("iAP2 out-of-order packet limit exceeded".into()));
            return;
        }
        let sequence = packet.sequence;
        self.out_of_order.push(packet);
        if ahead > 1 {
            if ahead >= self.peer.max_outgoing {
                let mut missing = Vec::new();
                let mut next = self.last_received_in_order;
                while distance(sequence, Some(next)) > 1 {
                    next = next.wrapping_add(1);
                    missing.push(next);
                }
                self.send_acknowledgement_deadline = None;
                self.write_packet(Some(&missing), self.sent_sequence, EXTENDED_ACKNOWLEDGE, 0);
            }
            return;
        }

        let base = self.last_received_in_order;
        self.out_of_order
            .sort_by_key(|held| distance(held.sequence, Some(base)));
        while self
            .out_of_order
            .first()
            .is_some_and(|held| distance(held.sequence, Some(self.last_received_in_order)) == 1)
        {
            let packet = self.out_of_order.remove(0);
            self.last_received_in_order = packet.sequence;
            self.enqueue_event(if packet.session == CONTROL_SESSION {
                Event::Control(packet.payload)
            } else {
                Event::Session {
                    id: packet.session,
                    payload: packet.payload,
                }
            });
        }

        if self.peer.max_acknowledgements == 0 {
            return;
        }
        let window = self
            .peer
            .max_outgoing
            .saturating_sub(self.config.max_outgoing_delta)
            .max(1);
        if distance(self.last_received_in_order, self.last_acknowledged_received) >= window {
            self.send_acknowledgement_deadline = None;
            self.last_acknowledged_received = Some(self.last_received_in_order);
            self.send_acknowledgement();
        } else {
            self.send_acknowledgement_deadline =
                Some(now + u64::from(self.peer.acknowledgement_timeout_ms));
        }
    }

    fn send_packet(&mut self, mut packet: Packet, now: u64) {
        if self.state != State::Normal
            || distance(self.sent_sequence, self.last_sent_acknowledged) > self.peer.max_outgoing
        {
            if self.queued.len() >= self.config.max_queued_packets {
                self.die(Some("iAP2 send queue limit exceeded".into()));
            } else {
                self.queued.push_back(packet);
                self.set_writable(false);
            }
            return;
        }
        self.sent_sequence = self.sent_sequence.wrapping_add(1);
        packet.sequence = self.sent_sequence;
        packet.retransmissions = 0;
        packet.deadline = now + u64::from(self.peer.retransmission_timeout_ms);
        self.send_acknowledgement_deadline = None;
        self.send_data(packet.sequence, packet.session, &packet.payload);
        self.last_acknowledged_received = Some(self.last_received_in_order);
        if self.peer.max_retransmissions > 0 {
            self.receive_acknowledgement_deadline = Some(packet.deadline);
            self.unacknowledged.push(packet);
        } else {
            self.last_sent_acknowledged = Some(self.sent_sequence);
        }
    }

    fn retransmit_due(&mut self, now: u64) {
        if self.state != State::Normal || self.unacknowledged.is_empty() {
            return;
        }
        let index = (0..self.unacknowledged.len())
            .min_by_key(|&i| self.unacknowledged[i].deadline)
            .expect("not empty");
        let next_other = self
            .unacknowledged
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != index)
            .map(|(_, packet)| packet.deadline)
            .min();
        let packet = &mut self.unacknowledged[index];
        packet.deadline = now + u64::from(self.peer.retransmission_timeout_ms);
        packet.retransmissions += 1;
        if packet.retransmissions == self.peer.max_retransmissions {
            let sequence = packet.sequence;
            self.die(Some(format!("iAP2 packet {sequence} was not acknowledged")));
            return;
        }
        let packet = packet.clone();
        self.send_data(packet.sequence, packet.session, &packet.payload);
        self.receive_acknowledgement_deadline = Some(next_other.unwrap_or(packet.deadline));
    }

    fn send_synchronization(&mut self) {
        let payload = self.config.synchronization().encode();
        self.write_packet(Some(&payload), self.sent_sequence, SYNCHRONIZE, 0);
    }

    fn send_acknowledgement(&mut self) {
        self.write_packet(None, self.sent_sequence, ACKNOWLEDGE, 0);
    }

    fn send_data(&mut self, sequence: u8, session: u8, payload: &[u8]) {
        self.write_packet(Some(payload), sequence, ACKNOWLEDGE, session);
    }

    fn write_packet(&mut self, payload: Option<&[u8]>, sequence: u8, control: u8, session: u8) {
        if self.state == State::Dead {
            return;
        }
        self.accumulated_acknowledgements = 0;
        let length = payload.map_or(HEADER_BYTES, |p| HEADER_BYTES + p.len() + CHECKSUM_BYTES);
        let mut packet = Vec::with_capacity(length);
        packet.extend_from_slice(&START);
        packet.extend_from_slice(&(length as u16).to_be_bytes());
        packet.extend_from_slice(&[control, sequence, self.last_received_in_order, session]);
        packet.push(checksum(&packet));
        if let Some(payload) = payload {
            packet.extend_from_slice(payload);
            packet.push(checksum(payload));
        }
        self.append_output(&packet);
    }

    fn set_writable(&mut self, writable: bool) {
        if self.writable != writable {
            self.writable = writable;
            self.enqueue_event(Event::Writable(writable));
        }
    }

    fn die(&mut self, reason: Option<String>) {
        if self.state == State::Dead {
            return;
        }
        self.marker_deadline = None;
        self.synchronization_deadline = None;
        self.send_acknowledgement_deadline = None;
        self.receive_acknowledgement_deadline = None;
        self.state = State::Dead;
        self.writable = false;
        self.output.clear();
        self.events.clear();
        self.events.push_back(Event::Dead(reason));
    }

    fn append_output(&mut self, bytes: &[u8]) -> bool {
        if self.state == State::Dead {
            return false;
        }
        if bytes.len() > self.config.max_pending_output_bytes - self.output.len() {
            self.die(Some("iAP2 pending output limit exceeded".into()));
            return false;
        }
        self.output.extend_from_slice(bytes);
        true
    }

    fn enqueue_event(&mut self, event: Event) {
        if self.state == State::Dead {
            return;
        }
        if self.events.len() >= self.config.max_pending_events {
            self.die(Some("iAP2 pending event limit exceeded".into()));
            return;
        }
        self.events.push_back(event);
    }

    /// A usable control session carries at least one byte besides the header and checksums.
    fn peer_limits_usable(&self) -> bool {
        usize::from(self.peer.max_length) > HEADER_BYTES + CHECKSUM_BYTES
    }

    fn peer_accepts(&self, payload: usize) -> bool {
        self.peer_limits_usable() && payload <= self.peer_max_payload()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(
        control: u8,
        sequence: u8,
        acknowledgement: u8,
        session: u8,
        payload: &[u8],
    ) -> Vec<u8> {
        let length = (HEADER_BYTES + CHECKSUM_BYTES + payload.len()) as u16;
        let mut bytes = vec![0xff, 0x5a];
        bytes.extend_from_slice(&length.to_be_bytes());
        bytes.extend_from_slice(&[control, sequence, acknowledgement, session]);
        bytes.push(checksum(&bytes));
        bytes.extend_from_slice(payload);
        bytes.push(checksum(payload));
        bytes
    }

    fn peer_synchronization(
        max_outgoing: u8,
        max_retransmissions: u8,
        max_acknowledgements: u8,
    ) -> Synchronization {
        Synchronization {
            max_outgoing,
            max_length: 4096,
            retransmission_timeout_ms: 2_000,
            acknowledgement_timeout_ms: 500,
            max_retransmissions,
            max_acknowledgements,
            sessions: vec![
                SessionDescriptor {
                    id: CONTROL_SESSION,
                    kind: 0,
                    version: 2,
                },
                SessionDescriptor {
                    id: FILE_TRANSFER_SESSION,
                    kind: 1,
                    version: 2,
                },
            ],
        }
    }

    /// An initiating engine that the peer synchronized with, its output and events drained.
    fn synchronized(synchronization: &Synchronization) -> Engine {
        let mut engine = Engine::new(Config::default());
        engine.start(true, 0);
        engine.take_output();
        engine.feed(&packet(0xc0, 1, 99, 0, &synchronization.encode()), 1);
        assert!(engine.writable());
        while engine.poll_event().is_some() {}
        engine.take_output();
        engine
    }

    #[test]
    fn synchronization_round_trips() {
        let synchronization = Config::bluetooth().synchronization();
        let bytes = synchronization.encode();
        assert_eq!(
            &bytes[..10],
            [1, 4, 0xff, 0xff, 0x0f, 0xa0, 0x01, 0xf4, 4, 3]
        );
        assert_eq!(&bytes[10..], [10, 0, 2, 11, 2, 1, 12, 1, 2]);
        assert_eq!(Synchronization::decode(&bytes), Some(synchronization));
        let tunnel = Config::tunnel().synchronization().encode();
        assert_eq!(&tunnel[..10], [1, 4, 0xff, 0xff, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Synchronization::decode(&bytes[..9]), None);
        assert_eq!(Synchronization::decode(&[2; 10]), None);
    }

    #[test]
    fn a_responder_waits_for_the_marker() {
        let mut engine = Engine::new(Config::bluetooth());
        engine.start(false, 0);
        assert_eq!(engine.take_output(), MARKER);
        assert_eq!(engine.state(), State::Detecting);
        engine.advance_time(999);
        assert!(engine.take_output().is_empty());
        engine.advance_time(1_000);
        assert_eq!(engine.take_output(), MARKER);
        engine.feed(&MARKER, 1_100);
        assert_eq!(engine.state(), State::Negotiating);
        let synchronization = engine.take_output();
        let payload = Config::bluetooth().synchronization().encode();
        assert_eq!(synchronization, packet(SYNCHRONIZE, 99, 0, 0, &payload));
        engine.advance_time(1_600);
        assert_eq!(engine.take_output(), synchronization);
    }

    #[test]
    fn a_wrong_marker_ends_the_link() {
        let mut engine = Engine::new(Config::bluetooth());
        engine.start(false, 0);
        engine.feed(&[0xff, 0x55, 0x02, 0x00, 0xee, 0x11], 1);
        assert_eq!(engine.state(), State::Dead);
        assert!(matches!(engine.poll_event(), Some(Event::Dead(Some(_)))));
    }

    #[test]
    fn the_file_transfer_session_is_routed_and_answered() {
        let mut engine = synchronized(&peer_synchronization(8, 4, 3));
        let datagram = [0x81, 4, 0, 0, 0, 0, 0, 0, 0, 5, 0, 2];
        engine.feed(&packet(0x40, 2, 99, FILE_TRANSFER_SESSION, &datagram), 2);
        assert_eq!(
            engine.poll_event(),
            Some(Event::Session {
                id: FILE_TRANSFER_SESSION,
                payload: datagram.to_vec()
            })
        );
        engine.take_output();
        engine
            .send_session(FILE_TRANSFER_SESSION, &[0x81, 1], 3)
            .unwrap();
        let reply = engine.take_output();
        assert_eq!(reply[7], FILE_TRANSFER_SESSION);
        assert_eq!(&reply[9..11], [0x81, 1]);
    }

    #[test]
    fn control_payloads_arrive_in_order_and_are_acknowledged() {
        let mut engine = synchronized(&peer_synchronization(8, 4, 3));
        // Sequence 3 before 2: held until 2 arrives.
        engine.feed(&packet(0x40, 3, 99, CONTROL_SESSION, b"second"), 10);
        assert_eq!(engine.poll_event(), None);
        engine.feed(&packet(0x40, 2, 99, CONTROL_SESSION, b"first"), 11);
        assert_eq!(engine.poll_event(), Some(Event::Control(b"first".to_vec())));
        assert_eq!(
            engine.poll_event(),
            Some(Event::Control(b"second".to_vec()))
        );
        engine.take_output();
        // The acknowledgement waits for the peer's acknowledgement timeout.
        engine.advance_time(11 + 499);
        assert!(engine.take_output().is_empty());
        engine.advance_time(11 + 500);
        assert_eq!(
            engine.take_output(),
            packet_without_payload(ACKNOWLEDGE, 99, 3)
        );
        // A packet received again is acknowledged again, and not delivered twice.
        engine.feed(&packet(0x40, 3, 99, CONTROL_SESSION, b"second"), 600);
        assert_eq!(engine.poll_event(), None);
        assert_eq!(
            engine.take_output(),
            packet_without_payload(ACKNOWLEDGE, 99, 3)
        );
    }

    fn packet_without_payload(control: u8, sequence: u8, acknowledgement: u8) -> Vec<u8> {
        let mut bytes = vec![0xff, 0x5a, 0, 9, control, sequence, acknowledgement, 0];
        bytes.push(checksum(&bytes));
        bytes
    }

    #[test]
    fn bad_checksums_and_garbage_are_ignored() {
        let mut engine = synchronized(&peer_synchronization(8, 4, 3));
        let mut bad = packet(0x40, 2, 99, CONTROL_SESSION, b"x");
        *bad.last_mut().unwrap() ^= 1;
        engine.feed(&[0x00, 0x13], 5);
        engine.feed(&bad, 5);
        assert_eq!(engine.poll_event(), None);
        engine.feed(&packet(0x40, 2, 99, CONTROL_SESSION, b"y"), 6);
        assert_eq!(engine.poll_event(), Some(Event::Control(b"y".to_vec())));
    }

    #[test]
    fn unacknowledged_packets_are_sent_again_then_give_up() {
        let mut engine = synchronized(&peer_synchronization(8, 3, 3));
        engine.send_control(b"hello", 100).unwrap();
        let first = engine.take_output();
        assert_eq!(
            first,
            packet(ACKNOWLEDGE, 100, 1, CONTROL_SESSION, b"hello")
        );
        assert_eq!(engine.next_deadline(), Some(2_100));
        engine.advance_time(2_100);
        assert_eq!(engine.take_output(), first);
        engine.advance_time(4_100);
        assert_eq!(engine.take_output(), first);
        engine.advance_time(6_100);
        assert_eq!(engine.state(), State::Dead);
        assert!(matches!(engine.poll_event(), Some(Event::Dead(Some(_)))));
    }

    #[test]
    fn an_acknowledgement_ends_retransmission() {
        let mut engine = synchronized(&peer_synchronization(8, 3, 3));
        engine.send_control(b"hello", 100).unwrap();
        engine.take_output();
        engine.feed(&packet_without_payload(ACKNOWLEDGE, 1, 100), 200);
        assert_eq!(engine.next_deadline(), None);
        engine.advance_time(10_000);
        assert!(engine.take_output().is_empty());
        assert_eq!(engine.state(), State::Normal);
    }

    #[test]
    fn a_reset_ends_the_link() {
        let mut engine = synchronized(&peer_synchronization(8, 3, 3));
        engine.feed(&packet_without_payload(RESET, 2, 99), 1);
        assert_eq!(engine.state(), State::Dead);
    }

    #[test]
    fn payloads_above_the_peer_limit_are_refused() {
        let mut engine = synchronized(&peer_synchronization(8, 3, 3));
        assert!(engine.send_control(&vec![0; 4096 - 10 + 1], 1).is_err());
        assert!(engine.send_control(&vec![0; 4096 - 10], 1).is_ok());
        assert_eq!(engine.peer_max_payload(), 4086);
    }

    #[test]
    fn the_window_queues_packets_until_acknowledged() {
        let mut engine = synchronized(&peer_synchronization(2, 3, 3));
        for i in 0..5u8 {
            engine.send_control(&[i], 10).unwrap();
        }
        let sent = engine.take_output();
        // Two in the window plus the one the window check lets through; the rest wait.
        assert_eq!(sent.len(), 3 * 11);
        assert_eq!(engine.poll_event(), Some(Event::Writable(false)));
        engine.feed(&packet_without_payload(ACKNOWLEDGE, 1, 102), 20);
        assert_eq!(engine.take_output().len(), 2 * 11);
        assert_eq!(engine.poll_event(), Some(Event::Writable(true)));
    }

    /// Runs two engines against each other until neither has anything to say.
    fn exchange(a: &mut Engine, b: &mut Engine, now: u64) {
        for _ in 0..20 {
            let (from_a, from_b) = (a.take_output(), b.take_output());
            if from_a.is_empty() && from_b.is_empty() {
                return;
            }
            b.feed(&from_a, now);
            a.feed(&from_b, now);
        }
        panic!("the engines kept talking");
    }

    fn controls(engine: &mut Engine) -> Vec<Vec<u8>> {
        let mut received = Vec::new();
        while let Some(event) = engine.poll_event() {
            if let Event::Control(payload) = event {
                received.push(payload);
            }
        }
        received
    }

    #[test]
    fn two_engines_link_up_and_talk() {
        for config in [Config::bluetooth(), Config::tunnel()] {
            let mut accessory = Engine::new(config.clone());
            let mut phone = Engine::new(config);
            phone.start(false, 0);
            accessory.start(true, 0);
            exchange(&mut accessory, &mut phone, 1);
            assert_eq!(accessory.state(), State::Normal);
            assert_eq!(phone.state(), State::Normal);
            for i in 0..10u8 {
                accessory.send_control(&[i; 100], 2).unwrap();
                phone.send_control(&[i; 50], 2).unwrap();
                exchange(&mut accessory, &mut phone, 2);
            }
            accessory.advance_time(10_000);
            phone.advance_time(10_000);
            exchange(&mut accessory, &mut phone, 10_000);
            let to_phone = controls(&mut phone);
            let to_accessory = controls(&mut accessory);
            assert_eq!(
                to_phone,
                (0..10u8).map(|i| vec![i; 100]).collect::<Vec<_>>()
            );
            assert_eq!(
                to_accessory,
                (0..10u8).map(|i| vec![i; 50]).collect::<Vec<_>>()
            );
            assert_eq!(accessory.state(), State::Normal);
        }
    }

    #[test]
    fn the_end_of_the_stream_ends_the_link() {
        let mut engine = Engine::new(Config::tunnel());
        engine.start(true, 0);
        engine.feed_end();
        assert_eq!(engine.state(), State::Dead);
        assert_eq!(engine.poll_event(), Some(Event::Dead(None)));
        engine.send_control(b"late", 1).unwrap();
        assert!(engine.take_output().is_empty());
    }
}
