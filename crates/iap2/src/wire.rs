// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's iap2/wire/Iap2Wire.kt,
// iap2/wire/Iap2WireCodec.kt, iap2/body/Iap2BodyBuilder.kt and iap2/body/Iap2BodyReader.kt (DiPlay commit 615b32c).

//! iAP2 control session messages: a message identifier and a body of parameters, each `u16 length | u16 id |
//! payload`, all big-endian. A parameter's payload is a scalar, a NUL-terminated UTF-8 string, raw bytes, or a
//! nested group of parameters.
//!
//! Parameters keep their order, and an identifier may repeat, so a body is a list rather than a map.

use std::fmt;
use std::io;

/// The largest message, header included.
pub const MAX_FRAME_BYTES: usize = 0xffff;
/// The message header: `0x4040 | u16 length | u16 message id`.
pub const FRAME_HEADER_BYTES: usize = 6;
/// The largest message body.
pub const MAX_BODY_BYTES: usize = MAX_FRAME_BYTES - FRAME_HEADER_BYTES;
/// A parameter's header: `u16 length | u16 id`.
pub const PARAMETER_HEADER_BYTES: usize = 4;
/// The largest parameter payload.
pub const MAX_PARAMETER_PAYLOAD: usize = 0xffff - PARAMETER_HEADER_BYTES;
/// The first two bytes of every message.
pub const FRAME_START: u16 = 0x4040;

/// A malformed, incomplete or oversized iAP2 message or parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolError(String);

impl ProtocolError {
    pub fn new(message: impl Into<String>) -> ProtocolError {
        ProtocolError(message.into())
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

impl From<ProtocolError> for io::Error {
    fn from(error: ProtocolError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// One body parameter.
#[derive(Clone, PartialEq, Eq)]
pub struct Parameter {
    id: u16,
    payload: Vec<u8>,
}

impl fmt::Debug for Parameter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Parameter(0x{:04x}, {} bytes)",
            self.id,
            self.payload.len()
        )
    }
}

impl Parameter {
    pub fn new(id: u16, payload: Vec<u8>) -> Parameter {
        Parameter { id, payload }
    }

    pub fn id(&self) -> u16 {
        self.id
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn sized<const N: usize>(&self) -> Result<[u8; N], ProtocolError> {
        self.payload.as_slice().try_into().map_err(|_| {
            ProtocolError(format!(
                "parameter 0x{:04x} has {} bytes, not {N}",
                self.id,
                self.payload.len()
            ))
        })
    }

    /// A parameter without payload, which means "present".
    pub fn as_void(&self) -> Result<(), ProtocolError> {
        self.sized::<0>().map(|_| ())
    }

    pub fn as_u8(&self) -> Result<u8, ProtocolError> {
        self.sized().map(u8::from_be_bytes)
    }

    pub fn as_i8(&self) -> Result<i8, ProtocolError> {
        self.sized().map(i8::from_be_bytes)
    }

    /// A byte, true when it is not zero.
    pub fn as_bool(&self) -> Result<bool, ProtocolError> {
        self.as_u8().map(|value| value != 0)
    }

    pub fn as_u16(&self) -> Result<u16, ProtocolError> {
        self.sized().map(u16::from_be_bytes)
    }

    pub fn as_i16(&self) -> Result<i16, ProtocolError> {
        self.sized().map(i16::from_be_bytes)
    }

    pub fn as_u32(&self) -> Result<u32, ProtocolError> {
        self.sized().map(u32::from_be_bytes)
    }

    pub fn as_i32(&self) -> Result<i32, ProtocolError> {
        self.sized().map(i32::from_be_bytes)
    }

    pub fn as_u64(&self) -> Result<u64, ProtocolError> {
        self.sized().map(u64::from_be_bytes)
    }

    /// A NUL-terminated string; invalid UTF-8 is replaced.
    pub fn as_string(&self) -> Result<String, ProtocolError> {
        match self.payload.split_last() {
            Some((0, text)) => Ok(String::from_utf8_lossy(text).into_owned()),
            _ => Err(ProtocolError(format!(
                "parameter 0x{:04x} is not a NUL-terminated string",
                self.id
            ))),
        }
    }

    /// Strings one after another, each NUL-terminated.
    pub fn as_strings(&self) -> Result<Vec<String>, ProtocolError> {
        match self.payload.split_last() {
            None => Ok(Vec::new()),
            Some((0, text)) => Ok(text
                .split(|&byte| byte == 0)
                .map(|item| String::from_utf8_lossy(item).into_owned())
                .collect()),
            Some(_) => Err(ProtocolError(format!(
                "parameter 0x{:04x} is not a list of NUL-terminated strings",
                self.id
            ))),
        }
    }

    /// Big-endian 16-bit values one after another.
    pub fn as_u16_list(&self) -> Result<Vec<u16>, ProtocolError> {
        if self.payload.len() % 2 != 0 {
            return Err(ProtocolError(format!(
                "parameter 0x{:04x} has an odd length for a u16 list",
                self.id
            )));
        }
        Ok(self
            .payload
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect())
    }

    /// A nested group of parameters.
    pub fn as_group(&self) -> Result<Parameters, ProtocolError> {
        Parameters::parse(&self.payload)
    }
}

/// A message body: parameters in order. Build one by chaining the typed methods, read one with
/// [`Parameters::first`], [`Parameters::all`] or [`Parameters::require`] and the [`Parameter`] accessors.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parameters {
    parameters: Vec<Parameter>,
}

impl Parameters {
    pub fn new() -> Parameters {
        Parameters::default()
    }

    /// Parses a complete body, rejecting cut-short headers, impossible lengths and trailing bytes.
    pub fn parse(mut bytes: &[u8]) -> Result<Parameters, ProtocolError> {
        let mut parameters = Vec::new();
        while !bytes.is_empty() {
            if bytes.len() < PARAMETER_HEADER_BYTES {
                return Err(ProtocolError::new("truncated parameter header"));
            }
            let length = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
            if length < PARAMETER_HEADER_BYTES || length > bytes.len() {
                return Err(ProtocolError(format!("invalid parameter length {length}")));
            }
            parameters.push(Parameter {
                id: u16::from_be_bytes([bytes[2], bytes[3]]),
                payload: bytes[PARAMETER_HEADER_BYTES..length].to_vec(),
            });
            bytes = &bytes[length..];
        }
        Ok(Parameters { parameters })
    }

    /// The encoded body, or an error when a parameter or the whole body is too long for the wire.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut out = Vec::new();
        for parameter in &self.parameters {
            if parameter.payload.len() > MAX_PARAMETER_PAYLOAD {
                return Err(ProtocolError(format!(
                    "parameter 0x{:04x} payload of {} bytes over {MAX_PARAMETER_PAYLOAD}",
                    parameter.id,
                    parameter.payload.len()
                )));
            }
            put_parameter(&mut out, parameter.id, &parameter.payload);
        }
        if out.len() > MAX_BODY_BYTES {
            return Err(ProtocolError(format!(
                "body of {} bytes over {MAX_BODY_BYTES}",
                out.len()
            )));
        }
        Ok(out)
    }

    pub fn len(&self) -> usize {
        self.parameters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parameters.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Parameter> {
        self.parameters.iter()
    }

    pub fn has(&self, id: u16) -> bool {
        self.first(id).is_some()
    }

    pub fn first(&self, id: u16) -> Option<&Parameter> {
        self.parameters.iter().find(|parameter| parameter.id == id)
    }

    pub fn all(&self, id: u16) -> impl Iterator<Item = &Parameter> {
        self.parameters
            .iter()
            .filter(move |parameter| parameter.id == id)
    }

    /// The first parameter `id`, or an error naming `what` is missing.
    pub fn require(&self, id: u16, what: &str) -> Result<&Parameter, ProtocolError> {
        self.first(id)
            .ok_or_else(|| ProtocolError(format!("missing {what} (parameter 0x{id:04x})")))
    }

    /// Appends a parameter with a raw payload.
    pub fn raw(mut self, id: u16, payload: Vec<u8>) -> Parameters {
        self.parameters.push(Parameter { id, payload });
        self
    }

    pub fn void(self, id: u16) -> Parameters {
        self.raw(id, Vec::new())
    }

    pub fn u8(self, id: u16, value: u8) -> Parameters {
        self.raw(id, vec![value])
    }

    pub fn i8(self, id: u16, value: i8) -> Parameters {
        self.raw(id, value.to_be_bytes().to_vec())
    }

    pub fn bool(self, id: u16, value: bool) -> Parameters {
        self.u8(id, u8::from(value))
    }

    pub fn u16(self, id: u16, value: u16) -> Parameters {
        self.raw(id, value.to_be_bytes().to_vec())
    }

    pub fn i16(self, id: u16, value: i16) -> Parameters {
        self.raw(id, value.to_be_bytes().to_vec())
    }

    pub fn u32(self, id: u16, value: u32) -> Parameters {
        self.raw(id, value.to_be_bytes().to_vec())
    }

    pub fn i32(self, id: u16, value: i32) -> Parameters {
        self.raw(id, value.to_be_bytes().to_vec())
    }

    pub fn u64(self, id: u16, value: u64) -> Parameters {
        self.raw(id, value.to_be_bytes().to_vec())
    }

    pub fn bytes(self, id: u16, value: &[u8]) -> Parameters {
        self.raw(id, value.to_vec())
    }

    /// A NUL-terminated string. A string cannot hold NUL, so the text ends at its first one.
    pub fn string(self, id: u16, value: &str) -> Parameters {
        self.raw(id, nul_terminated(value))
    }

    /// Strings one after another, each NUL-terminated.
    pub fn strings<'a>(self, id: u16, values: impl IntoIterator<Item = &'a str>) -> Parameters {
        let payload = values.into_iter().flat_map(nul_terminated).collect();
        self.raw(id, payload)
    }

    pub fn u16_list(self, id: u16, values: impl IntoIterator<Item = u16>) -> Parameters {
        let payload = values.into_iter().flat_map(u16::to_be_bytes).collect();
        self.raw(id, payload)
    }

    /// A nested group. Its size is checked when the outer body is encoded.
    pub fn group(self, id: u16, group: Parameters) -> Parameters {
        let mut payload = Vec::new();
        for parameter in &group.parameters {
            put_parameter(&mut payload, parameter.id, &parameter.payload);
        }
        self.raw(id, payload)
    }
}

impl<'a> IntoIterator for &'a Parameters {
    type Item = &'a Parameter;
    type IntoIter = std::slice::Iter<'a, Parameter>;

    fn into_iter(self) -> Self::IntoIter {
        self.parameters.iter()
    }
}

fn nul_terminated(text: &str) -> Vec<u8> {
    let text = text.split('\0').next().unwrap_or_default();
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(0);
    bytes
}

/// Writes one parameter; a payload too long for the length field is caught by [`Parameters::encode`].
fn put_parameter(out: &mut Vec<u8>, id: u16, payload: &[u8]) {
    let length = (PARAMETER_HEADER_BYTES + payload.len()).min(0xffff) as u16;
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(payload);
}

/// One control session message.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    message_id: u16,
    body: Vec<u8>,
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Frame(0x{:04x}, {} bytes)",
            self.message_id,
            self.body.len()
        )
    }
}

impl Frame {
    /// A message with an encoded body.
    pub fn new(message_id: u16, body: Vec<u8>) -> Result<Frame, ProtocolError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ProtocolError(format!(
                "body of {} bytes over {MAX_BODY_BYTES}",
                body.len()
            )));
        }
        Ok(Frame { message_id, body })
    }

    pub fn with_parameters(
        message_id: u16,
        parameters: &Parameters,
    ) -> Result<Frame, ProtocolError> {
        Frame::new(message_id, parameters.encode()?)
    }

    pub fn message_id(&self) -> u16 {
        self.message_id
    }

    /// The body without the header.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn parameters(&self) -> Result<Parameters, ProtocolError> {
        Parameters::parse(&self.body)
    }

    /// The whole message, header included.
    pub fn encode(&self) -> Vec<u8> {
        let length = (FRAME_HEADER_BYTES + self.body.len()) as u16;
        let mut out = Vec::with_capacity(usize::from(length));
        out.extend_from_slice(&FRAME_START.to_be_bytes());
        out.extend_from_slice(&length.to_be_bytes());
        out.extend_from_slice(&self.message_id.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_and_unknown_ids_keep_their_order() {
        let encoded = Parameters::new()
            .raw(0, vec![1])
            .raw(0, vec![2])
            .raw(0x7fff, vec![3, 4])
            .encode()
            .unwrap();
        let decoded = Parameters::parse(&encoded).unwrap();
        assert_eq!(
            decoded.iter().map(Parameter::id).collect::<Vec<_>>(),
            [0, 0, 0x7fff]
        );
        let zeros: Vec<&[u8]> = decoded.all(0).map(Parameter::payload).collect();
        assert_eq!(zeros, [&[1u8][..], &[2u8][..]]);
        assert_eq!(decoded.first(0x7fff).unwrap().payload(), [3, 4]);
    }

    #[test]
    fn nested_groups_build_and_read() {
        let frame = Frame::with_parameters(
            0x0d01,
            &Parameters::new()
                .u16(0, 1)
                .group(4, Parameters::new().u32(0, 7).u8(1, 2)),
        )
        .unwrap();
        let body = frame.parameters().unwrap();
        let group = body.require(4, "road sign").unwrap().as_group().unwrap();
        assert_eq!(body.first(0).unwrap().as_u16(), Ok(1));
        assert_eq!(group.first(0).unwrap().as_u32(), Ok(7));
        assert_eq!(group.first(1).unwrap().as_u8(), Ok(2));
    }

    #[test]
    fn scalars_have_exact_sizes() {
        let body = Parameters::new()
            .i8(0, -2)
            .i16(1, -300)
            .i32(2, -70_000)
            .u64(3, u64::MAX)
            .bool(4, true)
            .void(5);
        let body = Parameters::parse(&body.encode().unwrap()).unwrap();
        assert_eq!(body.first(0).unwrap().as_i8(), Ok(-2));
        assert_eq!(body.first(1).unwrap().as_i16(), Ok(-300));
        assert_eq!(body.first(2).unwrap().as_i32(), Ok(-70_000));
        assert_eq!(body.first(3).unwrap().as_u64(), Ok(u64::MAX));
        assert_eq!(body.first(4).unwrap().as_bool(), Ok(true));
        assert_eq!(body.first(5).unwrap().as_void(), Ok(()));
        assert!(body.first(1).unwrap().as_u8().is_err());
        assert!(body.first(3).unwrap().as_u32().is_err());
        assert!(body.first(4).unwrap().as_void().is_err());
    }

    #[test]
    fn strings_are_nul_terminated() {
        let body = Parameters::new()
            .string(0, "LIVI")
            .strings(1, ["192.168.1.1", "192.168.1.2"])
            .string(2, "cut\0off")
            .u16_list(3, [0x5703, 0x4e0d]);
        let encoded = body.encode().unwrap();
        assert_eq!(&encoded[..9], [0, 9, 0, 0, b'L', b'I', b'V', b'I', 0]);
        let body = Parameters::parse(&encoded).unwrap();
        assert_eq!(body.first(0).unwrap().as_string().unwrap(), "LIVI");
        assert_eq!(
            body.first(1).unwrap().as_strings().unwrap(),
            ["192.168.1.1", "192.168.1.2"]
        );
        assert_eq!(body.first(2).unwrap().as_string().unwrap(), "cut");
        assert_eq!(
            body.first(3).unwrap().as_u16_list().unwrap(),
            [0x5703, 0x4e0d]
        );
        assert!(Parameter::new(9, b"no end".to_vec()).as_string().is_err());
        assert!(Parameter::new(9, Vec::new()).as_string().is_err());
        assert_eq!(
            Parameter::new(9, Vec::new()).as_strings().unwrap(),
            Vec::<String>::new()
        );
        assert!(Parameter::new(9, vec![1, 2, 3]).as_u16_list().is_err());
    }

    #[test]
    fn malformed_bodies_are_rejected() {
        assert!(Parameters::parse(&[0, 1, 2]).is_err());
        assert!(Parameters::parse(&[0, 3, 0, 0]).is_err());
        assert!(Parameters::parse(&[0, 9, 0, 0, 1]).is_err());
        assert!(Parameters::parse(&[0, 4, 0, 0, 7]).is_err());
        assert_eq!(Parameters::parse(&[]).unwrap(), Parameters::new());
    }

    #[test]
    fn oversized_bodies_are_rejected() {
        let big = Parameters::new().raw(0, vec![0; MAX_PARAMETER_PAYLOAD + 1]);
        assert!(big.encode().is_err());
        let full = Parameters::new().raw(0, vec![0; MAX_PARAMETER_PAYLOAD]);
        assert!(
            full.encode().is_err(),
            "a full parameter leaves no room for the frame header"
        );
        let fits = Parameters::new().raw(0, vec![0; MAX_BODY_BYTES - PARAMETER_HEADER_BYTES]);
        assert_eq!(fits.encode().unwrap().len(), MAX_BODY_BYTES);
        assert!(Frame::new(1, vec![0; MAX_BODY_BYTES + 1]).is_err());
        let grouped =
            Parameters::new().group(1, Parameters::new().raw(0, vec![0; MAX_PARAMETER_PAYLOAD]));
        assert!(grouped.encode().is_err());
    }

    #[test]
    fn frames_encode_with_their_header() {
        let frame = Frame::new(0x4301, vec![0, 5, 0, 0, 1]).unwrap();
        assert_eq!(
            frame.encode(),
            [0x40, 0x40, 0, 11, 0x43, 0x01, 0, 5, 0, 0, 1]
        );
    }
}
