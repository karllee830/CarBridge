// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/Tlv8Codec.kt (DiPlay commit
// 615b32c).

//! TLV8, the type-length-value format of the pairing messages: a type byte, a length byte and up to 255 bytes of
//! value. A longer value is split into fragments of the same type; two items of one type in a row are kept apart
//! by an empty separator item.

use std::collections::BTreeMap;

/// The item types pairing uses.
pub mod kind {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0a;
    pub const SEPARATOR: u8 = 0xff;
}

const MAX_FRAGMENT: usize = 255;

/// Writes TLV8 items in order.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    out: Vec<u8>,
    last: Option<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    /// Adds an item, split into fragments as needed. An empty value is one empty item.
    pub fn item(mut self, kind: u8, value: &[u8]) -> Writer {
        if self.last == Some(kind) {
            self.out.extend_from_slice(&[kind::SEPARATOR, 0]);
        }
        let mut fragments = value.chunks(MAX_FRAGMENT).peekable();
        if fragments.peek().is_none() {
            self.out.extend_from_slice(&[kind, 0]);
        }
        for fragment in fragments {
            self.out.push(kind);
            self.out.push(fragment.len() as u8);
            self.out.extend_from_slice(fragment);
        }
        self.last = Some(kind);
        self
    }

    /// Adds a one-byte item.
    pub fn byte(self, kind: u8, value: u8) -> Writer {
        self.item(kind, &[value])
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// Decoded TLV8 items by type. A later item of a type replaces an earlier one, except that the fragments of one
/// long value are joined again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Items {
    items: BTreeMap<u8, Vec<u8>>,
}

impl Items {
    /// Decodes `bytes`; an item cut short at the end is dropped.
    pub fn parse(bytes: &[u8]) -> Items {
        let mut items: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        let mut last: Option<(u8, usize)> = None;
        let mut rest = bytes;
        while let [kind, length, tail @ ..] = rest {
            let (kind, length) = (*kind, usize::from(*length));
            if tail.len() < length {
                break;
            }
            let (value, tail) = tail.split_at(length);
            rest = tail;
            match (last, items.get_mut(&kind)) {
                (Some((previous, MAX_FRAGMENT)), Some(joined)) if previous == kind => {
                    joined.extend_from_slice(value)
                }
                _ => {
                    items.insert(kind, value.to_vec());
                }
            }
            last = Some((kind, length));
        }
        Items { items }
    }

    pub fn get(&self, kind: u8) -> Option<&[u8]> {
        self.items.get(&kind).map(Vec::as_slice)
    }

    /// The first byte of an item.
    pub fn byte(&self, kind: u8) -> Option<u8> {
        self.get(kind)?.first().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_items_encode_in_order() {
        let bytes = Writer::new()
            .byte(kind::STATE, 2)
            .item(kind::SALT, &[1, 2, 3])
            .into_bytes();
        assert_eq!(bytes, [0x06, 1, 2, 0x02, 3, 1, 2, 3]);
        let items = Items::parse(&bytes);
        assert_eq!(items.byte(kind::STATE), Some(2));
        assert_eq!(items.get(kind::SALT), Some(&[1u8, 2, 3][..]));
    }

    #[test]
    fn long_values_are_fragmented_and_joined_again() {
        let key: Vec<u8> = (0..384).map(|i| i as u8).collect();
        let bytes = Writer::new()
            .item(kind::PUBLIC_KEY, &key)
            .byte(kind::STATE, 4)
            .into_bytes();
        assert_eq!(&bytes[..2], [kind::PUBLIC_KEY, 255]);
        assert_eq!(&bytes[257..259], [kind::PUBLIC_KEY, 129]);
        assert_eq!(bytes.len(), 2 + 255 + 2 + 129 + 3);
        let items = Items::parse(&bytes);
        assert_eq!(items.get(kind::PUBLIC_KEY), Some(&key[..]));
        assert_eq!(items.byte(kind::STATE), Some(4));
    }

    #[test]
    fn a_value_of_exactly_255_bytes_is_one_fragment() {
        let value = vec![9u8; 255];
        let bytes = Writer::new().item(kind::PROOF, &value).into_bytes();
        assert_eq!(bytes.len(), 257);
        assert_eq!(Items::parse(&bytes).get(kind::PROOF), Some(&value[..]));
    }

    #[test]
    fn repeated_types_are_separated() {
        let bytes = Writer::new()
            .item(kind::IDENTIFIER, b"a")
            .item(kind::IDENTIFIER, b"b")
            .into_bytes();
        assert_eq!(bytes, [0x01, 1, b'a', 0xff, 0, 0x01, 1, b'b']);
        // The later item replaces the earlier one.
        assert_eq!(Items::parse(&bytes).get(kind::IDENTIFIER), Some(&b"b"[..]));
    }

    #[test]
    fn empty_values_and_truncated_items() {
        let bytes = Writer::new().item(kind::METHOD, &[]).into_bytes();
        assert_eq!(bytes, [0x00, 0]);
        assert_eq!(Items::parse(&bytes).get(kind::METHOD), Some(&[][..]));
        let items = Items::parse(&[0x06, 1, 3, 0x03, 5, 1, 2]);
        assert_eq!(items.byte(kind::STATE), Some(3));
        assert_eq!(items.get(kind::PUBLIC_KEY), None);
        assert_eq!(Items::parse(&[0x06]), Items::default());
    }
}
