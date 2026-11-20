// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The JSON values of the GATT exchange: the phone's client info and the head unit's server info.

use serde::{Deserialize, Serialize};

/// The network the head unit opens for the phone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkType {
    /// A Wi-Fi P2P group, the head unit as group owner.
    P2pGroup,
    /// A soft access point.
    SoftAp,
}

impl NetworkType {
    pub fn code(self) -> i32 {
        match self {
            NetworkType::P2pGroup => 1001,
            NetworkType::SoftAp => 1002,
        }
    }

    pub fn from_code(code: i32) -> Option<NetworkType> {
        match code {
            1001 => Some(NetworkType::P2pGroup),
            1002 => Some(NetworkType::SoftAp),
            _ => None,
        }
    }
}

/// Wi-Fi band the phone asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Band {
    Ghz2_4,
    Ghz5,
}

/// What the phone writes to the client info characteristic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientInfo {
    /// The phone's identity; the same `id` goes into authentication.
    pub id: String,
    /// Shown on the head unit.
    pub name: String,
    pub model: String,
    pub band: Band,
    /// The phone's Wi-Fi Direct device address, `xx:xx:xx:xx:xx:xx`.
    pub mac: String,
    pub network: NetworkType,
    /// Preferred channel, 0 for none.
    pub channel: u32,
    /// The phone's maker, lower case.
    pub manufacturer: Option<String>,
    /// The phone checked itself that this head unit is the one it expects.
    pub direct: Option<bool>,
}

/// Address sent when the phone does not disclose its own.
pub const UNKNOWN_MAC: &str = "02:00:00:00:00:00";

#[derive(Serialize)]
struct ClientInfoJson<'a> {
    id: &'a str,
    name: &'a str,
    model: &'a str,
    band: u8,
    mac: &'a str,
    #[serde(rename = "type")]
    network: i32,
    channel: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    manufactures: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct: Option<bool>,
}

impl ClientInfo {
    /// The UTF-8 JSON value to write.
    pub fn to_json(&self) -> Vec<u8> {
        let json = ClientInfoJson {
            id: &self.id,
            name: &self.name,
            model: &self.model,
            band: match self.band {
                Band::Ghz2_4 => 0,
                Band::Ghz5 => 1,
            },
            mac: &self.mac,
            network: self.network.code(),
            channel: self.channel,
            manufactures: self.manufacturer.as_deref(),
            direct: self.direct,
        };
        serde_json::to_vec(&json).expect("plain strings and numbers always serialize")
    }
}

/// What the head unit indicates on the server info characteristic: the network to join.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServerInfo {
    /// The head unit's short name.
    #[serde(default)]
    pub name: String,
    pub ssid: String,
    pub psk: String,
    /// The network's BSSID, or [`UNKNOWN_MAC`].
    #[serde(default)]
    pub mac: String,
    /// Frequency in MHz.
    pub freq: u32,
    #[serde(default)]
    pub port: u32,
    #[serde(rename = "type")]
    pub network_code: i32,
    /// The head unit's address on a soft AP.
    #[serde(rename = "ipAddress", default)]
    pub ip_address: Option<String>,
}

impl ServerInfo {
    /// Reads the indicated value; trailing zero bytes are ignored.
    pub fn parse(value: &[u8]) -> Result<ServerInfo, serde_json::Error> {
        let end = value.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        serde_json::from_slice(&value[..end])
    }

    pub fn network(&self) -> Option<NetworkType> {
        NetworkType::from_code(self.network_code)
    }

    /// The BSSID, when the head unit disclosed it.
    pub fn bssid(&self) -> Option<&str> {
        (!self.mac.is_empty() && !self.mac.eq_ignore_ascii_case(UNKNOWN_MAC))
            .then_some(self.mac.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ClientInfo {
        ClientInfo {
            id: "a1b2c3".into(),
            name: "iPhone".into(),
            model: "CarBridge".into(),
            band: Band::Ghz5,
            mac: UNKNOWN_MAC.into(),
            network: NetworkType::P2pGroup,
            channel: 149,
            manufacturer: None,
            direct: None,
        }
    }

    #[test]
    fn client_info_json() {
        let json: serde_json::Value = serde_json::from_slice(&client().to_json()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"id":"a1b2c3","name":"iPhone","model":"CarBridge","band":1,
                "mac":"02:00:00:00:00:00","type":1001,"channel":149})
        );
        let full = ClientInfo {
            manufacturer: Some("apple".into()),
            direct: Some(true),
            ..client()
        };
        let json: serde_json::Value = serde_json::from_slice(&full.to_json()).unwrap();
        assert_eq!(json["manufactures"], "apple");
        assert_eq!(json["direct"], true);
    }

    #[test]
    fn server_info_with_padding() {
        let mut value =
            br#"{"name":"","ssid":"DIRECT-vs-AB12CD","psk":"12345678","mac":"02:00:00:00:00:00","freq":5745,"port":0,"type":1001}"#
                .to_vec();
        value.extend([0, 0, 0]);
        let info = ServerInfo::parse(&value).unwrap();
        assert_eq!(info.ssid, "DIRECT-vs-AB12CD");
        assert_eq!(info.freq, 5745);
        assert_eq!(info.network(), Some(NetworkType::P2pGroup));
        assert_eq!(info.bssid(), None);
        assert_eq!(info.ip_address, None);
    }

    #[test]
    fn soft_ap_server_info() {
        let value = br#"{"ssid":"DIRECT-ICCOA-1","psk":"p","mac":"aa:bb:cc:dd:ee:ff","freq":2437,"type":1002,"ipAddress":"192.168.43.1"}"#;
        let info = ServerInfo::parse(value).unwrap();
        assert_eq!(info.network(), Some(NetworkType::SoftAp));
        assert_eq!(info.bssid(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(info.ip_address.as_deref(), Some("192.168.43.1"));
    }

    #[test]
    fn server_info_needs_credentials() {
        assert!(ServerInfo::parse(br#"{"ssid":"x","freq":1,"type":1001}"#).is_err());
    }
}
