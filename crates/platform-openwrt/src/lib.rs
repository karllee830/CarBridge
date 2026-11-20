// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! OpenWrt system interfaces: BlueZ, wpa_supplicant, uci and ubus.
//!
//! - [`bluez`]: LE scanning through bluetoothd.
//! - [`classic`]: Bluetooth Classic: name, pairing, an RFCOMM profile and a pairing agent.
//! - [`mgmt`]: the kernel's Bluetooth management interface: the class of device.
//! - [`rfcomm`]: a device's RFCOMM service, found over SDP and opened directly.
//! - [`access_point`]: the router's own access point.
//! - [`wifi`]: a Wi-Fi station run by the system's wpa_supplicant.
//! - [`dhcp`]: an address by DHCP on an interface.
//! - [`ubus`]: calls to ubus objects.

pub mod access_point;
pub mod bluez;
pub mod classic;
pub mod dhcp;
pub mod mgmt;
pub mod rfcomm;
pub mod ubus;
pub mod wifi;
