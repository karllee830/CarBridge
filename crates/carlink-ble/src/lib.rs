// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Carlink discovery and BLE handoff: the head unit's advertisement and the GATT exchange.
//!
//! - [`advert`]: reading the head unit's advertisement and scan response.
//! - [`discovery`]: recognising a head unit among scan reports.
//! - [`exchange`]: the client info the phone writes and the server info the head unit indicates.
//! - [`gatt`]: the exchange itself, as a minimal ATT client.
//! - [`att`]: the ATT PDUs it uses.
//! - [`hash`]: how the head unit recognises a phone it knows.
//! - `link` (Linux only): the LE link, a kernel L2CAP socket on the ATT channel.

pub mod advert;
pub mod att;
pub mod discovery;
pub mod exchange;
pub mod gatt;
pub mod hash;
#[cfg(target_os = "linux")]
pub mod link;
