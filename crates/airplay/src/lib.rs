// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's AirPlay code (DiPlay commit
// 615b32c), which builds on xcertplay.

//! The AirPlay side of a wireless CarPlay receiver.
//!
//! This crate is a Rust rewrite of the AirPlay code of DiPlay (<https://github.com/shihabal3amri/DiPlay>), which
//! builds on xcertplay (<https://github.com/shilapi/xcertplay>). Both are licensed under the GNU General Public
//! License version 3, and so is this crate.

pub mod audio;
pub mod bplist;
pub mod control_cipher;
pub mod crypto;
pub mod hid;
pub mod identity;
pub mod info;
pub mod microphone;
pub mod net;
pub mod ntp;
pub mod pairing;
pub mod rtsp;
pub mod sap;
pub mod screen;
pub mod session;
pub mod srp;
pub mod tlv8;
pub mod tunnel;
