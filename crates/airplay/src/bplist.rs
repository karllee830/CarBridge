// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/BplistCodec.kt (DiPlay
// commit 615b32c).

//! Apple binary property lists (`bplist00`), the bodies of most CarPlay control requests.
//!
//! The control channel decodes plists from a peer before it is authenticated, so decoding is bounded: nesting
//! depth, the number of object reads and the bytes copied out of shared objects are capped relative to the input's
//! size, and a hostile plist fails with an [`Error`] instead of exhausting memory or the stack.
//!
//! Dictionaries keep their keys in insertion order, and encoding writes them in that order. Integers are signed
//! 64-bit as in Apple's format: an identifier the iPhone sends with its high bit set reads as a negative
//! [`Value::Integer`] and is written back with the same bits ([`Value::as_u64`] gives the unsigned reading).

use std::fmt;
use std::io;

const MAGIC: &[u8; 8] = b"bplist00";
const TRAILER_BYTES: usize = 32;
/// Seconds from the Unix epoch to Apple's, 2001-01-01 UTC.
const APPLE_EPOCH_UNIX_SECONDS: f64 = 978_307_200.0;

/// Real messages nest a few levels; a plist that refers to itself would otherwise recurse without end.
const MAX_DEPTH: usize = 32;
/// Object reads and copied bytes allowed per input byte, on top of a fixed base.
const WORK_PER_BYTE: u64 = 4;
const WORK_BASE: u64 = 1024;
/// Real messages hold a few hundred objects.
const MAX_READS: u64 = 100_000;
/// The largest real payload is a picture of about 512 KiB, read once.
const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024;
/// Strings and data up to this size cost one read and nothing more, as writers share equal keys.
const SMALL_PAYLOAD_BYTES: u64 = 64;
/// Arrays reserve no more than this up front, whatever count they declare.
const INITIAL_CAPACITY: usize = 64;

/// A property list value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// A signed 64-bit integer; 1-, 2- and 4-byte integers on the wire are unsigned.
    Integer(i64),
    Real(f64),
    /// Seconds since 2001-01-01 UTC.
    Date(f64),
    Data(Vec<u8>),
    String(String),
    /// A keyed archive's object reference.
    Uid(u64),
    Array(Vec<Value>),
    Dict(Dict),
}

/// A dictionary with string keys, in insertion order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict {
    entries: Vec<(String, Value)>,
}

impl Dict {
    pub fn new() -> Dict {
        Dict::default()
    }

    /// The dictionary with `key` set to `value`, for building one in a single expression.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Dict {
        self.insert(key, value);
        self
    }

    /// Sets `key`, keeping its place if it is already there, and gives the value it replaced.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Value>) -> Option<Value> {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => Some(std::mem::replace(slot, value)),
            None => {
                self.entries.push((key, value));
                None
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let index = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(index).1)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Dict {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Dict {
        let mut dict = Dict::new();
        for (key, value) in iter {
            dict.insert(key, value);
        }
        dict
    }
}

impl IntoIterator for Dict {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// An integer's bits read as unsigned, or a UID.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Integer(value) => Some(*value as u64),
            Value::Uid(value) => Some(*value),
            _ => None,
        }
    }

    /// A real, or an integer as a real.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Real(value) => Some(*value),
            Value::Integer(value) => Some(*value as f64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_data(&self) -> Option<&[u8]> {
        match self {
            Value::Data(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Value::Dict(value) => Some(value),
            _ => None,
        }
    }

    /// A date as seconds since the Unix epoch.
    pub fn as_unix_seconds(&self) -> Option<f64> {
        match self {
            Value::Date(value) => Some(value + APPLE_EPOCH_UNIX_SECONDS),
            _ => None,
        }
    }

    /// A date at `seconds` since the Unix epoch.
    pub fn date_from_unix_seconds(seconds: f64) -> Value {
        Value::Date(seconds - APPLE_EPOCH_UNIX_SECONDS)
    }

    /// The value under `key`, if this is a dictionary that has it.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?.get(key)
    }
}

macro_rules! from_unsigned {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(value: $t) -> Value {
                Value::Integer(i64::from(value))
            }
        }
    )*};
}
from_unsigned!(u8, u16, u32, i8, i16, i32, i64);

impl From<u64> for Value {
    /// The same bits as a signed integer, which is how the wire carries values above `i64::MAX`.
    fn from(value: u64) -> Value {
        Value::Integer(value as i64)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Value {
        Value::Bool(value)
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Value {
        Value::Real(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Value {
        Value::String(value.to_owned())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Value {
        Value::String(value)
    }
}

impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Value {
        Value::Data(value)
    }
}

impl From<&[u8]> for Value {
    fn from(value: &[u8]) -> Value {
        Value::Data(value.to_vec())
    }
}

impl From<Vec<Value>> for Value {
    fn from(value: Vec<Value>) -> Value {
        Value::Array(value)
    }
}

impl From<Dict> for Value {
    fn from(value: Dict) -> Value {
        Value::Dict(value)
    }
}

/// Why a plist could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(&'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bplist: {}", self.0)
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(error: Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

fn fail<T>(reason: &'static str) -> Result<T, Error> {
    Err(Error(reason))
}

/// Reads `size` bytes at `offset` as a big-endian unsigned number.
fn read_be(bytes: &[u8], offset: usize, size: usize) -> Result<u64, Error> {
    let end = offset.checked_add(size).filter(|&end| end <= bytes.len());
    let Some(end) = end else {
        return fail("value runs past the data");
    };
    Ok(bytes[offset..end]
        .iter()
        .fold(0u64, |value, &byte| (value << 8) | u64::from(byte)))
}

/// Decodes a binary plist.
pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
    if bytes.len() < MAGIC.len() + TRAILER_BYTES {
        return fail("too short");
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return fail("bad magic");
    }
    let trailer = bytes.len() - TRAILER_BYTES;
    let offset_size = usize::from(bytes[trailer + 6]);
    let ref_size = usize::from(bytes[trailer + 7]);
    if !(1..=8).contains(&offset_size) || !(1..=8).contains(&ref_size) {
        return fail("bad offset or reference size");
    }
    let objects = read_be(bytes, trailer + 8, 8)?;
    let top = read_be(bytes, trailer + 16, 8)?;
    let table = read_be(bytes, trailer + 24, 8)?;
    // Each object takes at least one byte, so no count above the data's length is real.
    if objects < 1 || objects > trailer as u64 || top >= objects {
        return fail("bad object count");
    }
    let table_end = objects
        .checked_mul(offset_size as u64)
        .and_then(|size| size.checked_add(table));
    if table < MAGIC.len() as u64 || table_end.is_none_or(|end| end > trailer as u64) {
        return fail("bad offset table");
    }
    let table = table as usize;
    let mut offsets = Vec::with_capacity(objects as usize);
    for index in 0..objects as usize {
        let offset = read_be(bytes, table + index * offset_size, offset_size)?;
        if offset < MAGIC.len() as u64 || offset >= table as u64 {
            return fail("object offset out of range");
        }
        offsets.push(offset as usize);
    }
    let allowance = (bytes.len() as u64)
        .saturating_mul(WORK_PER_BYTE)
        .saturating_add(WORK_BASE);
    let mut reader = Reader {
        bytes,
        offsets,
        ref_size,
        reads: allowance.min(MAX_READS),
        payload: allowance.min(MAX_PAYLOAD_BYTES),
    };
    reader.object(top as usize, 0)
}

struct Reader<'a> {
    bytes: &'a [u8],
    offsets: Vec<usize>,
    ref_size: usize,
    /// Object reads left.
    reads: u64,
    /// Bytes that may still be copied out of strings and data.
    payload: u64,
}

impl Reader<'_> {
    fn object(&mut self, index: usize, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return fail("nested too deep");
        }
        if self.reads == 0 {
            return fail("expands to more objects than its size allows");
        }
        self.reads -= 1;
        let Some(&start) = self.offsets.get(index) else {
            return fail("reference out of range");
        };
        let marker = self.bytes[start];
        let (kind, nibble) = (marker >> 4, marker & 0x0f);
        let mut position = start + 1;
        let bytes = self.bytes;
        let value = match kind {
            0x0 => match nibble {
                0x0 => Value::Null,
                0x8 => Value::Bool(false),
                0x9 => Value::Bool(true),
                _ => return fail("unsupported primitive"),
            },
            0x1 => {
                if nibble > 3 {
                    return fail("integer wider than 8 bytes");
                }
                Value::Integer(read_be(bytes, position, 1 << nibble)? as i64)
            }
            0x2 => match nibble {
                2 => Value::Real(f64::from(f32::from_bits(
                    read_be(bytes, position, 4)? as u32
                ))),
                3 => Value::Real(f64::from_bits(read_be(bytes, position, 8)?)),
                _ => return fail("unsupported real size"),
            },
            0x3 => Value::Date(f64::from_bits(read_be(bytes, position, 8)?)),
            0x4 => {
                let count = self.count(nibble, &mut position)?;
                self.charge(count as u64)?;
                Value::Data(bytes[position..position + count].to_vec())
            }
            0x5 => {
                let count = self.count(nibble, &mut position)?;
                self.charge(count as u64)?;
                Value::String(
                    String::from_utf8_lossy(&bytes[position..position + count]).into_owned(),
                )
            }
            0x6 => {
                let count = self.count(nibble, &mut position)?;
                if count.saturating_mul(2) > bytes.len() - position {
                    return fail("string longer than the data");
                }
                self.charge(count as u64 * 2)?;
                let units: Vec<u16> = bytes[position..position + count * 2]
                    .chunks_exact(2)
                    .map(|unit| u16::from_be_bytes([unit[0], unit[1]]))
                    .collect();
                Value::String(String::from_utf16_lossy(&units))
            }
            0x8 => {
                if nibble > 7 {
                    return fail("uid wider than 8 bytes");
                }
                Value::Uid(read_be(bytes, position, usize::from(nibble) + 1)?)
            }
            0xa | 0xc => {
                let count = self.count(nibble, &mut position)?;
                if count.saturating_mul(self.ref_size) > bytes.len() - position {
                    return fail("array larger than the data");
                }
                // Not `count`: an array that holds itself would reserve that much at every level.
                let mut items = Vec::with_capacity(count.min(INITIAL_CAPACITY));
                for item in 0..count {
                    let reference = self.reference(position + item * self.ref_size)?;
                    items.push(self.object(reference, depth + 1)?);
                }
                Value::Array(items)
            }
            0xd => {
                let count = self.count(nibble, &mut position)?;
                if count.saturating_mul(2 * self.ref_size) > bytes.len() - position {
                    return fail("dictionary larger than the data");
                }
                let mut dict = Dict::new();
                for entry in 0..count {
                    let key = self.reference(position + entry * self.ref_size)?;
                    let value = self.reference(position + (count + entry) * self.ref_size)?;
                    let Value::String(key) = self.object(key, depth + 1)? else {
                        return fail("dictionary key is not a string");
                    };
                    let value = self.object(value, depth + 1)?;
                    dict.insert(key, value);
                }
                Value::Dict(dict)
            }
            _ => return fail("unsupported object type"),
        };
        Ok(value)
    }

    /// The count in a marker's low nibble, or the integer after the marker when the nibble is 0xf.
    fn count(&self, nibble: u8, position: &mut usize) -> Result<usize, Error> {
        if nibble != 0x0f {
            return Ok(usize::from(nibble));
        }
        let Some(&size_marker) = self.bytes.get(*position) else {
            return fail("value runs past the data");
        };
        *position += 1;
        if size_marker >> 4 != 0x1 {
            return fail("count is not an integer");
        }
        let width = 1usize << (size_marker & 0x0f);
        if width > 8 {
            return fail("count wider than 8 bytes");
        }
        let count = read_be(self.bytes, *position, width)?;
        *position += width;
        // A count is the length of what follows, so it cannot exceed the data that is left.
        if count > (self.bytes.len() - *position) as u64 {
            return fail("count larger than the data");
        }
        Ok(count as usize)
    }

    fn reference(&self, offset: usize) -> Result<usize, Error> {
        let reference = read_be(self.bytes, offset, self.ref_size)?;
        usize::try_from(reference).or(fail("reference out of range"))
    }

    /// Strings and data are copied at every read, so a large one costs the budget its size each time.
    fn charge(&mut self, size: u64) -> Result<(), Error> {
        if size <= SMALL_PAYLOAD_BYTES {
            return Ok(());
        }
        match self.payload.checked_sub(size) {
            Some(left) => {
                self.payload = left;
                Ok(())
            }
            None => fail("shared data is read more often than its size allows"),
        }
    }
}

/// An encoded object: its bytes up to its references, and the references of a container.
struct Node {
    head: Vec<u8>,
    refs: Vec<usize>,
}

/// Encodes a value as a binary plist. Every value is written as its own object, in depth-first order.
pub fn encode(root: &Value) -> Vec<u8> {
    let mut nodes = Vec::new();
    let top = add(&mut nodes, root);
    let ref_size = width_for(nodes.len() as u64);

    let mut out = MAGIC.to_vec();
    let mut offsets = Vec::with_capacity(nodes.len());
    for node in &nodes {
        offsets.push(out.len() as u64);
        out.extend_from_slice(&node.head);
        for &reference in &node.refs {
            put_be(&mut out, reference as u64, ref_size);
        }
    }
    let table = out.len() as u64;
    let offset_size = width_for(table);
    for offset in offsets {
        put_be(&mut out, offset, offset_size);
    }
    out.extend_from_slice(&[0; 6]);
    out.push(offset_size as u8);
    out.push(ref_size as u8);
    put_be(&mut out, nodes.len() as u64, 8);
    put_be(&mut out, top as u64, 8);
    put_be(&mut out, table, 8);
    out
}

fn add(nodes: &mut Vec<Node>, value: &Value) -> usize {
    let index = nodes.len();
    nodes.push(Node {
        head: Vec::new(),
        refs: Vec::new(),
    });
    let mut refs = Vec::new();
    let head = match value {
        Value::Null => vec![0x00],
        Value::Bool(false) => vec![0x08],
        Value::Bool(true) => vec![0x09],
        Value::Integer(number) => {
            // Non-negative integers take the fewest bytes; negative ones are 8-byte two's complement.
            let width = if *number < 0 {
                8
            } else {
                width_for(*number as u64)
            };
            let mut head = vec![0x10 | width.trailing_zeros() as u8];
            put_be(&mut head, *number as u64, width);
            head
        }
        Value::Real(number) => {
            let mut head = vec![0x23];
            put_be(&mut head, number.to_bits(), 8);
            head
        }
        Value::Date(seconds) => {
            let mut head = vec![0x33];
            put_be(&mut head, seconds.to_bits(), 8);
            head
        }
        Value::Uid(uid) => {
            let width = (8 - (uid.leading_zeros() as usize) / 8).max(1);
            let mut head = vec![0x80 | (width - 1) as u8];
            put_be(&mut head, *uid, width);
            head
        }
        Value::Data(data) => {
            let mut head = marker(0x4, data.len());
            head.extend_from_slice(data);
            head
        }
        Value::String(text) if text.is_ascii() => {
            let mut head = marker(0x5, text.len());
            head.extend_from_slice(text.as_bytes());
            head
        }
        Value::String(text) => {
            let units: Vec<u16> = text.encode_utf16().collect();
            let mut head = marker(0x6, units.len());
            for unit in units {
                head.extend_from_slice(&unit.to_be_bytes());
            }
            head
        }
        Value::Array(items) => {
            refs = items.iter().map(|item| add(nodes, item)).collect();
            marker(0xa, items.len())
        }
        Value::Dict(dict) => {
            let keys: Vec<usize> = dict
                .keys()
                .map(|key| add(nodes, &Value::String(key.to_owned())))
                .collect();
            let values: Vec<usize> = dict.iter().map(|(_, value)| add(nodes, value)).collect();
            refs = keys.into_iter().chain(values).collect();
            marker(0xd, dict.len())
        }
    };
    nodes[index] = Node { head, refs };
    index
}

/// An object's marker byte, followed by an integer count when the count does not fit the low nibble.
fn marker(kind: u8, count: usize) -> Vec<u8> {
    if count < 0x0f {
        return vec![(kind << 4) | count as u8];
    }
    let width = width_for(count as u64);
    let mut head = vec![(kind << 4) | 0x0f, 0x10 | width.trailing_zeros() as u8];
    put_be(&mut head, count as u64, width);
    head
}

/// The fewest of 1, 2, 4 or 8 bytes that hold `value`.
fn width_for(value: u64) -> usize {
    match value {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        0x1_0000..=0xffff_ffff => 4,
        _ => 8,
    }
}

fn put_be(out: &mut Vec<u8>, value: u64, size: usize) {
    out.extend_from_slice(&value.to_be_bytes()[8 - size..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    /// A plist built by hand: `objects` in order, one-byte references, the top object is object 0.
    fn plist(objects: &[Vec<u8>], declared: u64, offset_size: usize) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        let mut offsets = Vec::new();
        for object in objects {
            offsets.push(out.len() as u64);
            out.extend_from_slice(object);
        }
        let table = out.len() as u64;
        for offset in offsets {
            put_be(&mut out, offset, offset_size);
        }
        out.extend_from_slice(&[0; 6]);
        out.push(offset_size as u8);
        out.push(1);
        put_be(&mut out, declared, 8);
        put_be(&mut out, 0, 8);
        put_be(&mut out, table, 8);
        out
    }

    fn plain(objects: &[Vec<u8>]) -> Vec<u8> {
        plist(objects, objects.len() as u64, 1)
    }

    fn wide(objects: &[Vec<u8>]) -> Vec<u8> {
        plist(objects, objects.len() as u64, 4)
    }

    /// An array of `count` one-byte references to object `target`, with a four-byte count.
    fn array_of(count: u32, target: u8) -> Vec<u8> {
        let mut array = vec![0xaf, 0x12];
        array.extend_from_slice(&count.to_be_bytes());
        array.resize(array.len() + count as usize, target);
        array
    }

    const FALSE: [u8; 1] = [0x08];

    #[test]
    fn decodes_dates() {
        // What Safari's insertPlayQueueItem carries: a Start-Date next to the media URL.
        let bytes = hex(concat!(
            "62706c6973743030d201020308546974656d5474797065d2040506075f1010436f6e74656e742d4c6f636174696f6e",
            "5a53746172742d446174655f101968747470733a2f2f6578616d706c652e636f6d2f762e6d70343341c83698a00000005f1013",
            "696e73657274506c617951756575654974656d080d12171c2f3a565f000000000000010100000000000000090000000000000000",
            "0000000000000075"
        ));
        let root = decode(&bytes).unwrap();
        let item = root.get("item").unwrap();
        assert_eq!(
            root.get("type").unwrap().as_str(),
            Some("insertPlayQueueItem")
        );
        assert_eq!(
            item.get("Content-Location").unwrap().as_str(),
            Some("https://example.com/v.mp4")
        );
        // 2026-09-30 12:00:00 UTC.
        let start = item.get("Start-Date").unwrap().as_unix_seconds().unwrap();
        assert_eq!((start * 1000.0).round() as i64, 1_790_769_600_000);
    }

    #[test]
    fn an_object_count_larger_than_the_plist_can_hold_is_rejected() {
        assert!(decode(&plist(&[FALSE.to_vec()], 0x7fff_ffff, 1)).is_err());
    }

    #[test]
    fn a_dictionary_that_contains_itself_is_rejected() {
        let dictionary = vec![0xd1, 1, 0];
        let key = vec![0x51, b'k'];
        assert!(decode(&plain(&[dictionary, key])).is_err());
    }

    #[test]
    fn an_array_count_larger_than_the_plist_can_hold_is_rejected() {
        let array = vec![0xaf, 0x12, 0x7f, 0xff, 0xff, 0xff];
        assert!(decode(&plain(&[array])).is_err());
    }

    #[test]
    fn a_small_plist_that_expands_exponentially_is_rejected() {
        // Object i is an array of two references to object i + 1: 2^30 leaves from about 100 bytes.
        let mut objects: Vec<Vec<u8>> = (0..30u8).map(|i| vec![0xa2, i + 1, i + 1]).collect();
        objects.push(FALSE.to_vec());
        assert!(decode(&plain(&objects)).is_err());
    }

    #[test]
    fn a_large_blob_that_many_references_point_at_is_rejected() {
        // A 200 KB data object and 20,000 references to it: 4 GB of copies from 220 KB.
        let mut blob = vec![0x4f, 0x12];
        blob.extend_from_slice(&200_000u32.to_be_bytes());
        blob.resize(blob.len() + 200_000, 0);
        assert!(decode(&wide(&[array_of(20_000, 1), blob])).is_err());
    }

    #[test]
    fn a_wide_integer_that_the_data_could_hold_is_rejected() {
        let mut integer = vec![0x1f];
        integer.resize(1 + 32_768, 0);
        assert!(decode(&wide(&[integer])).is_err());
    }

    #[test]
    fn a_wide_uid_is_rejected() {
        let mut uid = vec![0x8f];
        uid.resize(17, 0);
        assert!(decode(&plain(&[uid])).is_err());
    }

    #[test]
    fn a_plist_that_shares_containers_into_millions_of_objects_is_rejected() {
        // 250,000 references to one array of 8 references to one empty array.
        let middle = [vec![0xa8], vec![2; 8]].concat();
        assert!(decode(&wide(&[array_of(250_000, 1), middle, vec![0xa0]])).is_err());
    }

    #[test]
    fn an_array_that_contains_itself_does_not_reserve_its_count_at_every_level() {
        assert!(decode(&wide(&[array_of(16_000_000, 0)])).is_err());
    }

    #[test]
    fn more_than_a_hundred_thousand_object_reads_are_rejected() {
        assert!(decode(&wide(&[array_of(250_000, 1), vec![0xa0]])).is_err());
    }

    #[test]
    fn a_writer_that_shares_the_keys_of_many_dictionaries_still_decodes() {
        let (dictionaries, keys) = (100u8, 5u8);
        let mut array = vec![0xaf, 0x12];
        array.extend_from_slice(&u32::from(dictionaries).to_be_bytes());
        array.extend((0..dictionaries).map(|i| i + 1));
        let mut dictionary = vec![0xd0 | keys];
        dictionary.extend((0..keys).map(|i| dictionaries + 1 + i));
        dictionary.extend((0..keys).map(|_| dictionaries + 1 + keys));
        let mut objects = vec![array];
        objects.extend((0..dictionaries).map(|_| dictionary.clone()));
        for i in 0..keys {
            let mut key = vec![0x5f, 0x10, 20];
            key.extend_from_slice(&format!("key-number-{i}-padxxxx").as_bytes()[..20]);
            objects.push(key);
        }
        objects.push(vec![0x10, 7]);
        let decoded = decode(&plist(&objects, objects.len() as u64, 2)).unwrap();
        let decoded = decoded.as_array().unwrap();
        assert_eq!(decoded.len(), 100);
        assert_eq!(decoded[0].as_dict().unwrap().len(), 5);
    }

    #[test]
    fn a_large_picture_inside_a_dictionary_still_decodes() {
        let mut picture = vec![0x4f, 0x12];
        picture.extend_from_slice(&500_000u32.to_be_bytes());
        picture.resize(picture.len() + 500_000, 7);
        let decoded = decode(&wide(&[vec![0xd1, 1, 2], vec![0x51, b'p'], picture])).unwrap();
        assert_eq!(decoded.get("p").unwrap().as_data().unwrap().len(), 500_000);
    }

    #[test]
    fn a_dictionary_key_that_is_not_a_string_is_rejected() {
        assert!(decode(&plain(&[vec![0xd1, 1, 2], vec![0xa0], FALSE.to_vec()])).is_err());
    }

    #[test]
    fn an_integer_that_runs_past_the_data_is_rejected() {
        assert!(decode(&plain(&[vec![0x1f]])).is_err());
    }

    #[test]
    fn repeated_references_to_one_string_decode() {
        let objects = [
            vec![0xd2, 1, 2, 3, 3],
            vec![0x51, b'a'],
            vec![0x51, b'b'],
            vec![0x52, b'o', b'k'],
        ];
        let expected = Value::Dict(Dict::new().with("a", "ok").with("b", "ok"));
        assert_eq!(decode(&plain(&objects)).unwrap(), expected);
    }

    #[test]
    fn a_nested_plist_round_trips() {
        let stream = Dict::new()
            .with("type", 110)
            .with("audioType", "default")
            .with("data", vec![1u8, 2, 3]);
        let value = Value::Dict(
            Dict::new()
                .with("streams", vec![Value::Dict(stream)])
                .with("name", "DiPlay")
                .with("flag", true),
        );
        let decoded = decode(&encode(&value)).unwrap();
        assert_eq!(decoded, value);
        let stream = &decoded.get("streams").unwrap().as_array().unwrap()[0];
        assert_eq!(stream.get("type").unwrap().as_i64(), Some(110));
    }

    #[test]
    fn twenty_nested_arrays_decode() {
        let mut nested = Value::Bool(true);
        for _ in 0..20 {
            nested = Value::Array(vec![nested]);
        }
        assert_eq!(decode(&encode(&nested)).unwrap(), nested);
    }

    // Bodies from an iOS 26 wireless session, from xcertplay's tests; they carry no device identifiers.
    const INFO_REQUEST: &str = concat!(
        "62706c6973743030d2010203075d616c7453637265656e55524c735d7569436f6e7465787455524c73a30405065f101f6d6170733a2f",
        "6361722f696e737472756d656e74636c75737465722f6d61705f102b6d6170733a2f6361722f696e737472756d656e74636c75",
        "737465722f696e737472756374696f6e636172645f101b6d6170733a2f6361722f696e737472756d656e74636c7573746572a4",
        "08090a0b556d6170733a5c6d6f62696c6570686f6e653a5b6e6f77706c6179696e673a596d657373616765733a080d1b292d4f",
        "7d9ba0a6b3bf0000000000000101000000000000000c000000000000000000000000000000c9"
    );
    const IAP_TUNNEL_SETUP: &str = concat!(
        "62706c6973743030d101025773747265616d73a103d80405060708090a0b0c0d0e0f101111135b636f6e74726f6c547970655963",
        "68616e6e656c494454736565645a636c69656e745555494454747970655f101477616e7473446564696361746564536f636b65",
        "745f100f73656e644d657373616765417349735e636c69656e74547970655555494410025f101930323a30303a30303a30303a",
        "30303a30322d5243532d3432321383acb4a2ca36c6645f102431344631433243362d434134422d343438312d424339412d4334",
        "4341424642313538464610820909",
        "5f102445393435394644302d424341442d344334352d383230462d3145373234343745463246320008000b0013001500260032",
        "003c0041004c00510068007a0089008b00a700b000d700d900da00db000000000000020100000000000000140000000000000000",
        "0000000000000102"
    );
    const ALT_SCREEN_SETUP: &str = concat!(
        "62706c6973743030d101025773747265616d73a103d40405060708090a0b5f101273747265616d436f6e6e656374696f6e494459",
        "6c6174656e63794d73547479706554757569641352b9f11587294458104b106f5f102462376536633561302d323232322d3430",
        "30302d383030302d303030303030303030303032080b13151e333d42475052540000000000000101000000000000000c000000",
        "0000000000000000000000007b"
    );

    fn single_stream(body: &str) -> Value {
        let root = decode(&hex(body)).unwrap();
        let streams = root.get("streams").unwrap().as_array().unwrap();
        assert_eq!(streams.len(), 1);
        streams[0].clone()
    }

    #[test]
    fn decodes_the_phone_info_request() {
        let request = decode(&hex(INFO_REQUEST)).unwrap();
        let strings = |key: &str| -> Vec<String> {
            request
                .get(key)
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            strings("altScreenURLs"),
            [
                "maps:/car/instrumentcluster/map",
                "maps:/car/instrumentcluster/instructioncard",
                "maps:/car/instrumentcluster"
            ]
        );
        assert_eq!(
            strings("uiContextURLs"),
            ["maps:", "mobilephone:", "nowplaying:", "messages:"]
        );
    }

    #[test]
    fn decodes_tunnel_setup_with_two_byte_offsets_and_a_high_bit_seed() {
        let stream = single_stream(IAP_TUNNEL_SETUP);
        assert_eq!(stream.get("type").unwrap().as_i64(), Some(130));
        assert_eq!(stream.get("controlType").unwrap().as_i64(), Some(2));
        assert_eq!(
            stream.get("seed").unwrap().as_i64(),
            Some(-8_958_586_947_476_470_172)
        );
        assert_eq!(
            stream.get("wantsDedicatedSocket").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(stream.get("sendMessageAsIs").unwrap().as_bool(), Some(true));
        assert_eq!(
            stream.get("clientTypeUUID").unwrap().as_str(),
            Some("E9459FD0-BCAD-4C45-820F-1E72447EF2F2")
        );
        assert_eq!(
            stream.get("channelID").unwrap().as_str(),
            Some("02:00:00:00:00:02-RCS-422")
        );
    }

    #[test]
    fn decodes_the_alt_screen_setup() {
        let stream = single_stream(ALT_SCREEN_SETUP);
        assert_eq!(stream.get("type").unwrap().as_i64(), Some(111));
        assert_eq!(stream.get("latencyMs").unwrap().as_i64(), Some(75));
        assert_eq!(
            stream.get("streamConnectionID").unwrap().as_i64(),
            Some(5_961_060_656_565_797_976)
        );
        assert_eq!(
            stream.get("uuid").unwrap().as_str(),
            Some("b7e6c5a0-2222-4000-8000-000000000002")
        );
    }

    #[test]
    fn re_encoding_phone_requests_keeps_their_content() {
        for body in [INFO_REQUEST, IAP_TUNNEL_SETUP, ALT_SCREEN_SETUP] {
            let decoded = decode(&hex(body)).unwrap();
            assert_eq!(decode(&encode(&decoded)).unwrap(), decoded);
        }
    }

    #[test]
    fn high_bit_identifiers_are_echoed_with_the_same_bits() {
        let seed = single_stream(IAP_TUNNEL_SETUP).get("seed").unwrap().clone();
        let echoed = decode(&encode(&seed)).unwrap();
        assert_eq!(echoed, seed);
        assert_eq!(echoed.as_u64(), Some(0x83ac_b4a2_ca36_c664));
        assert_eq!(&encode(&seed)[8..17], &hex("1383acb4a2ca36c664")[..]);
    }

    #[test]
    fn round_trips_every_type() {
        let blob: Vec<u8> = (0..300).map(|i| i as u8).collect();
        let root = Value::Dict(
            Dict::new()
                .with("bool", false)
                .with("u8", 0xffu32)
                .with("u16", 0x1234u32)
                .with("u32", 0x1234_5678u32)
                .with("u64", 0x12_3456_789au64)
                .with("unsignedMax", u64::MAX)
                .with("real", 0.5)
                .with("negative", -3)
                .with("ascii", "x".repeat(20))
                .with("utf16", "车机·CarPlay")
                .with("data", blob.clone())
                .with("null", Value::Null)
                .with("uid", Value::Uid(0x1_0000))
                .with("date", Value::Date(1.5))
                .with(
                    "nested",
                    vec![
                        Value::Dict(Dict::new().with("empty", Vec::<Value>::new())),
                        true.into(),
                    ],
                ),
        );
        let decoded = decode(&encode(&root)).unwrap();
        assert_eq!(decoded, root);
        let dict = decoded.as_dict().unwrap();
        assert_eq!(
            dict.keys().collect::<Vec<_>>(),
            root.as_dict().unwrap().keys().collect::<Vec<_>>()
        );
        assert_eq!(dict.get("unsignedMax").unwrap().as_i64(), Some(-1));
        assert_eq!(dict.get("negative").unwrap().as_i64(), Some(-3));
        assert_eq!(dict.get("utf16").unwrap().as_str(), Some("车机·CarPlay"));
        assert_eq!(dict.get("data").unwrap().as_data(), Some(&blob[..]));
    }

    #[test]
    fn integers_take_the_fewest_bytes() {
        let head = |value: Value| encode(&value)[8..].to_vec();
        assert_eq!(head(0xffu32.into())[..2], [0x10, 0xff]);
        assert_eq!(head(0x100u32.into())[..3], [0x11, 0x01, 0x00]);
        assert_eq!(head(0x1_0000u32.into())[..5], [0x12, 0, 1, 0, 0]);
        assert_eq!(head(0x1_0000_0000u64.into())[0], 0x13);
        assert_eq!(
            head((-1i64).into())[..9],
            [0x13, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn many_objects_use_wide_references() {
        let list = Value::Array((0..400).map(|i| Value::from(format!("item{i}"))).collect());
        let bytes = encode(&list);
        assert_eq!(bytes[bytes.len() - 25], 2);
        assert_eq!(decode(&bytes).unwrap(), list);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(decode(&[0; 39]).is_err());
        assert!(decode(&[0; 64]).is_err());
        let mut truncated = encode(&Value::from("hello"));
        truncated.truncate(truncated.len() - 1);
        assert!(decode(&truncated).is_err());
    }
}
