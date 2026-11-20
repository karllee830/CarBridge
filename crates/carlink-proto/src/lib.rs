// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Carlink wire format: message header, encrypted records, authentication and protobuf messages.
//!
//! Everything here is free of I/O: callers read and write the sockets and pass bytes in and out.
//!
//! - [`header`]: the 20-byte message header and its checksum.
//! - [`frame`]: whole messages, encrypted or not, encoded to and decoded from a byte stream.
//! - [`record`]: the AES-128-GCM records that encrypted bodies are made of.
//! - [`auth`]: the phone's side of authentication and the session key.
//! - [`certificate`]: what the head unit's certificate says about the head unit.
//! - [`command`]: categories' command numbers.
//! - [`pb`]: the protobuf messages.

pub mod auth;
pub mod certificate;
pub mod command;
mod crc;
mod error;
pub mod frame;
pub mod header;
pub mod record;

pub use crc::crc16_arc;
pub use error::Error;

/// Protobuf messages of the control protocol, generated from `proto/carlink.proto`.
#[allow(clippy::all, clippy::pedantic, missing_docs)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/carlink.rs"));
}

/// Protocol version the phone sends in `AuthRequest.version` (1.7).
pub const PROTOCOL_VERSION: i32 = 0x0701;

/// TCP ports of the channels.
pub mod port {
    /// Authentication; the head unit listens.
    pub const AUTH: u16 = 57209;
    /// Session control and heartbeats; the phone listens.
    pub const CONTROL: u16 = 57219;
    /// Audio other than the cast's music, and the microphone; the phone listens.
    pub const MEDIA: u16 = 57229;
    /// Car data; the phone listens.
    pub const SENSOR: u16 = 57239;
    /// The head unit's certificate; the phone listens.
    pub const CERT: u16 = 57249;
    /// The car's cameras; the phone listens.
    pub const VIDEO: u16 = 57259;
}
