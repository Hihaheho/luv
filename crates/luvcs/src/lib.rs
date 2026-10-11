//! luv core.
//!
//! Canonical CBOR, keyed IDs, `ObjectState` / `Commit` / `Conflict`, the
//! Patricia snapshot tree, `record.v1` values, merge drivers and (feature
//! `store`) the SQLite-backed local store with the request API on top.
//!
//! Hosts supply hashing through [`digest::Digest`]; with `rust-digest` the
//! crate brings a Rust implementation.

pub mod blob;
pub mod bookmark;
pub mod cbor;
pub mod digest;
pub mod id;
pub mod merge;
pub mod model;
pub mod record;
pub mod redaction;
pub mod remote;
pub mod tree;

#[cfg(feature = "store")]
pub mod api;
pub mod api_types;
#[cfg(feature = "store")]
pub mod blob_store;
#[cfg(feature = "store")]
pub mod store;
#[cfg(feature = "store")]
pub mod sync;

#[doc(hidden)]
pub use serde as __serde;

// UniFFI's derives look the type tag up at the crate root.
#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub use digest::{Digest, DigestError, Hash32};
pub use id::{
    BlobId, BookmarkId, BookmarkUpdateId, ChangeId, CommitId, ConflictId, ContentId, IdKey,
    ObjectId, ObjectStateId, OperationId, RealmId, RedactionId, RepoId, TreeId, WorkspaceId,
    content_id,
};
pub use model::{
    Actor, ActorKind, BlobRef, Capabilities, Commit, Conflict, ConflictTerm, CoreError, ObjectBody,
    ObjectState, TermKind,
};
pub use record::{Element, FieldPath, FieldPolicy, RecordSchema, RecordValue};
