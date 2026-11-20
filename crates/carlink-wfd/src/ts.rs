// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! An MPEG-2 transport stream of one program: H.264 or H.265 video, optionally ADTS AAC audio.
//!
//! The PAT and PMT go out before every key frame and at least every 100 ms; the video packets carry the program clock
//! reference.

use mpeg2ts::es::{StreamId, StreamType};
use mpeg2ts::pes::PesHeader;
use mpeg2ts::time::{ClockReference, Timestamp};
use mpeg2ts::ts::payload::{Bytes, Pat, Pes, Pmt};
use mpeg2ts::ts::{
    AdaptationField, ContinuityCounter, EsInfo, Pid, ProgramAssociation,
    TransportScramblingControl, TsHeader, TsPacket, TsPayload, VersionNumber, WriteTsPacket,
};

use crate::Codec;

/// Size of a transport stream packet.
pub const PACKET_SIZE: usize = TsPacket::SIZE;

const PMT_PID: u16 = 0x100;
const VIDEO_PID: u16 = 0x1011;
const AUDIO_PID: u16 = 0x1100;
const PROGRAM: u16 = 1;
/// Longest gap between two PAT/PMT pairs, in 90 kHz ticks.
const TABLE_INTERVAL: u64 = 9_000;
/// How far presentation times run ahead of the clock reference, in 90 kHz ticks: the sink's decoding margin.
const PTS_OFFSET: u64 = 4_500;
/// PES header with a PTS: start code, stream id, length, flags, header length, PTS.
const PES_HEADER_LEN: usize = 6 + 3 + 5;
/// Size of an adaptation field that carries only a PCR.
const PCR_FIELD_LEN: usize = 2 + 6;

/// Builds the transport stream packets of a program.
pub struct Muxer {
    codec: Codec,
    audio: bool,
    /// Version of the PMT, which changes when the audio stream joins.
    pmt_version: VersionNumber,
    pat_cc: ContinuityCounter,
    pmt_cc: ContinuityCounter,
    video_cc: ContinuityCounter,
    audio_cc: ContinuityCounter,
    last_tables: Option<u64>,
}

impl Muxer {
    /// A program with video in `codec`, and with an AAC stream when `audio` is set.
    pub fn new(codec: Codec, audio: bool) -> Muxer {
        Muxer {
            codec,
            audio,
            pat_cc: ContinuityCounter::new(),
            pmt_cc: ContinuityCounter::new(),
            video_cc: ContinuityCounter::new(),
            audio_cc: ContinuityCounter::new(),
            pmt_version: VersionNumber::new(),
            last_tables: None,
        }
    }

    /// Appends the packets of one access unit in Annex B form, presented at `time` (90 kHz), to `out`.
    pub fn video(&mut self, access_unit: &[u8], time: u64, key_frame: bool, out: &mut Vec<u8>) {
        if key_frame
            || self
                .last_tables
                .is_none_or(|last| time.saturating_sub(last) >= TABLE_INTERVAL)
        {
            self.tables(out);
            self.last_tables = Some(time);
        }
        let pcr = ClockReference::new(time % (1 << 33) * 300).expect("33-bit PCR base");
        let adaptation = AdaptationField {
            discontinuity_indicator: false,
            random_access_indicator: key_frame,
            es_priority_indicator: false,
            pcr: Some(pcr),
            opcr: None,
            splice_countdown: None,
            transport_private_data: Vec::new(),
            extension: None,
        };
        let pid = Pid::new(VIDEO_PID).expect("valid PID");
        let mut cc = self.video_cc;
        pes(
            pid,
            &mut cc,
            StreamId::new(0xE0),
            time + PTS_OFFSET,
            access_unit,
            Some(adaptation),
            out,
        );
        self.video_cc = cc;
    }

    /// Appends the packets of one ADTS AAC frame presented at `time` (90 kHz) to `out`. The first frame of a program
    /// made without audio adds the audio stream to it, with a new PMT.
    pub fn audio(&mut self, adts_frame: &[u8], time: u64, out: &mut Vec<u8>) {
        if !self.audio {
            self.audio = true;
            self.pmt_version.increment();
            self.tables(out);
            self.last_tables = Some(time);
        }
        let pid = Pid::new(AUDIO_PID).expect("valid PID");
        let mut cc = self.audio_cc;
        pes(
            pid,
            &mut cc,
            StreamId::new(0xC0),
            time + PTS_OFFSET,
            adts_frame,
            None,
            out,
        );
        self.audio_cc = cc;
    }

    fn tables(&mut self, out: &mut Vec<u8>) {
        let pat = Pat {
            transport_stream_id: 1,
            version_number: VersionNumber::new(),
            table: vec![ProgramAssociation {
                program_num: PROGRAM,
                program_map_pid: Pid::new(PMT_PID).unwrap(),
            }],
        };
        write(
            header(Pid::new(0).unwrap(), &mut self.pat_cc),
            None,
            TsPayload::Pat(pat),
            out,
        );
        let mut es_info = vec![EsInfo {
            stream_type: match self.codec {
                Codec::H264 => StreamType::H264,
                Codec::H265 => StreamType::H265,
            },
            elementary_pid: Pid::new(VIDEO_PID).unwrap(),
            descriptors: Vec::new(),
        }];
        if self.audio {
            es_info.push(EsInfo {
                stream_type: StreamType::AdtsAac,
                elementary_pid: Pid::new(AUDIO_PID).unwrap(),
                descriptors: Vec::new(),
            });
        }
        let pmt = Pmt {
            program_num: PROGRAM,
            pcr_pid: Some(Pid::new(VIDEO_PID).unwrap()),
            version_number: self.pmt_version,
            program_info: Vec::new(),
            es_info,
        };
        write(
            header(Pid::new(PMT_PID).unwrap(), &mut self.pmt_cc),
            None,
            TsPayload::Pmt(pmt),
            out,
        );
    }
}

fn header(pid: Pid, cc: &mut ContinuityCounter) -> TsHeader {
    let header = TsHeader {
        transport_error_indicator: false,
        transport_priority: false,
        pid,
        transport_scrambling_control: TransportScramblingControl::NotScrambled,
        continuity_counter: *cc,
    };
    cc.increment();
    header
}

fn write(
    header: TsHeader,
    adaptation_field: Option<AdaptationField>,
    payload: TsPayload,
    out: &mut Vec<u8>,
) {
    let packet = TsPacket {
        header,
        adaptation_field,
        payload: Some(payload),
    };
    let mut writer = mpeg2ts::ts::TsPacketWriter::new(out);
    writer.write_ts_packet(&packet).expect("packet fits");
}

/// Splits one PES packet over transport stream packets.
fn pes(
    pid: Pid,
    cc: &mut ContinuityCounter,
    stream_id: StreamId,
    pts: u64,
    data: &[u8],
    adaptation: Option<AdaptationField>,
    out: &mut Vec<u8>,
) {
    let pes_len = PES_HEADER_LEN - 6 + data.len();
    // Video may exceed the 16-bit length; 0 means "unbounded", which only video may use.
    let pes_packet_len = if stream_id.as_u8() >= 0xE0 || pes_len > usize::from(u16::MAX) {
        0
    } else {
        pes_len as u16
    };
    let first_room =
        PACKET_SIZE - 4 - PES_HEADER_LEN - adaptation.as_ref().map_or(0, |_| PCR_FIELD_LEN);
    let (first, mut rest) = data.split_at(data.len().min(first_room));
    let header_pes = PesHeader {
        stream_id,
        priority: false,
        data_alignment_indicator: true,
        copyright: false,
        original_or_copy: false,
        pts: Some(Timestamp::new(pts % (1 << 33)).expect("33-bit PTS")),
        dts: None,
        escr: None,
    };
    let pes = Pes {
        header: header_pes,
        pes_packet_len,
        data: Bytes::new(first).expect("fits"),
    };
    write(header(pid, cc), adaptation, TsPayload::PesStart(pes), out);
    while !rest.is_empty() {
        let (part, next) = rest.split_at(rest.len().min(Bytes::MAX_SIZE));
        write(
            header(pid, cc),
            None,
            TsPayload::PesContinuation(Bytes::new(part).expect("fits")),
            out,
        );
        rest = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mpeg2ts::ts::{ReadTsPacket, TsPacketReader};

    fn packets(bytes: &[u8]) -> Vec<TsPacket> {
        let mut reader = TsPacketReader::new(bytes);
        let mut out = Vec::new();
        while let Some(packet) = reader.read_ts_packet().unwrap() {
            out.push(packet);
        }
        out
    }

    #[test]
    fn key_frame_starts_with_tables_and_pcr() {
        let mut muxer = Muxer::new(Codec::H264, false);
        let mut out = Vec::new();
        let frame = vec![0xAB; 1000];
        muxer.video(&frame, 90_000, true, &mut out);
        assert_eq!(out.len() % PACKET_SIZE, 0);
        let packets = packets(&out);
        assert!(matches!(packets[0].payload, Some(TsPayload::Pat(_))));
        assert!(matches!(packets[1].payload, Some(TsPayload::Pmt(_))));
        let start = &packets[2];
        assert_eq!(start.header.pid.as_u16(), VIDEO_PID);
        let field = start.adaptation_field.as_ref().unwrap();
        assert!(field.random_access_indicator);
        assert_eq!(field.pcr.unwrap().as_u64(), 90_000 * 300);
        let Some(TsPayload::PesStart(pes)) = &start.payload else {
            panic!("PES start")
        };
        assert_eq!(pes.header.pts.unwrap().as_u64(), 90_000 + PTS_OFFSET);
        let mut data = pes.data.to_vec();
        for packet in &packets[3..] {
            let Some(TsPayload::PesContinuation(bytes)) = &packet.payload else {
                panic!("continuation")
            };
            data.extend_from_slice(bytes);
        }
        assert_eq!(data, frame);
    }

    #[test]
    fn continuity_counters_and_table_interval() {
        let mut muxer = Muxer::new(Codec::H264, true);
        let mut out = Vec::new();
        muxer.video(&[1; 400], 0, true, &mut out);
        muxer.video(&[2; 400], 3_000, false, &mut out);
        muxer.audio(&[3; 300], 3_000, &mut out);
        muxer.video(&[4; 400], 12_000, false, &mut out);
        let packets = packets(&out);
        let tables = packets
            .iter()
            .filter(|p| matches!(p.payload, Some(TsPayload::Pat(_))))
            .count();
        assert_eq!(tables, 2);
        let mut last = std::collections::HashMap::new();
        for packet in &packets {
            let cc = packet.header.continuity_counter.as_u8();
            if let Some(previous) = last.insert(packet.header.pid.as_u16(), cc) {
                assert_eq!(cc, (previous + 1) % 16);
            }
        }
        let Some(TsPayload::Pmt(pmt)) = &packets[1].payload else {
            panic!("PMT")
        };
        assert_eq!(pmt.es_info.len(), 2);
    }

    #[test]
    fn audio_joins_with_a_new_pmt() {
        let mut muxer = Muxer::new(Codec::H264, false);
        let mut out = Vec::new();
        muxer.video(&[1; 100], 0, true, &mut out);
        let before = packets(&out);
        let Some(TsPayload::Pmt(first)) = &before[1].payload else {
            panic!("PMT")
        };
        assert_eq!((first.es_info.len(), first.version_number.as_u8()), (1, 0));
        out.clear();
        muxer.audio(
            &[0xFF, 0xF1, 0x4C, 0x80, 0x0C, 0xFF, 0xFC, 1, 2],
            3000,
            &mut out,
        );
        let after = packets(&out);
        let Some(TsPayload::Pmt(second)) = &after[1].payload else {
            panic!("PMT")
        };
        assert_eq!(
            (second.es_info.len(), second.version_number.as_u8()),
            (2, 1)
        );
        assert_eq!(after[2].header.pid.as_u16(), AUDIO_PID);
    }

    #[test]
    fn video_stream_type() {
        for (codec, stream_type) in [(Codec::H264, 0x1B), (Codec::H265, 0x24)] {
            let mut muxer = Muxer::new(codec, false);
            let mut out = Vec::new();
            muxer.video(&[1; 100], 0, true, &mut out);
            let Some(TsPayload::Pmt(pmt)) = &packets(&out)[1].payload else {
                panic!("PMT")
            };
            assert_eq!(pmt.es_info[0].stream_type as u8, stream_type);
            assert_eq!(pmt.es_info[0].elementary_pid.as_u16(), VIDEO_PID);
        }
    }
}
