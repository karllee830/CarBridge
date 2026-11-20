// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Joining the Wi-Fi network a Carlink head unit opens for the phone.
//!
//! The phone joins the network as a plain WPA2 station, a P2P group included, and takes an address by DHCP. This crate
//! turns the head unit's server info into the network to join and the station's configuration; the system's own
//! wpa_supplicant and DHCP client do the joining.

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use carbridge_carlink_ble::exchange::{NetworkType, ServerInfo};
use pbkdf2::pbkdf2_hmac;
use sha1::Sha1;

/// Address of the group owner of a Wi-Fi Direct group.
pub const GROUP_OWNER_ADDRESS: Ipv4Addr = Ipv4Addr::new(192, 168, 49, 1);
/// How long the head unit waits for the phone to join a P2P group.
pub const P2P_JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the head unit waits for the phone to join a soft AP.
pub const SOFT_AP_JOIN_TIMEOUT: Duration = Duration::from_secs(20);

/// The network the head unit opened.
#[derive(Clone, PartialEq, Eq)]
pub struct Network {
    pub kind: NetworkType,
    pub ssid: String,
    /// WPA2 pre-shared key derived from the passphrase and SSID.
    pub psk: [u8; 32],
    /// The BSSID, when the head unit gave it, `aa:bb:cc:dd:ee:ff`.
    pub bssid: Option<String>,
    /// Frequency in MHz, 0 when not given.
    pub freq: u32,
    /// The head unit's address on the network.
    pub head_unit: Ipv4Addr,
}

impl fmt::Debug for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Network")
            .field("kind", &self.kind)
            .field("ssid", &self.ssid)
            .field("bssid", &self.bssid)
            .field("freq", &self.freq)
            .field("head_unit", &self.head_unit)
            .finish_non_exhaustive()
    }
}

/// Why the server info does not describe a network the phone can join.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `type` is neither 1001 nor 1002.
    UnknownType(i32),
    /// The SSID is empty or longer than 32 bytes.
    Ssid,
    /// The passphrase is not 8 to 63 printable ASCII characters.
    Passphrase,
    /// A soft AP without a valid `ipAddress`.
    HeadUnitAddress,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownType(code) => write!(f, "unknown network type {code}"),
            Error::Ssid => f.write_str("SSID empty or longer than 32 bytes"),
            Error::Passphrase => f.write_str("passphrase not 8 to 63 printable ASCII characters"),
            Error::HeadUnitAddress => f.write_str("soft AP without the head unit's address"),
        }
    }
}

impl std::error::Error for Error {}

impl Network {
    pub fn from_server_info(info: &ServerInfo) -> Result<Network, Error> {
        let kind = info
            .network()
            .ok_or(Error::UnknownType(info.network_code))?;
        if info.ssid.is_empty() || info.ssid.len() > 32 {
            return Err(Error::Ssid);
        }
        if !(8..=63).contains(&info.psk.len())
            || !info.psk.bytes().all(|b| (0x20..0x7f).contains(&b))
        {
            return Err(Error::Passphrase);
        }
        let head_unit = match kind {
            NetworkType::P2pGroup => GROUP_OWNER_ADDRESS,
            NetworkType::SoftAp => info
                .ip_address
                .as_deref()
                .and_then(|ip| ip.parse().ok())
                .ok_or(Error::HeadUnitAddress)?,
        };
        let mut psk = [0u8; 32];
        pbkdf2_hmac::<Sha1>(info.psk.as_bytes(), info.ssid.as_bytes(), 4096, &mut psk);
        Ok(Network {
            kind,
            ssid: info.ssid.clone(),
            psk,
            bssid: info.bssid().map(str::to_lowercase),
            freq: info.freq,
            head_unit,
        })
    }

    /// How long the head unit waits for the phone to join.
    pub fn join_timeout(&self) -> Duration {
        match self.kind {
            NetworkType::P2pGroup => P2P_JOIN_TIMEOUT,
            NetworkType::SoftAp => SOFT_AP_JOIN_TIMEOUT,
        }
    }

    /// The wpa_supplicant configuration that joins the network: WPA2-PSK only, scanning only its frequency when
    /// known.
    pub fn wpa_supplicant_config(&self) -> String {
        let mut out = String::from("network={\n");
        out.push_str(&format!("\tssid={}\n", hex(self.ssid.as_bytes())));
        out.push_str(&format!("\tpsk={}\n", hex(&self.psk)));
        out.push_str("\tkey_mgmt=WPA-PSK\n\tproto=RSN\n\tpairwise=CCMP\n\tscan_ssid=1\n");
        if self.freq > 0 {
            out.push_str(&format!("\tscan_freq={0}\n\tfreq_list={0}\n", self.freq));
        }
        if let Some(bssid) = &self.bssid {
            out.push_str(&format!("\tbssid={bssid}\n"));
        }
        out.push_str("}\n");
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(json: &str) -> ServerInfo {
        ServerInfo::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn p2p_group() {
        let network = Network::from_server_info(&info(
            r#"{"name":"","ssid":"DIRECT-vs-1A2B3C","psk":"1a2b3c4d","mac":"02:00:00:00:00:00","freq":5200,"port":0,"type":1001}"#,
        ))
        .unwrap();
        assert_eq!(network.kind, NetworkType::P2pGroup);
        assert_eq!(network.head_unit, GROUP_OWNER_ADDRESS);
        assert_eq!(network.bssid, None);
        assert_eq!(network.join_timeout(), Duration::from_secs(30));
        let config = network.wpa_supplicant_config();
        assert!(config.contains("\tssid=4449524543542d76732d314132423343\n"));
        assert!(config.contains("\tfreq_list=5200\n"));
        assert!(!config.contains("bssid"));
        assert!(!config.contains("1a2b3c4d"));
        assert!(!format!("{network:?}").contains("psk"));
    }

    #[test]
    fn psk_is_wpa2_derivation() {
        // IEEE 802.11i test vector: passphrase "password", SSID "IEEE".
        let network = Network::from_server_info(&info(
            r#"{"ssid":"IEEE","psk":"password","freq":0,"type":1001}"#,
        ))
        .unwrap();
        assert_eq!(
            hex(&network.psk),
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e"
        );
        assert!(!network.wpa_supplicant_config().contains("freq"));
    }

    #[test]
    fn soft_ap() {
        let network = Network::from_server_info(&info(
            r#"{"ssid":"DIRECT-ICCOA-A1B","psk":"Ab3dEf7h","mac":"AA:BB:CC:DD:EE:FF","freq":2412,"type":1002,"ipAddress":"192.168.43.1"}"#,
        ))
        .unwrap();
        assert_eq!(network.head_unit, Ipv4Addr::new(192, 168, 43, 1));
        assert_eq!(network.bssid.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert!(network
            .wpa_supplicant_config()
            .contains("\tbssid=aa:bb:cc:dd:ee:ff\n"));
        assert_eq!(network.join_timeout(), Duration::from_secs(20));
    }

    #[test]
    fn rejects_bad_server_info() {
        let soft_ap_without_ip = info(r#"{"ssid":"x","psk":"12345678","freq":1,"type":1002}"#);
        assert_eq!(
            Network::from_server_info(&soft_ap_without_ip),
            Err(Error::HeadUnitAddress)
        );
        let short = info(r#"{"ssid":"x","psk":"1234567","freq":1,"type":1001}"#);
        assert_eq!(Network::from_server_info(&short), Err(Error::Passphrase));
        let no_ssid = info(r#"{"ssid":"","psk":"12345678","freq":1,"type":1001}"#);
        assert_eq!(Network::from_server_info(&no_ssid), Err(Error::Ssid));
        let unknown = info(r#"{"ssid":"x","psk":"12345678","freq":1,"type":7}"#);
        assert_eq!(
            Network::from_server_info(&unknown),
            Err(Error::UnknownType(7))
        );
    }
}
