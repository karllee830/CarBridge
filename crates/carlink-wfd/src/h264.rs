// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! H.264 in Annex B form: NAL units after start codes, and access units.

/// NAL unit types used here.
pub mod nal {
    pub const IDR: u8 = 5;
    pub const SEI: u8 = 6;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// The NAL units of an Annex B stream, without their start codes.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= stream.len() {
        if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(stream.len(), |&next| next - 3);
        // A four-byte start code leaves a zero byte behind the previous unit.
        while n + 1 < starts.len() && end > start && stream[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            units.push(&stream[start..end]);
        }
    }
    units
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

pub fn nal_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| b & 0x1F)
}

/// Whether `unit` is a coded slice that starts a picture (`first_mb_in_slice` is 0).
fn starts_picture(unit: &[u8]) -> bool {
    matches!(nal_type(unit), 1..=5) && unit.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// An access unit: the NAL units of one picture, in Annex B form with four-byte start codes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    pub data: Vec<u8>,
    /// It holds a picture decodable on its own: IDR in H.264, IRAP in H.265.
    pub key: bool,
}

/// Groups the NAL units of an Annex B stream into access units. NAL units before the first picture join it.
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
            && (matches!(kind, nal::AUD | nal::SEI | nal::SPS | nal::PPS) || starts_picture(unit));
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
        if matches!(kind, 1..=5) {
            has_picture = true;
            current.key |= kind == nal::IDR;
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

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 0x88, 4,
        ];
        let units = nal_units(&stream);
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 0x88, 4]]);
    }

    #[test]
    fn delimiters_left_out() {
        let au = [
            0, 0, 0, 1, 0x09, 0x10, 0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(
            without_delimiters(&au),
            [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x65, 0x88]
        );
    }

    #[test]
    fn groups_pictures() {
        let mut stream = Vec::new();
        for unit in [
            &[0x67u8, 1][..],
            &[0x68, 2],
            &[0x65, 0x88],
            &[0x65, 0x08],
            &[0x41, 0x9A],
            &[0x41, 0x9B],
        ] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(unit);
        }
        let units = access_units(&stream);
        assert_eq!(units.len(), 3);
        assert!(units[0].key);
        assert_eq!(nal_units(&units[0].data).len(), 4);
        assert!(!units[1].key);
        assert_eq!(nal_units(&units[2].data), [&[0x41, 0x9B][..]]);
    }
}
