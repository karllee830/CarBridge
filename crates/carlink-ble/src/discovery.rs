// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Recognising a Carlink head unit among the devices a scan reports.

use std::collections::HashMap;

use crate::advert::{
    parse_name, Address, AdvertData, ADVERT_DATA_UUID16, NAME_DATA_UUID16, SERVICE_UUID16,
};

/// A 16-bit UUID as lower-case 128-bit text, on the Bluetooth base UUID.
pub fn uuid16_text(uuid: u16) -> String {
    format!("0000{uuid:04x}-0000-1000-8000-00805f9b34fb")
}

/// A head unit found by a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadUnit {
    /// The address to connect to.
    pub address: Address,
    /// Connect with a public address rather than a random one.
    pub public: bool,
    pub rssi: Option<i16>,
    pub advert: AdvertData,
    /// The name of the scan response, when the head unit gives one.
    pub name: Option<String>,
}

impl HeadUnit {
    /// Reads a scan report: the device's address, its address type, its RSSI, the service UUIDs it advertises,
    /// whether it is paired and its service data by UUID, all UUIDs as 128-bit text in any case. Returns `None` for
    /// a device that is not a Carlink head unit.
    ///
    /// A paired device is reported under its identity address with the UUIDs of its services rather than those of
    /// its advertisement; its service data alone identifies it as a head unit.
    pub fn from_report(
        address: &str,
        public: bool,
        rssi: Option<i16>,
        uuids: &[String],
        paired: bool,
        service_data: &HashMap<String, Vec<u8>>,
    ) -> Option<HeadUnit> {
        let service = uuid16_text(SERVICE_UUID16);
        let advertised = uuids.iter().any(|u| u.eq_ignore_ascii_case(&service));
        let data = |uuid16| {
            let key = uuid16_text(uuid16);
            service_data
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| v.as_slice())
        };
        let advert = AdvertData::parse(data(ADVERT_DATA_UUID16)?)?;
        if !advertised && !paired {
            return None;
        }
        Some(HeadUnit {
            address: Address::parse(address)?,
            public,
            rssi,
            advert,
            name: data(NAME_DATA_UUID16).and_then(parse_name),
        })
    }

    /// Whether the phone is close enough by the head unit's own threshold. Unknown RSSI counts as close.
    pub fn in_range(&self) -> bool {
        self.rssi
            .is_none_or(|rssi| rssi >= i16::from(self.advert.rssi_threshold))
    }

    /// Whether the head unit waits for one particular phone and it is the one whose stored hash gives `tag`.
    pub fn waits_for(&self, tag: u16) -> bool {
        self.advert.expected_phone == Some(tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> (Vec<String>, HashMap<String, Vec<u8>>) {
        let uuids = vec!["0000FCFB-0000-1000-8000-00805F9B34FB".to_owned()];
        let mut data = HashMap::new();
        data.insert(
            "00000001-0000-1000-8000-00805f9b34fb".to_owned(),
            vec![
                1, 7, 0x5A, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x00, 0x28, 0x00, 0x15, 0, 0, 0xC4,
                0x12, 0x34,
            ],
        );
        let mut name = vec![0u8; 16];
        name[..3].copy_from_slice(b"C10");
        data.insert("00000002-0000-1000-8000-00805F9B34FB".to_owned(), name);
        (uuids, data)
    }

    #[test]
    fn head_unit_from_report() {
        let (uuids, data) = report();
        let unit =
            HeadUnit::from_report("11:22:33:44:55:66", true, Some(-50), &uuids, false, &data)
                .unwrap();
        assert_eq!(unit.address, Address([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]));
        assert_eq!(unit.advert.car_id, unit.address);
        assert_eq!(unit.name.as_deref(), Some("C10"));
        assert!(unit.in_range());
        assert!(unit.waits_for(0x1234));
        assert!(!unit.waits_for(0x4321));
    }

    #[test]
    fn out_of_range() {
        let (uuids, data) = report();
        let unit =
            HeadUnit::from_report("11:22:33:44:55:66", true, Some(-75), &uuids, false, &data)
                .unwrap();
        assert!(!unit.in_range());
    }

    #[test]
    fn not_a_head_unit() {
        let (uuids, mut data) = report();
        assert!(
            HeadUnit::from_report("11:22:33:44:55:66", true, None, &[], false, &data).is_none()
        );
        data.remove("00000001-0000-1000-8000-00805f9b34fb");
        assert!(
            HeadUnit::from_report("11:22:33:44:55:66", true, None, &uuids, false, &data).is_none()
        );
    }

    #[test]
    fn paired_head_unit() {
        let (_, data) = report();
        let services = vec!["0000110a-0000-1000-8000-00805f9b34fb".to_owned()];
        assert!(
            HeadUnit::from_report("11:22:33:44:55:66", true, None, &services, true, &data)
                .is_some()
        );
    }

    #[test]
    fn uuid_text() {
        assert_eq!(uuid16_text(0xFCFB), "0000fcfb-0000-1000-8000-00805f9b34fb");
    }
}
