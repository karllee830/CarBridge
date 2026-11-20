// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The iPhone's screen as access units the head unit's player takes: Annex B with four-byte start codes, without
//! access unit delimiters, and with the parameter sets in front of every key picture: the SPS and PPS of H.264 before
//! an IDR picture, the VPS, SPS and PPS of H.265 before an IRAP picture.
//!
//! The units of a key picture go in the order the codec's syntax gives an access unit: the parameter sets, then the
//! picture's other units in the order they came, prefix SEI before the slices and suffix SEI after them. The parameter
//! sets are the latest complete group known ([`ParameterSets`]): from the iPhone's codec configuration, or from a key
//! picture that carries its own, so a key picture without its own still gets them, whichever way the iPhone gives
//! them. Access unit delimiters, which the MPEG-2 transport stream otherwise asks for, are left out: the head unit's
//! player tells a key picture by the unit at the very start of the access unit.

pub use carbridge_airplay::screen::VideoCodec;

/// One picture from the iPhone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    /// Annex B, four-byte start codes.
    pub data: Vec<u8>,
    /// It holds a picture decodable on its own (IDR in H.264, IRAP in H.265), led by the parameter sets.
    pub key: bool,
    /// For a key picture: the parameter sets it is led by are a complete group (VPS, SPS and PPS in H.265, SPS and PPS
    /// in H.264). A key picture without them cannot be decoded by a player that has not had them before.
    pub complete: bool,
    /// The iPhone's timestamp in nanoseconds.
    pub sender_nanos: u64,
    pub codec: VideoCodec,
}

/// H.264 NAL unit types.
mod avc {
    pub const IDR: u8 = 5;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const AUD: u8 = 9;
}

/// H.265 NAL unit types.
mod hevc {
    /// The intra random access point pictures: BLA, IDR and CRA.
    pub const IRAP: std::ops::RangeInclusive<u8> = 16..=21;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
}

/// What sets a codec's NAL units apart.
#[derive(Clone, Copy)]
struct Nal(VideoCodec);

impl Nal {
    fn kind(self, unit: &[u8]) -> u8 {
        match self.0 {
            VideoCodec::H264 => unit[0] & 0x1F,
            VideoCodec::H265 => (unit[0] >> 1) & 0x3F,
        }
    }

    fn is_delimiter(self, unit: &[u8]) -> bool {
        let kind = self.kind(unit);
        match self.0 {
            VideoCodec::H264 => kind == avc::AUD,
            VideoCodec::H265 => kind == hevc::AUD,
        }
    }

    fn is_key(self, unit: &[u8]) -> bool {
        let kind = self.kind(unit);
        match self.0 {
            VideoCodec::H264 => kind == avc::IDR,
            VideoCodec::H265 => hevc::IRAP.contains(&kind),
        }
    }

    fn is_parameter_set(self, unit: &[u8]) -> bool {
        let kind = self.kind(unit);
        match self.0 {
            VideoCodec::H264 => matches!(kind, avc::SPS | avc::PPS),
            VideoCodec::H265 => matches!(kind, hevc::VPS | hevc::SPS | hevc::PPS),
        }
    }
}

/// The parameter sets of a codec configuration record, `avcC` for H.264 or `hvcC` for H.265, in Annex B form; also of
/// a configuration given as Annex B units, as some streams do. `None` without a complete group.
pub fn parameter_sets(codec: VideoCodec, record: &[u8]) -> Option<Vec<u8>> {
    let sets = match codec {
        VideoCodec::H264 => avc_parameter_sets(record),
        VideoCodec::H265 => hevc_parameter_sets(record),
    }
    .or_else(|| {
        // Annex B: the record starts with a start code.
        (record.starts_with(&[0, 0, 1]) || record.starts_with(&[0, 0, 0, 1]))
            .then(|| group(Nal(codec), nal_units(record).into_iter()))
            .flatten()
    })?;
    complete(Nal(codec), &sets).then_some(sets)
}

/// Where the parameter sets of the screen come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetsSource {
    /// The iPhone's codec configuration.
    Config,
    /// A key picture that carried its own.
    Stream,
}

/// The latest complete group of parameter sets of the screen's stream, in Annex B form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParameterSets {
    sets: Option<(VideoCodec, Vec<u8>, SetsSource)>,
}

impl ParameterSets {
    /// Forgets them, as when the screen's stream ends.
    pub fn clear(&mut self) {
        self.sets = None;
    }

    /// Takes the sets of a codec configuration record; whether it had a complete group. A record without one leaves
    /// the sets known as they are.
    pub fn config(&mut self, codec: VideoCodec, record: &[u8]) -> bool {
        match parameter_sets(codec, record) {
            Some(sets) => {
                self.sets = Some((codec, sets, SetsSource::Config));
                true
            }
            None => false,
        }
    }

    /// The sets known for `codec` and where they come from.
    pub fn get(&self, codec: VideoCodec) -> Option<(&[u8], SetsSource)> {
        self.sets
            .as_ref()
            .filter(|(known, _, _)| *known == codec)
            .map(|(_, sets, source)| (sets.as_slice(), *source))
    }
}

/// The parameter sets among `units`, VPS, SPS and PPS in that order, each kind in the order it came.
fn group<'a>(nal: Nal, units: impl Iterator<Item = &'a [u8]>) -> Option<Vec<u8>> {
    let mut sets: Vec<&[u8]> = units.filter(|unit| nal.is_parameter_set(unit)).collect();
    sets.sort_by_key(|unit| nal.kind(unit));
    let mut out = Vec::new();
    for unit in sets {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    (!out.is_empty()).then_some(out)
}

/// Whether Annex B `sets` hold every kind of parameter set a key picture needs.
fn complete(nal: Nal, sets: &[u8]) -> bool {
    let kinds: Vec<u8> = nal_units(sets).iter().map(|unit| nal.kind(unit)).collect();
    match nal.0 {
        VideoCodec::H264 => [avc::SPS, avc::PPS].iter().all(|k| kinds.contains(k)),
        VideoCodec::H265 => [hevc::VPS, hevc::SPS, hevc::PPS]
            .iter()
            .all(|k| kinds.contains(k)),
    }
}

/// The kinds of the units of an Annex B access unit, for the log: `VPS SPS PPS SEI IDR_W_RADL`, other kinds by number.
pub fn unit_kinds(codec: VideoCodec, annex_b: &[u8]) -> String {
    let nal = Nal(codec);
    nal_units(annex_b)
        .iter()
        .map(|unit| {
            let kind = nal.kind(unit);
            let name = match (codec, kind) {
                (VideoCodec::H264, 1) => "non-IDR",
                (VideoCodec::H264, avc::IDR) => "IDR",
                (VideoCodec::H264, 6) => "SEI",
                (VideoCodec::H264, avc::SPS) => "SPS",
                (VideoCodec::H264, avc::PPS) => "PPS",
                (VideoCodec::H264, avc::AUD) => "AUD",
                (VideoCodec::H265, 0..=9) => "TRAIL/TSA/STSA/RADL/RASL",
                (VideoCodec::H265, 16..=18) => "BLA",
                (VideoCodec::H265, 19) => "IDR_W_RADL",
                (VideoCodec::H265, 20) => "IDR_N_LP",
                (VideoCodec::H265, 21) => "CRA",
                (VideoCodec::H265, hevc::VPS) => "VPS",
                (VideoCodec::H265, hevc::SPS) => "SPS",
                (VideoCodec::H265, hevc::PPS) => "PPS",
                (VideoCodec::H265, hevc::AUD) => "AUD",
                (VideoCodec::H265, 39) => "prefix-SEI",
                (VideoCodec::H265, 40) => "suffix-SEI",
                _ => return kind.to_string(),
            };
            name.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The parameter sets of an `avcC` record (AVCDecoderConfigurationRecord), in Annex B form.
fn avc_parameter_sets(avcc: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 5;
    for count_mask in [0x1F, 0xFF] {
        let count = usize::from(*avcc.get(at)? & count_mask);
        at += 1;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
            at += 2;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(avcc.get(at..at + len)?);
            at += len;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The VPS, SPS and PPS of an `hvcC` record (HEVCDecoderConfigurationRecord), in Annex B form and in that order.
/// The record's other NAL units, such as SEI, are left out.
fn hevc_parameter_sets(hvcc: &[u8]) -> Option<Vec<u8>> {
    // The fixed part is 22 bytes, up to and including lengthSizeMinusOne; numOfArrays follows.
    let arrays = *hvcc.get(22)?;
    let mut at = 23;
    let mut sets: Vec<(u8, &[u8])> = Vec::new();
    for _ in 0..arrays {
        let kind = *hvcc.get(at)? & 0x3F;
        let count = u16::from_be_bytes([*hvcc.get(at + 1)?, *hvcc.get(at + 2)?]);
        at += 3;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes([*hvcc.get(at)?, *hvcc.get(at + 1)?]));
            at += 2;
            let unit = hvcc.get(at..at + len)?;
            at += len;
            if matches!(kind, hevc::VPS | hevc::SPS | hevc::PPS) && !unit.is_empty() {
                sets.push((kind, unit));
            }
        }
    }
    sets.sort_by_key(|&(kind, _)| kind);
    let mut out = Vec::new();
    for (_, unit) in sets {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    (!out.is_empty()).then_some(out)
}

/// The NAL units of an Annex B buffer.
fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .filter_map(|(n, &start)| {
            let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            while n + 1 < starts.len() && end > start && data[end - 1] == 0 {
                end -= 1;
            }
            (end > start).then(|| &data[start..end])
        })
        .collect()
}

/// Rebuilds one access unit in `codec` for the head unit: drops access unit delimiters, and leads a key picture by the
/// latest complete group of parameter sets, its own if it carries one (which `sets` then keeps), else those `sets`
/// knows; the picture's own parameter sets are not repeated after them. Other pictures keep their units in their order.
pub fn access_unit(
    codec: VideoCodec,
    annex_b: &[u8],
    sets: &mut ParameterSets,
    sender_nanos: u64,
) -> VideoFrame {
    let nal = Nal(codec);
    let units: Vec<&[u8]> = nal_units(annex_b)
        .into_iter()
        .filter(|unit| !nal.is_delimiter(unit))
        .collect();
    let key = units.iter().any(|unit| nal.is_key(unit));
    let (own, rest): (Vec<&[u8]>, Vec<&[u8]>) = units
        .into_iter()
        .partition(|unit| nal.is_parameter_set(unit));
    let mut data = Vec::with_capacity(annex_b.len() + 64);
    let mut complete_sets = false;
    if key {
        if let Some(group) = group(nal, own.iter().copied()).filter(|g| complete(nal, g)) {
            sets.sets = Some((codec, group, SetsSource::Stream));
        }
        match sets.get(codec) {
            Some((known, _)) => {
                data.extend_from_slice(known);
                complete_sets = true;
            }
            // No complete group known: what the picture carries, as it is.
            None => {
                if let Some(partial) = group(nal, own.iter().copied()) {
                    data.extend_from_slice(&partial);
                }
            }
        }
    } else if let Some(partial) = group(nal, own.iter().copied()) {
        data.extend_from_slice(&partial);
    }
    for unit in rest {
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(unit);
    }
    VideoFrame {
        data,
        key,
        complete: key && complete_sets,
        sender_nanos,
        codec,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // avcC: version 1, profile 0x42, compatibility, level 0x1f, length size 4, one SPS, one PPS.
    const AVCC: &[u8] = &[
        1, 0x42, 0xC0, 0x1F, 0xFF, 0xE1, 0, 3, 0x67, 0x42, 0x1F, 1, 0, 2, 0x68, 0xCE,
    ];

    #[test]
    fn parameter_sets_from_avcc() {
        assert_eq!(
            parameter_sets(VideoCodec::H264, AVCC).unwrap(),
            [0, 0, 0, 1, 0x67, 0x42, 0x1F, 0, 0, 0, 1, 0x68, 0xCE]
        );
        assert_eq!(parameter_sets(VideoCodec::H264, &AVCC[..9]), None);
    }

    /// The sets of `AVCC`, known from the configuration.
    fn avc_sets() -> ParameterSets {
        let mut sets = ParameterSets::default();
        assert!(sets.config(VideoCodec::H264, AVCC));
        sets
    }

    #[test]
    fn idr_gets_parameter_sets_and_loses_delimiter() {
        let mut sets = avc_sets();
        let frame = access_unit(
            VideoCodec::H264,
            &[0, 0, 0, 1, 0x09, 0xF0, 0, 0, 0, 1, 0x65, 0x88, 0x80],
            &mut sets,
            42,
        );
        assert!(frame.key && frame.complete);
        assert_eq!(frame.sender_nanos, 42);
        let mut expected = parameter_sets(VideoCodec::H264, AVCC).unwrap();
        expected.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x80]);
        assert_eq!(frame.data, expected);
    }

    #[test]
    fn parameter_sets_in_the_frame_come_first_and_are_kept() {
        let mut sets = ParameterSets::default();
        let frame = access_unit(
            VideoCodec::H264,
            &[
                0, 0, 0, 1, 0x06, 1, 0, 0, 0, 1, 0x67, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
            ],
            &mut sets,
            0,
        );
        assert!(frame.key && frame.complete);
        assert_eq!(
            frame.data,
            [0, 0, 0, 1, 0x67, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x06, 1, 0, 0, 0, 1, 0x65, 4]
        );
        // A later IDR without its own gets them.
        let later = access_unit(VideoCodec::H264, &[0, 0, 0, 1, 0x65, 5], &mut sets, 0);
        assert!(later.complete);
        assert_eq!(
            later.data,
            [0, 0, 0, 1, 0x67, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 5]
        );
        assert_eq!(sets.get(VideoCodec::H264).unwrap().1, SetsSource::Stream);
    }

    #[test]
    fn other_pictures_pass() {
        let mut sets = avc_sets();
        let frame = access_unit(VideoCodec::H264, &[0, 0, 0, 1, 0x41, 0x9A], &mut sets, 0);
        assert!(!frame.key && !frame.complete);
        assert_eq!(frame.data, [0, 0, 0, 1, 0x41, 0x9A]);
    }

    #[test]
    fn a_key_picture_with_no_sets_known_is_not_complete() {
        let mut sets = ParameterSets::default();
        let frame = access_unit(VideoCodec::H264, &[0, 0, 0, 1, 0x65, 1], &mut sets, 0);
        assert!(frame.key && !frame.complete);
        assert_eq!(frame.data, [0, 0, 0, 1, 0x65, 1]);
        // An SPS alone is no complete group: passed on, not kept.
        let frame = access_unit(
            VideoCodec::H264,
            &[0, 0, 0, 1, 0x67, 2, 0, 0, 0, 1, 0x65, 1],
            &mut sets,
            0,
        );
        assert!(!frame.complete);
        assert_eq!(frame.data, [0, 0, 0, 1, 0x67, 2, 0, 0, 0, 1, 0x65, 1]);
        assert_eq!(sets.get(VideoCodec::H264), None);
    }

    /// An `hvcC` record: Main profile, level 3.1, four-byte lengths, and arrays of a prefix SEI, a PPS, an SPS and a
    /// VPS, out of order.
    fn hvcc() -> Vec<u8> {
        let mut record = vec![
            1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5D, 0xF0, 0, 0xFC, 0xFD, 0xF8, 0xF8, 0,
            0, 0x0F,
        ];
        let arrays: [(u8, &[&[u8]]); 4] = [
            (39, &[&[0x4E, 0x01, 0x05]]),
            (34, &[&[0x44, 0x01, 0xC1]]),
            (33, &[&[0x42, 0x01, 0x01, 0x60]]),
            (32, &[&[0x40, 0x01, 0x0C]]),
        ];
        record.push(arrays.len() as u8);
        for (kind, units) in arrays {
            record.push(0x80 | kind);
            record.extend_from_slice(&(units.len() as u16).to_be_bytes());
            for unit in units {
                record.extend_from_slice(&(unit.len() as u16).to_be_bytes());
                record.extend_from_slice(unit);
            }
        }
        record
    }

    const HEVC_SETS: &[u8] = &[
        0, 0, 0, 1, 0x40, 0x01, 0x0C, 0, 0, 0, 1, 0x42, 0x01, 0x01, 0x60, 0, 0, 0, 1, 0x44, 0x01,
        0xC1,
    ];

    #[test]
    fn parameter_sets_from_hvcc() {
        let record = hvcc();
        assert_eq!(
            parameter_sets(VideoCodec::H265, &record).unwrap(),
            HEVC_SETS
        );
        assert_eq!(parameter_sets(VideoCodec::H265, &record[..30]), None);
        assert_eq!(parameter_sets(VideoCodec::H265, &record[..23]), None);
    }

    #[test]
    fn parameter_sets_from_an_annex_b_configuration() {
        let mut record = vec![0, 0, 0, 1, 0x4E, 0x01, 0x05];
        record.extend_from_slice(&[0, 0, 0, 1, 0x44, 0x01, 0xC1]);
        record.extend_from_slice(&[0, 0, 1, 0x42, 0x01, 0x01, 0x60]);
        record.extend_from_slice(&[0, 0, 0, 1, 0x40, 0x01, 0x0C]);
        assert_eq!(
            parameter_sets(VideoCodec::H265, &record).unwrap(),
            HEVC_SETS
        );
        // Without a VPS the group is not complete.
        assert_eq!(parameter_sets(VideoCodec::H265, &record[..21]), None);
    }

    #[test]
    fn a_configuration_without_sets_keeps_those_known() {
        let mut sets = ParameterSets::default();
        assert!(sets.config(VideoCodec::H265, &hvcc()));
        assert!(!sets.config(VideoCodec::H265, &[1, 2, 3]));
        assert_eq!(
            sets.get(VideoCodec::H265),
            Some((HEVC_SETS, SetsSource::Config))
        );
        // Sets of the other codec are not taken for this one.
        assert_eq!(sets.get(VideoCodec::H264), None);
        sets.clear();
        assert_eq!(sets.get(VideoCodec::H265), None);
    }

    #[test]
    fn hevc_key_frames() {
        // IDR_W_RADL, IDR_N_LP, CRA and BLA_W_LP start a key frame; TRAIL_R and RASL do not.
        for (header, key) in [
            (0x26, true),
            (0x28, true),
            (0x2A, true),
            (0x20, true),
            (0x02, false),
            (0x10, false),
        ] {
            let mut sets = ParameterSets::default();
            let frame = access_unit(
                VideoCodec::H265,
                &[0, 0, 0, 1, header, 0x01, 0xAF],
                &mut sets,
                0,
            );
            assert_eq!(frame.key, key, "{header:#x}");
        }
        // An H.264 IDR header is no H.265 key picture.
        let mut sets = ParameterSets::default();
        let frame = access_unit(
            VideoCodec::H265,
            &[0, 0, 0, 1, 0x65, 0x01, 0xAF],
            &mut sets,
            0,
        );
        assert!(!frame.key);
    }

    #[test]
    fn hevc_key_frame_gets_parameter_sets_and_loses_delimiter() {
        let mut sets = ParameterSets::default();
        sets.config(VideoCodec::H265, &hvcc());
        let frame = access_unit(
            VideoCodec::H265,
            &[0, 0, 0, 1, 0x46, 0x01, 0x10, 0, 0, 0, 1, 0x26, 0x01, 0xAF],
            &mut sets,
            7,
        );
        assert!(frame.key && frame.complete);
        let mut expected = HEVC_SETS.to_vec();
        expected.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01, 0xAF]);
        assert_eq!(frame.data, expected);
    }

    /// An IRAP access unit as the iPhone may send it: a delimiter, a prefix SEI, its own VPS, SPS and PPS, the slice and
    /// a suffix SEI.
    const HEVC_IRAP: &[u8] = &[
        0, 0, 0, 1, 0x46, 0x01, 0x10, 0, 0, 0, 1, 0x4E, 0x01, 1, 0, 0, 0, 1, 0x40, 0x01, 2, 0, 0,
        0, 1, 0x42, 0x01, 3, 0, 0, 0, 1, 0x44, 0x01, 4, 0, 0, 0, 1, 0x26, 0x01, 5, 0, 0, 0, 1,
        0x50, 0x01, 6,
    ];

    /// The same unit in the order the head unit gets it: its parameter sets, then the prefix SEI, the slice and the
    /// suffix SEI.
    const HEVC_IRAP_OUT: &[u8] = &[
        0, 0, 0, 1, 0x40, 0x01, 2, 0, 0, 0, 1, 0x42, 0x01, 3, 0, 0, 0, 1, 0x44, 0x01, 4, 0, 0, 0,
        1, 0x4E, 0x01, 1, 0, 0, 0, 1, 0x26, 0x01, 5, 0, 0, 0, 1, 0x50, 0x01, 6,
    ];

    #[test]
    fn hevc_own_parameter_sets_lead_and_replace_those_of_the_configuration() {
        let mut sets = ParameterSets::default();
        sets.config(VideoCodec::H265, &hvcc());
        let frame = access_unit(VideoCodec::H265, HEVC_IRAP, &mut sets, 0);
        assert!(frame.key && frame.complete);
        assert_eq!(frame.data, HEVC_IRAP_OUT);
        // Kept for the next IRAP, which carries none of its own.
        assert_eq!(sets.get(VideoCodec::H265).unwrap().1, SetsSource::Stream);
        let next = access_unit(VideoCodec::H265, &[0, 0, 0, 1, 0x2A, 0x01, 7], &mut sets, 0);
        assert!(next.complete);
        assert_eq!(&next.data[..21], &HEVC_IRAP_OUT[..21]);
        assert_eq!(&next.data[21..], [0, 0, 0, 1, 0x2A, 0x01, 7]);
    }

    #[test]
    fn hevc_stream_sets_without_a_configuration() {
        let mut sets = ParameterSets::default();
        let frame = access_unit(VideoCodec::H265, HEVC_IRAP, &mut sets, 0);
        assert!(frame.complete);
        assert_eq!(frame.data, HEVC_IRAP_OUT);
    }

    #[test]
    fn hevc_nothing_known_passes_what_the_picture_carries() {
        let mut sets = ParameterSets::default();
        // A PPS alone, a prefix SEI and the slice: the PPS first, not kept.
        let frame = access_unit(
            VideoCodec::H265,
            &[
                0, 0, 0, 1, 0x4E, 0x01, 1, 0, 0, 0, 1, 0x44, 0x01, 4, 0, 0, 0, 1, 0x26, 0x01, 5,
            ],
            &mut sets,
            0,
        );
        assert!(frame.key && !frame.complete);
        assert_eq!(
            frame.data,
            [0, 0, 0, 1, 0x44, 0x01, 4, 0, 0, 0, 1, 0x4E, 0x01, 1, 0, 0, 0, 1, 0x26, 0x01, 5]
        );
        assert_eq!(sets.get(VideoCodec::H265), None);
    }

    #[test]
    fn hevc_other_pictures_pass() {
        let mut sets = ParameterSets::default();
        sets.config(VideoCodec::H265, &hvcc());
        let frame = access_unit(
            VideoCodec::H265,
            &[0, 0, 0, 1, 0x02, 0x01, 0xD0],
            &mut sets,
            0,
        );
        assert!(!frame.key);
        assert_eq!(frame.data, [0, 0, 0, 1, 0x02, 0x01, 0xD0]);
    }

    #[test]
    fn unit_kinds_for_the_log() {
        assert_eq!(
            unit_kinds(VideoCodec::H265, HEVC_IRAP),
            "AUD prefix-SEI VPS SPS PPS IDR_W_RADL suffix-SEI"
        );
        assert_eq!(
            unit_kinds(VideoCodec::H264, &[0, 0, 0, 1, 0x65, 1, 0, 0, 1, 0x0C, 2]),
            "IDR 12"
        );
    }
}
