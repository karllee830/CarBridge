// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

/// CRC-16/ARC: reflected polynomial 0xA001, initial value 0, no final XOR. Protects the message header.
pub fn crc16_arc(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::crc16_arc;

    #[test]
    fn check_value() {
        assert_eq!(crc16_arc(b"123456789"), 0xBB3D);
    }

    #[test]
    fn empty() {
        assert_eq!(crc16_arc(&[]), 0);
    }
}
