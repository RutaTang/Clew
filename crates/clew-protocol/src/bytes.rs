//! Byte payloads on the wire: standard base64 (with padding) in a JSON string.
//!
//! serde's own encoding of a `Vec<u8>` is a JSON array of numbers — three to
//! four bytes of JSON per payload byte, each one parsed as a number on the way
//! in. The byte payloads here are the bulk of the traffic: a proxied language
//! server's stdout, a debug adapter's stream, a notebook's embedded images.
//! They all share the one ordered stdio stream with every other reply, so their
//! size is everyone's latency. Base64 costs 4/3 and decodes in one pass.
//!
//! Used as `#[serde(with = "crate::bytes")]` on a `Vec<u8>` field.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::de::{self, Visitor};
use serde::{Deserializer, Serializer};

pub(crate) fn serialize<S, T>(bytes: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: AsRef<[u8]>,
{
    serializer.serialize_str(&STANDARD.encode(bytes.as_ref()))
}

pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_str(Base64Visitor)
}

/// Decodes straight from the frame's borrowed text: no intermediate `String`.
struct Base64Visitor;

impl Visitor<'_> for Base64Visitor {
    type Value = Vec<u8>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a base64-encoded byte string")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Vec<u8>, E> {
        STANDARD
            .decode(text)
            .map_err(|e| E::custom(format!("invalid base64 payload: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Frame {
        #[serde(with = "crate::bytes")]
        data: Vec<u8>,
    }

    #[test]
    fn bytes_cross_as_a_base64_string_and_come_back_whole() {
        let frame = Frame {
            data: vec![0, 1, 2, 0xfe, 0xff, b'\n', b'"'],
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert_eq!(json, r#"{"data":"AAEC/v8KIg=="}"#);
        assert_eq!(serde_json::from_str::<Frame>(&json).unwrap(), frame);
        // Empty stays empty (and is not `null`).
        let empty = serde_json::to_string(&Frame { data: vec![] }).unwrap();
        assert_eq!(empty, r#"{"data":""}"#);
        assert!(
            serde_json::from_str::<Frame>(&empty)
                .unwrap()
                .data
                .is_empty()
        );
    }

    #[test]
    fn malformed_base64_is_a_decode_error_not_empty_bytes() {
        let err = serde_json::from_str::<Frame>(r#"{"data":"not base64!"}"#).unwrap_err();
        assert!(err.to_string().contains("invalid base64"), "{err}");
        // The old array-of-numbers shape is refused outright, not misread.
        assert!(serde_json::from_str::<Frame>(r#"{"data":[1,2,3]}"#).is_err());
    }
}
