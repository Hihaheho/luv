//! `record.v2`: structured values in the Eure data model, and their
//! canonical encoding.
//!
//! A record is what an Eure document holds once it is complete: null,
//! booleans, integers, `f32` / `f64` floats, exact decimals, text with its
//! language, bytes, arrays, tuples, maps, sets, and extensions (`$variant`,
//! say) on any value. Holes are not values and are refused. How a record
//! merges is the schema's business ([`crate::schema`]), never the value's.
//!
//! Encoding to canonical CBOR is unambiguous, and the same value always has
//! the same bytes:
//! - Null, booleans, integers in the CBOR range, plain text (`"..."`),
//!   bytes and arrays are themselves.
//! - A map whose keys are all text not starting with `$` is a CBOR map;
//!   any other map is `{"$map": [[key, value], ...]}` in key order.
//! - Every other kind is a one-key map: `{"$int": "<digits>"}` for an
//!   integer outside the CBOR range, `{"$f32": bits}`, `{"$f64": bits}`,
//!   `{"$dec": [mantissa, scale]}`, `{"$code": text}` (implicit language) or
//!   `{"$code": text, "$lang": lang}`, `{"$tuple": [...]}`, and
//!   `{"$set": [...]}` in element order.
//! - A value with extensions is `{"$ext": {name: value, ...}, "$of": value}`.
//!
//! A decimal keeps the scale it was written with (`1.10` stays `1.10`), and
//! compares numerically, as Eure does: `1.10 == 1.1`. NaN has one bit
//! pattern. Missing (no key) and `null` are different.

use std::collections::{BTreeMap, BTreeSet};

use eure_document::document::EureDocument;
use eure_document::document::NodeId;
use eure_document::document::node::{NodeArray, NodeTuple, NodeValue};
use eure_document::identifier::Identifier;
use eure_document::text::Language as EureLanguage;
use eure_document::value::{PrimitiveValue, Tuple};
use num_bigint::BigInt;

use crate::cbor::{self, CborError, Value};

pub use eure_document::decimal::Decimal;
pub use eure_document::value::ObjectKey;

pub const RECORD_FORMAT_V2: u32 = 2;
/// Eure numbers tuple elements with a `u8`.
pub const MAX_TUPLE_LEN: usize = 256;

const F32_NAN: u32 = 0x7fc0_0000;
const F64_NAN: u64 = 0x7ff8_0000_0000_0000;

/// A record value: data and the extensions attached to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordValue {
    pub data: Data,
    /// Extensions by name (`variant` for `$variant`).
    pub ext: BTreeMap<String, RecordValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    Null,
    Bool(bool),
    Integer(BigInt),
    /// An `f32`, as its bits.
    F32(u32),
    /// An `f64`, as its bits.
    F64(u64),
    Decimal(Decimal),
    Text(Text),
    Bytes(Vec<u8>),
    Array(Vec<RecordValue>),
    Tuple(Vec<RecordValue>),
    Map(BTreeMap<ObjectKey, RecordValue>),
    Set(BTreeSet<ObjectKey>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    pub content: String,
    pub language: Language,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    /// `"..."`.
    Plaintext,
    /// `` `...` ``: code whose language the schema decides.
    Implicit,
    /// `` lang`...` ``.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    #[error("{0}")]
    Cbor(#[from] CborError),
    #[error("record: {0}")]
    Invalid(String),
    #[error("eure: {0}")]
    Eure(String),
}

fn invalid(message: impl Into<String>) -> RecordError {
    RecordError::Invalid(message.into())
}

impl From<Data> for RecordValue {
    fn from(data: Data) -> Self {
        RecordValue {
            data,
            ext: BTreeMap::new(),
        }
    }
}

impl RecordValue {
    pub fn null() -> Self {
        Data::Null.into()
    }

    pub fn bool(b: bool) -> Self {
        Data::Bool(b).into()
    }

    pub fn int(i: impl Into<BigInt>) -> Self {
        Data::Integer(i.into()).into()
    }

    pub fn f64(f: f64) -> Self {
        Data::F64(if f.is_nan() { F64_NAN } else { f.to_bits() }).into()
    }

    pub fn f32(f: f32) -> Self {
        Data::F32(if f.is_nan() { F32_NAN } else { f.to_bits() }).into()
    }

    pub fn decimal(d: Decimal) -> Self {
        Data::Decimal(d).into()
    }

    /// Plain text (`"..."`).
    pub fn text(s: impl Into<String>) -> Self {
        Data::Text(Text {
            content: s.into(),
            language: Language::Plaintext,
        })
        .into()
    }

    /// Code (`` lang`...` ``, or `` `...` `` without a language).
    pub fn code(s: impl Into<String>, language: Option<&str>) -> Self {
        Data::Text(Text {
            content: s.into(),
            language: match language {
                Some(l) => Language::Other(l.to_owned()),
                None => Language::Implicit,
            },
        })
        .into()
    }

    pub fn bytes(b: impl Into<Vec<u8>>) -> Self {
        Data::Bytes(b.into()).into()
    }

    pub fn array(items: impl IntoIterator<Item = RecordValue>) -> Self {
        Data::Array(items.into_iter().collect()).into()
    }

    pub fn tuple(items: impl IntoIterator<Item = RecordValue>) -> Self {
        Data::Tuple(items.into_iter().collect()).into()
    }

    pub fn map<I, K>(entries: I) -> Self
    where
        I: IntoIterator<Item = (K, RecordValue)>,
        K: Into<ObjectKey>,
    {
        Data::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect()).into()
    }

    pub fn set<I, K>(elements: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<ObjectKey>,
    {
        Data::Set(elements.into_iter().map(Into::into).collect()).into()
    }

    pub fn with_ext(mut self, name: impl Into<String>, value: RecordValue) -> Self {
        self.ext.insert(name.into(), value);
        self
    }

    pub fn as_map(&self) -> Option<&BTreeMap<ObjectKey, RecordValue>> {
        match &self.data {
            Data::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[RecordValue]> {
        match &self.data {
            Data::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&Text> {
        match &self.data {
            Data::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The value at `key` of a map.
    pub fn get(&self, key: &str) -> Option<&RecordValue> {
        self.as_map()?.get(&ObjectKey::String(key.to_owned()))
    }

    /// The `$variant` extension, when it is text.
    pub fn variant(&self) -> Option<&str> {
        self.ext
            .get("variant")
            .and_then(RecordValue::as_text)
            .map(|t| t.content.as_str())
    }

    /// The value as a map key or set element, when it can be one.
    pub fn as_key(&self) -> Option<ObjectKey> {
        if !self.ext.is_empty() {
            return None;
        }
        match &self.data {
            Data::Integer(i) => Some(ObjectKey::Number(i.clone())),
            Data::Text(t) => Some(ObjectKey::String(t.content.clone())),
            Data::Tuple(items) => items
                .iter()
                .map(RecordValue::as_key)
                .collect::<Option<Vec<_>>>()
                .map(|keys| ObjectKey::Tuple(Tuple(keys))),
            _ => None,
        }
    }

    /// Checks extension names and tuple lengths.
    pub fn validate(&self) -> Result<(), RecordError> {
        for (name, v) in &self.ext {
            name.parse::<Identifier>()
                .map_err(|e| invalid(format!("invalid extension name {name:?}: {e}")))?;
            v.validate()?;
        }
        match &self.data {
            Data::Array(items) => items.iter().try_for_each(RecordValue::validate),
            Data::Tuple(items) => {
                if items.len() > MAX_TUPLE_LEN {
                    return Err(invalid("tuple too long"));
                }
                items.iter().try_for_each(RecordValue::validate)
            }
            Data::Map(m) => {
                for k in m.keys() {
                    validate_key(k)?;
                }
                m.values().try_for_each(RecordValue::validate)
            }
            Data::Set(s) => s.iter().try_for_each(validate_key),
            Data::F32(bits) if f32::from_bits(*bits).is_nan() && *bits != F32_NAN => {
                Err(invalid("non-canonical f32 NaN"))
            }
            Data::F64(bits) if f64::from_bits(*bits).is_nan() && *bits != F64_NAN => {
                Err(invalid("non-canonical f64 NaN"))
            }
            Data::Text(Text {
                language: Language::Other(l),
                ..
            }) if l.is_empty() => Err(invalid("empty text language")),
            _ => Ok(()),
        }
    }

    pub fn to_cbor(&self) -> Value {
        let data = data_to_cbor(&self.data);
        if self.ext.is_empty() {
            return data;
        }
        Value::map([
            (
                "$ext",
                Value::map(self.ext.iter().map(|(k, v)| (k.clone(), v.to_cbor()))),
            ),
            ("$of", data),
        ])
    }

    pub fn from_cbor(value: &Value) -> Result<RecordValue, RecordError> {
        let v = from_cbor(value)?;
        v.validate()?;
        Ok(v)
    }

    /// Canonical `record.v2` bytes.
    pub fn encode(&self) -> Result<Vec<u8>, RecordError> {
        self.validate()?;
        Ok(cbor::encode(&self.to_cbor()))
    }

    pub fn decode(bytes: &[u8]) -> Result<RecordValue, RecordError> {
        RecordValue::from_cbor(&cbor::decode(bytes)?)
    }

    /// Follows a field path through maps, extensions, tuples and array
    /// elements.
    pub fn at(&self, path: &FieldPath) -> Option<&RecordValue> {
        let mut cur = self;
        for seg in &path.0 {
            cur = cur.child(seg)?;
        }
        Some(cur)
    }

    pub fn child(&self, seg: &Segment) -> Option<&RecordValue> {
        match (seg, &self.data) {
            (Segment::Ext(name), _) => self.ext.get(name),
            (Segment::Key(k), Data::Map(m)) => m.get(k),
            (Segment::Index(i), Data::Tuple(items))
            | (Segment::Position(i), Data::Array(items)) => items.get(*i),
            (Segment::Item { field, key }, Data::Array(items)) => items
                .iter()
                .find(|e| element_key(e, field).as_ref() == Some(key)),
            _ => None,
        }
    }

    /// Parses an Eure document. The whole document is the record: a root
    /// that is a single value (`= 1`) is that value.
    pub fn from_eure(source: &str) -> Result<RecordValue, RecordError> {
        let doc = eure_reader::parse_to_document(source, "record.eure")
            .map_err(|e| RecordError::Eure(e.to_string()))?;
        RecordValue::from_document(&doc)
    }

    /// The record as Eure source.
    pub fn to_eure(&self) -> String {
        let doc = self.to_document();
        match eure_document::plan::LayoutPlan::auto(doc) {
            Ok(plan) => eure_fmt::format_source_document(&plan.emit()),
            // `auto` plans every document it is given; keep a readable form
            // rather than fail a view.
            Err(e) => format!("// unprintable record: {e:?}\n"),
        }
    }

    pub fn from_document(doc: &EureDocument) -> Result<RecordValue, RecordError> {
        let v = node_value(doc, doc.get_root_id(), 0)?;
        v.validate()?;
        Ok(v)
    }

    pub fn to_document(&self) -> EureDocument {
        self.to_document_traced().0
    }

    /// The document, and the field path of every node in it (array elements
    /// by [`Segment::Position`]).
    pub fn to_document_traced(&self) -> (EureDocument, Vec<(NodeId, FieldPath)>) {
        let mut doc = EureDocument::new();
        let root = doc.get_root_id();
        let mut paths = Vec::new();
        fill_node(&mut doc, root, self, &FieldPath::root(), &mut paths);
        (doc, paths)
    }
}

/// The key an array element has under a `$merge-key` field.
pub fn element_key(element: &RecordValue, field: &str) -> Option<ObjectKey> {
    element.get(field)?.as_key()
}

fn validate_key(key: &ObjectKey) -> Result<(), RecordError> {
    match key {
        ObjectKey::Tuple(t) if t.0.len() > MAX_TUPLE_LEN => Err(invalid("tuple key too long")),
        ObjectKey::Tuple(t) => t.0.iter().try_for_each(validate_key),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Canonical CBOR
// ---------------------------------------------------------------------------

fn int_to_cbor(i: &BigInt) -> Value {
    if let Ok(u) = u64::try_from(i) {
        return Value::Uint(u);
    }
    // -1 - n for n in u64.
    let n = -(i + 1u8);
    match u64::try_from(&n) {
        Ok(n) => Value::Nint(n),
        Err(_) => Value::map([("$int", Value::text(i.to_string()))]),
    }
}

fn int_from_cbor(v: &Value) -> Option<BigInt> {
    match v {
        Value::Uint(u) => Some(BigInt::from(*u)),
        Value::Nint(n) => Some(-BigInt::from(*n) - 1u8),
        _ => None,
    }
}

fn key_to_cbor(key: &ObjectKey) -> Value {
    match key {
        ObjectKey::String(s) => Value::text(s.clone()),
        ObjectKey::Number(n) => int_to_cbor(n),
        ObjectKey::Tuple(t) => Value::map([(
            "$tuple",
            Value::Array(t.0.iter().map(key_to_cbor).collect()),
        )]),
    }
}

fn key_from_cbor(v: &Value) -> Result<ObjectKey, RecordError> {
    match from_cbor(v)? {
        k if k.ext.is_empty() => match &k.data {
            Data::Text(Text {
                language: Language::Plaintext,
                ..
            })
            | Data::Integer(_)
            | Data::Tuple(_) => k.as_key().ok_or_else(|| invalid("invalid key")),
            _ => Err(invalid("a key is an integer, a string or a tuple of those")),
        },
        _ => Err(invalid("a key has no extensions")),
    }
}

fn plain_keys(m: &BTreeMap<ObjectKey, RecordValue>) -> bool {
    m.keys()
        .all(|k| matches!(k, ObjectKey::String(s) if !s.starts_with('$')))
}

fn data_to_cbor(data: &Data) -> Value {
    match data {
        Data::Null => Value::Null,
        Data::Bool(b) => Value::Bool(*b),
        Data::Integer(i) => int_to_cbor(i),
        Data::F32(bits) => Value::map([("$f32", Value::Uint(u64::from(*bits)))]),
        Data::F64(bits) => Value::map([("$f64", Value::Uint(*bits))]),
        Data::Decimal(d) => Value::map([(
            "$dec",
            Value::Array(vec![int_to_cbor(d.mantissa()), Value::int(d.scale())]),
        )]),
        Data::Text(t) => match &t.language {
            Language::Plaintext => Value::text(t.content.clone()),
            Language::Implicit => Value::map([("$code", Value::text(t.content.clone()))]),
            Language::Other(l) => Value::map([
                ("$code", Value::text(t.content.clone())),
                ("$lang", Value::text(l.clone())),
            ]),
        },
        Data::Bytes(b) => Value::Bytes(b.clone()),
        Data::Array(items) => Value::Array(items.iter().map(RecordValue::to_cbor).collect()),
        Data::Tuple(items) => Value::map([(
            "$tuple",
            Value::Array(items.iter().map(RecordValue::to_cbor).collect()),
        )]),
        Data::Map(m) if plain_keys(m) => Value::Map(
            m.iter()
                .map(|(k, v)| (key_to_cbor(k), v.to_cbor()))
                .collect(),
        ),
        Data::Map(m) => Value::map([(
            "$map",
            Value::Array(
                m.iter()
                    .map(|(k, v)| Value::Array(vec![key_to_cbor(k), v.to_cbor()]))
                    .collect(),
            ),
        )]),
        Data::Set(s) => Value::map([("$set", Value::Array(s.iter().map(key_to_cbor).collect()))]),
    }
}

fn from_cbor(value: &Value) -> Result<RecordValue, RecordError> {
    let data = match value {
        Value::Null => Data::Null,
        Value::Bool(b) => Data::Bool(*b),
        Value::Uint(_) | Value::Nint(_) => Data::Integer(int_from_cbor(value).expect("an integer")),
        Value::Text(s) => Data::Text(Text {
            content: s.clone(),
            language: Language::Plaintext,
        }),
        Value::Bytes(b) => Data::Bytes(b.clone()),
        Value::Array(items) => Data::Array(items.iter().map(from_cbor).collect::<Result<_, _>>()?),
        Value::Map(entries) => return tagged_from_cbor(value, entries),
    };
    Ok(data.into())
}

fn tagged_from_cbor(value: &Value, entries: &[(Value, Value)]) -> Result<RecordValue, RecordError> {
    let tag = entries.iter().find_map(|(k, _)| match k {
        Value::Text(s) if s.starts_with('$') => Some(s.as_str()),
        _ => None,
    });
    let Some(tag) = tag else {
        let mut m = BTreeMap::new();
        for (k, v) in entries {
            let Value::Text(k) = k else {
                return Err(invalid("a plain map has text keys"));
            };
            m.insert(ObjectKey::String(k.clone()), from_cbor(v)?);
        }
        return Ok(Data::Map(m).into());
    };
    let fields = value
        .map_entries()
        .ok_or_else(|| invalid("a tagged value has text keys"))?;
    let only = |keys: &[&str]| -> Result<(), RecordError> {
        if fields.len() == keys.len() && keys.iter().all(|k| fields.contains_key(k)) {
            Ok(())
        } else {
            Err(invalid(format!("{tag} has unexpected keys")))
        }
    };
    let array = |key: &str| -> Result<&[Value], RecordError> {
        fields[key]
            .as_array()
            .ok_or_else(|| invalid(format!("{key} is not an array")))
    };
    if fields.contains_key("$ext") {
        only(&["$ext", "$of"])?;
        let ext_entries = fields["$ext"]
            .map_entries()
            .ok_or_else(|| invalid("$ext is not a map with text keys"))?;
        if ext_entries.is_empty() {
            return Err(invalid("$ext is empty"));
        }
        let mut v = from_cbor(fields["$of"])?;
        if !v.ext.is_empty() {
            return Err(invalid("$of has extensions of its own"));
        }
        for (k, e) in ext_entries {
            v.ext.insert(k.to_owned(), from_cbor(e)?);
        }
        return Ok(v);
    }
    if fields.contains_key("$code") {
        let content = fields["$code"]
            .as_text()
            .ok_or_else(|| invalid("$code is not text"))?
            .to_owned();
        let language = match fields.get("$lang") {
            Some(l) => {
                only(&["$code", "$lang"])?;
                Language::Other(
                    l.as_text()
                        .ok_or_else(|| invalid("$lang is not text"))?
                        .to_owned(),
                )
            }
            None => {
                only(&["$code"])?;
                Language::Implicit
            }
        };
        return Ok(Data::Text(Text { content, language }).into());
    }
    let data = match tag {
        "$int" => {
            only(&["$int"])?;
            let i: BigInt = fields["$int"]
                .as_text()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| invalid("$int is not an integer"))?;
            if !matches!(int_to_cbor(&i), Value::Map(_)) {
                return Err(invalid("$int within the CBOR integer range"));
            }
            Data::Integer(i)
        }
        "$f32" => {
            only(&["$f32"])?;
            Data::F32(
                fields["$f32"]
                    .as_u64()
                    .and_then(|b| u32::try_from(b).ok())
                    .ok_or_else(|| invalid("$f32 is not 32 bits"))?,
            )
        }
        "$f64" => {
            only(&["$f64"])?;
            Data::F64(
                fields["$f64"]
                    .as_u64()
                    .ok_or_else(|| invalid("$f64 is not 64 bits"))?,
            )
        }
        "$dec" => {
            only(&["$dec"])?;
            let [m, s] = array("$dec")? else {
                return Err(invalid("$dec is not [mantissa, scale]"));
            };
            let mantissa = match from_cbor(m)? {
                RecordValue {
                    data: Data::Integer(i),
                    ext,
                } if ext.is_empty() => i,
                _ => return Err(invalid("$dec mantissa is not an integer")),
            };
            let scale = s
                .as_i64()
                .ok_or_else(|| invalid("$dec scale is not an i64"))?;
            Data::Decimal(Decimal::new(mantissa, scale))
        }
        "$tuple" => {
            only(&["$tuple"])?;
            Data::Tuple(
                array("$tuple")?
                    .iter()
                    .map(from_cbor)
                    .collect::<Result<_, _>>()?,
            )
        }
        "$set" => {
            only(&["$set"])?;
            let mut out = BTreeSet::new();
            let mut last: Option<ObjectKey> = None;
            for e in array("$set")? {
                let k = key_from_cbor(e)?;
                if last.as_ref().is_some_and(|l| *l >= k) {
                    return Err(invalid("$set elements out of order or repeated"));
                }
                last = Some(k.clone());
                out.insert(k);
            }
            Data::Set(out)
        }
        "$map" => {
            only(&["$map"])?;
            let mut out = BTreeMap::new();
            let mut last: Option<ObjectKey> = None;
            for e in array("$map")? {
                let [k, v] = e.as_array().unwrap_or_default() else {
                    return Err(invalid("$map entry is not [key, value]"));
                };
                let k = key_from_cbor(k)?;
                if last.as_ref().is_some_and(|l| *l >= k) {
                    return Err(invalid("$map keys out of order or repeated"));
                }
                last = Some(k.clone());
                out.insert(k, from_cbor(v)?);
            }
            if plain_keys(&out) {
                return Err(invalid("$map whose keys fit a plain map"));
            }
            Data::Map(out)
        }
        other => return Err(invalid(format!("unknown tag {other}"))),
    };
    Ok(data.into())
}

// ---------------------------------------------------------------------------
// Eure documents
// ---------------------------------------------------------------------------

fn node_value(doc: &EureDocument, id: NodeId, depth: usize) -> Result<RecordValue, RecordError> {
    if depth > cbor::MAX_DEPTH {
        return Err(invalid("nested too deeply"));
    }
    let node = doc.node(id);
    let child = |id: NodeId| node_value(doc, id, depth + 1);
    let data = match &node.content {
        NodeValue::Hole(_) | NodeValue::PartialMap(_) => {
            return Err(invalid("a record has no holes"));
        }
        NodeValue::Primitive(p) => match p {
            PrimitiveValue::Null => Data::Null,
            PrimitiveValue::Bool(b) => Data::Bool(*b),
            PrimitiveValue::Integer(i) => Data::Integer(i.clone()),
            PrimitiveValue::F32(f) => RecordValue::f32(*f).data,
            PrimitiveValue::F64(f) => RecordValue::f64(*f).data,
            PrimitiveValue::Decimal(d) => Data::Decimal(d.clone()),
            PrimitiveValue::Text(t) => Data::Text(Text {
                content: t.content.clone(),
                language: match &t.language {
                    EureLanguage::Plaintext => Language::Plaintext,
                    EureLanguage::Implicit => Language::Implicit,
                    EureLanguage::Other(l) => Language::Other(l.to_string()),
                },
            }),
            PrimitiveValue::Bytes(b) => Data::Bytes(b.clone()),
        },
        NodeValue::Array(a) => Data::Array(
            a.to_vec()
                .into_iter()
                .map(child)
                .collect::<Result<_, _>>()?,
        ),
        NodeValue::Tuple(t) => Data::Tuple(
            t.to_vec()
                .into_iter()
                .map(child)
                .collect::<Result<_, _>>()?,
        ),
        NodeValue::Map(m) => Data::Map(
            m.iter()
                .map(|(k, v)| Ok((k.clone(), child(*v)?)))
                .collect::<Result<_, RecordError>>()?,
        ),
        NodeValue::Set(s) => Data::Set(s.keys().cloned().collect()),
    };
    let mut ext = BTreeMap::new();
    for (name, id) in node.extensions.iter() {
        ext.insert(name.to_string(), child(*id)?);
    }
    Ok(RecordValue { data, ext })
}

fn fill_node(
    doc: &mut EureDocument,
    id: NodeId,
    v: &RecordValue,
    path: &FieldPath,
    paths: &mut Vec<(NodeId, FieldPath)>,
) {
    paths.push((id, path.clone()));
    let content = match &v.data {
        Data::Null => NodeValue::Primitive(PrimitiveValue::Null),
        Data::Bool(b) => NodeValue::Primitive(PrimitiveValue::Bool(*b)),
        Data::Integer(i) => NodeValue::Primitive(PrimitiveValue::Integer(i.clone())),
        Data::F32(bits) => NodeValue::Primitive(PrimitiveValue::F32(f32::from_bits(*bits))),
        Data::F64(bits) => NodeValue::Primitive(PrimitiveValue::F64(f64::from_bits(*bits))),
        Data::Decimal(d) => NodeValue::Primitive(PrimitiveValue::Decimal(d.clone())),
        Data::Text(t) => NodeValue::Primitive(PrimitiveValue::Text(eure_document::Text::new(
            t.content.clone(),
            match &t.language {
                Language::Plaintext => EureLanguage::Plaintext,
                Language::Implicit => EureLanguage::Implicit,
                Language::Other(l) => EureLanguage::Other(l.clone().into()),
            },
        ))),
        Data::Bytes(b) => NodeValue::Primitive(PrimitiveValue::Bytes(b.clone())),
        Data::Array(_) => NodeValue::Array(NodeArray::default()),
        Data::Tuple(_) => NodeValue::Tuple(NodeTuple::default()),
        Data::Map(_) => NodeValue::Map(Default::default()),
        Data::Set(_) => NodeValue::Set(Default::default()),
    };
    doc.set_content(id, content);
    // The document was built empty, so every insert below is into a fresh
    // container of the right kind: none of them can fail.
    match &v.data {
        Data::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let child = doc.add_array_element(None, id).expect("array").node_id;
                fill_node(doc, child, item, &path.child(Segment::Position(i)), paths);
            }
        }
        Data::Tuple(items) => {
            for (i, item) in items.iter().enumerate() {
                let child = doc.add_tuple_element(i as u8, id).expect("tuple").node_id;
                fill_node(doc, child, item, &path.child(Segment::Index(i)), paths);
            }
        }
        Data::Map(m) => {
            for (k, item) in m {
                let child = doc.add_map_child(k.clone(), id).expect("map").node_id;
                fill_node(
                    doc,
                    child,
                    item,
                    &path.child(Segment::Key(k.clone())),
                    paths,
                );
            }
        }
        Data::Set(s) => {
            for k in s {
                doc.add_set_element(k.clone(), id).expect("set");
            }
        }
        _ => {}
    }
    for (name, item) in &v.ext {
        let ident: Identifier = name.parse().expect("validated extension name");
        let child = doc.add_extension(ident, id).expect("extension").node_id;
        fill_node(
            doc,
            child,
            item,
            &path.child(Segment::Ext(name.clone())),
            paths,
        );
    }
}

// ---------------------------------------------------------------------------
// Field paths
// ---------------------------------------------------------------------------

/// One step of a field path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment {
    /// A map key.
    Key(ObjectKey),
    /// An extension (`$variant`).
    Ext(String),
    /// A tuple element.
    Index(usize),
    /// An array element by its `$merge-key` field.
    Item { field: String, key: ObjectKey },
    /// An array element by position. Elements of an array without a merge
    /// key are not merged one by one, so merge paths never hold this.
    Position(usize),
    /// A conflicting region of a text field (conflict terms only).
    Region(u32),
}

/// A field path. Displayed in Eure's path syntax: `a.b`, `"key with
/// spaces"`, `$variant`, `.#0` for a tuple element, `[id="x"]` for an array
/// element by its merge key, `[3]` by position, and `#0` for a text region.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FieldPath(pub Vec<Segment>);

impl FieldPath {
    pub fn root() -> Self {
        Self(Vec::new())
    }

    pub fn key(self, key: impl Into<ObjectKey>) -> Self {
        self.child(Segment::Key(key.into()))
    }

    pub fn child(&self, seg: Segment) -> Self {
        let mut v = self.0.clone();
        v.push(seg);
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

fn write_key(f: &mut std::fmt::Formatter<'_>, key: &ObjectKey) -> std::fmt::Result {
    match key {
        ObjectKey::String(s) if s.parse::<Identifier>().is_ok() => f.write_str(s),
        other => write!(f, "{other}"),
    }
}

impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, seg) in self.0.iter().enumerate() {
            let dot = if i == 0 { "" } else { "." };
            match seg {
                Segment::Key(k) => {
                    f.write_str(dot)?;
                    write_key(f, k)?;
                }
                Segment::Ext(name) => write!(f, "{dot}${name}")?,
                Segment::Index(n) => write!(f, "{dot}#{n}")?,
                Segment::Item { field, key } => write!(f, "[{field}={key}]")?,
                Segment::Position(n) => write!(f, "[{n}]")?,
                Segment::Region(n) => write!(f, "#{n}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note() -> RecordValue {
        RecordValue::map([
            ("title", RecordValue::text("A")),
            ("body", RecordValue::code("fn main() {}\n", Some("rust"))),
            ("pinned", RecordValue::bool(false)),
            ("score", RecordValue::decimal("1.50".parse().unwrap())),
            ("ratio", RecordValue::f64(0.25)),
            ("n", RecordValue::int(-3)),
            (
                "big",
                RecordValue::int("123456789012345678901234567890".parse::<BigInt>().unwrap()),
            ),
            ("blob", RecordValue::bytes(vec![1, 2, 3])),
            ("tags", RecordValue::set(["x", "y"])),
            (
                "pair",
                RecordValue::tuple([RecordValue::int(1), RecordValue::text("b")]),
            ),
            (
                "links",
                RecordValue::array([RecordValue::map([("id", RecordValue::text("a"))])]),
            ),
            (
                "status",
                RecordValue::map([("reason", RecordValue::text("r"))])
                    .with_ext("variant", RecordValue::text("rejected")),
            ),
            ("nothing", RecordValue::null()),
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
    fn maps_with_any_key_and_dollar_strings_take_the_tagged_form() {
        let m = RecordValue::map([
            (ObjectKey::from(1), RecordValue::text("one")),
            (ObjectKey::from("$x"), RecordValue::null()),
            (
                ObjectKey::Tuple(Tuple(vec![ObjectKey::from(1), ObjectKey::from("a")])),
                RecordValue::bool(true),
            ),
        ]);
        let bytes = m.encode().unwrap();
        assert_eq!(RecordValue::decode(&bytes).unwrap(), m);
        // A plain-keyed map in the tagged form is not canonical.
        let tagged = cbor::encode(&Value::map([(
            "$map",
            Value::Array(vec![Value::Array(vec![Value::text("a"), Value::Null])]),
        )]));
        assert!(RecordValue::decode(&tagged).is_err());
    }

    #[test]
    fn missing_and_null_differ() {
        let a = RecordValue::map([("x", RecordValue::null())]);
        let b = RecordValue::map::<[(&str, RecordValue); 0], &str>([]);
        assert_ne!(a.encode().unwrap(), b.encode().unwrap());
    }

    #[test]
    fn decimals_keep_their_scale_and_compare_numerically() {
        let a = RecordValue::decimal("1.10".parse().unwrap());
        let b = RecordValue::decimal("1.1".parse().unwrap());
        assert_eq!(a, b);
        assert_ne!(a.encode().unwrap(), b.encode().unwrap());
        assert_eq!(
            RecordValue::decode(&a.encode().unwrap()).unwrap().to_eure(),
            "= 1.10\n"
        );
    }

    #[test]
    fn rejects_non_canonical_and_invalid_values() {
        assert!(
            RecordValue::null()
                .with_ext("$x", RecordValue::null())
                .validate()
                .is_err()
        );
        assert!(RecordValue::code("x", Some("")).validate().is_err());
        let small_big = cbor::encode(&Value::map([("$int", Value::text("5"))]));
        assert!(RecordValue::decode(&small_big).is_err());
        let set = cbor::encode(&Value::map([(
            "$set",
            Value::Array(vec![Value::text("b"), Value::text("a")]),
        )]));
        assert!(RecordValue::decode(&set).is_err());
        let nan = cbor::encode(&Value::map([("$f64", Value::Uint(0x7ff8_0000_0000_0001))]));
        assert!(RecordValue::decode(&nan).is_err());
        let empty_ext = cbor::encode(&Value::map([
            ("$ext", Value::Map(vec![])),
            ("$of", Value::Null),
        ]));
        assert!(RecordValue::decode(&empty_ext).is_err());
    }

    #[test]
    fn eure_source_round_trips() {
        let src = r#"
title = "A"
body = rust`fn main() {}`
n = -3
score = 1.50
blob = hex`010203`
tags = #{"x", "y"}
pair = (1, "b")
status.$variant = "rejected"
status.reason = "r"
links[] {
  id = "a"
}
"#;
        let v = RecordValue::from_eure(src).unwrap();
        assert_eq!(v.get("status").unwrap().variant(), Some("rejected"));
        assert_eq!(v.get("blob"), Some(&RecordValue::bytes(vec![1, 2, 3])));
        assert_eq!(
            v.get("body").unwrap().as_text().unwrap().language,
            Language::Other("rust".into())
        );
        let again = RecordValue::from_eure(&v.to_eure()).unwrap();
        assert_eq!(again, v);
        assert!(RecordValue::from_eure("a = !").is_err());
        assert_eq!(
            RecordValue::from_eure("= true").unwrap(),
            RecordValue::bool(true)
        );
    }

    #[test]
    fn paths_display_in_eure_syntax_and_resolve() {
        let n = note();
        let path = FieldPath::root().key("links").child(Segment::Item {
            field: "id".into(),
            key: "a".into(),
        });
        assert_eq!(path.to_string(), "links[id=\"a\"]");
        assert_eq!(
            n.at(&path.child(Segment::Key("id".into()))),
            Some(&RecordValue::text("a"))
        );
        let tuple = FieldPath::root().key("pair").child(Segment::Index(1));
        assert_eq!(tuple.to_string(), "pair.#1");
        assert_eq!(n.at(&tuple), Some(&RecordValue::text("b")));
        let ext = FieldPath::root()
            .key("status")
            .child(Segment::Ext("variant".into()));
        assert_eq!(ext.to_string(), "status.$variant");
        assert_eq!(n.at(&ext), Some(&RecordValue::text("rejected")));
        assert_eq!(FieldPath::root().key("a b").to_string(), "\"a b\"");
        assert_eq!(
            FieldPath::root()
                .key("body")
                .child(Segment::Region(0))
                .to_string(),
            "body#0"
        );
    }
}
