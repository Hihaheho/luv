//! Redactions: the record that some bytes of a repo were erased.
//!
//! History is content-addressed, so erasing data cannot rewrite it: a
//! state names its content by ID, a tree names states, a commit names a
//! tree. A redaction keeps every ID and drops only the bytes behind some
//! content IDs and blob IDs. Commits, trees and states stay valid and
//! verifiable; reading a redacted body reports it as redacted.
//!
//! A redaction is an immutable object that travels like a commit, so every
//! device that takes it in erases the same bytes, and a remote is asked to
//! drop them from the entries before the one that carried it. Bytes written
//! again later (the same text saved anew) are new data and travel again.

use std::collections::BTreeSet;

use crate::cbor::{self, Value};
use crate::digest::{Digest, DigestError};
use crate::id::{BlobId, ContentId, Domain, IdKey, RedactionId, RepoId, keyed_id};
use crate::model::{
    Actor, Capabilities, CoreError, Fields, caps_value, check_support, validate_capabilities,
};

pub const REDACTION_FORMAT_V1: u32 = 1;
/// The capability every redaction requires.
pub const REDACTION_CAPABILITY_V1: &str = "redaction.v1";
/// Most IDs one redaction names, so it fits a remote entry with room to
/// spare; a larger erasure is split over several redactions.
pub const MAX_REDACTED_IDS: usize = 16 * 1024;
pub const MAX_REASON_BYTES: usize = 1024;

const KIND: &str = "redaction";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub format: u32,
    pub repo_id: RepoId,
    /// Sorted, unique.
    pub contents: Vec<ContentId>,
    /// Sorted, unique.
    pub blobs: Vec<BlobId>,
    pub author: Actor,
    pub created_at_unix_ms: i64,
    /// Why, for the people who read the history. It travels to every
    /// device and the remote, so it must not repeat what was erased.
    pub reason: String,
    pub required_capabilities: Vec<String>,
}

impl Redaction {
    /// A version-1 redaction.
    pub fn new(
        repo_id: RepoId,
        contents: impl IntoIterator<Item = ContentId>,
        blobs: impl IntoIterator<Item = BlobId>,
        author: Actor,
        created_at_unix_ms: i64,
        reason: &str,
    ) -> Self {
        let contents: BTreeSet<ContentId> = contents.into_iter().collect();
        let blobs: BTreeSet<BlobId> = blobs.into_iter().collect();
        Self {
            format: REDACTION_FORMAT_V1,
            repo_id,
            contents: contents.into_iter().collect(),
            blobs: blobs.into_iter().collect(),
            author,
            created_at_unix_ms,
            reason: reason.to_owned(),
            required_capabilities: vec![REDACTION_CAPABILITY_V1.to_owned()],
        }
    }

    pub fn to_cbor(&self) -> Value {
        Value::map([
            ("format", Value::Uint(u64::from(self.format))),
            ("repo_id", self.repo_id.to_cbor()),
            (
                "contents",
                Value::Array(self.contents.iter().map(ContentId::to_cbor).collect()),
            ),
            (
                "blobs",
                Value::Array(self.blobs.iter().map(BlobId::to_cbor).collect()),
            ),
            ("author", self.author.to_cbor()),
            ("created_at_unix_ms", Value::int(self.created_at_unix_ms)),
            ("reason", Value::text(self.reason.clone())),
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
        if self.format != REDACTION_FORMAT_V1 {
            return Err(CoreError::invalid(KIND, "format is not 1"));
        }
        if self.contents.is_empty() && self.blobs.is_empty() {
            return Err(CoreError::invalid(KIND, "redacts nothing"));
        }
        if self.contents.len() + self.blobs.len() > MAX_REDACTED_IDS {
            return Err(CoreError::invalid(
                KIND,
                format!("names more than {MAX_REDACTED_IDS} ids"),
            ));
        }
        if self.contents.windows(2).any(|p| p[0] >= p[1])
            || self.blobs.windows(2).any(|p| p[0] >= p[1])
        {
            return Err(CoreError::invalid(KIND, "ids must be sorted and unique"));
        }
        if self.reason.len() > MAX_REASON_BYTES {
            return Err(CoreError::invalid(
                KIND,
                format!("reason exceeds {MAX_REASON_BYTES} bytes"),
            ));
        }
        validate_capabilities(KIND, &self.required_capabilities)
    }

    pub fn decode(bytes: &[u8], caps: &Capabilities) -> Result<Self, CoreError> {
        Self::from_cbor(&cbor::decode(bytes)?, caps)
    }

    pub fn from_cbor(value: &Value, caps: &Capabilities) -> Result<Self, CoreError> {
        let f = Fields::new(
            KIND,
            value,
            &[
                "format",
                "repo_id",
                "contents",
                "blobs",
                "author",
                "created_at_unix_ms",
                "reason",
                "required_capabilities",
            ],
        )?;
        let format = f.u32("format")?;
        let required_capabilities = f.capabilities()?;
        check_support(
            KIND,
            format,
            REDACTION_FORMAT_V1,
            &required_capabilities,
            caps,
        )?;
        let redaction = Self {
            format,
            repo_id: f.id("repo_id", RepoId::from_cbor)?,
            contents: f.ids("contents", ContentId::from_cbor)?,
            blobs: f.ids("blobs", BlobId::from_cbor)?,
            author: Actor::from_cbor(f.get("author")?)?,
            created_at_unix_ms: f.i64("created_at_unix_ms")?,
            reason: f.text("reason", MAX_REASON_BYTES)?,
            required_capabilities,
        };
        redaction.validate()?;
        Ok(redaction)
    }

    pub fn id<D: Digest>(&self, digest: &D, key: &IdKey) -> Result<RedactionId, DigestError> {
        Ok(RedactionId(keyed_id(
            digest,
            key,
            Domain::Redaction,
            self.format,
            self.to_cbor(),
        )?))
    }
}

#[cfg(all(test, feature = "rust-digest"))]
mod tests {
    use super::*;
    use crate::digest::RustDigest;
    use crate::model::ActorKind;

    fn actor() -> Actor {
        Actor {
            kind: ActorKind::User,
            device_id: "d".into(),
            description: String::new(),
        }
    }

    #[test]
    fn round_trips_canonically_and_refuses_empty_or_unsorted() {
        let r = Redaction::new(
            RepoId([1; 16]),
            [ContentId([3; 32]), ContentId([2; 32]), ContentId([3; 32])],
            [],
            actor(),
            5,
            "erasure request",
        );
        assert_eq!(r.contents, vec![ContentId([2; 32]), ContentId([3; 32])]);
        let bytes = r.encode().unwrap();
        let back = Redaction::decode(&bytes, &Capabilities::v1()).unwrap();
        assert_eq!(back, r);
        assert_eq!(
            back.id(&RustDigest, &IdKey([9; 32])).unwrap(),
            r.id(&RustDigest, &IdKey([9; 32])).unwrap()
        );

        let empty = Redaction::new(RepoId([1; 16]), [], [], actor(), 5, "");
        assert!(empty.encode().is_err());
        let mut unsorted = r.clone();
        unsorted.contents.reverse();
        assert!(unsorted.encode().is_err());
        // A reader without the capability refuses it.
        assert!(matches!(
            Redaction::decode(&bytes, &Capabilities::default()),
            Err(CoreError::Unsupported { .. })
        ));
    }
}
