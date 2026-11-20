// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's iap2/message/Iap2Messages.kt and
// the endpoint identifiers of iap2/catalog/Iap2Endpoint.kt (DiPlay commit 615b32c).

//! The control session messages a wireless CarPlay accessory sends and reads.

use crate::wire::{Frame, Parameters, ProtocolError};

/// Message identifiers.
pub mod id {
    pub const START_IDENTIFICATION: u16 = 0x1d00;
    pub const IDENTIFICATION_INFORMATION: u16 = 0x1d01;
    pub const IDENTIFICATION_ACCEPTED: u16 = 0x1d02;
    pub const IDENTIFICATION_REJECTED: u16 = 0x1d03;

    pub const REQUEST_AUTHENTICATION_CERTIFICATE: u16 = 0xaa00;
    pub const AUTHENTICATION_CERTIFICATE: u16 = 0xaa01;
    pub const REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE: u16 = 0xaa02;
    pub const AUTHENTICATION_RESPONSE: u16 = 0xaa03;
    pub const AUTHENTICATION_FAILED: u16 = 0xaa04;
    pub const AUTHENTICATION_SUCCEEDED: u16 = 0xaa05;

    pub const START_EXTERNAL_ACCESSORY_PROTOCOL_SESSION: u16 = 0xea00;
    pub const STOP_EXTERNAL_ACCESSORY_PROTOCOL_SESSION: u16 = 0xea01;

    pub const START_NOW_PLAYING_UPDATES: u16 = 0x5000;
    pub const NOW_PLAYING_UPDATE: u16 = 0x5001;
    pub const STOP_NOW_PLAYING_UPDATES: u16 = 0x5002;

    pub const START_POWER_UPDATES: u16 = 0xae00;
    pub const POWER_UPDATE: u16 = 0xae01;
    pub const STOP_POWER_UPDATES: u16 = 0xae02;

    pub const START_CALL_STATE_UPDATES: u16 = 0x4154;
    pub const CALL_STATE_UPDATE: u16 = 0x4155;
    pub const STOP_CALL_STATE_UPDATES: u16 = 0x4156;
    pub const START_COMMUNICATIONS_UPDATES: u16 = 0x4157;
    pub const COMMUNICATIONS_UPDATE: u16 = 0x4158;
    pub const STOP_COMMUNICATIONS_UPDATES: u16 = 0x4159;

    pub const CARPLAY_AVAILABILITY: u16 = 0x4300;
    pub const CARPLAY_START_SESSION: u16 = 0x4301;

    pub const WIRELESS_CARPLAY_UPDATE: u16 = 0x4e0d;
    pub const DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION: u16 = 0x4e0e;
    pub const REQUEST_ACCESSORY_WIFI_CONFIGURATION: u16 = 0x5702;
    pub const ACCESSORY_WIFI_CONFIGURATION: u16 = 0x5703;
}

fn frame(message_id: u16, parameters: Parameters) -> Result<Frame, ProtocolError> {
    Frame::with_parameters(message_id, &parameters)
}

/// Wi-Fi security as 0x5703 and 0x4301 carry it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Security {
    None = 0,
    Wep = 1,
    WpaWpa2 = 2,
    Wpa3Transition = 3,
    Wpa3Only = 4,
}

/// The access point the iPhone joins for CarPlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// Empty for an open network.
    pub passphrase: String,
    pub channel: u8,
    pub security: Security,
    /// The access point's address, a hint the iPhone may use to find it.
    pub bssid: Option<[u8; 6]>,
}

/// Where the iPhone reaches the AirPlay receiver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receiver {
    /// Its addresses on the network, as text.
    pub addresses: Vec<String>,
    pub port: u16,
    pub device_id: String,
    /// Its long-term public key, in hex.
    pub public_key: String,
    pub source_version: String,
}

/// AccessoryWiFiConfigurationInformation (0x5703): the access point to join.
pub fn accessory_wifi_configuration(network: &Network) -> Result<Frame, ProtocolError> {
    let mut parameters = Parameters::new();
    if let Some(bssid) = network.bssid {
        parameters = parameters.bytes(0, &bssid);
    }
    frame(
        id::ACCESSORY_WIFI_CONFIGURATION,
        parameters
            .string(1, &network.ssid)
            .string(2, &network.passphrase)
            .u8(3, network.security as u8)
            .u8(4, network.channel),
    )
}

/// CarPlayStartSession (0x4301) for CarPlay over Wi-Fi: the network and the AirPlay receiver on it.
pub fn carplay_start_session(
    network: &Network,
    receiver: &Receiver,
) -> Result<Frame, ProtocolError> {
    if network.ssid.is_empty() || receiver.addresses.is_empty() || receiver.port == 0 {
        return Err(ProtocolError::new(
            "CarPlayStartSession needs an SSID, an address and a port",
        ));
    }
    if network.security != Security::None && network.passphrase.is_empty() {
        return Err(ProtocolError::new("a secured network needs a passphrase"));
    }
    let mut wireless = Parameters::new()
        .string(0, &network.ssid)
        .string(1, &network.passphrase)
        .u8(2, network.channel);
    for address in &receiver.addresses {
        wireless = wireless.string(3, address);
    }
    let wireless = wireless.u8(4, network.security as u8);
    frame(
        id::CARPLAY_START_SESSION,
        Parameters::new()
            .group(1, wireless)
            .u32(2, u32::from(receiver.port))
            .string(3, &receiver.device_id)
            .string(4, &receiver.public_key)
            .string(5, &receiver.source_version),
    )
}

/// The updates the accessory subscribes to once it is authenticated: now playing (title, duration, album,
/// artist and artwork; status, position and app), power, communications and call state.
pub fn subscriptions() -> Vec<Frame> {
    let voids = |ids: &[u16]| ids.iter().fold(Parameters::new(), |p, id| p.void(*id));
    [
        (
            id::START_NOW_PLAYING_UPDATES,
            Parameters::new()
                .group(0, voids(&[1, 4, 6, 12, 26]))
                .group(1, voids(&[0, 1, 7])),
        ),
        (id::START_POWER_UPDATES, voids(&[4, 5, 6])),
        (id::START_COMMUNICATIONS_UPDATES, voids(&[0, 4, 5])),
        (id::START_CALL_STATE_UPDATES, voids(&[0, 1, 2, 3, 4, 11])),
    ]
    .into_iter()
    .map(|(message_id, parameters)| frame(message_id, parameters).expect("subscriptions are small"))
    .collect()
}

/// AuthenticationCertificate (0xaa01).
pub fn authentication_certificate(certificate: &[u8]) -> Result<Frame, ProtocolError> {
    frame(
        id::AUTHENTICATION_CERTIFICATE,
        Parameters::new().bytes(0, certificate),
    )
}

/// AuthenticationResponse (0xaa03).
pub fn authentication_response(signature: &[u8]) -> Result<Frame, ProtocolError> {
    frame(
        id::AUTHENTICATION_RESPONSE,
        Parameters::new().bytes(0, signature),
    )
}

/// The challenge of RequestAuthenticationChallengeResponse (0xaa02).
pub fn authentication_challenge(frame: &Frame) -> Result<Vec<u8>, ProtocolError> {
    Ok(frame
        .parameters()?
        .require(0, "authentication challenge")?
        .payload()
        .to_vec())
}

/// Whether a transport is available, from CarPlayAvailability (0x4300).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Availability {
    pub wired: Option<bool>,
    pub wireless: Option<bool>,
    pub theme_assets: Option<bool>,
}

/// Reads CarPlayAvailability (0x4300). An availability is 0 or 1; any other value is an error.
pub fn carplay_availability(frame: &Frame) -> Result<Availability, ProtocolError> {
    let body = frame.parameters()?;
    let flag = |group: u16| -> Result<Option<bool>, ProtocolError> {
        let Some(group) = body.first(group) else {
            return Ok(None);
        };
        match group.as_group()?.first(0) {
            None => Ok(None),
            Some(value) => boolean(value.as_u8()?).map(Some),
        }
    };
    Ok(Availability {
        wired: flag(0)?,
        wireless: flag(1)?,
        theme_assets: flag(2)?,
    })
}

fn boolean(value: u8) -> Result<bool, ProtocolError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(ProtocolError::new(format!(
            "availability must be 0 or 1, not {other}"
        ))),
    }
}

/// Whether wireless CarPlay is available, from WirelessCarPlayUpdate (0x4e0d).
pub fn wireless_carplay_available(frame: &Frame) -> Result<bool, ProtocolError> {
    boolean(
        frame
            .parameters()?
            .require(0, "wireless CarPlay availability")?
            .as_u8()?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Parameter;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn network(bssid: Option<[u8; 6]>) -> Network {
        Network {
            ssid: "LIVI".into(),
            passphrase: "secret123".into(),
            channel: 36,
            security: Security::Wpa3Transition,
            bssid,
        }
    }

    fn receiver() -> Receiver {
        Receiver {
            addresses: vec!["192.168.2.1".into()],
            port: 49152,
            device_id: "dev-1".into(),
            public_key: "aabbcc".into(),
            source_version: "1.0".into(),
        }
    }

    #[test]
    fn wifi_configuration_matches_the_reference_vector() {
        assert_eq!(
            hex(&accessory_wifi_configuration(&network(None))
                .unwrap()
                .encode()),
            "404000275703000900014c49564900000e00027365637265743132330000050003030005000424"
        );
    }

    #[test]
    fn start_session_matches_the_reference_vector() {
        assert_eq!(
            hex(&carplay_start_session(&network(None), &receiver()).unwrap().encode()),
            concat!(
                "40400060430100350001000900004c49564900000e000173656372657431323300000500022400100003",
                "3139322e3136382e322e31000005000403000800020000c000000a00036465762d3100000b000461",
                "61626263630000080005312e3000"
            )
        );
    }

    #[test]
    fn the_access_point_hint_is_only_in_the_wifi_configuration() {
        let bssid = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];
        let configuration = accessory_wifi_configuration(&network(Some(bssid))).unwrap();
        let body = configuration.parameters().unwrap();
        assert_eq!(
            body.iter().map(Parameter::id).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert_eq!(body.first(0).unwrap().payload(), bssid);
        assert_eq!(
            carplay_start_session(&network(Some(bssid)), &receiver()).unwrap(),
            carplay_start_session(&network(None), &receiver()).unwrap()
        );
    }

    #[test]
    fn an_open_network_sends_an_empty_passphrase() {
        let open = Network {
            ssid: "Guest".into(),
            passphrase: String::new(),
            channel: 36,
            security: Security::None,
            bssid: None,
        };
        let configuration = accessory_wifi_configuration(&open)
            .unwrap()
            .parameters()
            .unwrap();
        assert_eq!(configuration.first(2).unwrap().payload(), [0]);
        assert_eq!(configuration.first(3).unwrap().payload(), [0]);
        let start = carplay_start_session(&open, &receiver())
            .unwrap()
            .parameters()
            .unwrap();
        let wireless = start.first(1).unwrap().as_group().unwrap();
        assert_eq!(wireless.first(1).unwrap().payload(), [0]);
        assert_eq!(wireless.first(4).unwrap().payload(), [0]);
        let secured = Network {
            security: Security::WpaWpa2,
            ..open
        };
        assert!(carplay_start_session(&secured, &receiver()).is_err());
    }

    #[test]
    fn several_addresses_repeat_the_parameter() {
        let mut receiver = receiver();
        receiver.addresses.push("fe80::1".into());
        let start = carplay_start_session(&network(None), &receiver)
            .unwrap()
            .parameters()
            .unwrap();
        let wireless = start.first(1).unwrap().as_group().unwrap();
        let addresses: Vec<String> = wireless.all(3).map(|p| p.as_string().unwrap()).collect();
        assert_eq!(addresses, ["192.168.2.1", "fe80::1"]);
        assert_eq!(start.first(2).unwrap().as_u32(), Ok(49152));
    }

    #[test]
    fn availability_is_a_strict_boolean() {
        let update =
            |value: u8| Frame::with_parameters(0x4e0d, &Parameters::new().u8(0, value)).unwrap();
        assert_eq!(wireless_carplay_available(&update(0)), Ok(false));
        assert_eq!(wireless_carplay_available(&update(1)), Ok(true));
        assert!(wireless_carplay_available(&update(2)).is_err());

        let wireless = Parameters::new().u8(0, 1).string(1, "Jane's iPhone");
        let frame = Frame::with_parameters(0x4300, &Parameters::new().group(1, wireless)).unwrap();
        assert_eq!(
            carplay_availability(&frame),
            Ok(Availability {
                wired: None,
                wireless: Some(true),
                theme_assets: None
            })
        );
        assert!(carplay_availability(&Frame::new(0x4300, vec![0]).unwrap()).is_err());
    }

    #[test]
    fn subscriptions_ask_for_now_playing_fields() {
        let frames = subscriptions();
        let ids: Vec<u16> = frames.iter().map(Frame::message_id).collect();
        assert_eq!(ids, [0x5000, 0xae00, 0x4157, 0x4154]);
        let now_playing = frames[0].parameters().unwrap();
        let media: Vec<u16> = now_playing
            .first(0)
            .unwrap()
            .as_group()
            .unwrap()
            .iter()
            .map(Parameter::id)
            .collect();
        let playback: Vec<u16> = now_playing
            .first(1)
            .unwrap()
            .as_group()
            .unwrap()
            .iter()
            .map(Parameter::id)
            .collect();
        assert_eq!(media, [1, 4, 6, 12, 26]);
        assert_eq!(playback, [0, 1, 7]);
    }

    #[test]
    fn authentication_messages() {
        let challenge =
            Frame::with_parameters(0xaa02, &Parameters::new().bytes(0, &[1, 2, 3])).unwrap();
        assert_eq!(authentication_challenge(&challenge).unwrap(), [1, 2, 3]);
        assert!(authentication_challenge(&Frame::new(0xaa02, vec![]).unwrap()).is_err());
        let response = authentication_response(&[9; 64]).unwrap();
        assert_eq!(response.message_id(), 0xaa03);
        assert_eq!(
            response.parameters().unwrap().first(0).unwrap().payload(),
            [9; 64]
        );
    }
}
