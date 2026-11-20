// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/RtspMessage.kt (DiPlay
// commit 615b32c).

//! The request framing of the control channel: a request line, headers and an optional body, as RTSP and HTTP
//! share it. Once pair-verify completes, [`crate::control_cipher`] wraps this framing.
//!
//! A peer must not make the receiver buffer without limit, so a header block or a body above its cap is an
//! [`Error`] that should close the connection.

use std::fmt;
use std::io;

/// Real requests carry headers under 1 KiB.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Above the largest real body, a corner mask picture in a plist (about 576 KiB).
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

const HEADER_END: &[u8] = b"\r\n\r\n";

/// A request or response that exceeds its cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    HeaderTooLong,
    BodyTooLong(u64),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::HeaderTooLong => write!(f, "RTSP header over {MAX_HEADER_BYTES} bytes"),
            Error::BodyTooLong(length) => {
                write!(f, "RTSP body of {length} bytes over {MAX_BODY_BYTES}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<Error> for io::Error {
    fn from(error: Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }
}

/// Header fields, with names in lower case. A repeated name keeps its last value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers {
    fields: Vec<(String, String)>,
}

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn insert(&mut self, name: &str, value: impl Into<String>) {
        let name = name.to_ascii_lowercase();
        let value = value.into();
        match self.fields.iter_mut().find(|(field, _)| *field == name) {
            Some((_, slot)) => *slot = value,
            None => self.fields.push((name, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }
}

/// A request from the controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// `RTSP/1.0` or `HTTP/1.1`; `RTSP/1.0` when the request line has none.
    pub protocol: String,
    pub headers: Headers,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Takes every complete request from the start of `buffer`, leaving a partial one for later.
pub fn take_requests(buffer: &mut Vec<u8>) -> Result<Vec<Request>, Error> {
    let mut requests = Vec::new();
    let mut offset = 0;
    while offset < buffer.len() {
        let rest = &buffer[offset..];
        // A header block ends within the cap or not at all.
        let window = &rest[..rest.len().min(MAX_HEADER_BYTES + HEADER_END.len())];
        let Some(header_len) = find(window, HEADER_END).filter(|&len| len <= MAX_HEADER_BYTES)
        else {
            if rest.len() > MAX_HEADER_BYTES {
                return Err(Error::HeaderTooLong);
            }
            break;
        };
        let text = String::from_utf8_lossy(&rest[..header_len]);
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next().unwrap_or_default().splitn(3, ' ');
        let method = request_line.next().unwrap_or_default().to_owned();
        let path = request_line.next().unwrap_or_default().to_owned();
        let protocol = request_line.next().unwrap_or("RTSP/1.0").to_owned();
        let mut headers = Headers::default();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim(), value.trim());
            }
        }
        let length = content_length(&headers)?;
        let body_start = header_len + HEADER_END.len();
        if rest.len() - body_start < length {
            break;
        }
        requests.push(Request {
            method,
            path,
            protocol,
            headers,
            body: rest[body_start..body_start + length].to_vec(),
        });
        offset += body_start + length;
    }
    buffer.drain(..offset);
    Ok(requests)
}

/// The declared body length: none, or one that does not read as a number, is 0, and a negative one too.
fn content_length(headers: &Headers) -> Result<usize, Error> {
    let Some(value) = headers.get("content-length") else {
        return Ok(0);
    };
    let value = value.trim();
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(0);
    }
    match digits.parse::<u64>() {
        Ok(length) if length <= MAX_BODY_BYTES as u64 => Ok(length as usize),
        Ok(length) => Err(Error::BodyTooLong(length)),
        Err(_) => Err(Error::BodyTooLong(u64::MAX)),
    }
}

/// The reason phrase of the statuses the receiver answers with.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        453 => "Not Enough Bandwidth",
        455 => "Method Not Valid in This State",
        470 => "Connection Authorization Required",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "OK",
    }
}

/// A response to a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// The reason phrase; the usual one for the status when `None`.
    pub reason: Option<String>,
    /// Sent in order, before `CSeq` and `Content-Length`, which [`Response::encode`] adds.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16) -> Response {
        Response {
            status,
            reason: None,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn ok() -> Response {
        Response::new(200)
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, content_type: &str, body: Vec<u8>) -> Response {
        self.headers
            .push(("Content-Type".to_owned(), content_type.to_owned()));
        self.body = body;
        self
    }

    /// The response to `request` in its protocol, echoing its `CSeq`.
    pub fn encode(&self, request: &Request) -> Vec<u8> {
        let reason = self.reason.as_deref().unwrap_or(reason(self.status));
        let mut head = format!("{} {} {}\r\n", request.protocol, self.status, reason);
        for (name, value) in &self.headers {
            if name.eq_ignore_ascii_case("cseq") || name.eq_ignore_ascii_case("content-length") {
                continue;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(sequence) = request.header("cseq") {
            head.push_str(&format!("CSeq: {sequence}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(length: impl fmt::Display, body: &[u8]) -> Vec<u8> {
        let mut bytes =
            format!("POST /x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {length}\r\n\r\n").into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn a_huge_content_length_is_rejected() {
        assert_eq!(
            take_requests(&mut request(1_500_000_000, &[])),
            Err(Error::BodyTooLong(1_500_000_000))
        );
        assert!(take_requests(&mut request("99999999999999999999999", &[])).is_err());
    }

    #[test]
    fn a_header_without_end_is_rejected() {
        let mut buffer = b"GET /info RTSP/1.0\r\nX: ".to_vec();
        buffer.resize(buffer.len() + 64 * 1024, b'a');
        assert_eq!(take_requests(&mut buffer), Err(Error::HeaderTooLong));
    }

    #[test]
    fn real_sized_messages_are_kept() {
        let setup_body: Vec<u8> = (0..756).map(|i| i as u8).collect();
        let mask: Vec<u8> = (0..512 * 1024 + 64 * 1024).map(|i| i as u8).collect();
        let mut buffer = request(756, &setup_body);
        buffer.extend(request(mask.len(), &mask));
        buffer.extend_from_slice(b"GET /in");
        let requests = take_requests(&mut buffer).unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, setup_body);
        assert_eq!(requests[1].body, mask);
        assert_eq!(buffer, b"GET /in");
    }

    #[test]
    fn a_partial_body_waits() {
        let mut buffer = request(756, &[0; 100]);
        assert!(take_requests(&mut buffer).unwrap().is_empty());
        assert_eq!(buffer.len(), request(756, &[0; 100]).len());
    }

    #[test]
    fn request_line_and_headers() {
        let mut buffer =
            b"GET /info RTSP/1.0\r\nCSeq: 3\r\nX-Apple-ProtocolVersion:  1 \r\nbroken line\r\ncseq: 4\r\n\r\n"
                .to_vec();
        buffer.extend_from_slice(b"OPTIONS *\r\n\r\n");
        let requests = take_requests(&mut buffer).unwrap();
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/info");
        assert_eq!(requests[0].protocol, "RTSP/1.0");
        assert_eq!(requests[0].header("CSEQ"), Some("4"));
        assert_eq!(requests[0].header("x-apple-protocolversion"), Some("1"));
        assert!(requests[0].body.is_empty());
        assert_eq!(requests[1].method, "OPTIONS");
        assert_eq!(requests[1].protocol, "RTSP/1.0");
        assert!(buffer.is_empty());
    }

    #[test]
    fn odd_content_lengths_mean_no_body() {
        for length in ["-5", "abc", ""] {
            let mut buffer = request(length, b"");
            let requests = take_requests(&mut buffer).unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].body.is_empty());
        }
    }

    #[test]
    fn responses_echo_protocol_and_sequence() {
        let mut buffer = b"POST /pair-verify HTTP/1.1\r\nCSeq: 7\r\n\r\n".to_vec();
        let request = take_requests(&mut buffer).unwrap().remove(0);
        let response = Response::ok()
            .header("Server", "AirTunes/366.0")
            .header("CSeq", "99")
            .body("application/octet-stream", vec![1, 2, 3])
            .encode(&request);
        assert_eq!(
            response,
            b"HTTP/1.1 200 OK\r\nServer: AirTunes/366.0\r\nContent-Type: application/octet-stream\r\nCSeq: 7\r\n\
Content-Length: 3\r\n\r\n\x01\x02\x03"
        );
        let not_found = Response::new(404).encode(&request);
        assert!(not_found
            .starts_with(b"HTTP/1.1 404 Not Found\r\nCSeq: 7\r\nContent-Length: 0\r\n\r\n"));
    }
}
