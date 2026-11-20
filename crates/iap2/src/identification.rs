// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's
// transport/Iap2IdentificationClient.kt (DiPlay commit 615b32c); wireless transport only, without location, vehicle
// status or route guidance.

//! Identification: the accessory tells the iPhone what it is, which messages it sends and receives, and how it
//! is connected. An iPhone that rejects only messages it does not know, as an older iOS does with the CarPlay
//! pair 0x4300 and 0x4301, is sent the identification once more without them.

use std::time::{Duration, Instant};

use crate::error::{Error, Rejection};
use crate::messages::id;
use crate::session::Session;
use crate::wire::{Frame, Parameters, ProtocolError};

/// What the accessory says it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accessory {
    pub name: String,
    pub model: String,
    pub manufacturer: String,
    pub serial_number: String,
    pub firmware_version: String,
    pub hardware_version: String,
    /// The language it speaks, such as `en`.
    pub language: String,
    /// The external accessory protocol it names; it need not be one an app uses.
    pub accessory_protocol: String,
}

/// How the accessory is reached without wires.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wireless {
    /// The Bluetooth adapter the iPhone paired with.
    pub bluetooth_address: [u8; 6],
    /// The access point the iPhone joins for CarPlay.
    pub ssid: String,
}

/// Reads a Bluetooth address written as six colon-separated hex bytes.
pub fn parse_bluetooth_address(text: &str) -> Option<[u8; 6]> {
    let mut address = [0; 6];
    let mut parts = text.split(':');
    for byte in &mut address {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(address)
}

/// The messages the accessory sends.
const SENT: &[u16] = &[
    id::AUTHENTICATION_CERTIFICATE,
    id::AUTHENTICATION_RESPONSE,
    id::START_NOW_PLAYING_UPDATES,
    id::STOP_NOW_PLAYING_UPDATES,
    id::START_POWER_UPDATES,
    id::STOP_POWER_UPDATES,
    id::START_COMMUNICATIONS_UPDATES,
    id::STOP_COMMUNICATIONS_UPDATES,
    id::START_CALL_STATE_UPDATES,
    id::STOP_CALL_STATE_UPDATES,
    id::CARPLAY_START_SESSION,
    id::ACCESSORY_WIFI_CONFIGURATION,
];

/// The messages the accessory receives.
const RECEIVED: &[u16] = &[
    id::REQUEST_AUTHENTICATION_CERTIFICATE,
    id::REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
    id::AUTHENTICATION_FAILED,
    id::AUTHENTICATION_SUCCEEDED,
    id::START_EXTERNAL_ACCESSORY_PROTOCOL_SESSION,
    id::STOP_EXTERNAL_ACCESSORY_PROTOCOL_SESSION,
    id::NOW_PLAYING_UPDATE,
    id::POWER_UPDATE,
    id::COMMUNICATIONS_UPDATE,
    id::CALL_STATE_UPDATE,
    id::CARPLAY_AVAILABILITY,
    id::WIRELESS_CARPLAY_UPDATE,
    id::DEVICE_TRANSPORT_IDENTIFIER_NOTIFICATION,
    id::REQUEST_ACCESSORY_WIFI_CONFIGURATION,
];

const AUTHENTICATION: &[u16] = &[
    id::REQUEST_AUTHENTICATION_CERTIFICATE,
    id::AUTHENTICATION_CERTIFICATE,
    id::REQUEST_AUTHENTICATION_CHALLENGE_RESPONSE,
    id::AUTHENTICATION_RESPONSE,
    id::AUTHENTICATION_FAILED,
    id::AUTHENTICATION_SUCCEEDED,
];

/// IdentificationInformation (0x1d01) for a wireless accessory, leaving out the messages in `omitted`.
pub fn identification_information(
    accessory: &Accessory,
    wireless: &Wireless,
    omitted: &[u16],
) -> Result<Frame, ProtocolError> {
    let kept = |list: &[u16]| -> Vec<u16> {
        list.iter()
            .copied()
            .filter(|id| !omitted.contains(id))
            .collect()
    };
    let parameters = Parameters::new()
        .string(0, &accessory.name)
        .string(1, &accessory.model)
        .string(2, &accessory.manufacturer)
        .string(3, &accessory.serial_number)
        .string(4, &accessory.firmware_version)
        .string(5, &accessory.hardware_version)
        .u16_list(6, kept(SENT))
        .u16_list(7, kept(RECEIVED))
        // Power source: none, as the accessory charges nothing without wires.
        .u8(8, 0)
        // Maximum current drawn from the device, in mA.
        .u16(9, 20)
        .group(
            10,
            Parameters::new()
                .u8(0, 1)
                .string(1, &accessory.accessory_protocol)
                .u8(2, 0),
        )
        .string(12, &accessory.language)
        .strings(13, [accessory.language.as_str()])
        .group(
            17,
            Parameters::new()
                .u16(0, 0)
                .string(1, "blue")
                .void(2)
                .bytes(3, &wireless.bluetooth_address)
                .string(4, "blue")
                .void(5),
        )
        .group(
            24,
            Parameters::new()
                .u16(0, 1)
                .string(1, &wireless.ssid)
                .void(2)
                .u16(3, 1)
                .void(4)
                .void(5),
        );
    Frame::with_parameters(id::IDENTIFICATION_INFORMATION, &parameters)
}

/// Reads IdentificationRejected (0x1d03).
pub fn rejection(frame: &Frame) -> Result<Rejection, ProtocolError> {
    let body = frame.parameters()?;
    let mut rejection = Rejection::default();
    for parameter in &body {
        if !rejection.parameters.contains(&parameter.id()) {
            rejection.parameters.push(parameter.id());
        }
        let list = match parameter.id() {
            6 => &mut rejection.unsupported_sent,
            7 => &mut rejection.unsupported_received,
            _ => continue,
        };
        match parameter.as_u16_list() {
            Ok(ids) if !ids.is_empty() => list.extend(ids),
            _ => rejection.malformed = true,
        }
    }
    Ok(rejection)
}

impl Rejection {
    /// The messages to leave out when the rejection asks for nothing else: it rejects only the two message lists,
    /// they read cleanly, and they name no authentication message.
    pub fn omittable(&self) -> Option<Vec<u16>> {
        if self.parameters.is_empty()
            || self.malformed
            || self.parameters.iter().any(|id| !matches!(id, 6 | 7))
        {
            return None;
        }
        let mut ids: Vec<u16> = self
            .unsupported_sent
            .iter()
            .chain(&self.unsupported_received)
            .copied()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() || ids.iter().any(|id| AUTHENTICATION.contains(id)) {
            return None;
        }
        Some(ids)
    }
}

/// Identifies the accessory on a session whose link is up, within `timeout`.
pub fn identify(
    session: &Session,
    accessory: &Accessory,
    wireless: &Wireless,
    timeout: Duration,
    log: &dyn Fn(&str),
) -> Result<(), Error> {
    let deadline = Instant::now() + timeout;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    if !session.await_ready(remaining())? {
        return Err(Error::TimedOut("waiting for the iAP2 link"));
    }
    let mut omitted: Option<Vec<u16>> = None;
    loop {
        let frame = session
            .recv(remaining())?
            .ok_or(Error::TimedOut("waiting for identification"))?;
        match frame.message_id() {
            id::START_IDENTIFICATION => {
                let information = identification_information(
                    accessory,
                    wireless,
                    omitted.as_deref().unwrap_or_default(),
                )?;
                session.send(&information, remaining())?;
            }
            id::IDENTIFICATION_ACCEPTED => return Ok(()),
            id::IDENTIFICATION_REJECTED => {
                let rejected = rejection(&frame)?;
                let retry = match (&omitted, rejected.omittable()) {
                    (None, Some(ids)) => ids,
                    _ => return Err(Error::IdentificationRejected(rejected)),
                };
                log(&format!(
                    "iAP2 identification sent again without messages {}",
                    retry
                        .iter()
                        .map(|id| format!("0x{id:04x}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                session.send(
                    &identification_information(accessory, wireless, &retry)?,
                    remaining(),
                )?;
                omitted = Some(retry);
            }
            other => return Err(Error::UnexpectedMessage(other)),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::wire::Parameter;

    pub(crate) fn accessory() -> Accessory {
        Accessory {
            name: "LIVI".into(),
            model: "LIVI".into(),
            manufacturer: "LIVI".into(),
            serial_number: "0123456".into(),
            firmware_version: "1.0.0".into(),
            hardware_version: "1.0".into(),
            language: "en".into(),
            accessory_protocol: "org.example.accessory".into(),
        }
    }

    pub(crate) fn wireless() -> Wireless {
        Wireless {
            bluetooth_address: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            ssid: "LIVI".into(),
        }
    }

    fn lists(frame: &Frame) -> (Vec<u16>, Vec<u16>) {
        let body = frame.parameters().unwrap();
        (
            body.first(6).unwrap().as_u16_list().unwrap(),
            body.first(7).unwrap().as_u16_list().unwrap(),
        )
    }

    fn rejected(parameters: &[(u16, &[u8])]) -> Frame {
        let body = parameters
            .iter()
            .fold(Parameters::new(), |body, (id, payload)| {
                body.bytes(*id, payload)
            });
        Frame::with_parameters(id::IDENTIFICATION_REJECTED, &body).unwrap()
    }

    #[test]
    fn wireless_identification_advertises_its_transports() {
        let frame = identification_information(&accessory(), &wireless(), &[]).unwrap();
        let body = frame.parameters().unwrap();
        assert!(body.first(16).is_none());
        assert_eq!(body.first(8).unwrap().payload(), [0]);
        let bluetooth = body.first(17).unwrap().as_group().unwrap();
        assert_eq!(bluetooth.first(1).unwrap().payload(), b"blue\0");
        assert_eq!(
            bluetooth.first(3).unwrap().payload(),
            [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
        );
        let carplay = body.first(24).unwrap().as_group().unwrap();
        assert_eq!(carplay.first(1).unwrap().payload(), b"LIVI\0");
        let (sent, received) = lists(&frame);
        for id in [0x5703, 0x4301, 0xaa01, 0xaa03] {
            assert!(sent.contains(&id), "sends 0x{id:04x}");
        }
        assert!(!sent.contains(&0xae03));
        for id in [0x4300, 0x4e0d, 0x4e0e, 0x5702, 0x5001, 0xaa05] {
            assert!(received.contains(&id), "receives 0x{id:04x}");
        }
        assert_eq!(
            body.iter().map(Parameter::id).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 13, 17, 24]
        );
    }

    #[test]
    fn omitted_messages_leave_both_lists() {
        let frame =
            identification_information(&accessory(), &wireless(), &[0x4300, 0x4301]).unwrap();
        let (sent, received) = lists(&frame);
        assert!(!sent.contains(&0x4301));
        assert!(!received.contains(&0x4300));
        assert!(sent.contains(&0xaa03));
        assert!(received.contains(&0xaa05));
    }

    #[test]
    fn only_message_list_rejections_can_be_corrected() {
        let omittable =
            |parameters: &[(u16, &[u8])]| rejection(&rejected(parameters)).unwrap().omittable();
        assert_eq!(
            omittable(&[(6, &[0x43, 0x01]), (7, &[0x43, 0x00])]),
            Some(vec![0x4300, 0x4301])
        );
        assert_eq!(omittable(&[(16, &[])]), None);
        assert_eq!(omittable(&[(6, &[0x43, 0x01]), (16, &[])]), None);
        assert_eq!(omittable(&[(6, &[0xaa, 0x01])]), None);
        assert_eq!(omittable(&[(7, &[0x43, 0x00, 0x50])]), None);
        assert_eq!(omittable(&[(6, &[])]), None);
    }

    #[test]
    fn rejected_message_ids_are_unsigned_big_endian_in_wire_order() {
        // carplayd's fixture: an empty parameter 30, [0x415a] in parameter 6, and parameter 20.
        let frame = Frame::new(
            id::IDENTIFICATION_REJECTED,
            vec![0, 4, 0, 30, 0, 6, 0, 6, 0x41, 0x5a, 0, 4, 0, 20],
        )
        .unwrap();
        let fixture = rejection(&frame).unwrap();
        assert_eq!(fixture.parameters, [30, 6, 20]);
        assert_eq!(fixture.unsupported_sent, [0x415a]);
        assert_eq!(fixture.omittable(), None);

        let both = rejection(&rejected(&[(6, &[0x50, 1]), (7, &[0xaa, 0, 0xff, 0xff])])).unwrap();
        assert_eq!(both.unsupported_sent, [0x5001]);
        assert_eq!(both.unsupported_received, [0xaa00, 0xffff]);
        let repeated = rejection(&rejected(&[(6, &[0x50, 0, 0x50, 0]), (6, &[0x52, 0])])).unwrap();
        assert_eq!(repeated.unsupported_sent, [0x5000, 0x5000, 0x5200]);
        assert_eq!(repeated.parameters, [6]);
    }

    #[test]
    fn bluetooth_addresses_parse() {
        assert_eq!(
            parse_bluetooth_address("AA:bb:CC:dd:EE:01"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x01])
        );
        assert_eq!(parse_bluetooth_address("AA:BB:CC:DD:EE"), None);
        assert_eq!(parse_bluetooth_address("AA:BB:CC:DD:EE:FF:00"), None);
        assert_eq!(parse_bluetooth_address("AAB:B:CC:DD:EE:FF"), None);
    }
}
