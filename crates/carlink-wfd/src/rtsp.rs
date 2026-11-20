// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! RTSP messages, as Wi-Fi Display uses them: requests and responses with headers and an optional body whose length
//! `Content-Length` gives.

use std::fmt::Write as _;

/// A request or a response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Request {
        method: String,
        uri: String,
        headers: Headers,
        body: String,
    },
    Response {
        status: u16,
        reason: String,
        headers: Headers,
        body: String,
    },
}

/// Headers in order, looked up without regard to case.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn push(&mut self, name: &str, value: impl Into<String>) {
        self.0.push((name.to_owned(), value.into()));
    }

    pub fn cseq(&self) -> Option<u32> {
        self.get("CSeq")?.trim().parse().ok()
    }
}

impl Message {
    pub fn request(method: &str, uri: &str, cseq: u32) -> Message {
        let mut headers = Headers::default();
        headers.push("CSeq", cseq.to_string());
        Message::Request {
            method: method.to_owned(),
            uri: uri.to_owned(),
            headers,
            body: String::new(),
        }
    }

    /// `200 OK` to a request with sequence number `cseq`.
    pub fn ok(cseq: u32) -> Message {
        let mut headers = Headers::default();
        headers.push("CSeq", cseq.to_string());
        Message::Response {
            status: 200,
            reason: "OK".into(),
            headers,
            body: String::new(),
        }
    }

    pub fn headers(&self) -> &Headers {
        match self {
            Message::Request { headers, .. } | Message::Response { headers, .. } => headers,
        }
    }

    pub fn headers_mut(&mut self) -> &mut Headers {
        match self {
            Message::Request { headers, .. } | Message::Response { headers, .. } => headers,
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Message::Request { body, .. } | Message::Response { body, .. } => body,
        }
    }

    /// Sets a `text/parameters` body.
    pub fn with_parameters(mut self, text: String) -> Message {
        self.headers_mut().push("Content-Type", "text/parameters");
        match &mut self {
            Message::Request { body, .. } | Message::Response { body, .. } => *body = text,
        }
        self
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::new();
        let (headers, body) = match self {
            Message::Request {
                method,
                uri,
                headers,
                body,
            } => {
                let _ = write!(out, "{method} {uri} RTSP/1.0\r\n");
                (headers, body)
            }
            Message::Response {
                status,
                reason,
                headers,
                body,
            } => {
                let _ = write!(out, "RTSP/1.0 {status} {reason}\r\n");
                (headers, body)
            }
        };
        for (name, value) in &headers.0 {
            let _ = write!(out, "{name}: {value}\r\n");
        }
        if !body.is_empty() {
            let _ = write!(out, "Content-Length: {}\r\n", body.len());
        }
        out.push_str("\r\n");
        out.push_str(body);
        out.into_bytes()
    }

    /// Reads the message at the start of `buf`; returns it and its length, or `None` when `buf` does not hold all of
    /// it yet.
    pub fn parse(buf: &[u8]) -> Result<Option<(Message, usize)>, String> {
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            return if buf.len() > 64 * 1024 {
                Err("header too long".into())
            } else {
                Ok(None)
            };
        };
        let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| "header not UTF-8")?;
        let mut lines = head.split("\r\n");
        let start = lines.next().unwrap_or_default();
        // Content-Length is kept apart: encoding derives it from the body.
        let mut headers = Headers::default();
        let mut length = 0usize;
        for line in lines {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| format!("bad header line {line:?}"))?;
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                length = value.trim().parse().map_err(|_| "bad Content-Length")?;
            } else {
                headers.push(name.trim(), value.trim());
            }
        }
        let total = head_end + 4 + length;
        if buf.len() < total {
            return Ok(None);
        }
        let body = String::from_utf8_lossy(&buf[head_end + 4..total]).into_owned();
        let mut parts = start.splitn(3, ' ');
        let (a, b, c) = (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
        );
        let message = if a.starts_with("RTSP/") {
            let status = b
                .parse()
                .map_err(|_| format!("bad status line {start:?}"))?;
            Message::Response {
                status,
                reason: c.to_owned(),
                headers,
                body,
            }
        } else if c.starts_with("RTSP/") {
            Message::Request {
                method: a.to_owned(),
                uri: b.to_owned(),
                headers,
                body,
            }
        } else {
            return Err(format!("bad start line {start:?}"));
        };
        Ok(Some((message, total)))
    }
}

/// The `name: value` lines of a `text/parameters` body.
pub fn parameters(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_body() {
        let message = Message::request("SET_PARAMETER", "rtsp://localhost/wfd1.0", 4)
            .with_parameters("wfd_trigger_method: SETUP\r\n".into());
        let bytes = message.encode();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0\r\nCSeq: 4\r\n"));
        assert!(text.contains("Content-Length: 27\r\n"));
        assert_eq!(
            Message::parse(&bytes).unwrap(),
            Some((message, bytes.len()))
        );
    }

    #[test]
    fn partial_and_back_to_back() {
        let mut bytes = Message::ok(1).encode();
        let first = bytes.len();
        bytes.extend(b"OPTIONS * RTSP/1.0\r\ncseq: 2\r\nRequire: org.wfa.wfd1.0\r\n\r\n");
        for cut in 0..first {
            assert_eq!(Message::parse(&bytes[..cut]).unwrap(), None);
        }
        let (response, used) = Message::parse(&bytes).unwrap().unwrap();
        assert!(matches!(response, Message::Response { status: 200, .. }));
        let (request, _) = Message::parse(&bytes[used..]).unwrap().unwrap();
        assert_eq!(request.headers().cseq(), Some(2));
        assert!(matches!(request, Message::Request { ref method, .. } if method == "OPTIONS"));
    }

    #[test]
    fn parameter_lines() {
        let params = parameters("wfd_audio_latency: 400\r\nwfd_uibc_capability: input_category_list=GENERIC;port=none\r\n");
        assert_eq!(params[0], ("wfd_audio_latency".into(), "400".into()));
        assert_eq!(params[1].1, "input_category_list=GENERIC;port=none");
    }
}
