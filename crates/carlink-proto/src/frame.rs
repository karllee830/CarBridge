// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! Whole messages: a header and a body, the body encrypted on encrypted channels.

use crate::command;
use crate::header::{BodyFormat, Category, Header, Kind, Source, HEADER_LEN};
use crate::record::SessionKey;
use crate::Error;

/// Largest message, header included, that [`decode`] accepts and [`Message::encode`] produces.
pub const MAX_MESSAGE_LEN: u32 = 16 << 20;

/// A message with its body in plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub sequence: u32,
    /// Unix time in seconds.
    pub time: u32,
    pub source: Source,
    pub format: BodyFormat,
    pub kind: Kind,
    pub category: Category,
    pub command: u16,
    pub reserved: u16,
    pub body: Vec<u8>,
}

impl Message {
    /// A message from the phone with sequence number and time 0, for the caller to fill in.
    pub fn new(
        category: Category,
        command: u16,
        kind: Kind,
        format: BodyFormat,
        body: Vec<u8>,
    ) -> Message {
        Message {
            sequence: 0,
            time: 0,
            source: Source::Phone,
            format,
            kind,
            category,
            command,
            reserved: 0,
            body,
        }
    }

    /// A message from the phone with a protobuf body.
    pub fn protobuf<M: prost::Message>(
        category: Category,
        command: u16,
        kind: Kind,
        body: &M,
    ) -> Message {
        Message::new(
            category,
            command,
            kind,
            BodyFormat::Protobuf,
            body.encode_to_vec(),
        )
    }

    /// Decodes the protobuf body.
    pub fn decode_body<M: prost::Message + Default>(&self) -> Result<M, Error> {
        if self.format != BodyFormat::Protobuf {
            return Err(Error::Protobuf(format!("body format is {:?}", self.format)));
        }
        Ok(M::decode(self.body.as_slice())?)
    }

    /// The phone's response to this request, with a protobuf body.
    pub fn response<M: prost::Message>(&self, command: u16, body: &M) -> Message {
        Message {
            sequence: self.sequence,
            time: self.time,
            reserved: self.reserved,
            ..Message::protobuf(self.category, command, Kind::Response, body)
        }
    }

    /// The phone's ack of this message, which the sender of a [`Kind::SendAck`] message waits for.
    pub fn ack(&self) -> Message {
        Message {
            sequence: self.sequence,
            time: self.time,
            reserved: self.reserved,
            ..Message::new(
                Category::ACK,
                command::ack::ACK,
                Kind::Response,
                BodyFormat::Raw,
                Vec::new(),
            )
        }
    }

    /// Whether the body is encrypted when sent with a session key: every body except an empty one or one in
    /// [`BodyFormat::Plain`].
    fn encrypts_body(&self) -> bool {
        self.format != BodyFormat::Plain && !self.body.is_empty()
    }

    /// Encodes the message. With `key`, the body is encrypted as one record.
    pub fn encode(&self, key: Option<&SessionKey>) -> Result<Vec<u8>, Error> {
        let mut out = vec![0u8; HEADER_LEN];
        match key {
            Some(key) if self.encrypts_body() => key.seal_into(&self.body, &mut out),
            _ => out.extend_from_slice(&self.body),
        }
        let length = u32::try_from(out.len())
            .ok()
            .filter(|&len| len <= MAX_MESSAGE_LEN);
        let length = length.ok_or(Error::Length(u32::MAX))?;
        let header = Header {
            length,
            sequence: self.sequence,
            time: self.time,
            source: self.source,
            format: self.format,
            kind: self.kind,
            category: self.category,
            command: self.command,
            reserved: self.reserved,
        };
        out[..HEADER_LEN].copy_from_slice(&header.encode());
        Ok(out)
    }
}

/// Decodes the message at the start of `buf`. With `key`, a body that is not in [`BodyFormat::Plain`] is
/// decrypted. Returns the message and the number of bytes it takes, or `None` when `buf` does not hold all of it
/// yet.
pub fn decode(buf: &[u8], key: Option<&SessionKey>) -> Result<Option<(Message, usize)>, Error> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let header = Header::decode(buf)?;
    if (header.length as usize) < HEADER_LEN || header.length > MAX_MESSAGE_LEN {
        return Err(Error::Length(header.length));
    }
    let len = header.length as usize;
    if buf.len() < len {
        return Ok(None);
    }
    let body = &buf[HEADER_LEN..len];
    let body = match key {
        Some(key) if header.format != BodyFormat::Plain && !body.is_empty() => key.open(body)?,
        _ => body.to_vec(),
    };
    let message = Message {
        sequence: header.sequence,
        time: header.time,
        source: header.source,
        format: header.format,
        kind: header.kind,
        category: header.category,
        command: header.command,
        reserved: header.reserved,
        body,
    };
    Ok(Some((message, len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pb;
    use crate::record::RECORD_OVERHEAD;

    fn heartbeat() -> Message {
        let mut message = Message::protobuf(
            Category::CONTROL,
            command::control::HEARTBEAT,
            Kind::Send,
            &pb::Heartbeat {
                timestamp: 1_700_000_000_000,
            },
        );
        message.sequence = 3;
        message.time = 1_700_000_000;
        message
    }

    #[test]
    fn plain_round_trip() {
        let message = heartbeat();
        let bytes = message.encode(None).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + message.body.len());
        assert_eq!(decode(&bytes, None).unwrap(), Some((message, bytes.len())));
    }

    #[test]
    fn encrypted_round_trip() {
        let key = SessionKey::new(&[7; 16]);
        let message = heartbeat();
        let bytes = message.encode(Some(&key)).unwrap();
        assert_eq!(
            bytes.len(),
            HEADER_LEN + message.body.len() + RECORD_OVERHEAD
        );
        assert_eq!(Header::decode(&bytes).unwrap().length as usize, bytes.len());
        let (decoded, used) = decode(&bytes, Some(&key)).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(decoded, message);
        assert_eq!(
            decoded.decode_body::<pb::Heartbeat>().unwrap().timestamp,
            1_700_000_000_000
        );
    }

    #[test]
    fn plain_format_is_never_encrypted() {
        let key = SessionKey::new(&[7; 16]);
        let message = Message::new(
            Category::AUDIO,
            1,
            Kind::Send,
            BodyFormat::Plain,
            vec![1, 2, 3],
        );
        let bytes = message.encode(Some(&key)).unwrap();
        assert_eq!(&bytes[HEADER_LEN..], &[1, 2, 3]);
        assert_eq!(decode(&bytes, Some(&key)).unwrap().unwrap().0, message);
    }

    #[test]
    fn empty_body_stays_empty() {
        let key = SessionKey::new(&[7; 16]);
        let ack = heartbeat().ack();
        let bytes = ack.encode(Some(&key)).unwrap();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(decode(&bytes, Some(&key)).unwrap().unwrap().0, ack);
    }

    #[test]
    fn ack_and_response_follow_request() {
        let mut request = heartbeat();
        request.kind = Kind::Request;
        request.reserved = 9;
        let ack = request.ack();
        assert_eq!(
            (ack.category, ack.command, ack.kind),
            (Category::ACK, 0, Kind::Response)
        );
        assert_eq!(
            (ack.sequence, ack.reserved, ack.format),
            (3, 9, BodyFormat::Raw)
        );
        let response = request.response(25, &pb::Heartbeat { timestamp: 1 });
        assert_eq!(
            (response.kind, response.sequence, response.reserved),
            (Kind::Response, 3, 9)
        );
    }

    #[test]
    fn waits_for_whole_message() {
        let bytes = heartbeat().encode(None).unwrap();
        for len in 0..bytes.len() {
            assert_eq!(decode(&bytes[..len], None), Ok(None));
        }
    }

    #[test]
    fn reads_messages_back_to_back() {
        let mut stream = heartbeat().encode(None).unwrap();
        let mut second = heartbeat();
        second.sequence = 4;
        stream.extend(second.encode(None).unwrap());
        let (first, used) = decode(&stream, None).unwrap().unwrap();
        assert_eq!(first.sequence, 3);
        let (next, _) = decode(&stream[used..], None).unwrap().unwrap();
        assert_eq!(next.sequence, 4);
    }

    #[test]
    fn rejects_bad_lengths() {
        for length in [0, HEADER_LEN as u32 - 1, MAX_MESSAGE_LEN + 1] {
            let header = Header {
                length,
                sequence: 1,
                time: 0,
                source: Source::HeadUnit,
                format: BodyFormat::Raw,
                kind: Kind::Send,
                category: Category::CONTROL,
                command: 1,
                reserved: 0,
            };
            assert_eq!(decode(&header.encode(), None), Err(Error::Length(length)));
        }
    }

    #[test]
    fn decode_body_needs_protobuf() {
        let message = Message::new(
            Category::CONTROL,
            1,
            Kind::Send,
            BodyFormat::Raw,
            vec![8, 1],
        );
        assert!(matches!(
            message.decode_body::<pb::Heartbeat>(),
            Err(Error::Protobuf(_))
        ));
    }
}
