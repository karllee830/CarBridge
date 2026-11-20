// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Carlink session: authentication and the control, media, sensor, certificate and video channels.
//!
//! The phone authenticates on the head unit's auth port ([`auth::authenticate`]), listens on its own ports
//! ([`session::Listeners`]), confirms the authentication, and runs the session ([`session::run`]) until it ends.

use std::fmt;
use std::io;

use carbridge_carlink_proto::auth::AuthError;

pub mod auth;
pub mod channel;
pub mod display;
pub mod session;

/// Why authentication or a session failed.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A message that cannot be read.
    Proto(carbridge_carlink_proto::Error),
    /// The head unit did not accept the authentication.
    Auth(AuthError),
    /// The named step took too long.
    Timeout(&'static str),
    /// The connection closed.
    Closed,
    /// The head unit sent something out of place.
    Unexpected(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "{err}"),
            Error::Proto(err) => write!(f, "{err}"),
            Error::Auth(err) => write!(f, "authentication: {err}"),
            Error::Timeout(step) => write!(f, "{step} timed out"),
            Error::Closed => f.write_str("connection closed"),
            Error::Unexpected(what) => write!(f, "unexpected: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<carbridge_carlink_proto::Error> for Error {
    fn from(err: carbridge_carlink_proto::Error) -> Self {
        Error::Proto(err)
    }
}

impl From<AuthError> for Error {
    fn from(err: AuthError) -> Self {
        Error::Auth(err)
    }
}

#[cfg(test)]
mod tests;
