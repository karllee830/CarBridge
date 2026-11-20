// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The CarPlay receiver, built on the carbridge-iap2 and carbridge-airplay crates.
//!
//! - [`audio`]: the iPhone's sound as the head unit plays it.
//! - [`input`]: touches and keys from the head unit as CarPlay's HID reports.
//! - [`receiver`]: the AirPlay control listener, its sessions, and the media they deliver.
//! - [`video`]: the iPhone's screen as access units for the head unit.
//! - [`wireless`]: wireless CarPlay's start over Bluetooth, iAP2 on RFCOMM.
//! - [`mfi`]: the MFi identity the receiver authenticates with.
//! - [`net`]: sockets for the AirPlay sessions.
//! - [`night`]: the head unit's day or night mode for the sessions.
//! - [`now_playing`]: what plays on the iPhone, from its iAP2 now-playing updates and artwork transfers.
//! - [`picture`]: the size, frame rate and density of the picture the iPhone renders.
//! - [`opus`]: the iPhone's Opus to and from the head unit's PCM.
//! - [`store`]: the AirPlay identity and the paired iPhones.
//! - [`tile`]: the car tile on the CarPlay home screen.

pub mod audio;
pub mod input;
pub mod mfi;
pub mod net;
pub mod night;
pub mod now_playing;
pub mod opus;
pub mod picture;
pub mod receiver;
pub mod store;
pub mod tile;
pub mod video;
pub mod wireless;

pub use carbridge_airplay::hid::Contact;
