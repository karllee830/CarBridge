// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! The head unit's certificate data, sent once on the cert channel.
//!
//! The data is Base64 text of a signed JSON document:
//!
//! ```text
//! { "content": [...], "signature": [...], "version": 1, "cryptoConfigIndex": "...", "SignAlgorithm": "SHA256withRSA" }
//! ```
//!
//! `content` and `signature` are byte arrays written as JSON arrays of numbers, signed bytes from -128 to 127.
//! `content` is UTF-8 text of a second JSON document that names the head unit:
//!
//! ```text
//! { "short_name": "...", "product_name": "...", "vendor_name": "...", "vendor_id": "0028", "product_id": "..." }
//! ```
//!
//! Only `short_name`, `product_name` and `vendor_id` are read. The signature is not checked.

use base64ct::{Base64, Encoding};
use serde_json::{Map, Value};

/// What the head unit's certificate says about the head unit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HeadUnitIdentity {
    /// The head unit's short name, such as the car's model.
    pub short_name: Option<String>,
    /// The head unit's product name.
    pub product_name: Option<String>,
    /// The head unit maker's ID, as text: a number is written with at least four digits (`0028`).
    pub vendor_id: Option<String>,
}

impl HeadUnitIdentity {
    /// Reads the certificate data; `None` when it is not the signed document described above.
    pub fn parse(data: &[u8]) -> Option<HeadUnitIdentity> {
        let text = std::str::from_utf8(data)
            .ok()?
            .trim_matches(|c: char| c.is_whitespace() || c == '\0');
        let outer = object(&Base64::decode_vec(&without_whitespace(text)).ok()?)?;
        let content = bytes(outer.get("content")?)?;
        let fields = object(&content)?;
        Some(HeadUnitIdentity {
            short_name: fields.get("short_name").and_then(field),
            product_name: fields.get("product_name").and_then(field),
            vendor_id: fields.get("vendor_id").and_then(field),
        })
    }
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// The JSON object in UTF-8 `bytes`.
fn object(bytes: &[u8]) -> Option<Map<String, Value>> {
    match serde_json::from_slice(bytes).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// A byte array written as a JSON array of numbers, signed or not.
fn bytes(value: &Value) -> Option<Vec<u8>> {
    value
        .as_array()?
        .iter()
        .map(|n| match n.as_i64()? {
            n @ -128..=-1 => Some((n + 256) as u8),
            n @ 0..=255 => Some(n as u8),
            _ => None,
        })
        .collect()
}

/// A field's value as text: a string that is not blank, or a number.
fn field(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(match number.as_u64() {
            Some(n) => format!("{n:04}"),
            None => number.to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Certificate data as a head unit sends it, around `fields`.
    fn certificate(fields: &str) -> Vec<u8> {
        let content: Vec<String> = fields.bytes().map(|b| (b as i8).to_string()).collect();
        let outer = format!(
            r#"{{"content":[{}],"signature":[1,-2,3],"version":1,"cryptoConfigIndex":"x","SignAlgorithm":"SHA256withRSA"}}"#,
            content.join(",")
        );
        Base64::encode_string(outer.as_bytes()).into_bytes()
    }

    #[test]
    fn names_the_head_unit() {
        let data = certificate(
            r#"{"short_name":"零跑C10","product_name":"零跑C10 2024款","vendor_name":"零跑","vendor_id":"0028","product_id":"0001"}"#,
        );
        assert_eq!(
            HeadUnitIdentity::parse(&data),
            Some(HeadUnitIdentity {
                short_name: Some("零跑C10".into()),
                product_name: Some("零跑C10 2024款".into()),
                vendor_id: Some("0028".into()),
            })
        );
    }

    #[test]
    fn line_breaks_and_a_trailing_nul() {
        let mut data = certificate(r#"{"short_name":"C10"}"#);
        data.insert(8, b'\n');
        data.push(0);
        assert_eq!(
            HeadUnitIdentity::parse(&data)
                .unwrap()
                .short_name
                .as_deref(),
            Some("C10")
        );
    }

    #[test]
    fn missing_blank_and_numeric_fields() {
        let data = certificate(r#"{"short_name":"  ","vendor_id":28}"#);
        assert_eq!(
            HeadUnitIdentity::parse(&data),
            Some(HeadUnitIdentity {
                short_name: None,
                product_name: None,
                vendor_id: Some("0028".into()),
            })
        );
    }

    #[test]
    fn not_a_certificate() {
        assert_eq!(HeadUnitIdentity::parse(b""), None);
        assert_eq!(HeadUnitIdentity::parse(b"not base64!"), None);
        let no_content = Base64::encode_string(br#"{"version":1}"#);
        assert_eq!(HeadUnitIdentity::parse(no_content.as_bytes()), None);
        let bad_byte = Base64::encode_string(br#"{"content":[300]}"#);
        assert_eq!(HeadUnitIdentity::parse(bad_byte.as_bytes()), None);
        assert_eq!(HeadUnitIdentity::parse(&certificate("[1,2]")), None);
    }
}
