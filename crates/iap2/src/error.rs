// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from the failure types of DiPlay's
// transport/Iap2IdentificationClient.kt, mfi/Iap2MfiAuthenticationClient.kt and transport/Iap2CsmChannel.kt (DiPlay
// commit 615b32c).

//! Why an iAP2 exchange failed.

use std::fmt;
use std::io;

use crate::wire::ProtocolError;

/// An iAP2 failure.
#[derive(Debug)]
pub enum Error {
    /// The byte stream failed, or the peer stopped answering.
    Io(io::Error),
    /// The link ended: the peer closed it, sent a reset, or the session was closed.
    Ended(Option<String>),
    /// A message that cannot be read, or one too large to send.
    Protocol(ProtocolError),
    /// A step did not finish in time.
    TimedOut(&'static str),
    /// The iPhone rejected the identification.
    IdentificationRejected(Rejection),
    /// A message that does not belong at this point of the exchange.
    UnexpectedMessage(u16),
    /// MFi authentication failed: the iPhone refused it, or the authenticator could not answer.
    Authentication(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(f, "iAP2 stream: {error}"),
            Error::Ended(Some(reason)) => write!(f, "iAP2 link ended: {reason}"),
            Error::Ended(None) => f.write_str("iAP2 link ended"),
            Error::Protocol(error) => write!(f, "iAP2: {error}"),
            Error::TimedOut(what) => write!(f, "iAP2 timed out {what}"),
            Error::IdentificationRejected(rejection) => {
                write!(f, "iAP2 identification rejected: {rejection}")
            }
            Error::UnexpectedMessage(id) => write!(f, "unexpected iAP2 message 0x{id:04x}"),
            Error::Authentication(reason) => write!(f, "iAP2 authentication failed: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(error) => Some(error),
            Error::Protocol(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

impl From<ProtocolError> for Error {
    fn from(error: ProtocolError) -> Error {
        Error::Protocol(error)
    }
}

impl From<Error> for io::Error {
    fn from(error: Error) -> io::Error {
        match error {
            Error::Io(error) => error,
            Error::TimedOut(_) => io::Error::new(io::ErrorKind::TimedOut, error),
            Error::Ended(_) => io::Error::new(io::ErrorKind::ConnectionAborted, error),
            Error::Protocol(_) | Error::UnexpectedMessage(_) => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
            other => io::Error::other(other),
        }
    }
}

/// What an IdentificationRejected message names: the parameters it rejects, and the message identifiers in the
/// rejected lists of messages sent (parameter 6) and received (parameter 7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rejection {
    pub parameters: Vec<u16>,
    pub unsupported_sent: Vec<u16>,
    pub unsupported_received: Vec<u16>,
    /// Whether a message list was empty or of odd length, so that its identifiers cannot be trusted.
    pub malformed: bool,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |ids: &[u16]| {
            ids.iter()
                .map(|id| format!("0x{id:04x}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        write!(
            f,
            "parameters [{}], unsupported sent [{}], received [{}]{}",
            self.parameters
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            list(&self.unsupported_sent),
            list(&self.unsupported_received),
            if self.malformed { ", malformed" } else { "" }
        )
    }
}
