// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The input back channel: touches and keys from the head unit, one JSON object per encrypted record.

use carbridge_carlink_proto::record::{Record, SessionKey};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

/// One pointer of a touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub id: i64,
    /// In the phone's picture, already scaled by the head unit.
    pub x: i64,
    pub y: i64,
}

/// An input event.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// `action` is Android's `MotionEvent` action: 0 down, 1 up, 2 move, 3 cancel, 5 and 6 a further pointer down or
    /// up with its index in bits 8 and up.
    Touch {
        action: i64,
        width: i64,
        height: i64,
        pointers: Vec<Pointer>,
    },
    /// `action` 0 down, 1 up, 2 press; an Android or Carlink key code.
    Key {
        action: i64,
        keycode: i64,
        meta_state: i64,
    },
    /// An object of another type.
    Other(Value),
}

impl Input {
    pub fn parse(json: &[u8]) -> Option<Input> {
        let value: Value = serde_json::from_slice(json).ok()?;
        let int = |name: &str| value.get(name).and_then(Value::as_i64);
        Some(match int("type") {
            Some(1) => {
                let count = int("count").unwrap_or(0).clamp(0, 10);
                let pointers = (0..count)
                    .filter_map(|n| {
                        Some(Pointer {
                            id: int(&format!("trackID{n}")).unwrap_or(n),
                            x: int(&format!("x{n}"))?,
                            y: int(&format!("y{n}"))?,
                        })
                    })
                    .collect();
                Input::Touch {
                    action: int("action")?,
                    width: int("width").unwrap_or(0),
                    height: int("height").unwrap_or(0),
                    pointers,
                }
            }
            Some(2) => Input::Key {
                action: int("action")?,
                keycode: int("keycode")?,
                meta_state: int("metaState").unwrap_or(0),
            },
            _ => Input::Other(value),
        })
    }
}

/// Reads input events from the head unit's connection to the phone's UIBC port.
pub struct UibcReader {
    stream: TcpStream,
    key: SessionKey,
    buf: Vec<u8>,
}

impl UibcReader {
    pub fn new(stream: TcpStream, key: SessionKey) -> UibcReader {
        UibcReader {
            stream,
            key,
            buf: Vec::new(),
        }
    }

    /// The next event; `None` when the head unit closed the connection. Records that do not decrypt or parse are
    /// skipped.
    pub async fn next(&mut self) -> std::io::Result<Option<Input>> {
        loop {
            match Record::split(&self.buf) {
                Ok(Some((record, used))) => {
                    let plain = self.key.open_record(&record);
                    self.buf.drain(..used);
                    if let Some(input) = plain.ok().as_deref().and_then(Input::parse) {
                        return Ok(Some(input));
                    }
                    continue;
                }
                Ok(None) => {}
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "malformed UIBC record",
                    ))
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch() {
        let json = br#"{"type":1,"action":261,"width":2458,"height":1484,"count":2,"trackID0":0,"x0":120,"y0":340,"trackID1":1,"x1":800,"y1":360}"#;
        let Some(Input::Touch {
            action,
            width,
            pointers,
            ..
        }) = Input::parse(json)
        else {
            panic!("touch")
        };
        assert_eq!((action, width), (0x105, 2458));
        assert_eq!(
            pointers,
            [
                Pointer {
                    id: 0,
                    x: 120,
                    y: 340
                },
                Pointer {
                    id: 1,
                    x: 800,
                    y: 360
                }
            ]
        );
    }

    #[test]
    fn key() {
        let input = Input::parse(br#"{"type":2,"action":0,"keycode":4,"metaState":0}"#);
        assert_eq!(
            input,
            Some(Input::Key {
                action: 0,
                keycode: 4,
                meta_state: 0
            })
        );
    }

    #[tokio::test]
    async fn encrypted_records_over_tcp() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let key = SessionKey::new(&[3; 16]);
        let sender_key = key.clone();
        let sender = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stream = TcpStream::connect(addr).await.unwrap();
            let mut bytes = sender_key.seal(br#"{"type":2,"action":0,"keycode":66,"metaState":0}"#);
            bytes.extend(sender_key.seal(br#"{"type":1,"action":0,"width":10,"height":10,"count":1,"trackID0":0,"x0":1,"y0":2}"#));
            // Two writes that split the second record.
            stream.write_all(&bytes[..bytes.len() - 5]).await.unwrap();
            stream.write_all(&bytes[bytes.len() - 5..]).await.unwrap();
        });
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = UibcReader::new(stream, key);
        assert_eq!(
            reader.next().await.unwrap(),
            Some(Input::Key {
                action: 0,
                keycode: 66,
                meta_state: 0
            })
        );
        assert!(matches!(
            reader.next().await.unwrap(),
            Some(Input::Touch { .. })
        ));
        sender.await.unwrap();
        assert_eq!(reader.next().await.unwrap(), None);
    }
}
