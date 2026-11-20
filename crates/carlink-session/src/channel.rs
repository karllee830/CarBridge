// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! One TCP connection carrying framed messages.

use std::time::{SystemTime, UNIX_EPOCH};

use carbridge_carlink_proto::frame::{self, Message};
use carbridge_carlink_proto::record::SessionKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::Error;

/// A connection with its read buffer, its key if it is encrypted, and the phone's sequence numbers on it.
pub struct Channel {
    stream: TcpStream,
    buf: Vec<u8>,
    key: Option<SessionKey>,
    sequence: u32,
}

impl Channel {
    pub fn new(stream: TcpStream, key: Option<SessionKey>) -> Channel {
        let _ = stream.set_nodelay(true);
        Channel {
            stream,
            buf: Vec::with_capacity(4096),
            key,
            sequence: 0,
        }
    }

    /// Receives the next message. Cancel-safe: a message partly read stays buffered for the next call.
    pub async fn recv(&mut self) -> Result<Message, Error> {
        loop {
            if let Some((message, used)) = frame::decode(&self.buf, self.key.as_ref())? {
                self.buf.drain(..used);
                return Ok(message);
            }
            let mut chunk = [0u8; 4096];
            // The head unit closes its sockets with a zero linger time, which resets the connection.
            let n = match self.stream.read(&mut chunk).await {
                Ok(0) => return Err(Error::Closed),
                Ok(n) => n,
                Err(err) if is_closed(&err) => return Err(Error::Closed),
                Err(err) => return Err(err.into()),
            };
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Sends a new message, numbered with the phone's next sequence number and stamped with the time. Returns the
    /// sequence number.
    pub async fn send(&mut self, mut message: Message) -> Result<u32, Error> {
        self.sequence = self.sequence.wrapping_add(1).max(1);
        message.sequence = self.sequence;
        message.time = now();
        self.write(&message).await?;
        Ok(message.sequence)
    }

    /// Sends a response or an ack, which keeps the sequence number of what it answers.
    pub async fn reply(&mut self, message: Message) -> Result<(), Error> {
        self.write(&message).await
    }

    async fn write(&mut self, message: &Message) -> Result<(), Error> {
        let bytes = message.encode(self.key.as_ref())?;
        self.stream.write_all(&bytes).await?;
        Ok(())
    }

    pub fn into_stream(self) -> TcpStream {
        self.stream
    }

    /// The connection the channel runs on.
    pub fn socket(&self) -> &TcpStream {
        &self.stream
    }
}

fn is_closed(err: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset};
    matches!(err.kind(), ConnectionReset | ConnectionAborted | BrokenPipe)
}

fn now() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32)
}

/// Milliseconds since the Unix epoch, as heartbeats carry them.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
