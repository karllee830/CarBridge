// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Calls to ubus objects, through the `ubus` command.

use std::fmt;
use std::io;

use serde_json::Value;
use tokio::process::Command;

/// A failed ubus call.
#[derive(Debug)]
pub enum Error {
    /// The `ubus` command could not run.
    Io(io::Error),
    /// The call failed; `status` is ubus's exit status, for example 4 for an object that does not exist.
    Call {
        object: String,
        method: String,
        status: i32,
        message: String,
    },
    /// The reply is not JSON.
    Reply(serde_json::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "ubus: {err}"),
            Error::Call {
                object,
                method,
                status,
                message,
            } => {
                write!(
                    f,
                    "ubus call {object} {method} failed ({status}): {message}"
                )
            }
            Error::Reply(err) => write!(f, "ubus reply: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    /// Whether the object or the requested item does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Error::Call {
                status: 4 | 252,
                ..
            }
        )
    }
}

/// Calls `method` of `object` with `args`; returns the reply, or `Value::Null` when there is none.
pub async fn call(object: &str, method: &str, args: &Value) -> Result<Value, Error> {
    let output = Command::new("ubus")
        .args(["call", object, method, &args.to_string()])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(Error::Io)?;
    if !output.status.success() {
        return Err(Error::Call {
            object: object.to_owned(),
            method: method.to_owned(),
            status: output.status.code().unwrap_or(-1),
            message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    parse_reply(&output.stdout)
}

fn parse_reply(stdout: &[u8]) -> Result<Value, Error> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(stdout).map_err(Error::Reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies() {
        assert_eq!(parse_reply(b"").unwrap(), Value::Null);
        assert_eq!(
            parse_reply(b"{\n\t\"pid\": 3666\n}\n").unwrap()["pid"],
            3666
        );
        assert!(parse_reply(b"Command failed").is_err());
    }
}
