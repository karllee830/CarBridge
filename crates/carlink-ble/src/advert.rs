// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The head unit's advertisement: service UUID 0xFCFB, service data 0x0001, and the name in the scan response's
//! service data 0x0002.

use std::fmt;

/// 16-bit service UUID that marks a Carlink head unit.
pub const SERVICE_UUID16: u16 = 0xFCFB;
/// 16-bit UUID of the advertisement's service data.
pub const ADVERT_DATA_UUID16: u16 = 0x0001;
/// 16-bit UUID of the scan response's service data: the head unit's name.
pub const NAME_DATA_UUID16: u16 = 0x0002;

const NAME_LEN: usize = 16;

/// A Bluetooth device address, most significant byte first as written (`AA:BB:CC:DD:EE:FF`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Address(pub [u8; 6]);

impl Address {
    /// Parses `AA:BB:CC:DD:EE:FF`, in either case.
    pub fn parse(text: &str) -> Option<Address> {
        let mut out = [0u8; 6];
        let mut parts = text.split(':');
        for byte in &mut out {
            let part = parts.next()?;
            if part.len() != 2 {
                return None;
            }
            *byte = u8::from_str_radix(part, 16).ok()?;
        }
        parts.next().is_none().then_some(Address(out))
    }
}

impl fmt::Display for Address {
    /// Upper case with colons, as Android writes addresses.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Address({self})")
    }
}

/// Service data 0x0001.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvertData {
    /// Protocol version, major and minor.
    pub version: (u8, u8),
    /// Changes when the head unit starts a new advertising session; the same while it keeps waiting.
    pub serial: u8,
    /// The head unit's Bluetooth address.
    pub car_id: Address,
    pub model_id: [u8; 4],
    pub vendor: [u8; 2],
    /// The head unit expects phones to connect only from closer than this, in dBm.
    pub rssi_threshold: i8,
    /// Present when the head unit waits for one particular phone: see [`crate::hash::phone_tag`].
    pub expected_phone: Option<u16>,
}

impl AdvertData {
    /// Reads service data 0x0001. Bytes after the expected phone are ignored.
    pub fn parse(data: &[u8]) -> Option<AdvertData> {
        if data.len() < 16 {
            return None;
        }
        let mut car_id = [0u8; 6];
        car_id.copy_from_slice(&data[3..9]);
        Some(AdvertData {
            version: (data[0], data[1]),
            serial: data[2],
            car_id: Address(car_id),
            model_id: [data[9], data[10], data[11], data[12]],
            vendor: [data[13], data[14]],
            rssi_threshold: data[15] as i8,
            expected_phone: data.get(16..18).map(|b| u16::from_be_bytes([b[0], b[1]])),
        })
    }
}

/// Reads the name in service data 0x0002: 16 bytes of UTF-8, padded with zero bytes. Returns `None` for an empty
/// name. A name cut in the middle of a character keeps the part before it.
pub fn parse_name(data: &[u8]) -> Option<String> {
    let data = &data[..data.len().min(NAME_LEN)];
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let text = match std::str::from_utf8(&data[..end]) {
        Ok(text) => text,
        Err(err) => std::str::from_utf8(&data[..err.valid_up_to()]).unwrap_or_default(),
    };
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const C10: [u8; 16] = [
        1, 7, 0x5A, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x00, 0x28, 0x00, 0x15, 0, 0, 0xC4,
    ];

    #[test]
    fn c10_advert() {
        let advert = AdvertData::parse(&C10).unwrap();
        assert_eq!(advert.version, (1, 7));
        assert_eq!(advert.serial, 0x5A);
        assert_eq!(advert.car_id, Address([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]));
        assert_eq!(advert.model_id, [0x00, 0x28, 0x00, 0x15]);
        assert_eq!(advert.vendor, [0, 0]);
        assert_eq!(advert.rssi_threshold, -60);
        assert_eq!(advert.expected_phone, None);
    }

    #[test]
    fn expected_phone() {
        let mut data = C10.to_vec();
        data.extend([0xAB, 0xCD]);
        assert_eq!(
            AdvertData::parse(&data).unwrap().expected_phone,
            Some(0xABCD)
        );
    }

    #[test]
    fn short_advert() {
        assert_eq!(AdvertData::parse(&C10[..15]), None);
    }

    #[test]
    fn names() {
        assert_eq!(parse_name(&[0; 16]), None);
        let mut data = [0u8; 16];
        data[..6].copy_from_slice(b"Leapmo");
        assert_eq!(parse_name(&data).as_deref(), Some("Leapmo"));
        let cut = "零跑汽车零跑".as_bytes();
        assert_eq!(parse_name(&cut[..16]).as_deref(), Some("零跑汽车零"));
    }

    #[test]
    fn address_text() {
        let address = Address::parse("aa:bb:cc:dd:ee:0f").unwrap();
        assert_eq!(address.to_string(), "AA:BB:CC:DD:EE:0F");
        assert_eq!(Address::parse("AA:BB:CC:DD:EE"), None);
        assert_eq!(Address::parse("AA:BB:CC:DD:EE:FF:00"), None);
        assert_eq!(Address::parse("AA:BB:CC:DD:EE:F"), None);
    }
}
