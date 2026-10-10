//! The host-facing request and answer types: every request a host, a UI or
//! the CLI can make and every answer it gets back, as `#[luv_ffi]` types
//! that export to TypeScript (feature `ts`) and Swift (feature `uniffi`).
//! They need no store; the implementation is `crate::api` (feature `store`).

use luvcs_macros::luv_ffi;
use serde::{Deserialize, Serialize};

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VcsRequest {
    /// A new, empty repo.
    CreateRepo {
        name: String,
    },
    /// Repos in this store.
    ListRepos,
    /// The device's workspace in a repo: base, unrecorded changes, heads.
    Status {
        repo_id: String,
    },
    /// Saves a text object into the working tree. Omit `object_id` to
    /// create a new object.
    SaveText {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        object_id: Option<String>,
        type_id: String,
        text: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        title: Option<String>,
    },
    /// Saves a record object (JSON form, see [`record_from_json`]).
    SaveRecord {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        object_id: Option<String>,
        type_id: String,
        schema_id: String,
        record_json: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        title: Option<String>,
    },
    /// Saves a binary object: the bytes become a local blob of the repo's
    /// realm (deduplicated by content) and the working tree gets a
    /// `binary` state that references it.
    SaveBinary {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        object_id: Option<String>,
        type_id: String,
        data_base64: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        title: Option<String>,
    },
    /// Stores bytes as a local blob of the repo's realm without touching
    /// the working tree.
    PutBlob {
        repo_id: String,
        data_base64: String,
    },
    /// Reads `[offset, offset + length)` of a local blob (the whole blob
    /// when both are omitted). A whole read is verified against the
    /// blob's digest; a range read is not.
    ReadBlob {
        repo_id: String,
        blob_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        offset: Option<u64>,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        length: Option<u64>,
    },
    ListBlobs {
        repo_id: String,
    },
    DeleteObject {
        repo_id: String,
        object_id: String,
    },
    /// Reads one object from the working tree, or from `commit_id`.
    ReadObject {
        repo_id: String,
        object_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        commit_id: Option<String>,
    },
    /// Lists the objects of the working tree, or of `commit_id`.
    ListObjects {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        commit_id: Option<String>,
    },
    /// Records a commit from the working tree, or only from `objects`
    /// when given (the rest stays unrecorded).
    Record {
        repo_id: String,
        description: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        objects: Option<Vec<String>>,
    },
    Amend {
        repo_id: String,
        description: String,
    },
    Undo {
        repo_id: String,
    },
    Redo {
        repo_id: String,
    },
    /// Puts `object_id` as it was in `commit_id` back into the working tree.
    Restore {
        repo_id: String,
        commit_id: String,
        object_id: String,
    },
    /// Three-way merges `commit_id` into the workspace.
    Integrate {
        repo_id: String,
        commit_id: String,
    },
    Resolve {
        repo_id: String,
        object_id: String,
        resolution: VcsResolution,
    },
    Log {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        from_commit_id: Option<String>,
        limit: u32,
    },
    Show {
        repo_id: String,
        commit_id: String,
    },
    Heads {
        repo_id: String,
    },
    Operations {
        repo_id: String,
        limit: u32,
    },
    Export {
        repo_id: String,
    },
    /// Imports an exported archive as a new local repo. The archive names
    /// the repo; `id_key_base64` is the repo's ID key as the export gave it.
    Import {
        name: String,
        archive_base64: String,
        id_key_base64: String,
    },
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VcsResolution {
    /// Take one side of the conflict whole (0 = ours / left, 1 = theirs).
    Side {
        index: u32,
    },
    Text {
        text: String,
    },
    Record {
        record_json: String,
    },
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
// A serialized reply crossing the host boundary; its size in memory is
// irrelevant and boxing would change the generated bindings.
#[allow(clippy::large_enum_variant)]
pub enum VcsResponse {
    Repo {
        repo: VcsRepoInfo,
    },
    Repos {
        repos: Vec<VcsRepoInfo>,
    },
    Status {
        status: VcsStatus,
    },
    Object {
        object: VcsObjectView,
    },
    Objects {
        objects: Vec<VcsObjectView>,
    },
    Commit {
        commit: VcsCommitView,
    },
    Commits {
        commits: Vec<VcsCommitView>,
    },
    Heads {
        heads: Vec<String>,
    },
    Integrated {
        commit_id: String,
        conflicts: Vec<VcsConflictView>,
    },
    Operation {
        operation: VcsOperationView,
    },
    Operations {
        operations: Vec<VcsOperationView>,
    },
    /// An exported repo. `id_key_base64` is the repo's secret ID key: keep
    /// the archive as private as the content itself.
    Archive {
        repo_id: String,
        archive_base64: String,
        id_key_base64: String,
        objects: u32,
    },
    Imported {
        repo: VcsRepoInfo,
        objects: u32,
    },
    Blob {
        blob: VcsBlobInfo,
    },
    Blobs {
        blobs: Vec<VcsBlobInfo>,
    },
    BlobBytes {
        blob: VcsBlobInfo,
        offset: u64,
        data_base64: String,
    },
    Ok,
    Error {
        error: VcsError,
    },
}

/// A local blob of a repo's realm.
#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsBlobInfo {
    pub realm_id: String,
    pub blob_id: String,
    pub byte_length: u64,
    pub raw_digest_hex: String,
    pub created_at_unix_ms: i64,
}

/// The semantic blob reference a binary object carries.
#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsBlobRefView {
    pub realm_id: String,
    pub blob_id: String,
    pub byte_length: u64,
    /// Whether the bytes are on this device.
    pub present: bool,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsError {
    /// `invalid`, `not_found`, `stale`, `nothing_to_record`, `conflict`,
    /// `unsupported`, `host`, `internal`.
    pub code: String,
    pub message: String,
}

impl VcsError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid", message)
    }
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsRepoInfo {
    pub repo_id: String,
    pub name: String,
    pub workspace_id: String,
    pub created_at_unix_ms: i64,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsChange {
    pub object_id: String,
    /// `added`, `changed`, `removed`.
    pub kind: String,
    pub type_id: String,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub title: Option<String>,
    pub conflicted: bool,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsStatus {
    pub repo_id: String,
    pub workspace_id: String,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub base_commit_id: Option<String>,
    pub token: u64,
    pub changes: Vec<VcsChange>,
    pub conflicts: Vec<VcsConflictView>,
    pub heads: Vec<String>,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsObjectView {
    pub object_id: String,
    pub state_id: String,
    pub type_id: String,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub title: Option<String>,
    /// `text`, `record`, `binary`, `conflict`, `deleted`.
    pub body_kind: String,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub text: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub record_json: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub schema_id: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub conflict: Option<VcsConflictView>,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub blob: Option<VcsBlobRefView>,
    pub metadata_json: String,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsConflictTermView {
    pub path: String,
    /// `value`, `order`, `deleted_vs_edited`, `text_region`.
    pub kind: String,
    /// One candidate per side: text for text states, JSON for record states.
    /// `None` when that side has no content (including deletion).
    pub candidates: Vec<Option<String>>,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsConflictView {
    pub object_id: String,
    pub conflict_id: String,
    pub driver: String,
    /// Everything that did merge, as provisional full text (text driver).
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub merged_text: Option<String>,
    /// Provisional record including non-conflicting fields, in record JSON form.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub merged_record_json: Option<String>,
    /// Full text or record JSON of each side, for whole-side resolution.
    pub sides: Vec<Option<String>>,
    pub terms: Vec<VcsConflictTermView>,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsCommitView {
    pub commit_id: String,
    pub change_id: String,
    pub parents: Vec<String>,
    pub description: String,
    pub author_kind: String,
    pub author_device: String,
    pub created_at_unix_ms: i64,
    pub changed_objects: Vec<String>,
    pub is_head: bool,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsOperationView {
    pub seq: i64,
    pub operation_id: String,
    pub kind: String,
    pub reason: String,
    pub created_at_unix_ms: i64,
    pub undone: bool,
}
