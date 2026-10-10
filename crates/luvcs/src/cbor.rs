//! Deterministic CBOR (RFC 8949 §4.2.1) for every content-addressed value.
//!
//! The value model is deliberately small: it is what the VCS contract needs,
//! not a general CBOR library. Encoding is canonical by construction (shortest
//! integer heads, map keys sorted by their encoded bytes, no indefinite
//! lengths, no floats, no tags). Decoding is strict: a byte string that is not
//! the canonical encoding of its value is rejected, so a logical ID always
//! names exactly one byte sequence and vice versa.

use std::collections::BTreeMap;

/// A canonical CBOR value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Non-negative integer (major type 0).
    Uint(u64),
    /// Negative integer `-1 - n` (major type 1). The payload is `n`.
    Nint(u64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Keys are kept sorted by their canonical encoding at encode time.
    Map(Vec<(Value, Value)>),
}

impl Value {
    pub fn int(v: i64) -> Value {
        if v >= 0 {
            Value::Uint(v as u64)
        } else {
            Value::Nint((-(v + 1)) as u64)
        }
    }

    pub fn text(s: impl Into<String>) -> Value {
        Value::Text(s.into())
    }

    pub fn bytes(b: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(b.into())
    }

    /// A map built from text keys. Keys are sorted at encode time.
    pub fn map<I, K>(entries: I) -> Value
    where
        I: IntoIterator<Item = (K, Value)>,
        K: Into<String>,
    {
        Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (Value::Text(k.into()), v))
                .collect(),
        )
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::Uint(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Uint(v) => i64::try_from(*v).ok(),
            Value::Nint(n) => i64::try_from(*n).ok().map(|n| -1 - n),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Looks a text key up in a map value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries
                .iter()
                .find(|(k, _)| k.as_text() == Some(key))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// The map entries as a sorted key→value view (text keys only).
    pub fn map_entries(&self) -> Option<BTreeMap<&str, &Value>> {
        match self {
            Value::Map(entries) => entries
                .iter()
                .map(|(k, v)| k.as_text().map(|k| (k, v)))
                .collect(),
            _ => None,
        }
    }
}

/// Why a byte string is not canonical CBOR of the supported subset.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CborError {
    #[error("unexpected end of input at byte {0}")]
    Truncated(usize),
    #[error("trailing bytes after value at byte {0}")]
    Trailing(usize),
    #[error("unsupported major type or simple value at byte {0}")]
    Unsupported(usize),
    #[error("integer head is not the shortest encoding at byte {0}")]
    NonMinimalHead(usize),
    #[error("indefinite length is not allowed at byte {0}")]
    Indefinite(usize),
    #[error("invalid UTF-8 in text string at byte {0}")]
    InvalidUtf8(usize),
    #[error("map keys are not in canonical order at byte {0}")]
    KeyOrder(usize),
    #[error("duplicate map key at byte {0}")]
    DuplicateKey(usize),
    #[error("nesting deeper than {0} levels")]
    TooDeep(usize),
    #[error("length {0} exceeds the limit {1}")]
    TooLong(u64, usize),
}

/// Structural depth limit: nesting deeper than this is refused.
pub const MAX_DEPTH: usize = 64;
/// Any single byte / text string, array or map may not exceed this many
/// elements or bytes while decoding. Larger payloads live in Blobs.
pub const MAX_LEN: usize = 16 * 1024 * 1024;

/// Encodes a value canonically.
pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(value, &mut out);
    out
}

fn head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= 0xff {
        out.push(m | 24);
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xffff_ffff {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn write(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Uint(n) => head(0, *n, out),
        Value::Nint(n) => head(1, *n, out),
        Value::Bytes(b) => {
            head(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        Value::Text(s) => {
            head(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(items) => {
            head(4, items.len() as u64, out);
            for item in items {
                write(item, out);
            }
        }
        Value::Map(entries) => {
            let mut encoded: Vec<(Vec<u8>, Vec<u8>)> = entries
                .iter()
                .map(|(k, v)| (encode(k), encode(v)))
                .collect();
            encoded.sort_by(|a, b| a.0.cmp(&b.0));
            encoded.dedup_by(|a, b| a.0 == b.0);
            head(5, encoded.len() as u64, out);
            for (k, v) in encoded {
                out.extend_from_slice(&k);
                out.extend_from_slice(&v);
            }
        }
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Null => out.push(0xf6),
    }
}

/// Decodes exactly one canonical value; anything else is an error.
pub fn decode(bytes: &[u8]) -> Result<Value, CborError> {
    let mut d = Decoder { bytes, pos: 0 };
    let v = d.value(0)?;
    if d.pos != bytes.len() {
        return Err(CborError::Trailing(d.pos));
    }
    Ok(v)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Decoder<'_> {
    fn byte(&mut self) -> Result<u8, CborError> {
        let b = *self
            .bytes
            .get(self.pos)
            .ok_or(CborError::Truncated(self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&[u8], CborError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(CborError::Truncated(self.pos))?;
        if end > self.bytes.len() {
            return Err(CborError::Truncated(self.pos));
        }
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn arg(&mut self, info: u8, at: usize) -> Result<u64, CborError> {
        let n = match info {
            0..=23 => u64::from(info),
            24 => {
                let v = u64::from(self.byte()?);
                if v < 24 {
                    return Err(CborError::NonMinimalHead(at));
                }
                v
            }
            25 => {
                let v = u64::from(u16::from_be_bytes(self.take(2)?.try_into().unwrap()));
                if v <= 0xff {
                    return Err(CborError::NonMinimalHead(at));
                }
                v
            }
            26 => {
                let v = u64::from(u32::from_be_bytes(self.take(4)?.try_into().unwrap()));
                if v <= 0xffff {
                    return Err(CborError::NonMinimalHead(at));
                }
                v
            }
            27 => {
                let v = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                if v <= 0xffff_ffff {
                    return Err(CborError::NonMinimalHead(at));
                }
                v
            }
            31 => return Err(CborError::Indefinite(at)),
            _ => return Err(CborError::Unsupported(at)),
        };
        Ok(n)
    }

    fn len(&mut self, info: u8, at: usize) -> Result<usize, CborError> {
        let n = self.arg(info, at)?;
        if n > MAX_LEN as u64 {
            return Err(CborError::TooLong(n, MAX_LEN));
        }
        Ok(n as usize)
    }

    fn value(&mut self, depth: usize) -> Result<Value, CborError> {
        if depth > MAX_DEPTH {
            return Err(CborError::TooDeep(MAX_DEPTH));
        }
        let at = self.pos;
        let b = self.byte()?;
        let major = b >> 5;
        let info = b & 0x1f;
        match major {
            0 => Ok(Value::Uint(self.arg(info, at)?)),
            1 => Ok(Value::Nint(self.arg(info, at)?)),
            2 => {
                let n = self.len(info, at)?;
                Ok(Value::Bytes(self.take(n)?.to_vec()))
            }
            3 => {
                let n = self.len(info, at)?;
                let s = self.take(n)?;
                let s = std::str::from_utf8(s).map_err(|_| CborError::InvalidUtf8(at))?;
                Ok(Value::Text(s.to_owned()))
            }
            4 => {
                let n = self.len(info, at)?;
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.value(depth + 1)?);
                }
                Ok(Value::Array(items))
            }
            5 => {
                let n = self.len(info, at)?;
                let mut entries: Vec<(Value, Value)> = Vec::with_capacity(n.min(1024));
                let mut prev_key: Option<&[u8]> = None;
                for _ in 0..n {
                    let key_start = self.pos;
                    let key = self.value(depth + 1)?;
                    let key_bytes = &self.bytes[key_start..self.pos];
                    if let Some(prev) = prev_key {
                        match prev.cmp(key_bytes) {
                            std::cmp::Ordering::Less => {}
                            std::cmp::Ordering::Equal => {
                                return Err(CborError::DuplicateKey(key_start));
                            }
                            std::cmp::Ordering::Greater => {
                                return Err(CborError::KeyOrder(key_start));
                            }
                        }
                    }
                    prev_key = Some(key_bytes);
                    let value = self.value(depth + 1)?;
                    entries.push((key, value));
                }
                Ok(Value::Map(entries))
            }
            7 => match info {
                20 => Ok(Value::Bool(false)),
                21 => Ok(Value::Bool(true)),
                22 => Ok(Value::Null),
                _ => Err(CborError::Unsupported(at)),
            },
            _ => Err(CborError::Unsupported(at)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(v: &Value) -> String {
        ::hex::encode(encode(v))
    }

    #[test]
    fn rfc_8949_examples() {
        assert_eq!(hex(&Value::Uint(0)), "00");
        assert_eq!(hex(&Value::Uint(23)), "17");
        assert_eq!(hex(&Value::Uint(24)), "1818");
        assert_eq!(hex(&Value::Uint(1000)), "1903e8");
        assert_eq!(hex(&Value::Uint(1_000_000)), "1a000f4240");
        assert_eq!(hex(&Value::Uint(1_000_000_000_000)), "1b000000e8d4a51000");
        assert_eq!(hex(&Value::int(-1)), "20");
        assert_eq!(hex(&Value::int(-1000)), "3903e7");
        assert_eq!(hex(&Value::Bool(true)), "f5");
        assert_eq!(hex(&Value::Null), "f6");
        assert_eq!(hex(&Value::text("IETF")), "6449455446");
        assert_eq!(hex(&Value::bytes(vec![1, 2, 3, 4])), "4401020304");
        assert_eq!(
            hex(&Value::Array(vec![Value::Uint(1), Value::Uint(2)])),
            "820102"
        );
    }

    #[test]
    fn map_keys_sort_by_encoded_bytes_shorter_first() {
        // RFC 8949 §4.2.1: length-first ordering falls out of byte ordering
        // because the head carries the length.
        let v = Value::map([("aa", Value::Uint(1)), ("b", Value::Uint(2))]);
        assert_eq!(hex(&v), "a261620262616101");
        let v2 = Value::map([("b", Value::Uint(2)), ("aa", Value::Uint(1))]);
        assert_eq!(encode(&v), encode(&v2));
    }

    #[test]
    fn roundtrip_nested() {
        let v = Value::map([
            ("n", Value::Null),
            ("neg", Value::int(-5)),
            (
                "arr",
                Value::Array(vec![Value::text("x"), Value::bytes(vec![0])]),
            ),
            ("m", Value::map([("k", Value::Bool(false))])),
        ]);
        let bytes = encode(&v);
        let back = decode(&bytes).unwrap();
        assert_eq!(encode(&back), bytes);
    }

    #[test]
    fn rejects_non_canonical_input() {
        assert_eq!(decode(&[0x18, 0x05]), Err(CborError::NonMinimalHead(0)));
        assert_eq!(
            decode(&[0x19, 0x00, 0x05]),
            Err(CborError::NonMinimalHead(0))
        );
        assert_eq!(decode(&[0x9f, 0xff]), Err(CborError::Indefinite(0)));
        assert_eq!(decode(&[0xf9, 0x00, 0x00]), Err(CborError::Unsupported(0)));
        assert_eq!(decode(&[0xc0, 0x00]), Err(CborError::Unsupported(0)));
        // map {"b":1,"a":2} with keys out of order
        assert_eq!(
            decode(&[0xa2, 0x61, 0x62, 0x01, 0x61, 0x61, 0x02]),
            Err(CborError::KeyOrder(4))
        );
        assert_eq!(
            decode(&[0xa2, 0x61, 0x61, 0x01, 0x61, 0x61, 0x02]),
            Err(CborError::DuplicateKey(4))
        );
        assert_eq!(decode(&[0x01, 0x02]), Err(CborError::Trailing(1)));
        assert_eq!(decode(&[0x62, 0x61]), Err(CborError::Truncated(1)));
        assert_eq!(decode(&[0x61, 0xff]), Err(CborError::InvalidUtf8(0)));
    }

    #[test]
    fn rejects_deep_nesting() {
        let mut bytes = vec![0x81u8; MAX_DEPTH + 2];
        bytes.push(0x00);
        assert_eq!(decode(&bytes), Err(CborError::TooDeep(MAX_DEPTH)));
    }
}
