// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! RTP packets of a transport stream, framed for the sink's TCP session: each packet preceded by its length, 16 bits
//! big-endian.

use crate::ts::PACKET_SIZE;

/// RTP payload type of an MPEG-2 transport stream.
pub const PAYLOAD_TYPE_MP2T: u8 = 33;
/// Transport stream packets per RTP packet.
pub const TS_PACKETS_PER_RTP: usize = 7;
const RTP_HEADER_LEN: usize = 12;

/// Numbers and frames RTP packets.
pub struct Packetizer {
    sequence: u16,
    ssrc: u32,
}

impl Packetizer {
    pub fn new(ssrc: u32) -> Packetizer {
        Packetizer { sequence: 0, ssrc }
    }

    /// Appends to `out` the framed RTP packets carrying `ts` (whole transport stream packets), stamped `time`
    /// (90 kHz).
    pub fn packetize(&mut self, ts: &[u8], time: u64, out: &mut Vec<u8>) {
        debug_assert_eq!(ts.len() % PACKET_SIZE, 0);
        for chunk in ts.chunks(PACKET_SIZE * TS_PACKETS_PER_RTP) {
            let len = (RTP_HEADER_LEN + chunk.len()) as u16;
            out.extend_from_slice(&len.to_be_bytes());
            out.push(0x80); // version 2, no padding, no extension, no CSRC
            out.push(PAYLOAD_TYPE_MP2T);
            out.extend_from_slice(&self.sequence.to_be_bytes());
            out.extend_from_slice(&(time as u32).to_be_bytes());
            out.extend_from_slice(&self.ssrc.to_be_bytes());
            out.extend_from_slice(chunk);
            self.sequence = self.sequence.wrapping_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing() {
        let mut packetizer = Packetizer::new(0x1234_5678);
        let ts = vec![0x47; PACKET_SIZE * 9];
        let mut out = Vec::new();
        packetizer.packetize(&ts, 0x1_0000_0001, &mut out);
        let first_len = usize::from(u16::from_be_bytes([out[0], out[1]]));
        assert_eq!(first_len, RTP_HEADER_LEN + 7 * PACKET_SIZE);
        assert_eq!(&out[2..4], &[0x80, 33]);
        assert_eq!(&out[4..6], &[0, 0]);
        assert_eq!(&out[6..10], &[0, 0, 0, 1]);
        assert_eq!(&out[10..14], &[0x12, 0x34, 0x56, 0x78]);
        let second = &out[2 + first_len..];
        assert_eq!(
            usize::from(u16::from_be_bytes([second[0], second[1]])),
            RTP_HEADER_LEN + 2 * PACKET_SIZE
        );
        assert_eq!(&second[4..6], &[0, 1]);
        assert_eq!(out.len(), 2 * (2 + RTP_HEADER_LEN) + ts.len());
    }
}
