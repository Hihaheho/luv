//! `record.v1`: structured values with field paths and stable element IDs,
//! and the schema that tells the merge driver how each field path merges.
//!
//! Encoding to canonical CBOR is unambiguous:
//! - `Object` → map with text keys; keys starting with `$` are reserved.
//! - `Decimal` → `{"$dec": "<digits>"}`.
//! - `Collection` → `{"$coll": [[id, value], ...]}` in the stored order.
//!
//! Missing (no key) and `Null` (key present, null value) are different.

use std::collections::BTreeMap;

use crate::cbor::{self, CborError, Value};

pub const RECORD_FORMAT_V1: u32 = 1;
pub const MAX_ELEMENT_ID_BYTES: usize = 256;
pub const MAX_FIELD_KEY_BYTES: usize = 256;

/// One element of a collection: a stable ID the application assigned and
/// the element's value. The driver never invents IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub id: String,
    pub value: RecordValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordValue {
    Null,
    Bool(bool),
    Int(i64),
    /// Arbitrary-precision decimal kept as its canonical string form.
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Object(BTreeMap<String, RecordValue>),
    /// Elements with stable IDs. Whether order matters is the schema's
    /// decision (`FieldPolicy::Set` vs `FieldPolicy::List`).
    Collection(Vec<Element>),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("{0}")]
    Cbor(#[from] CborError),
    #[error("record: {0}")]
    Invalid(String),
}

impl RecordValue {
    pub fn object<I, K>(entries: I) -> RecordValue
    where
        I: IntoIterator<Item = (K, RecordValue)>,
        K: Into<String>,
    {
        RecordValue::Object(entries.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn collection<I, K>(elements: I) -> RecordValue
    where
        I: IntoIterator<Item = (K, RecordValue)>,
        K: Into<String>,
    {
        RecordValue::Collection(
            elements
                .into_iter()
                .map(|(id, value)| Element {
                    id: id.into(),
                    value,
                })
                .collect(),
        )
    }

    pub fn text(s: impl Into<String>) -> RecordValue {
        RecordValue::Text(s.into())
    }

    pub fn as_object(&self) -> Option<&BTreeMap<String, RecordValue>> {
        match self {
            RecordValue::Object(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_collection(&self) -> Option<&[Element]> {
        match self {
            RecordValue::Collection(e) => Some(e),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            RecordValue::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Checks reserved keys, element ID uniqueness and length limits.
    pub fn validate(&self) -> Result<(), RecordError> {
        match self {
            RecordValue::Object(m) => {
                for (k, v) in m {
                    if k.is_empty() || k.starts_with('$') || k.contains('/') {
                        return Err(RecordError::Invalid(format!("invalid field key {k:?}")));
                    }
                    if k.len() > MAX_FIELD_KEY_BYTES {
                        return Err(RecordError::Invalid("field key too long".into()));
                    }
                    v.validate()?;
                }
                Ok(())
            }
            RecordValue::Collection(elements) => {
                let mut seen = std::collections::BTreeSet::new();
                for e in elements {
                    if e.id.is_empty() || e.id.len() > MAX_ELEMENT_ID_BYTES || e.id.contains('/') {
                        return Err(RecordError::Invalid(format!(
                            "invalid element id {:?}",
                            e.id
                        )));
                    }
                    if !seen.insert(&e.id) {
                        return Err(RecordError::Invalid(format!(
                            "duplicate element id {:?}",
                            e.id
                        )));
                    }
                    e.value.validate()?;
                }
                Ok(())
            }
            RecordValue::Decimal(d) => validate_decimal(d),
            _ => Ok(()),
        }
    }

    pub fn to_cbor(&self) -> Value {
        match self {
            RecordValue::Null => Value::Null,
            RecordValue::Bool(b) => Value::Bool(*b),
            RecordValue::Int(i) => Value::int(*i),
            RecordValue::Decimal(d) => Value::map([("$dec", Value::text(d.clone()))]),
            RecordValue::Text(s) => Value::text(s.clone()),
            RecordValue::Bytes(b) => Value::Bytes(b.clone()),
            RecordValue::Object(m) => Value::map(m.iter().map(|(k, v)| (k.clone(), v.to_cbor()))),
            RecordValue::Collection(elements) => Value::map([(
                "$coll",
                Value::Array(
                    elements
                        .iter()
                        .map(|e| Value::Array(vec![Value::text(e.id.clone()), e.value.to_cbor()]))
                        .collect(),
                ),
            )]),
        }
    }

    pub fn from_cbor(value: &Value) -> Result<RecordValue, RecordError> {
        let v = match value {
            Value::Null => RecordValue::Null,
            Value::Bool(b) => RecordValue::Bool(*b),
            Value::Uint(_) | Value::Nint(_) => RecordValue::Int(
                value
                    .as_i64()
                    .ok_or_else(|| RecordError::Invalid("integer out of i64 range".into()))?,
            ),
            Value::Text(s) => RecordValue::Text(s.clone()),
            Value::Bytes(b) => RecordValue::Bytes(b.clone()),
            Value::Array(_) => {
                return Err(RecordError::Invalid(
                    "bare arrays are not allowed; use a collection with element ids".into(),
                ));
            }
            Value::Map(_) => {
                let entries = value
                    .map_entries()
                    .ok_or_else(|| RecordError::Invalid("map with non-text key".into()))?;
                if let Some(dec) = entries.get("$dec") {
                    if entries.len() != 1 {
                        return Err(RecordError::Invalid("$dec with other keys".into()));
                    }
                    let d = dec
                        .as_text()
                        .ok_or_else(|| RecordError::Invalid("$dec is not text".into()))?;
                    RecordValue::Decimal(d.to_owned())
                } else if let Some(coll) = entries.get("$coll") {
                    if entries.len() != 1 {
                        return Err(RecordError::Invalid("$coll with other keys".into()));
                    }
                    let items = coll
                        .as_array()
                        .ok_or_else(|| RecordError::Invalid("$coll is not an array".into()))?;
                    let mut elements = Vec::with_capacity(items.len());
                    for item in items {
                        let pair = item.as_array().filter(|p| p.len() == 2).ok_or_else(|| {
                            RecordError::Invalid("collection element is not [id, value]".into())
                        })?;
                        let id = pair[0]
                            .as_text()
                            .ok_or_else(|| RecordError::Invalid("element id is not text".into()))?;
                        elements.push(Element {
                            id: id.to_owned(),
                            value: RecordValue::from_cbor(&pair[1])?,
                        });
                    }
                    RecordValue::Collection(elements)
                } else {
                    let mut m = BTreeMap::new();
                    for (k, v) in entries {
                        m.insert(k.to_owned(), RecordValue::from_cbor(v)?);
                    }
                    RecordValue::Object(m)
                }
            }
        };
        v.validate()?;
        Ok(v)
    }

    /// Canonical `record.v1` bytes of a top-level record (must be an object).
    pub fn encode(&self) -> Result<Vec<u8>, RecordError> {
        if self.as_object().is_none() {
            return Err(RecordError::Invalid(
                "top-level record must be an object".into(),
            ));
        }
        self.validate()?;
        Ok(cbor::encode(&self.to_cbor()))
    }

    pub fn decode(bytes: &[u8]) -> Result<RecordValue, RecordError> {
        let v = RecordValue::from_cbor(&cbor::decode(bytes)?)?;
        if v.as_object().is_none() {
            return Err(RecordError::Invalid(
                "top-level record must be an object".into(),
            ));
        }
        Ok(v)
    }

    /// Follows a field path (`a/b/<element id>/c`) through objects and
    /// collections.
    pub fn at(&self, path: &FieldPath) -> Option<&RecordValue> {
        let mut cur = self;
        for seg in &path.0 {
            cur = match cur {
                RecordValue::Object(m) => m.get(seg)?,
                RecordValue::Collection(elements) => &elements.iter().find(|e| &e.id == seg)?.value,
                _ => return None,
            };
        }
        Some(cur)
    }
}

fn validate_decimal(d: &str) -> Result<(), RecordError> {
    // Canonical form: optional '-', digits with no leading zero (except a
    // lone "0"), optional '.' followed by digits without trailing zero.
    let body = d.strip_prefix('-').unwrap_or(d);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let ok_int = !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && (int == "0" || !int.starts_with('0'));
    let ok_frac = frac.is_none_or(|f| {
        !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()) && !f.ends_with('0')
    });
    let ok_sign = !(d.starts_with('-') && int == "0" && frac.is_none());
    if ok_int && ok_frac && ok_sign {
        Ok(())
    } else {
        Err(RecordError::Invalid(format!("non-canonical decimal {d:?}")))
    }
}

/// A field path: object keys and element IDs, joined by `/` for display.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FieldPath(pub Vec<String>);

impl FieldPath {
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn parse(s: &str) -> Self {
        if s.is_empty() {
            return Self::root();
        }
        Self(s.split('/').map(str::to_owned).collect())
    }

    pub fn child(&self, seg: impl Into<String>) -> Self {
        let mut v = self.0.clone();
        v.push(seg.into());
        Self(v)
    }

    pub fn parent(&self) -> Option<Self> {
        let mut v = self.0.clone();
        v.pop().map(|_| Self(v))
    }

    pub fn is_prefix_of(&self, other: &FieldPath) -> bool {
        other.0.len() >= self.0.len() && other.0[..self.0.len()] == self.0[..]
    }
}

impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

/// How a field path merges. Declared by the schema; the driver never
/// guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldPolicy {
    /// Whole-value comparison. One side changed → take it; both changed to
    /// different values → conflict on this path. Objects recurse unless
    /// marked `Opaque`.
    #[default]
    Scalar,
    /// Collection whose order carries no meaning: element add / remove /
    /// modify per ID, adds union.
    Set,
    /// Collection whose order matters: elements merge per ID, order is taken
    /// from the side that changed it, both changed → order conflict.
    List,
    /// UTF-8 text merged by the text driver.
    Text,
    /// Compared as a whole, never recursed into.
    Opaque,
}

/// Merge policies by field path. A path with no entry inherits `Scalar`,
/// except that paths below a collection element are looked up relative to
/// the collection (`tags/*/label` matches every element).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordSchema {
    pub id: String,
    pub policies: BTreeMap<FieldPath, FieldPolicy>,
}

impl RecordSchema {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            policies: BTreeMap::new(),
        }
    }

    pub fn field(mut self, path: &str, policy: FieldPolicy) -> Self {
        self.policies.insert(FieldPath::parse(path), policy);
        self
    }

    /// Resolves the policy of a concrete path. Element IDs match the `*`
    /// segment of a schema path.
    pub fn policy_of(&self, path: &FieldPath) -> FieldPolicy {
        if let Some(p) = self.policies.get(path) {
            return *p;
        }
        'outer: for (pattern, policy) in &self.policies {
            if pattern.0.len() != path.0.len() {
                continue;
            }
            for (a, b) in pattern.0.iter().zip(&path.0) {
                if a != "*" && a != b {
                    continue 'outer;
                }
            }
            return *policy;
        }
        FieldPolicy::Scalar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note() -> RecordValue {
        RecordValue::object([
            ("title", RecordValue::text("A")),
            ("body", RecordValue::text("hello\n")),
            ("pinned", RecordValue::Bool(false)),
            ("score", RecordValue::Decimal("1.5".into())),
            ("n", RecordValue::Int(-3)),
            (
                "tags",
                RecordValue::collection([
                    (
                        "t1",
                        RecordValue::object([("label", RecordValue::text("x"))]),
                    ),
                    (
                        "t2",
                        RecordValue::object([("label", RecordValue::text("y"))]),
                    ),
                ]),
            ),
            ("nothing", RecordValue::Null),
        ])
    }

    #[test]
    fn roundtrip_is_canonical() {
        let n = note();
        let bytes = n.encode().unwrap();
        let back = RecordValue::decode(&bytes).unwrap();
        assert_eq!(back, n);
        assert_eq!(back.encode().unwrap(), bytes);
    }

    #[test]
    fn missing_and_null_differ() {
        let a = RecordValue::object([("x", RecordValue::Null)]);
        let b = RecordValue::object::<[(&str, RecordValue); 0], &str>([]);
        assert_ne!(a.encode().unwrap(), b.encode().unwrap());
    }

    #[test]
    fn rejects_reserved_keys_duplicate_ids_bare_arrays_and_bad_decimals() {
        assert!(
            RecordValue::object([("$x", RecordValue::Null)])
                .validate()
                .is_err()
        );
        assert!(
            RecordValue::object([("a/b", RecordValue::Null)])
                .validate()
                .is_err()
        );
        assert!(
            RecordValue::collection([("i", RecordValue::Null), ("i", RecordValue::Null)])
                .validate()
                .is_err()
        );
        let bare = cbor::encode(&Value::map([("a", Value::Array(vec![Value::Uint(1)]))]));
        assert!(RecordValue::decode(&bare).is_err());
        for bad in ["01", "1.", ".5", "1.50", "-0", "abc", ""] {
            assert!(
                RecordValue::Decimal(bad.into()).validate().is_err(),
                "{bad}"
            );
        }
        for good in ["0", "10", "-1.5", "0.001"] {
            assert!(
                RecordValue::Decimal(good.into()).validate().is_ok(),
                "{good}"
            );
        }
    }

    #[test]
    fn paths_and_schema_lookup() {
        let n = note();
        assert_eq!(
            n.at(&FieldPath::parse("tags/t2/label")),
            Some(&RecordValue::text("y"))
        );
        assert_eq!(n.at(&FieldPath::parse("tags/t9")), None);
        let schema = RecordSchema::new("note.v1")
            .field("body", FieldPolicy::Text)
            .field("tags", FieldPolicy::Set)
            .field("tags/*/label", FieldPolicy::Opaque);
        assert_eq!(
            schema.policy_of(&FieldPath::parse("body")),
            FieldPolicy::Text
        );
        assert_eq!(
            schema.policy_of(&FieldPath::parse("tags")),
            FieldPolicy::Set
        );
        assert_eq!(
            schema.policy_of(&FieldPath::parse("tags/t1/label")),
            FieldPolicy::Opaque
        );
        assert_eq!(
            schema.policy_of(&FieldPath::parse("title")),
            FieldPolicy::Scalar
        );
    }
}
