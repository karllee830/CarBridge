// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's iAP2 code (DiPlay commit 615b32c),
// which builds on xcertplay.

//! iAP2 for a wireless CarPlay accessory.
//!
//! This crate is a Rust rewrite of the iAP2 code of DiPlay (<https://github.com/shihabal3amri/DiPlay>), which
//! builds on xcertplay (<https://github.com/shilapi/xcertplay>). Both are licensed under the GNU General Public
//! License version 3, and so is this crate.

pub mod csm;
pub mod error;
pub mod file_transfer;
pub mod identification;
pub mod link;
pub mod messages;
pub mod mfi;
pub mod now_playing;
pub mod session;
pub mod stream;
pub mod wire;
pub mod wireless;

pub use error::{Error, Rejection};
pub use session::{Session, Transport};
pub use stream::{Received, Stream};
