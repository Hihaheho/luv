//! The content-addressed core objects: `ObjectState`, `Commit`, `Conflict`.
//!
//! Every object carries a `format` and `required_capabilities`. A reader
//! that does not know a format or a capability refuses the object with
//! [`CoreError::Unsupported`] instead of guessing. Unknown map keys are
//! rejected too, so a newer writer cannot smuggle semantics a version-1
//! reader would silently drop.

use std::collections::BTreeSet;

use crate::cbor::{self, CborError, Value};
use crate::digest::{Digest, DigestError};
use crate::id::{
    ChangeId, CommitId, ConflictId, ContentId, Domain, IdKey, ObjectId, ObjectStateId, RepoId,
    TreeId, keyed_id,
};

pub const CONTENT_FORMAT_V1: u32 = 1;
pub const STATE_FORMAT_V1: u32 = 1;
pub const COMMIT_FORMAT_V1: u32 = 1;
pub const TREE_FORMAT_V1: u32 = 1;
pub const CONFLICT_FORMAT_V1: u32 = 1;

/// Limits a reader enforces before it accepts an object.
pub const MAX_PARENTS: usize = 32;
pub const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;
pub const MAX_METADATA_BYTES: usize = 64 * 1024;
pub const MAX_CAPABILITIES: usize = 64;
pub const MAX_CAPABILITY_ID_BYTES: usize = 128;
pub const MAX_TYPE_ID_BYTES: usize = 256;

/// Capability IDs this build understands. Objects that require anything
/// else are refused.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities(BTreeSet<String>);

impl Capabilities {
    /// The capabilities a version-1 reader has.
    pub fn v1() -> Self {
        Self(
            [
                "core.v1",
                "text.v1",
                "record.v2",
                "binary.v1",
                "tree.patricia128.v1",
                "merge.text.v1",
                "merge.record.v2",
                "merge.object.v1",
                "bookmark.v1",
                "redaction.v1",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
    }

    pub fn with(mut self, id: &str) -> Self {
        self.0.insert(id.to_owned());
        self
    }

    pub fn supports(&self, id: &str) -> bool {
        self.0.contains(id)
    }

    fn missing<'a>(&self, required: impl IntoIterator<Item = &'a String>) -> Vec<String> {
        required
            .into_iter()
            .filter(|c| !self.0.contains(c.as_str()))
            .cloned()
            .collect()
    }
}

/// What an object needs that this reader cannot provide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub kind: &'static str,
    pub format: u32,
    pub missing_capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error("{0}")]
    Cbor(#[from] CborError),
    #[error("{0}")]
    Digest(#[from] DigestError),
    #[error("{kind} format {format} or capabilities {missing_capabilities:?} are not supported")]
    Unsupported {
        kind: &'static str,
        format: u32,
        missing_capabilities: Vec<String>,
    },
    #[error("{kind}: {detail}")]
    Invalid { kind: &'static str, detail: String },
}

impl CoreError {
    pub(crate) fn invalid(kind: &'static str, detail: impl Into<String>) -> Self {
        CoreError::Invalid {
            kind,
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Shared decoding helpers
// ---------------------------------------------------------------------------

pub(crate) struct Fields<'a> {
    kind: &'static str,
    value: &'a Value,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(
        kind: &'static str,
        value: &'a Value,
        allowed: &[&str],
    ) -> Result<Self, CoreError> {
        let entries = value
            .map_entries()
            .ok_or_else(|| CoreError::invalid(kind, "not a map with text keys"))?;
        for key in entries.keys() {
            if !allowed.contains(key) {
                return Err(CoreError::invalid(kind, format!("unknown field {key:?}")));
            }
        }
        Ok(Self { kind, value })
    }

    pub(crate) fn get(&self, key: &str) -> Result<&'a Value, CoreError> {
        self.value
            .get(key)
            .ok_or_else(|| CoreError::invalid(self.kind, format!("missing field {key:?}")))
    }

    pub(crate) fn u32(&self, key: &str) -> Result<u32, CoreError> {
        self.get(key)?
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| CoreError::invalid(self.kind, format!("{key} is not a u32")))
    }

    pub(crate) fn i64(&self, key: &str) -> Result<i64, CoreError> {
        self.get(key)?
            .as_i64()
            .ok_or_else(|| CoreError::invalid(self.kind, format!("{key} is not an i64")))
    }

    pub(crate) fn text(&self, key: &str, max: usize) -> Result<String, CoreError> {
        let s = self
            .get(key)?
            .as_text()
            .ok_or_else(|| CoreError::invalid(self.kind, format!("{key} is not text")))?;
        if s.len() > max {
            return Err(CoreError::invalid(
                self.kind,
                format!("{key} exceeds {max} bytes"),
            ));
        }
        Ok(s.to_owned())
    }

    pub(crate) fn id<T>(&self, key: &str, parse: fn(&Value) -> Option<T>) -> Result<T, CoreError> {
        parse(self.get(key)?)
            .ok_or_else(|| CoreError::invalid(self.kind, format!("{key} is not a valid id")))
    }

    pub(crate) fn ids<T>(
        &self,
        key: &str,
        parse: fn(&Value) -> Option<T>,
    ) -> Result<Vec<T>, CoreError> {
        self.get(key)?
            .as_array()
            .ok_or_else(|| CoreError::invalid(self.kind, format!("{key} is not an array")))?
            .iter()
            .map(|v| {
                parse(v).ok_or_else(|| {
                    CoreError::invalid(self.kind, format!("{key} contains an invalid id"))
                })
            })
            .collect()
    }

    pub(crate) fn capabilities(&self) -> Result<Vec<String>, CoreError> {
        let caps: Vec<String> = self
            .get("required_capabilities")?
            .as_array()
            .ok_or_else(|| CoreError::invalid(self.kind, "required_capabilities is not an array"))?
            .iter()
            .map(|v| {
                v.as_text().map(str::to_owned).ok_or_else(|| {
                    CoreError::invalid(self.kind, "required_capabilities contains non-text")
                })
            })
            .collect::<Result<_, _>>()?;
        validate_capabilities(self.kind, &caps)?;
        Ok(caps)
    }
}

pub(crate) fn validate_capabilities(kind: &'static str, caps: &[String]) -> Result<(), CoreError> {
    if caps.len() > MAX_CAPABILITIES {
        return Err(CoreError::invalid(kind, "too many required capabilities"));
    }
    for pair in caps.windows(2) {
        if pair[0] >= pair[1] {
            return Err(CoreError::invalid(
                kind,
                "required_capabilities must be sorted and unique",
            ));
        }
    }
    if caps
        .iter()
        .any(|c| c.is_empty() || c.len() > MAX_CAPABILITY_ID_BYTES)
    {
        return Err(CoreError::invalid(
            kind,
            "capability id length out of range",
        ));
    }
    Ok(())
}

pub(crate) fn check_support(
    kind: &'static str,
    format: u32,
    known_format: u32,
    required: &[String],
    caps: &Capabilities,
) -> Result<(), CoreError> {
    let missing = caps.missing(required);
    if format > known_format || format == 0 || !missing.is_empty() {
        return Err(CoreError::Unsupported {
            kind,
            format,
            missing_capabilities: missing,
        });
    }
    Ok(())
}

pub(crate) fn caps_value(caps: &[String]) -> Value {
    Value::Array(caps.iter().map(|c| Value::text(c.clone())).collect())
}

/// Sorts and dedupes a capability list into its canonical form.
pub fn normalize_capabilities(caps: impl IntoIterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = caps.into_iter().collect();
    set.into_iter().collect()
}

// ---------------------------------------------------------------------------
// ObjectState
// ---------------------------------------------------------------------------

/// Semantic reference to a Blob: realm, id and plaintext length only.
/// Physical locators, wrapped keys and representations never enter the
/// canonical state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub realm_id: Vec<u8>,
    pub blob_id: Vec<u8>,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectBody {
    Text {
        content: ContentId,
    },
    /// `schema` is the content ID of the record schema (itself a record).
    Record {
        content: ContentId,
        schema: ContentId,
    },
    Binary {
        blob: BlobRef,
    },
    Conflict {
        conflict: ConflictId,
    },
    Deleted,
}

impl ObjectBody {
    pub fn kind(&self) -> &'static str {
        match self {
            ObjectBody::Text { .. } => "text",
            ObjectBody::Record { .. } => "record",
            ObjectBody::Binary { .. } => "binary",
            ObjectBody::Conflict { .. } => "conflict",
            ObjectBody::Deleted => "deleted",
        }
    }

    fn to_cbor(&self) -> Value {
        match self {
            ObjectBody::Text { content } => Value::map([
                ("kind", Value::text("text")),
                ("content", content.to_cbor()),
            ]),
            ObjectBody::Record { content, schema } => Value::map([
                ("kind", Value::text("record")),
                ("content", content.to_cbor()),
                ("schema", schema.to_cbor()),
            ]),
            ObjectBody::Binary { blob } => Value::map([
                ("kind", Value::text("binary")),
                ("realm_id", Value::Bytes(blob.realm_id.clone())),
                ("blob_id", Value::Bytes(blob.blob_id.clone())),
                ("length", Value::Uint(blob.length)),
            ]),
            ObjectBody::Conflict { conflict } => Value::map([
                ("kind", Value::text("conflict")),
                ("conflict", conflict.to_cbor()),
            ]),
            ObjectBody::Deleted => Value::map([("kind", Value::text("deleted"))]),
        }
    }

    fn from_cbor(value: &Value) -> Result<Self, CoreError> {
        const KIND: &str = "object_state.body";
        let kind = value
            .get("kind")
            .and_then(Value::as_text)
            .ok_or_else(|| CoreError::invalid(KIND, "missing kind"))?;
        match kind {
            "text" => {
                let f = Fields::new(KIND, value, &["kind", "content"])?;
                Ok(ObjectBody::Text {
                    content: f.id("content", ContentId::from_cbor)?,
                })
            }
            "record" => {
                let f = Fields::new(KIND, value, &["kind", "content", "schema"])?;
                Ok(ObjectBody::Record {
                    content: f.id("content", ContentId::from_cbor)?,
                    schema: f.id("schema", ContentId::from_cbor)?,
                })
            }
            "binary" => {
                let f = Fields::new(KIND, value, &["kind", "realm_id", "blob_id", "length"])?;
                let realm_id = f
                    .get("realm_id")?
                    .as_bytes()
                    .ok_or_else(|| CoreError::invalid(KIND, "realm_id is not bytes"))?
                    .to_vec();
                let blob_id = f
                    .get("blob_id")?
                    .as_bytes()
                    .ok_or_else(|| CoreError::invalid(KIND, "blob_id is not bytes"))?
                    .to_vec();
                if realm_id.is_empty() || blob_id.is_empty() {
                    return Err(CoreError::invalid(KIND, "empty blob reference"));
                }
                let length = f
                    .get("length")?
                    .as_u64()
                    .ok_or_else(|| CoreError::invalid(KIND, "length is not a u64"))?;
                Ok(ObjectBody::Binary {
                    blob: BlobRef {
                        realm_id,
                        blob_id,
                        length,
                    },
                })
            }
            "conflict" => {
                let f = Fields::new(KIND, value, &["kind", "conflict"])?;
                Ok(ObjectBody::Conflict {
                    conflict: f.id("conflict", ConflictId::from_cbor)?,
                })
            }
            "deleted" => {
                Fields::new(KIND, value, &["kind"])?;
                Ok(ObjectBody::Deleted)
            }
            other => Err(CoreError::Unsupported {
                kind: KIND,
                format: STATE_FORMAT_V1,
                missing_capabilities: vec![format!("body.{other}")],
            }),
        }
    }
}

/// Versioned state of one object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectState {
    pub format: u32,
    pub object_id: ObjectId,
    pub type_id: String,
    pub body: ObjectBody,
    /// Canonical map with text keys; name, MIME, title live here, never in
    /// the bytes.
    pub metadata: Value,
    pub required_capabilities: Vec<String>,
}

impl ObjectState {
    pub fn new(object_id: ObjectId, type_id: impl Into<String>, body: ObjectBody) -> Self {
        Self {
            format: STATE_FORMAT_V1,
            object_id,
            type_id: type_id.into(),
            body,
            metadata: Value::Map(Vec::new()),
            required_capabilities: vec!["core.v1".to_owned()],
        }
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_capabilities(mut self, caps: impl IntoIterator<Item = String>) -> Self {
        self.required_capabilities = normalize_capabilities(caps);
        self
    }

    pub fn to_cbor(&self) -> Value {
        Value::map([
            ("format", Value::Uint(u64::from(self.format))),
            ("object_id", self.object_id.to_cbor()),
            ("type_id", Value::text(self.type_id.clone())),
            ("body", self.body.to_cbor()),
            ("metadata", self.metadata.clone()),
            (
                "required_capabilities",
                caps_value(&self.required_capabilities),
            ),
        ])
    }

    pub fn encode(&self) -> Result<Vec<u8>, CoreError> {
        self.validate()?;
        Ok(cbor::encode(&self.to_cbor()))
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        const KIND: &str = "object_state";
        if self.format != STATE_FORMAT_V1 {
            return Err(CoreError::invalid(KIND, "format is not 1"));
        }
        if self.type_id.is_empty() || self.type_id.len() > MAX_TYPE_ID_BYTES {
            return Err(CoreError::invalid(KIND, "type_id length out of range"));
        }
        if self.metadata.map_entries().is_none() {
            return Err(CoreError::invalid(
                KIND,
                "metadata is not a map with text keys",
            ));
        }
        if cbor::encode(&self.metadata).len() > MAX_METADATA_BYTES {
            return Err(CoreError::invalid(KIND, "metadata exceeds 64 KiB"));
        }
        validate_capabilities(KIND, &self.required_capabilities)
    }

    pub fn decode(bytes: &[u8], caps: &Capabilities) -> Result<Self, CoreError> {
        Self::from_cbor(&cbor::decode(bytes)?, caps)
    }

    pub fn from_cbor(value: &Value, caps: &Capabilities) -> Result<Self, CoreError> {
        const KIND: &str = "object_state";
        let f = Fields::new(
            KIND,
            value,
            &[
                "format",
                "object_id",
                "type_id",
                "body",
                "metadata",
                "required_capabilities",
            ],
        )?;
        let format = f.u32("format")?;
        let required_capabilities = f.capabilities()?;
        check_support(KIND, format, STATE_FORMAT_V1, &required_capabilities, caps)?;
        let state = Self {
            format,
            object_id: f.id("object_id", ObjectId::from_cbor)?,
            type_id: f.text("type_id", MAX_TYPE_ID_BYTES)?,
            body: ObjectBody::from_cbor(f.get("body")?)?,
            metadata: f.get("metadata")?.clone(),
            required_capabilities,
        };
        state.validate()?;
        Ok(state)
    }

    pub fn id<D: Digest>(&self, digest: &D, key: &IdKey) -> Result<ObjectStateId, DigestError> {
        Ok(ObjectStateId(keyed_id(
            digest,
            key,
            Domain::State,
            self.format,
            self.to_cbor(),
        )?))
    }
}

// ---------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
    User,
    Agent,
    Import,
    System,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::User => "user",
            ActorKind::Agent => "agent",
            ActorKind::Import => "import",
            ActorKind::System => "system",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => ActorKind::User,
            "agent" => ActorKind::Agent,
            "import" => ActorKind::Import,
            "system" => ActorKind::System,
            _ => return None,
        })
    }
}

/// Provenance of an authenticated operation; not a signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub kind: ActorKind,
    pub device_id: String,
    pub description: String,
}

impl Actor {
    pub(crate) fn to_cbor(&self) -> Value {
        Value::map([
            ("kind", Value::text(self.kind.as_str())),
            ("device_id", Value::text(self.device_id.clone())),
            ("description", Value::text(self.description.clone())),
        ])
    }

    pub(crate) fn from_cbor(value: &Value) -> Result<Self, CoreError> {
        const KIND: &str = "commit.author";
        let f = Fields::new(KIND, value, &["kind", "device_id", "description"])?;
        let kind = f.text("kind", 16)?;
        Ok(Self {
            kind: ActorKind::parse(&kind)
                .ok_or_else(|| CoreError::invalid(KIND, format!("unknown actor kind {kind}")))?,
            device_id: f.text("device_id", 256)?,
            description: f.text("description", 1024)?,
        })
    }
}

/// A commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub format: u32,
    pub repo_id: RepoId,
    pub change_id: ChangeId,
    pub parents: Vec<CommitId>,
    pub root: TreeId,
    pub author: Actor,
    pub created_at_unix_ms: i64,
    pub description: String,
    pub required_capabilities: Vec<String>,
}

impl Commit {
    pub fn to_cbor(&self) -> Value {
        Value::map([
            ("format", Value::Uint(u64::from(self.format))),
            ("repo_id", self.repo_id.to_cbor()),
            ("change_id", self.change_id.to_cbor()),
            (
                "parents",
                Value::Array(self.parents.iter().map(CommitId::to_cbor).collect()),
            ),
            ("root", self.root.to_cbor()),
            ("author", self.author.to_cbor()),
            ("created_at_unix_ms", Value::int(self.created_at_unix_ms)),
            ("description", Value::text(self.description.clone())),
            (
                "required_capabilities",
                caps_value(&self.required_capabilities),
            ),
        ])
    }

    pub fn encode(&self) -> Result<Vec<u8>, CoreError> {
        self.validate()?;
        Ok(cbor::encode(&self.to_cbor()))
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        const KIND: &str = "commit";
        if self.format != COMMIT_FORMAT_V1 {
            return Err(CoreError::invalid(KIND, "format is not 1"));
        }
        if self.parents.len() > MAX_PARENTS {
            return Err(CoreError::invalid(KIND, "more than 32 parents"));
        }
        let unique: BTreeSet<&CommitId> = self.parents.iter().collect();
        if unique.len() != self.parents.len() {
            return Err(CoreError::invalid(KIND, "duplicate parent"));
        }
        if self.description.len() > MAX_DESCRIPTION_BYTES {
            return Err(CoreError::invalid(KIND, "description exceeds 16 KiB"));
        }
        validate_capabilities(KIND, &self.required_capabilities)
    }

    pub fn decode(bytes: &[u8], caps: &Capabilities) -> Result<Self, CoreError> {
        Self::from_cbor(&cbor::decode(bytes)?, caps)
    }

    pub fn from_cbor(value: &Value, caps: &Capabilities) -> Result<Self, CoreError> {
        const KIND: &str = "commit";
        let f = Fields::new(
            KIND,
            value,
            &[
                "format",
                "repo_id",
                "change_id",
                "parents",
                "root",
                "author",
                "created_at_unix_ms",
                "description",
                "required_capabilities",
            ],
        )?;
        let format = f.u32("format")?;
        let required_capabilities = f.capabilities()?;
        check_support(KIND, format, COMMIT_FORMAT_V1, &required_capabilities, caps)?;
        let commit = Self {
            format,
            repo_id: f.id("repo_id", RepoId::from_cbor)?,
            change_id: f.id("change_id", ChangeId::from_cbor)?,
            parents: f.ids("parents", CommitId::from_cbor)?,
            root: f.id("root", TreeId::from_cbor)?,
            author: Actor::from_cbor(f.get("author")?)?,
            created_at_unix_ms: f.i64("created_at_unix_ms")?,
            description: f.text("description", MAX_DESCRIPTION_BYTES)?,
            required_capabilities,
        };
        commit.validate()?;
        Ok(commit)
    }

    pub fn id<D: Digest>(&self, digest: &D, key: &IdKey) -> Result<CommitId, DigestError> {
        Ok(CommitId(keyed_id(
            digest,
            key,
            Domain::Commit,
            self.format,
            self.to_cbor(),
        )?))
    }
}

// ---------------------------------------------------------------------------
// Conflict
// ---------------------------------------------------------------------------

/// What kind of disagreement one conflict term records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    /// Both sides set a field (or the whole object) to different values.
    Value,
    /// Both sides reordered the same list.
    Order,
    /// One side deleted what the other side edited.
    DeletedVsEdited,
    /// A text region both sides changed differently.
    TextRegion,
    /// Both sides set a metadata key (the term's path) to different values.
    /// Candidates are the canonical CBOR of each side's value.
    Metadata,
    /// The sides merged cleanly, but the result breaks the record schema
    /// at this path.
    Invalid,
}

impl TermKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TermKind::Value => "value",
            TermKind::Order => "order",
            TermKind::DeletedVsEdited => "deleted_vs_edited",
            TermKind::TextRegion => "text_region",
            TermKind::Metadata => "metadata",
            TermKind::Invalid => "invalid",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "value" => TermKind::Value,
            "order" => TermKind::Order,
            "deleted_vs_edited" => TermKind::DeletedVsEdited,
            "text_region" => TermKind::TextRegion,
            "metadata" => TermKind::Metadata,
            "invalid" => TermKind::Invalid,
            _ => return None,
        })
    }
}

/// One unresolved question inside a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictTerm {
    /// Field path (`""` for the whole object; text regions use `"#<index>"`).
    pub path: String,
    pub kind: TermKind,
    /// One candidate per side, in side order. `None` means "absent" on
    /// that side (deleted field, deleted object).
    pub candidates: Vec<Option<ContentId>>,
}

/// A stored conflict value. The display markers are derived from it, never
/// the other way round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub format: u32,
    pub object_id: ObjectId,
    /// Merge driver ID and version, e.g. `merge.text.v1`.
    pub driver: String,
    /// Zero (no common ancestor), one, or several (ambiguous base) states.
    pub base: Vec<ObjectStateId>,
    pub sides: Vec<ObjectStateId>,
    /// Everything that did merge, as a provisional full content.
    pub merged: Option<ContentId>,
    pub terms: Vec<ConflictTerm>,
}

impl Conflict {
    pub fn to_cbor(&self) -> Value {
        Value::map([
            ("format", Value::Uint(u64::from(self.format))),
            ("object_id", self.object_id.to_cbor()),
            ("driver", Value::text(self.driver.clone())),
            (
                "base",
                Value::Array(self.base.iter().map(ObjectStateId::to_cbor).collect()),
            ),
            (
                "sides",
                Value::Array(self.sides.iter().map(ObjectStateId::to_cbor).collect()),
            ),
            (
                "merged",
                self.merged.as_ref().map_or(Value::Null, ContentId::to_cbor),
            ),
            (
                "terms",
                Value::Array(
                    self.terms
                        .iter()
                        .map(|t| {
                            Value::map([
                                ("path", Value::text(t.path.clone())),
                                ("kind", Value::text(t.kind.as_str())),
                                (
                                    "candidates",
                                    Value::Array(
                                        t.candidates
                                            .iter()
                                            .map(|c| {
                                                c.as_ref().map_or(Value::Null, ContentId::to_cbor)
                                            })
                                            .collect(),
                                    ),
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }

    pub fn encode(&self) -> Result<Vec<u8>, CoreError> {
        self.validate()?;
        Ok(cbor::encode(&self.to_cbor()))
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        const KIND: &str = "conflict";
        if self.format != CONFLICT_FORMAT_V1 {
            return Err(CoreError::invalid(KIND, "format is not 1"));
        }
        if self.sides.len() < 2 || self.sides.len() > MAX_PARENTS {
            return Err(CoreError::invalid(KIND, "a conflict needs 2..=32 sides"));
        }
        if self.base.len() > MAX_PARENTS {
            return Err(CoreError::invalid(KIND, "too many base states"));
        }
        if self.driver.is_empty() || self.driver.len() > MAX_CAPABILITY_ID_BYTES {
            return Err(CoreError::invalid(KIND, "driver id length out of range"));
        }
        if self.terms.is_empty() {
            return Err(CoreError::invalid(
                KIND,
                "a conflict needs at least one term",
            ));
        }
        for term in &self.terms {
            if term.candidates.len() != self.sides.len() {
                return Err(CoreError::invalid(
                    KIND,
                    "each term needs one candidate per side",
                ));
            }
        }
        Ok(())
    }

    pub fn decode(bytes: &[u8], caps: &Capabilities) -> Result<Self, CoreError> {
        Self::from_cbor(&cbor::decode(bytes)?, caps)
    }

    pub fn from_cbor(value: &Value, caps: &Capabilities) -> Result<Self, CoreError> {
        const KIND: &str = "conflict";
        let f = Fields::new(
            KIND,
            value,
            &[
                "format",
                "object_id",
                "driver",
                "base",
                "sides",
                "merged",
                "terms",
            ],
        )?;
        let format = f.u32("format")?;
        let driver = f.text("driver", MAX_CAPABILITY_ID_BYTES)?;
        check_support(
            KIND,
            format,
            CONFLICT_FORMAT_V1,
            std::slice::from_ref(&driver),
            caps,
        )?;
        let merged = match f.get("merged")? {
            Value::Null => None,
            v => Some(
                ContentId::from_cbor(v)
                    .ok_or_else(|| CoreError::invalid(KIND, "merged is not a content id"))?,
            ),
        };
        let terms = f
            .get("terms")?
            .as_array()
            .ok_or_else(|| CoreError::invalid(KIND, "terms is not an array"))?
            .iter()
            .map(|t| {
                let tf = Fields::new("conflict.term", t, &["path", "kind", "candidates"])?;
                let kind = tf.text("kind", 32)?;
                let candidates = tf
                    .get("candidates")?
                    .as_array()
                    .ok_or_else(|| CoreError::invalid(KIND, "candidates is not an array"))?
                    .iter()
                    .map(|c| match c {
                        Value::Null => Ok(None),
                        v => ContentId::from_cbor(v)
                            .map(Some)
                            .ok_or_else(|| CoreError::invalid(KIND, "bad candidate id")),
                    })
                    .collect::<Result<_, _>>()?;
                Ok(ConflictTerm {
                    path: tf.text("path", 4096)?,
                    kind: TermKind::parse(&kind).ok_or_else(|| {
                        CoreError::invalid(KIND, format!("unknown term kind {kind}"))
                    })?,
                    candidates,
                })
            })
            .collect::<Result<Vec<_>, CoreError>>()?;
        let conflict = Self {
            format,
            object_id: f.id("object_id", ObjectId::from_cbor)?,
            driver,
            base: f.ids("base", ObjectStateId::from_cbor)?,
            sides: f.ids("sides", ObjectStateId::from_cbor)?,
            merged,
            terms,
        };
        conflict.validate()?;
        Ok(conflict)
    }

    pub fn id<D: Digest>(&self, digest: &D, key: &IdKey) -> Result<ConflictId, DigestError> {
        Ok(ConflictId(keyed_id(
            digest,
            key,
            Domain::Conflict,
            self.format,
            self.to_cbor(),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::RustDigest;

    fn key() -> IdKey {
        IdKey([9; 32])
    }

    fn state() -> ObjectState {
        ObjectState::new(
            ObjectId([1; 16]),
            "note",
            ObjectBody::Text {
                content: ContentId([2; 32]),
            },
        )
        .with_metadata(Value::map([("title", Value::text("hello"))]))
    }

    fn commit() -> Commit {
        Commit {
            format: COMMIT_FORMAT_V1,
            repo_id: RepoId([3; 16]),
            change_id: ChangeId([4; 16]),
            parents: vec![CommitId([5; 32])],
            root: TreeId([6; 32]),
            author: Actor {
                kind: ActorKind::User,
                device_id: "dev".into(),
                description: String::new(),
            },
            created_at_unix_ms: 1_700_000_000_000,
            description: "first".into(),
            required_capabilities: vec!["core.v1".into()],
        }
    }

    #[test]
    fn state_roundtrips_and_id_is_stable() {
        let s = state();
        let bytes = s.encode().unwrap();
        let back = ObjectState::decode(&bytes, &Capabilities::v1()).unwrap();
        assert_eq!(back, s);
        assert_eq!(
            back.id(&RustDigest, &key()).unwrap(),
            s.id(&RustDigest, &key()).unwrap()
        );
    }

    #[test]
    fn commit_roundtrips() {
        let c = commit();
        let bytes = c.encode().unwrap();
        assert_eq!(Commit::decode(&bytes, &Capabilities::v1()).unwrap(), c);
    }

    #[test]
    fn unknown_capability_or_newer_format_is_refused_not_misread() {
        let mut s = state();
        s.required_capabilities = vec!["core.v1".into(), "zz.future".into()];
        let bytes = s.encode().unwrap();
        let err = ObjectState::decode(&bytes, &Capabilities::v1()).unwrap_err();
        assert_eq!(
            err,
            CoreError::Unsupported {
                kind: "object_state",
                format: 1,
                missing_capabilities: vec!["zz.future".into()],
            }
        );
        // A reader that gained the capability accepts it.
        assert!(ObjectState::decode(&bytes, &Capabilities::v1().with("zz.future")).is_ok());

        let mut v = commit().to_cbor();
        if let Value::Map(entries) = &mut v {
            for (k, val) in entries.iter_mut() {
                if k.as_text() == Some("format") {
                    *val = Value::Uint(2);
                }
            }
        }
        let err = Commit::from_cbor(&v, &Capabilities::v1()).unwrap_err();
        assert!(matches!(
            err,
            CoreError::Unsupported {
                kind: "commit",
                format: 2,
                ..
            }
        ));
    }

    #[test]
    fn unknown_fields_and_unknown_body_kinds_are_rejected() {
        let mut v = state().to_cbor();
        if let Value::Map(entries) = &mut v {
            entries.push((Value::text("extra"), Value::Uint(1)));
        }
        assert!(matches!(
            ObjectState::from_cbor(&v, &Capabilities::v1()),
            Err(CoreError::Invalid { .. })
        ));
        let mut s = state().to_cbor();
        if let Value::Map(entries) = &mut s {
            for (k, val) in entries.iter_mut() {
                if k.as_text() == Some("body") {
                    *val = Value::map([("kind", Value::text("hologram"))]);
                }
            }
        }
        assert!(matches!(
            ObjectState::from_cbor(&s, &Capabilities::v1()),
            Err(CoreError::Unsupported { .. })
        ));
    }

    #[test]
    fn commit_limits() {
        let mut c = commit();
        c.parents = vec![CommitId([5; 32]), CommitId([5; 32])];
        assert!(c.validate().is_err());
        c.parents = (0..33).map(|i| CommitId([i; 32])).collect();
        assert!(c.validate().is_err());
        c.parents.clear();
        c.required_capabilities = vec!["b".into(), "a".into()];
        assert!(c.validate().is_err());
        c.required_capabilities = normalize_capabilities(["b".to_owned(), "a".to_owned()]);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn conflict_roundtrips() {
        let c = Conflict {
            format: CONFLICT_FORMAT_V1,
            object_id: ObjectId([1; 16]),
            driver: "merge.text.v1".into(),
            base: vec![ObjectStateId([1; 32])],
            sides: vec![ObjectStateId([2; 32]), ObjectStateId([3; 32])],
            merged: None,
            terms: vec![ConflictTerm {
                path: "#0".into(),
                kind: TermKind::TextRegion,
                candidates: vec![Some(ContentId([4; 32])), None],
            }],
        };
        let bytes = c.encode().unwrap();
        assert_eq!(Conflict::decode(&bytes, &Capabilities::v1()).unwrap(), c);
        let mut bad = c.clone();
        bad.terms[0].candidates.pop();
        assert!(bad.validate().is_err());
    }
}
