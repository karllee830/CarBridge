// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The ATT PDUs the exchange uses, and the answers the phone gives to the head unit's own requests. Multi-byte
//! fields are little-endian.

/// The ATT MTU the phone asks for and offers.
pub const MTU: u16 = 512;
/// ATT MTU before an exchange.
pub const DEFAULT_MTU: u16 = 23;

pub mod opcode {
    pub const ERROR_RSP: u8 = 0x01;
    pub const EXCHANGE_MTU_REQ: u8 = 0x02;
    pub const EXCHANGE_MTU_RSP: u8 = 0x03;
    pub const FIND_INFORMATION_REQ: u8 = 0x04;
    pub const FIND_BY_TYPE_VALUE_REQ: u8 = 0x06;
    pub const FIND_BY_TYPE_VALUE_RSP: u8 = 0x07;
    pub const READ_BY_TYPE_REQ: u8 = 0x08;
    pub const READ_BY_TYPE_RSP: u8 = 0x09;
    pub const READ_REQ: u8 = 0x0A;
    pub const READ_BLOB_REQ: u8 = 0x0C;
    pub const READ_MULTIPLE_REQ: u8 = 0x0E;
    pub const READ_BY_GROUP_TYPE_REQ: u8 = 0x10;
    pub const WRITE_REQ: u8 = 0x12;
    pub const WRITE_RSP: u8 = 0x13;
    pub const PREPARE_WRITE_REQ: u8 = 0x16;
    pub const PREPARE_WRITE_RSP: u8 = 0x17;
    pub const EXECUTE_WRITE_REQ: u8 = 0x18;
    pub const EXECUTE_WRITE_RSP: u8 = 0x19;
    pub const HANDLE_VALUE_NTF: u8 = 0x1B;
    pub const HANDLE_VALUE_IND: u8 = 0x1D;
    pub const HANDLE_VALUE_CFM: u8 = 0x1E;
    pub const READ_MULTIPLE_VARIABLE_REQ: u8 = 0x20;
}

pub mod error {
    pub const REQUEST_NOT_SUPPORTED: u8 = 0x06;
    pub const ATTRIBUTE_NOT_FOUND: u8 = 0x0A;
}

/// UUID of a primary service declaration.
pub const PRIMARY_SERVICE: u16 = 0x2800;
/// UUID of a characteristic declaration.
pub const CHARACTERISTIC: u16 = 0x2803;

/// Whether `opcode` is a request, which the receiver must answer.
pub fn is_request(opcode: u8) -> bool {
    use opcode::*;
    matches!(
        opcode,
        EXCHANGE_MTU_REQ
            | FIND_INFORMATION_REQ
            | FIND_BY_TYPE_VALUE_REQ
            | READ_BY_TYPE_REQ
            | READ_REQ
            | READ_BLOB_REQ
            | READ_MULTIPLE_REQ
            | READ_BY_GROUP_TYPE_REQ
            | WRITE_REQ
            | PREPARE_WRITE_REQ
            | EXECUTE_WRITE_REQ
            | READ_MULTIPLE_VARIABLE_REQ
    )
}

/// Whether `opcode` answers a request.
pub fn is_response(opcode: u8) -> bool {
    use opcode::*;
    opcode == ERROR_RSP
        || (opcode & 1 == 1 && (EXCHANGE_MTU_RSP..=EXECUTE_WRITE_RSP).contains(&opcode))
        || opcode == 0x21
}

/// The phone's answer to a request from the head unit, which serves no attributes: its MTU to an MTU exchange,
/// Attribute Not Found to discovery, Request Not Supported to anything else.
pub fn answer_peer_request(pdu: &[u8]) -> Vec<u8> {
    use opcode::*;
    let request = pdu[0];
    match request {
        EXCHANGE_MTU_REQ => mtu_request_like(EXCHANGE_MTU_RSP, MTU),
        FIND_INFORMATION_REQ
        | FIND_BY_TYPE_VALUE_REQ
        | READ_BY_TYPE_REQ
        | READ_BY_GROUP_TYPE_REQ => {
            let start = pdu
                .get(1..3)
                .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
            error_response(request, start, error::ATTRIBUTE_NOT_FOUND)
        }
        _ => error_response(request, 0, error::REQUEST_NOT_SUPPORTED),
    }
}

pub fn error_response(request: u8, handle: u16, code: u8) -> Vec<u8> {
    let mut pdu = vec![opcode::ERROR_RSP, request];
    pdu.extend_from_slice(&handle.to_le_bytes());
    pdu.push(code);
    pdu
}

fn mtu_request_like(opcode: u8, mtu: u16) -> Vec<u8> {
    let mut pdu = vec![opcode];
    pdu.extend_from_slice(&mtu.to_le_bytes());
    pdu
}

pub fn exchange_mtu_request(mtu: u16) -> Vec<u8> {
    mtu_request_like(opcode::EXCHANGE_MTU_REQ, mtu)
}

/// Find By Type Value Request for primary services with the given 128-bit UUID, written most significant byte first.
pub fn find_service_request(start: u16, uuid: &[u8; 16]) -> Vec<u8> {
    let mut pdu = vec![opcode::FIND_BY_TYPE_VALUE_REQ];
    pdu.extend_from_slice(&start.to_le_bytes());
    pdu.extend_from_slice(&0xFFFFu16.to_le_bytes());
    pdu.extend_from_slice(&PRIMARY_SERVICE.to_le_bytes());
    pdu.extend(uuid.iter().rev());
    pdu
}

pub fn read_by_type_request(start: u16, end: u16, uuid16: u16) -> Vec<u8> {
    let mut pdu = vec![opcode::READ_BY_TYPE_REQ];
    pdu.extend_from_slice(&start.to_le_bytes());
    pdu.extend_from_slice(&end.to_le_bytes());
    pdu.extend_from_slice(&uuid16.to_le_bytes());
    pdu
}

pub fn write_request(handle: u16, value: &[u8]) -> Vec<u8> {
    let mut pdu = vec![opcode::WRITE_REQ];
    pdu.extend_from_slice(&handle.to_le_bytes());
    pdu.extend_from_slice(value);
    pdu
}

pub fn prepare_write_request(handle: u16, offset: u16, value: &[u8]) -> Vec<u8> {
    let mut pdu = vec![opcode::PREPARE_WRITE_REQ];
    pdu.extend_from_slice(&handle.to_le_bytes());
    pdu.extend_from_slice(&offset.to_le_bytes());
    pdu.extend_from_slice(value);
    pdu
}

/// Execute Write Request that writes the prepared values.
pub fn execute_write_request() -> Vec<u8> {
    vec![opcode::EXECUTE_WRITE_REQ, 0x01]
}

pub fn confirmation() -> Vec<u8> {
    vec![opcode::HANDLE_VALUE_CFM]
}

/// A characteristic declaration found by discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Characteristic {
    pub declaration: u16,
    pub properties: u8,
    pub value_handle: u16,
    /// The UUID, most significant byte first; a 16-bit UUID is expanded with the Bluetooth base UUID.
    pub uuid: [u8; 16],
}

const BASE_UUID: [u8; 16] = [
    0, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0x80, 0x5F, 0x9B, 0x34, 0xFB,
];

/// Reads a Read By Type Response to a characteristic discovery.
pub fn parse_characteristics(pdu: &[u8]) -> Option<Vec<Characteristic>> {
    let (&len, data) = pdu.get(1..)?.split_first()?;
    let len = usize::from(len);
    if pdu[0] != opcode::READ_BY_TYPE_RSP || !(len == 7 || len == 21) || data.len() % len != 0 {
        return None;
    }
    Some(
        data.chunks(len)
            .map(|c| {
                let mut uuid = BASE_UUID;
                if len == 7 {
                    uuid[2] = c[6];
                    uuid[3] = c[5];
                } else {
                    for (i, b) in c[5..21].iter().rev().enumerate() {
                        uuid[i] = *b;
                    }
                }
                Characteristic {
                    declaration: u16::from_le_bytes([c[0], c[1]]),
                    properties: c[2],
                    value_handle: u16::from_le_bytes([c[3], c[4]]),
                    uuid,
                }
            })
            .collect(),
    )
}

/// Reads a Find By Type Value Response: the handle ranges found.
pub fn parse_handle_ranges(pdu: &[u8]) -> Option<Vec<(u16, u16)>> {
    let data = pdu.get(1..)?;
    if pdu[0] != opcode::FIND_BY_TYPE_VALUE_RSP || data.is_empty() || data.len() % 4 != 0 {
        return None;
    }
    Some(
        data.chunks(4)
            .map(|c| {
                (
                    u16::from_le_bytes([c[0], c[1]]),
                    u16::from_le_bytes([c[2], c[3]]),
                )
            })
            .collect(),
    )
}

/// The request opcode and error code of an Error Response.
pub fn parse_error(pdu: &[u8]) -> Option<(u8, u8)> {
    (pdu.len() >= 5 && pdu[0] == opcode::ERROR_RSP).then(|| (pdu[1], pdu[4]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_unit_feature_read_is_not_found() {
        // Read By Type, handles 0x0001-0xFFFF, Server Supported Features (0x2B3A).
        let answer = answer_peer_request(&[0x08, 0x01, 0x00, 0xFF, 0xFF, 0x3A, 0x2B]);
        assert_eq!(answer, [0x01, 0x08, 0x01, 0x00, 0x0A]);
    }

    #[test]
    fn peer_mtu_exchange() {
        assert_eq!(answer_peer_request(&[0x02, 0x00, 0x02]), [0x03, 0x00, 0x02]);
    }

    #[test]
    fn other_requests_not_supported() {
        assert_eq!(
            answer_peer_request(&[0x0A, 0x03, 0x00]),
            [0x01, 0x0A, 0x00, 0x00, 0x06]
        );
        assert_eq!(
            answer_peer_request(&[0x12, 0x03, 0x00, 1]),
            [0x01, 0x12, 0x00, 0x00, 0x06]
        );
    }

    #[test]
    fn classification() {
        assert!(is_request(0x08) && !is_response(0x08));
        assert!(is_response(0x09) && !is_request(0x09));
        assert!(is_response(0x01));
        assert!(!is_request(0x1D) && !is_response(0x1D));
        assert!(!is_request(0x52) && !is_response(0x1E));
    }

    #[test]
    fn find_service_puts_uuid_little_endian() {
        let uuid = [
            0x2a, 0xbc, 0xc8, 0x50, 0x99, 0x35, 0x4f, 0x8a, 0xba, 0x84, 0x12, 0x34, 0x56, 0x78,
            0x91, 0x00,
        ];
        let pdu = find_service_request(1, &uuid);
        assert_eq!(&pdu[..7], &[0x06, 0x01, 0x00, 0xFF, 0xFF, 0x00, 0x28]);
        assert_eq!(pdu[7], 0x00);
        assert_eq!(pdu[22], 0x2a);
    }

    #[test]
    fn characteristics_128_and_16_bit() {
        let mut pdu = vec![0x09, 21, 0x02, 0x00, 0x08, 0x03, 0x00];
        pdu.extend((0..16u8).rev());
        let found = parse_characteristics(&pdu).unwrap();
        assert_eq!(found[0].value_handle, 3);
        assert_eq!(found[0].properties, 0x08);
        assert_eq!(found[0].uuid, core::array::from_fn(|i| i as u8));
        let short =
            parse_characteristics(&[0x09, 7, 0x02, 0x00, 0x02, 0x03, 0x00, 0x00, 0x2A]).unwrap();
        assert_eq!(&short[0].uuid[..4], &[0, 0, 0x2A, 0x00]);
        assert!(parse_characteristics(&[0x09, 8, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }
}
