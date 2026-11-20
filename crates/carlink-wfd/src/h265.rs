// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! H.265 in Annex B form: NAL units after start codes, and access units.

use crate::h264::{nal_units, AccessUnit};

/// NAL unit types used here.
pub mod nal {
    /// The intra random access point pictures: BLA, IDR and CRA.
    pub const IRAP: std::ops::RangeInclusive<u8> = 16..=21;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
    pub const PREFIX_SEI: u8 = 39;
}

pub fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| (b >> 1) & 0x3F)
}

/// Whether `unit` is a coded slice segment, of any picture type.
fn is_slice(unit: &[u8]) -> bool {
    nal_type(unit) < 32
}

/// Whether `unit` is a coded slice segment that starts a picture (`first_slice_segment_in_pic_flag` is set).
fn starts_picture(unit: &[u8]) -> bool {
    is_slice(unit) && unit.get(2).is_some_and(|b| b & 0x80 != 0)
}

/// The access unit without its access unit delimiters, in Annex B form with four-byte start codes.
pub fn without_delimiters(access_unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(access_unit.len());
    for unit in nal_units(access_unit)
        .into_iter()
        .filter(|unit| nal_type(unit) != nal::AUD)
    {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    out
}

/// Groups the NAL units of an Annex B stream into access units; an access unit is `key` when it holds an IRAP
/// picture. NAL units before the first picture join it.
pub fn access_units(stream: &[u8]) -> Vec<AccessUnit> {
    let mut units: Vec<AccessUnit> = Vec::new();
    let mut current = AccessUnit {
        data: Vec::new(),
        key: false,
    };
    let mut has_picture = false;
    for unit in nal_units(stream) {
        let kind = nal_type(unit);
        let new_unit = has_picture
            && (matches!(
                kind,
                nal::VPS | nal::SPS | nal::PPS | nal::AUD | nal::PREFIX_SEI
            ) || starts_picture(unit));
        if new_unit {
            units.push(std::mem::replace(
                &mut current,
                AccessUnit {
                    data: Vec::new(),
                    key: false,
                },
            ));
            has_picture = false;
        }
        current.data.extend_from_slice(&[0, 0, 0, 1]);
        current.data.extend_from_slice(unit);
        if is_slice(unit) {
            has_picture = true;
            current.key |= nal::IRAP.contains(&kind);
        }
    }
    if has_picture {
        units.push(current);
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(units: &[&[u8]]) -> Vec<u8> {
        let mut stream = Vec::new();
        for unit in units {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(unit);
        }
        stream
    }

    #[test]
    fn nal_types() {
        assert_eq!(nal_type(&[0x40, 0x01]), nal::VPS);
        assert_eq!(nal_type(&[0x42, 0x01]), nal::SPS);
        assert_eq!(nal_type(&[0x44, 0x01]), nal::PPS);
        assert_eq!(nal_type(&[0x46, 0x01]), nal::AUD);
        assert_eq!(nal_type(&[0x26, 0x01]), 19);
        assert_eq!(nal_type(&[0x02, 0x01]), 1);
    }

    #[test]
    fn delimiters_left_out() {
        let au = stream(&[&[0x46, 0x01, 0x50], &[0x26, 0x01, 0xAF]]);
        assert_eq!(without_delimiters(&au), [0, 0, 0, 1, 0x26, 0x01, 0xAF]);
        // An H.264 delimiter's first byte is an H.265 slice: it stays.
        let au = stream(&[&[0x09, 0x01, 0x80]]);
        assert_eq!(without_delimiters(&au), au);
    }

    #[test]
    fn groups_pictures() {
        let units = access_units(&stream(&[
            &[0x40, 0x01, 0x0C],
            &[0x42, 0x01, 0x01],
            &[0x44, 0x01, 0xC1],
            // IDR_W_RADL in two slice segments.
            &[0x26, 0x01, 0xAF],
            &[0x26, 0x01, 0x20],
            // TRAIL_R pictures.
            &[0x02, 0x01, 0xD0],
            &[0x02, 0x01, 0xD1],
            // CRA after a prefix SEI.
            &[0x4E, 0x01, 0x05],
            &[0x2A, 0x01, 0xAD],
        ]));
        assert_eq!(units.len(), 4);
        assert!(units[0].key);
        assert_eq!(nal_units(&units[0].data).len(), 5);
        assert!(!units[1].key);
        assert_eq!(nal_units(&units[2].data), [&[0x02, 0x01, 0xD1][..]]);
        assert!(units[3].key);
        assert_eq!(nal_units(&units[3].data).len(), 2);
    }
}
