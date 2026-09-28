//! Byte payloads on the wire (AGT-1378): base64 in JSON, with the legacy
//! number-array form still accepted on read.
//!
//! serde's default for `Vec<u8>` is a sequence, which serde_json writes as
//! `[108,111,114,111,…]` — about 3.6 bytes of JSON per byte of payload.
//! The first live `pm backup` of the vault (12,531 ops) produced an 87 MB
//! `ops/agt.jsonl` that way, most of it `body.edit` Loro updates, on a
//! path toward GitHub's 100 MB per-file limit; the same bloat sat in the
//! SQLite `ops.payload` and `ticket_view.view` columns. Base64 is 1.33
//! bytes per byte, and a string is what every JSON consumer expects an
//! opaque blob to be.
//!
//! [`base64`] is a serde `with` module: `#[serde(with =
//! "pm_core::bytes::base64")]` on a `Vec<u8>` field writes the standard
//! (RFC 4648 §4, padded) alphabet and reads back **either** a base64
//! string or the legacy array of integers, so every op log, backup and
//! view row written before this change parses unchanged. The encoder and
//! decoder are hand-rolled (a few dozen lines) rather than a crate: the
//! wire format must never drift under a dependency bump, and pm-core
//! stays free of anything beyond serde.

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserializer, Serializer};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with `=` padding.
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Why a string is not base64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// A character outside the alphabet (or `=` somewhere other than the
    /// end), with its byte offset.
    InvalidChar { at: usize },
    /// The length (ignoring padding) leaves one dangling character, which
    /// can never encode a whole byte.
    InvalidLength,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::InvalidChar { at } => write!(f, "invalid base64 character at byte {at}"),
            DecodeError::InvalidLength => write!(f, "invalid base64 length"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn sextet(c: u8) -> Option<u32> {
    match c {
        b'A'..=b'Z' => Some((c - b'A') as u32),
        b'a'..=b'z' => Some((c - b'a') as u32 + 26),
        b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes standard base64. Padding is optional (`QQ` and `QQ==` are the
/// same byte) but, when present, must be at the end.
pub fn decode(text: &str) -> Result<Vec<u8>, DecodeError> {
    let bytes = text.as_bytes();
    let end = bytes.iter().rposition(|&c| c != b'=').map_or(0, |i| i + 1);
    if bytes.len() - end > 2 {
        return Err(DecodeError::InvalidChar { at: end });
    }
    let body = &bytes[..end];
    if body.len() % 4 == 1 {
        return Err(DecodeError::InvalidLength);
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    for (i, chunk) in body.chunks(4).enumerate() {
        let mut n: u32 = 0;
        for (j, &c) in chunk.iter().enumerate() {
            let v = sextet(c).ok_or(DecodeError::InvalidChar { at: i * 4 + j })?;
            n |= v << (18 - 6 * j);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// The serde `with` module: `#[serde(with = "pm_core::bytes::base64")]`.
pub mod base64 {
    use super::*;

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        deserializer.deserialize_any(BytesVisitor)
    }
}

/// Accepts a base64 string (the form written since AGT-1378), a sequence
/// of integers (the legacy `Vec<u8>` form) or raw bytes (a non-JSON
/// format).
struct BytesVisitor;

impl<'de> Visitor<'de> for BytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a base64 string or an array of bytes")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<u8>, E> {
        decode(v).map_err(E::custom)
    }

    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Vec<u8>, E> {
        Ok(v.to_vec())
    }

    fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(v)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
        let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(byte) = seq.next_element::<u8>()? {
            out.push(byte);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    /// RFC 4648 §10 test vectors.
    #[test]
    fn rfc_4648_vectors() {
        let cases: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (bytes, text) in cases {
            assert_eq!(encode(bytes), *text);
            assert_eq!(decode(text).unwrap(), bytes.to_vec());
        }
    }

    #[test]
    fn every_byte_value_round_trips() {
        let all: Vec<u8> = (0..=255).collect();
        for len in 0..all.len() {
            let sample = &all[..len];
            assert_eq!(decode(&encode(sample)).unwrap(), sample);
        }
        // The two alphabet characters `+` and `/` come out of high bits.
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn decode_tolerates_missing_padding_and_rejects_garbage() {
        assert_eq!(decode("Zg").unwrap(), b"f");
        assert_eq!(decode("Zm8").unwrap(), b"fo");
        assert_eq!(
            decode("Z").unwrap_err(),
            DecodeError::InvalidLength,
            "one dangling sextet"
        );
        assert_eq!(
            decode("Zm9v!A==").unwrap_err(),
            DecodeError::InvalidChar { at: 4 }
        );
        assert_eq!(
            decode("Zg==Zg==").unwrap_err(),
            DecodeError::InvalidChar { at: 2 },
            "padding must be terminal"
        );
        assert_eq!(
            decode("Zg===").unwrap_err(),
            DecodeError::InvalidChar { at: 2 },
            "at most two padding characters"
        );
        assert_eq!(
            decode("Zm9v @").unwrap_err(),
            DecodeError::InvalidChar { at: 4 }
        );
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Blob {
        #[serde(with = "base64")]
        data: Vec<u8>,
    }

    #[test]
    fn with_module_writes_a_string_and_reads_both_forms() {
        let blob = Blob {
            data: b"loro".to_vec(),
        };
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(json, r#"{"data":"bG9ybw=="}"#);
        assert_eq!(serde_json::from_str::<Blob>(&json).unwrap(), blob);

        // The form every op log, backup and view row was written in before
        // AGT-1378.
        let legacy: Blob = serde_json::from_str(r#"{"data":[108,111,114,111]}"#).unwrap();
        assert_eq!(legacy, blob);

        let empty: Blob = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert_eq!(empty.data, Vec::<u8>::new());
        assert_eq!(serde_json::to_string(&empty).unwrap(), r#"{"data":""}"#);
    }

    #[test]
    fn with_module_rejects_non_byte_arrays_and_bad_strings() {
        assert!(serde_json::from_str::<Blob>(r#"{"data":[256]}"#).is_err());
        assert!(serde_json::from_str::<Blob>(r#"{"data":[-1]}"#).is_err());
        assert!(serde_json::from_str::<Blob>(r#"{"data":"not base64!"}"#).is_err());
        assert!(serde_json::from_str::<Blob>(r#"{"data":42}"#).is_err());
    }
}
