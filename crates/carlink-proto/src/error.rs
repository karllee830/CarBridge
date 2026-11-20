// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

use std::fmt;

/// A message, record or key that cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The header's checksum does not match its first 18 bytes.
    Checksum { expected: u16, actual: u16 },
    /// The header's length is shorter than the header or longer than [`crate::frame::MAX_MESSAGE_LEN`].
    Length(u32),
    /// The body format bits hold the unused value 3.
    BodyFormat(u8),
    /// An encrypted body is cut short or its IV is not 12 bytes long.
    Record,
    /// A record does not decrypt with the session key.
    Decrypt,
    /// A protobuf body does not decode, or the message's body is not protobuf.
    Protobuf(String),
    /// A public key is not a point on P-256.
    PublicKey,
    /// A signature is not DER-encoded ECDSA, or does not verify.
    Signature,
    /// A stored private key is not a valid P-256 scalar.
    PrivateKey,
    /// A message field is missing or has the wrong size.
    Field(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Checksum { expected, actual } => {
                write!(f, "header checksum {actual:#06x}, expected {expected:#06x}")
            }
            Error::Length(len) => write!(f, "message length {len} out of range"),
            Error::BodyFormat(bits) => write!(f, "unknown body format {bits}"),
            Error::Record => f.write_str("malformed encrypted record"),
            Error::Decrypt => f.write_str("record does not decrypt"),
            Error::Protobuf(err) => write!(f, "protobuf body: {err}"),
            Error::PublicKey => f.write_str("invalid P-256 public key"),
            Error::Signature => f.write_str("invalid signature"),
            Error::PrivateKey => f.write_str("invalid P-256 private key"),
            Error::Field(name) => write!(f, "field {name} missing or of the wrong size"),
        }
    }
}

impl std::error::Error for Error {}

impl From<prost::DecodeError> for Error {
    fn from(err: prost::DecodeError) -> Self {
        Error::Protobuf(err.to_string())
    }
}
