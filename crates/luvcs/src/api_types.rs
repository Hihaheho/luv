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
    /// the working tree. The blob is pinned: the host refers to it from
    /// outside the object model (a record field, say), so push sends it and
    /// `Gc` keeps it until a `Purge` names it.
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
    /// Saves the object as it is in the working tree under a new title (no
    /// title when omitted), keeping its body and its other metadata: a
    /// rename or a move, with no need to send the content again.
    SetTitle {
        repo_id: String,
        object_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        title: Option<String>,
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
    /// Every bookmark (deleted and fused ones included), the repo default
    /// and the workspace's selection.
    ListBookmarks {
        repo_id: String,
    },
    CreateBookmark {
        repo_id: String,
        name: String,
        commit_id: String,
    },
    /// Changes a bookmark the caller listed with `token`: moves it
    /// (`commit_id`), renames it (`name`), or sets its `life` (delete,
    /// fuse into another, revive). Resolving a conflict is the same edit:
    /// give the chosen value for the field in conflict. `new_default`
    /// changes the repo default in the same operation, and is needed to
    /// delete the default bookmark.
    EditBookmark {
        repo_id: String,
        bookmark_id: String,
        token: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        name: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        commit_id: Option<String>,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        life: Option<VcsBookmarkLife>,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        new_default: Option<VcsNewDefault>,
    },
    /// Sets the repo default (none when `bookmark_id` is omitted); `token`
    /// is the listing's `default_token`.
    SetDefaultBookmark {
        repo_id: String,
        token: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        bookmark_id: Option<String>,
    },
    /// Selects the bookmark records here move forward (none when omitted).
    /// Local to this device.
    SelectBookmark {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        bookmark_id: Option<String>,
    },
    /// Records like `Record`, then moves the selected bookmark to the new
    /// commit if it is still as listed with `token` and the commit descends
    /// from where it points. The commit is recorded either way.
    RecordAdvancing {
        repo_id: String,
        description: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        objects: Option<Vec<String>>,
        token: String,
    },
    Export {
        repo_id: String,
    },
    /// Erases from all history the bodies `object_ids` ever had (text,
    /// record, binary, conflict terms) and the pinned blobs `blob_ids`. The
    /// objects must be deleted (or absent) in every head and in the working
    /// tree. IDs, titles and commit descriptions stay; reading an erased
    /// body reports `redacted`. A body another object here also has is
    /// kept (only this device's objects are known to the check).
    /// The erasure travels with the next push: every device that pulls it
    /// drops the bytes, and the remote drops them from its log. `reason`
    /// travels too, so it must not repeat what is erased. No undo.
    Purge {
        repo_id: String,
        object_ids: Vec<String>,
        #[serde(default)]
        blob_ids: Vec<String>,
        reason: String,
    },
    /// Deletes objects and blobs this device alone has that nothing
    /// reaches any more: drafts of the working tree, amended-away and
    /// undone commits that were never pushed. With `keep_operations`, first
    /// forgets all but that many newest operations (undo stops there).
    /// Anything pushed or pulled is kept.
    Gc {
        repo_id: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        keep_operations: Option<u32>,
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
    Side { index: u32 },
    /// Give the content. The resolved object keeps the metadata both sides
    /// agree on; `title` sets the title, and is required when the title is
    /// itself in conflict.
    Text {
        text: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        title: Option<String>,
    },
    Record {
        record_json: String,
        #[serde(default)]
        #[cfg_attr(feature = "ts", ts(optional = nullable))]
        title: Option<String>,
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
    Bookmarks {
        bookmarks: VcsBookmarks,
    },
    Bookmark {
        bookmark: VcsBookmarkView,
    },
    Recorded {
        commit: VcsCommitView,
        advance: VcsAdvance,
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
    Purged {
        /// Empty when there was nothing left to erase.
        redaction_ids: Vec<String>,
        contents: u32,
        blobs: u32,
        /// Bodies kept because another object has the same bytes.
        kept_shared: u32,
    },
    Collected {
        objects: u32,
        blobs: u32,
        operations: u32,
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

/// What kind of failure a `VcsError` reports; serialized as snake_case.
#[luv_ffi]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VcsErrorCode {
    Invalid,
    NotFound,
    Stale,
    NothingToRecord,
    UnrecordedChanges,
    Conflict,
    Unsupported,
    /// The bytes were erased by a redaction (see `Purge`).
    Redacted,
    Host,
    Internal,
}

impl VcsErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::NotFound => "not_found",
            Self::Stale => "stale",
            Self::NothingToRecord => "nothing_to_record",
            Self::UnrecordedChanges => "unrecorded_changes",
            Self::Conflict => "conflict",
            Self::Unsupported => "unsupported",
            Self::Redacted => "redacted",
            Self::Host => "host",
            Self::Internal => "internal",
        }
    }
}

impl std::fmt::Display for VcsErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsError {
    pub code: VcsErrorCode,
    pub message: String,
}

impl VcsError {
    pub fn new(code: VcsErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(VcsErrorCode::Invalid, message)
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
    /// The body was erased by a `Purge`: `text`, `record_json` or the blob's
    /// bytes are gone, the rest of the view is as it was.
    #[serde(default)]
    pub redacted: bool,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsConflictTermView {
    pub path: String,
    /// `value`, `order`, `deleted_vs_edited`, `text_region`, `metadata`
    /// (`path` is then the metadata key, e.g. `title`).
    pub kind: String,
    /// One candidate per side: text for text states, JSON for record states,
    /// the value for a text metadata key.
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
    /// For a text conflict: the full text with `<<<<<<<` / `=======` /
    /// `>>>>>>>` markers around each conflicting region, for a file a person
    /// edits. Display only; the conflict itself is never stored this way.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub marked_text: Option<String>,
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

/// Whether a bookmark exists; serialized as snake_case.
#[luv_ffi]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VcsBookmarkLifeKind {
    Live,
    Deleted,
    /// Merged into the bookmark `fused_into`, which carries on for it.
    Fused,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsBookmarkLife {
    pub kind: VcsBookmarkLifeKind,
    /// Set exactly when `kind` is `fused`.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub fused_into: Option<String>,
}

/// The repo default to set alongside a bookmark edit.
#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VcsNewDefault {
    Bookmark { bookmark_id: String },
    Unset,
}

/// One bookmark. A field in conflict is `None`, with its candidates in the
/// matching `*_candidates` list (empty otherwise). Nothing here picks a
/// candidate; the host shows them and an `EditBookmark` settles them.
#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsBookmarkView {
    pub bookmark_id: String,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub name: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub target_commit_id: Option<String>,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub life: Option<VcsBookmarkLife>,
    pub name_candidates: Vec<String>,
    pub target_candidates: Vec<String>,
    pub life_candidates: Vec<VcsBookmarkLife>,
    /// Another bookmark that is not deleted or fused goes by the same name,
    /// so the name alone does not pick this one.
    pub name_shared: bool,
    /// The repo default, after following fused bookmarks.
    pub is_default: bool,
    /// Selected in this device's workspace.
    pub is_selected: bool,
    /// Opaque; what the caller saw. Pass it back to change this bookmark.
    pub token: String,
}

#[luv_ffi]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VcsBookmarks {
    pub bookmarks: Vec<VcsBookmarkView>,
    /// The default as set, which may name a deleted or fused bookmark;
    /// `None` when unset or in conflict.
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub default_bookmark_id: Option<String>,
    /// When the default is in conflict: every candidate (`None` = unset).
    pub default_candidates: Vec<Option<String>>,
    /// Opaque; pass it to `SetDefaultBookmark`.
    pub default_token: String,
    #[cfg_attr(feature = "ts", ts(optional = nullable))]
    pub selected_bookmark_id: Option<String>,
}

/// What `RecordAdvancing` did to the selected bookmark.
#[luv_ffi]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum VcsAdvance {
    Advanced,
    NoSelection,
    /// The bookmark changed since the token was read.
    Stale,
    /// Its life or target is in conflict, or it is deleted or fused.
    Unresolved,
    /// The new commit does not descend from where it points.
    NotForward,
}
