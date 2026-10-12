//! The local canonical store: immutable objects, commits,
//! workspaces, the operation log with undo / redo, and the integrate /
//! resolve path. Stage 1: no Blob transport, no push.
//!
//! Every command runs in one SQLite transaction, so a crash leaves either
//! the whole operation or nothing. Immutable objects are append-only and a
//! logical ID is never overwritten: storing different bytes under an existing
//! ID is an [`StoreError::IdentityConflict`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::bookmark::BookmarkUpdate;
use crate::cbor::{self, Value};
use crate::digest::{Digest, DigestError};
use crate::id::{
    BlobId, BookmarkId, BookmarkUpdateId, ChangeId, CommitId, ConflictId, ContentId, IdKey,
    ObjectId, ObjectStateId, OperationId, RepoId, TreeId, WorkspaceId, content_id,
};
use crate::merge::{self, RECORD_DRIVER_V1, TEXT_DRIVER_V1};
use crate::model::{
    Actor, COMMIT_FORMAT_V1, CONFLICT_FORMAT_V1, Capabilities, Commit, Conflict, ConflictTerm,
    CoreError, ObjectBody, ObjectState, TermKind,
};
use crate::record::{RecordSchema, RecordValue};
use crate::redaction::Redaction;
use crate::tree::{self, Node, TreeError};

mod bookmarks;
mod retention;
pub use bookmarks::{Advance, BookmarkEdit, BookmarkEntry};
pub use retention::{GcOutcome, PurgeOutcome, Reach, Roots};

pub const STORE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Digest(#[from] DigestError),
    #[error("{0}")]
    Cbor(#[from] crate::cbor::CborError),
    #[error("{0}")]
    Tree(#[from] TreeError),
    #[error("record: {0}")]
    Record(#[from] crate::record::RecordError),
    #[error("repo {0} is not in this store")]
    UnknownRepo(RepoId),
    #[error("repo {0} already exists")]
    RepoExists(RepoId),
    #[error("workspace {0} is not in this store")]
    UnknownWorkspace(WorkspaceId),
    #[error("commit {0} is not in this store")]
    UnknownCommit(CommitId),
    #[error("object {kind} {id} is not in this store")]
    MissingObject { kind: &'static str, id: String },
    #[error("{kind} {id} was redacted")]
    Redacted { kind: &'static str, id: String },
    #[error("workspace token is {actual}, expected {expected}")]
    StaleToken { expected: u64, actual: u64 },
    #[error("logical id {id} already holds different bytes")]
    IdentityConflict { id: String },
    #[error("nothing to record: the snapshot equals the base")]
    NothingToRecord,
    #[error("the working tree has unrecorded changes; record them first")]
    UnrecordedChanges,
    #[error("workspace has no candidate")]
    NoCandidate,
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
    #[error("record schema {0} is not registered")]
    UnknownSchema(String),
    #[error("operation {0} was already applied")]
    DuplicateOperation(OperationId),
    #[error("binding {0} is not adopted by this database")]
    UnknownBinding(String),
    #[error("bookmark {0} is not in this repo")]
    UnknownBookmark(BookmarkId),
    #[error("the bookmark heads changed since they were read")]
    StaleBookmark,
    #[error("another bookmark is named {0:?}")]
    NameTaken(String),
    #[error("bookmark {0} is the default; give a new default to delete it")]
    BookmarkIsDefault(BookmarkId),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// (seq, operation_id, kind, reason, created_at, before, after)
type OpRow = (i64, Vec<u8>, String, String, i64, Vec<u8>, Vec<u8>);

/// Who, where and why for one atomic operation.
#[derive(Debug, Clone)]
pub struct OperationContext {
    pub operation_id: OperationId,
    pub workspace_id: WorkspaceId,
    /// The token the caller last observed; a mismatch aborts the operation.
    pub expected_token: Option<u64>,
    pub actor: Actor,
    pub reason: String,
    pub now_unix_ms: i64,
}

/// A workspace's durable position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub base: Option<CommitId>,
    pub working: TreeId,
    pub candidate: Option<TreeId>,
    /// The bookmark a record here moves forward, if any.
    pub selected_bookmark: Option<BookmarkId>,
    pub token: u64,
}

impl Workspace {
    fn to_cbor(&self) -> Value {
        Value::map([
            ("id", self.id.to_cbor()),
            (
                "base",
                self.base.as_ref().map_or(Value::Null, CommitId::to_cbor),
            ),
            ("working", self.working.to_cbor()),
            (
                "candidate",
                self.candidate.as_ref().map_or(Value::Null, TreeId::to_cbor),
            ),
            (
                "selected_bookmark",
                self.selected_bookmark
                    .as_ref()
                    .map_or(Value::Null, BookmarkId::to_cbor),
            ),
            ("token", Value::Uint(self.token)),
        ])
    }

    fn from_cbor(v: &Value) -> Result<Self> {
        let bad = || StoreError::Invalid("corrupt workspace snapshot".into());
        Ok(Self {
            id: v
                .get("id")
                .and_then(WorkspaceId::from_cbor)
                .ok_or_else(bad)?,
            base: match v.get("base") {
                Some(Value::Null) | None => None,
                Some(x) => Some(CommitId::from_cbor(x).ok_or_else(bad)?),
            },
            working: v
                .get("working")
                .and_then(TreeId::from_cbor)
                .ok_or_else(bad)?,
            candidate: match v.get("candidate") {
                Some(Value::Null) | None => None,
                Some(x) => Some(TreeId::from_cbor(x).ok_or_else(bad)?),
            },
            // Absent in snapshots written before bookmarks.
            selected_bookmark: match v.get("selected_bookmark") {
                Some(Value::Null) | None => None,
                Some(x) => Some(BookmarkId::from_cbor(x).ok_or_else(bad)?),
            },
            token: v.get("token").and_then(Value::as_u64).ok_or_else(bad)?,
        })
    }
}

/// What to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordSource {
    Working,
    Candidate,
}

/// One object's new state in a `save_working` call. `None` deletes it from
/// the working tree (as opposed to recording an explicit `Deleted` state).
pub type WorkingChange = (ObjectId, Option<ObjectStateId>);

/// Result of an integrate: either a clean merge commit or a merge commit
/// whose tree carries `Conflict` states for the listed objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrateOutcome {
    pub commit: CommitId,
    pub conflicts: Vec<(ObjectId, ConflictId)>,
}

/// A commit with its ID, for history listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitEntry {
    pub id: CommitId,
    pub commit: Commit,
}

/// The log entry of one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationEntry {
    pub seq: i64,
    pub id: OperationId,
    pub kind: String,
    pub reason: String,
    pub created_at_unix_ms: i64,
    pub undone: bool,
}

/// Record schemas by schema id, for the record merge driver.
#[derive(Debug, Clone, Default)]
pub struct SchemaRegistry {
    schemas: HashMap<String, RecordSchema>,
}

impl SchemaRegistry {
    pub fn register(mut self, schema: RecordSchema) -> Self {
        self.schemas.insert(schema.id.clone(), schema);
        self
    }

    pub fn get(&self, id: &str) -> Option<&RecordSchema> {
        self.schemas.get(id)
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS vcs_meta (
  key TEXT PRIMARY KEY NOT NULL,
  value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_repos (
  repo_id BLOB PRIMARY KEY NOT NULL,
  created_at_unix_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_objects (
  repo_id BLOB NOT NULL,
  kind TEXT NOT NULL,
  logical_id BLOB NOT NULL,
  bytes BLOB NOT NULL,
  PRIMARY KEY (repo_id, kind, logical_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_commits (
  repo_id BLOB NOT NULL,
  commit_id BLOB NOT NULL,
  change_id BLOB NOT NULL,
  root BLOB NOT NULL,
  created_at_unix_ms INTEGER NOT NULL,
  PRIMARY KEY (repo_id, commit_id)
) STRICT;
CREATE INDEX IF NOT EXISTS vcs_commits_change ON vcs_commits (repo_id, change_id);
CREATE TABLE IF NOT EXISTS vcs_commit_parents (
  repo_id BLOB NOT NULL,
  commit_id BLOB NOT NULL,
  ordinal INTEGER NOT NULL,
  parent_id BLOB NOT NULL,
  PRIMARY KEY (repo_id, commit_id, ordinal)
) STRICT;
CREATE INDEX IF NOT EXISTS vcs_commit_parents_parent ON vcs_commit_parents (repo_id, parent_id);
CREATE TABLE IF NOT EXISTS vcs_visible (
  repo_id BLOB NOT NULL,
  commit_id BLOB NOT NULL,
  PRIMARY KEY (repo_id, commit_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_rewrites (
  repo_id BLOB NOT NULL,
  operation_id BLOB NOT NULL,
  from_commit BLOB NOT NULL,
  to_commit BLOB NOT NULL,
  PRIMARY KEY (repo_id, operation_id, from_commit, to_commit)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_bookmark_updates (
  repo_id BLOB NOT NULL,
  update_id BLOB NOT NULL,
  subject BLOB NOT NULL,
  PRIMARY KEY (repo_id, update_id)
) STRICT;
CREATE INDEX IF NOT EXISTS vcs_bookmark_updates_subject ON vcs_bookmark_updates (repo_id, subject);
CREATE TABLE IF NOT EXISTS vcs_workspaces (
  repo_id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  snapshot BLOB NOT NULL,
  PRIMARY KEY (repo_id, workspace_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_operations (
  repo_id BLOB NOT NULL,
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  operation_id BLOB NOT NULL,
  kind TEXT NOT NULL,
  reason TEXT NOT NULL,
  created_at_unix_ms INTEGER NOT NULL,
  before BLOB NOT NULL,
  after BLOB NOT NULL,
  undone INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS vcs_operations_id ON vcs_operations (repo_id, operation_id);
CREATE INDEX IF NOT EXISTS vcs_operations_undone ON vcs_operations (repo_id, undone, seq);
CREATE TABLE IF NOT EXISTS vcs_redacted (
  repo_id BLOB NOT NULL,
  kind TEXT NOT NULL,
  logical_id BLOB NOT NULL,
  PRIMARY KEY (repo_id, kind, logical_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_operation_objects (
  repo_id BLOB NOT NULL,
  operation_id BLOB NOT NULL,
  ordinal INTEGER NOT NULL,
  kind TEXT NOT NULL,
  logical_id BLOB NOT NULL,
  PRIMARY KEY (repo_id, operation_id, ordinal)
) STRICT;
"#;

/// The store over a borrowed connection, creating the schema if needed.
/// Hosts keep their own connection (a `SqliteDb`, a `WasmDb`, a file) and
/// lend it for each call; transactions use `unchecked_transaction`, so the
/// caller must not hold another transaction on the same connection.
pub struct Store<'c, D: Digest> {
    conn: &'c Connection,
    digest: D,
    caps: Capabilities,
    schemas: SchemaRegistry,
}

impl<'c, D: Digest> Store<'c, D> {
    pub fn open(conn: &'c Connection, digest: D) -> Result<Self> {
        conn.execute_batch(SCHEMA_SQL)?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM vcs_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match version.as_deref() {
            None => {
                conn.execute(
                    "INSERT INTO vcs_meta (key, value) VALUES ('schema_version', ?1)",
                    params![STORE_SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) if v == STORE_SCHEMA_VERSION.to_string() => {}
            Some(v) => {
                return Err(StoreError::Invalid(format!(
                    "store schema version {v} is newer than this build ({STORE_SCHEMA_VERSION})"
                )));
            }
        }
        Ok(Self {
            conn,
            digest,
            caps: Capabilities::v1(),
            schemas: SchemaRegistry::default(),
        })
    }

    pub fn with_schemas(mut self, schemas: SchemaRegistry) -> Self {
        self.schemas = schemas;
        self
    }

    pub fn connection(&self) -> &Connection {
        self.conn
    }

    pub fn create_repo(&mut self, repo_id: RepoId, now_unix_ms: i64) -> Result<()> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO vcs_repos (repo_id, created_at_unix_ms) VALUES (?1, ?2)",
            params![repo_id.to_vec(), now_unix_ms],
        )?;
        if n == 0 {
            return Err(StoreError::RepoExists(repo_id));
        }
        Ok(())
    }

    pub fn repos(&self) -> Result<Vec<RepoId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT repo_id FROM vcs_repos ORDER BY created_at_unix_ms, repo_id")?;
        let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| RepoId::from_bytes(&r?).map_err(|e| StoreError::Invalid(e.to_string())))
            .collect()
    }

    /// A handle on one repo under its stable ID key.
    pub fn repo(&mut self, repo_id: RepoId, key: IdKey) -> Result<Repo<'_, 'c, D>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM vcs_repos WHERE repo_id = ?1",
                params![repo_id.to_vec()],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::UnknownRepo(repo_id));
        }
        Ok(Repo {
            store: self,
            repo_id,
            key,
        })
    }
}

pub struct Repo<'s, 'c, D: Digest> {
    store: &'s mut Store<'c, D>,
    repo_id: RepoId,
    key: IdKey,
}

impl<'c, D: Digest> Repo<'_, 'c, D> {
    /// The VCS database connection this repository lives in.
    pub fn connection(&self) -> &'c Connection {
        self.store.conn
    }

    pub fn repo_id(&self) -> RepoId {
        self.repo_id
    }

    pub fn digest(&self) -> &D {
        &self.store.digest
    }

    /// The blob realm of this repo (v1: the repo itself).
    pub fn realm(&self) -> crate::blob::Realm {
        crate::blob_store::repo_realm(self.repo_id, &self.key)
    }

    /// The stored bytes of one immutable object, by kind and logical id.
    pub fn object_bytes(&self, kind: &str, logical_id: &[u8]) -> Result<Option<Vec<u8>>> {
        self.reader().object(kind, logical_id)
    }

    /// Parents of a commit, in order.
    pub fn parents(&self, id: CommitId) -> Result<Vec<CommitId>> {
        self.reader().parents(id)
    }

    /// The node ids of a snapshot tree, verified while walked.
    pub fn tree_nodes(&self, root: TreeId) -> Result<Vec<TreeId>> {
        let reader = self.reader();
        let mut nodes = Vec::new();
        let mut err = None;
        let entries = tree::walk_verified(&self.store.digest, &self.key, root, |id| {
            nodes.push(*id);
            match reader.object(KIND_TREE, id.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    None
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        entries?;
        Ok(nodes)
    }

    /// Whether `ancestor` is `commit` itself or reachable through parents.
    pub fn is_ancestor(&self, ancestor: CommitId, commit: CommitId) -> Result<bool> {
        self.reader().is_ancestor(ancestor, commit)
    }

    /// Imports verified immutable objects `(kind, logical id, bytes)` that
    /// arrived through replication: every object is re-hashed under this
    /// repo's key, commits are indexed, and an identity conflict aborts
    /// the whole import. Returns how many objects were taken in.
    pub fn import_objects(&mut self, objects: &[(String, Vec<u8>, Vec<u8>)]) -> Result<usize> {
        let tx = self.store.conn.unchecked_transaction()?;
        let count = self.import_objects_tx(
            &tx,
            objects
                .iter()
                .map(|(k, i, b)| (k.as_str(), i.as_slice(), b.as_slice())),
        )?;
        tx.commit()?;
        Ok(count)
    }

    /// Makes a replicated commit a head: it joins the visible set and every
    /// visible ancestor of it leaves; a commit that is already an ancestor
    /// of a head changes nothing.
    pub fn merge_visible(&mut self, commit_id: CommitId) -> Result<()> {
        self.commit(commit_id)?;
        self.tree_entries(self.commit(commit_id)?.root)?;
        let tx = self.store.conn.unchecked_transaction()?;
        let visible = self.visible()?;
        let mut covered = false;
        for v in &visible {
            if *v == commit_id || self.is_ancestor(commit_id, *v)? {
                covered = true;
            } else if self.is_ancestor(*v, commit_id)? {
                tx.execute(
                    "DELETE FROM vcs_visible WHERE repo_id = ?1 AND commit_id = ?2",
                    params![self.repo_id.as_bytes(), v.to_vec()],
                )?;
            }
        }
        if !covered {
            tx.execute(
                "INSERT OR IGNORE INTO vcs_visible (repo_id, commit_id) VALUES (?1, ?2)",
                params![self.repo_id.as_bytes(), commit_id.to_vec()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

const KIND_CONTENT: &str = "content";
const KIND_STATE: &str = "state";
const KIND_TREE: &str = "tree";
const KIND_COMMIT: &str = "commit";

/// Whether the store holds commit `commit_id` of repo `repo_id`, without
/// opening the repo (no key needed): the question a binding asks to tell
/// "the content has not reached this device" from "the projection is not
/// built yet". A store without the schema holds nothing.
pub fn has_commit_object(
    conn: &Connection,
    repo_id: RepoId,
    commit_id: CommitId,
) -> rusqlite::Result<bool> {
    let has_table: bool = conn.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'vcs_objects'",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(false);
    }
    conn.query_row(
        "SELECT count(*) > 0 FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
        rusqlite::params![repo_id.as_bytes(), KIND_COMMIT, commit_id.as_bytes()],
        |r| r.get(0),
    )
}
const KIND_CONFLICT: &str = "conflict";
const KIND_BOOKMARK_UPDATE: &str = "bookmark_update";
const KIND_REDACTION: &str = "redaction";

/// Read side shared by `Repo` and the in-transaction executor.
struct Reader<'a> {
    conn: &'a Connection,
    repo: &'a [u8],
}

impl Reader<'_> {
    fn object(&self, kind: &str, id: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT bytes FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                params![self.repo, kind, id],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn workspace(&self, id: WorkspaceId) -> Result<Workspace> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT snapshot FROM vcs_workspaces WHERE repo_id = ?1 AND workspace_id = ?2",
                params![self.repo, id.to_vec()],
                |r| r.get(0),
            )
            .optional()?;
        let bytes = bytes.ok_or(StoreError::UnknownWorkspace(id))?;
        Workspace::from_cbor(&cbor::decode(&bytes)?)
    }

    fn commit(&self, id: CommitId, caps: &Capabilities) -> Result<Commit> {
        let bytes = self
            .object(KIND_COMMIT, id.as_bytes())?
            .ok_or(StoreError::UnknownCommit(id))?;
        Ok(Commit::decode(&bytes, caps)?)
    }

    fn state(&self, id: ObjectStateId, caps: &Capabilities) -> Result<ObjectState> {
        let bytes =
            self.object(KIND_STATE, id.as_bytes())?
                .ok_or_else(|| StoreError::MissingObject {
                    kind: KIND_STATE,
                    id: id.to_string(),
                })?;
        Ok(ObjectState::decode(&bytes, caps)?)
    }

    fn tree_entries<D: Digest>(
        &self,
        digest: &D,
        key: &IdKey,
        root: TreeId,
    ) -> Result<BTreeMap<ObjectId, ObjectStateId>> {
        let mut err = None;
        let entries = tree::walk_verified(digest, key, root, |id| {
            match self.object(KIND_TREE, id.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    None
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        Ok(entries?)
    }

    fn tree_lookup<D: Digest>(
        &self,
        digest: &D,
        key: &IdKey,
        root: TreeId,
        object: ObjectId,
    ) -> Result<Option<ObjectStateId>> {
        let mut err = None;
        let found = tree::lookup(digest, key, root, &object, |id| {
            match self.object(KIND_TREE, id.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    None
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        Ok(found?)
    }

    fn tree_diff<D: Digest>(
        &self,
        digest: &D,
        key: &IdKey,
        before: TreeId,
        after: TreeId,
    ) -> Result<tree::TreeDiff> {
        let mut err = None;
        let diff = tree::diff_trees(digest, key, before, after, |id| {
            match self.object(KIND_TREE, id.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    None
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        Ok(diff?)
    }

    fn visible(&self) -> Result<BTreeSet<CommitId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT commit_id FROM vcs_visible WHERE repo_id = ?1")?;
        let rows = stmt.query_map(params![self.repo], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| CommitId::from_bytes(&r?).map_err(|e| StoreError::Invalid(e.to_string())))
            .collect()
    }

    /// Whether `ancestor` is `commit` itself or reachable through parents.
    fn is_ancestor(&self, ancestor: CommitId, commit: CommitId) -> Result<bool> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![commit];
        while let Some(c) = stack.pop() {
            if c == ancestor {
                return Ok(true);
            }
            if !seen.insert(c) {
                continue;
            }
            stack.extend(self.parents(c)?);
        }
        Ok(false)
    }

    fn parents(&self, id: CommitId) -> Result<Vec<CommitId>> {
        let mut stmt = self.conn.prepare(
            "SELECT parent_id FROM vcs_commit_parents WHERE repo_id = ?1 AND commit_id = ?2 ORDER BY ordinal",
        )?;
        let rows = stmt.query_map(params![self.repo, id.to_vec()], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| CommitId::from_bytes(&r?).map_err(|e| StoreError::Invalid(e.to_string())))
            .collect()
    }
}

/// Pending immutable writes of one operation, applied on commit.
#[derive(Default)]
struct Pending {
    objects: Vec<(&'static str, Vec<u8>, Vec<u8>)>,
}

impl<D: Digest> Repo<'_, '_, D> {
    pub fn id(&self) -> RepoId {
        self.repo_id
    }

    fn reader(&self) -> Reader<'_> {
        Reader {
            conn: self.store.conn,
            repo: self.repo_id.as_bytes(),
        }
    }

    // ----- immutable objects ------------------------------------------------

    /// Stores an immutable object; `true` when it was not there before.
    fn put_object_tx(
        tx: &Transaction<'_>,
        repo: &[u8],
        kind: &str,
        id: &[u8],
        bytes: &[u8],
    ) -> Result<bool> {
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT bytes FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                params![repo, kind, id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(old) if old == bytes => Ok(false),
            Some(_) => Err(StoreError::IdentityConflict {
                id: format!("{kind}:{}", hex::encode(id)),
            }),
            None => {
                tx.execute(
                    "INSERT INTO vcs_objects (repo_id, kind, logical_id, bytes) VALUES (?1, ?2, ?3, ?4)",
                    params![repo, kind, id, bytes],
                )?;
                Ok(true)
            }
        }
    }

    /// Stores exact bytes and returns their content ID. Idempotent.
    pub fn put_content(&mut self, bytes: &[u8]) -> Result<ContentId> {
        let id = content_id(&self.store.digest, &self.key, bytes)?;
        let tx = self.store.conn.unchecked_transaction()?;
        Self::put_object_tx(
            &tx,
            self.repo_id.as_bytes(),
            KIND_CONTENT,
            id.as_bytes(),
            bytes,
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn get_content(&self, id: ContentId) -> Result<Option<Vec<u8>>> {
        self.reader().object(KIND_CONTENT, id.as_bytes())
    }

    /// Stores an object state and returns its ID. Idempotent.
    pub fn put_state(&mut self, state: &ObjectState) -> Result<ObjectStateId> {
        let bytes = state.encode()?;
        let id = state.id(&self.store.digest, &self.key)?;
        let tx = self.store.conn.unchecked_transaction()?;
        Self::put_object_tx(
            &tx,
            self.repo_id.as_bytes(),
            KIND_STATE,
            id.as_bytes(),
            &bytes,
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn get_state(&self, id: ObjectStateId) -> Result<ObjectState> {
        self.reader().state(id, &self.store.caps)
    }

    pub fn get_conflict(&self, id: ConflictId) -> Result<Conflict> {
        let bytes = self
            .reader()
            .object(KIND_CONFLICT, id.as_bytes())?
            .ok_or_else(|| StoreError::MissingObject {
                kind: KIND_CONFLICT,
                id: id.to_string(),
            })?;
        Ok(Conflict::decode(&bytes, &self.store.caps)?)
    }

    /// Convenience: store text bytes and a `Text` state for `object_id`.
    pub fn put_text_state(
        &mut self,
        object_id: ObjectId,
        type_id: &str,
        text: &[u8],
        metadata: Value,
    ) -> Result<ObjectStateId> {
        let content = self.put_content(text)?;
        let state = ObjectState::new(object_id, type_id, ObjectBody::Text { content })
            .with_metadata(metadata);
        self.put_state(&state)
    }

    /// Convenience: store a record and a `Record` state for `object_id`.
    pub fn put_record_state(
        &mut self,
        object_id: ObjectId,
        type_id: &str,
        schema: &str,
        record: &RecordValue,
        metadata: Value,
    ) -> Result<ObjectStateId> {
        let content = self.put_content(&record.encode()?)?;
        let state = ObjectState::new(
            object_id,
            type_id,
            ObjectBody::Record {
                content,
                schema: schema.to_owned(),
            },
        )
        .with_metadata(metadata);
        self.put_state(&state)
    }

    // ----- commits and trees --------------------------------------------------

    pub fn commit(&self, id: CommitId) -> Result<Commit> {
        self.reader().commit(id, &self.store.caps)
    }

    pub fn has_commit(&self, id: CommitId) -> Result<bool> {
        Ok(self.reader().object(KIND_COMMIT, id.as_bytes())?.is_some())
    }

    pub fn tree_entries(&self, root: TreeId) -> Result<BTreeMap<ObjectId, ObjectStateId>> {
        self.reader()
            .tree_entries(&self.store.digest, &self.key, root)
    }

    /// The state `object` has in the tree at `root`, reading only its path.
    pub fn lookup(&self, root: TreeId, object: ObjectId) -> Result<Option<ObjectStateId>> {
        self.reader()
            .tree_lookup(&self.store.digest, &self.key, root, object)
    }

    /// What changes from the tree at `before` (`None`: empty) to the one at
    /// `after`, reading only the parts that differ.
    pub fn diff(&self, before: Option<TreeId>, after: TreeId) -> Result<tree::TreeDiff> {
        match before {
            Some(before) => self
                .reader()
                .tree_diff(&self.store.digest, &self.key, before, after),
            None => Ok(tree::TreeDiff {
                added: self.tree_entries(after)?.into_iter().collect(),
                ..tree::TreeDiff::default()
            }),
        }
    }

    /// Entries of a commit's snapshot.
    pub fn snapshot(&self, id: CommitId) -> Result<BTreeMap<ObjectId, ObjectStateId>> {
        let c = self.commit(id)?;
        self.tree_entries(c.root)
    }

    /// Commits with no visible descendant: the repo's current heads.
    pub fn visible(&self) -> Result<Vec<CommitId>> {
        Ok(self.reader().visible()?.into_iter().collect())
    }

    /// All commits sharing a change ID, oldest first: the rewrite history
    /// of one change.
    pub fn commits_of_change(&self, change_id: ChangeId) -> Result<Vec<CommitId>> {
        let mut stmt = self.store.conn.prepare(
            "SELECT commit_id FROM vcs_commits WHERE repo_id = ?1 AND change_id = ?2 ORDER BY rowid",
        )?;
        let rows = stmt.query_map(params![self.repo_id.as_bytes(), change_id.to_vec()], |r| {
            r.get::<_, Vec<u8>>(0)
        })?;
        rows.map(|r| CommitId::from_bytes(&r?).map_err(|e| StoreError::Invalid(e.to_string())))
            .collect()
    }

    /// Ancestors of `from` (inclusive), breadth-first by parent order, up to
    /// `limit` entries.
    pub fn history(&self, from: CommitId, limit: usize) -> Result<Vec<CommitEntry>> {
        let reader = self.reader();
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([from]);
        while let Some(id) = queue.pop_front() {
            if out.len() >= limit {
                break;
            }
            if !seen.insert(id) {
                continue;
            }
            let commit = reader.commit(id, &self.store.caps)?;
            queue.extend(commit.parents.iter().copied());
            out.push(CommitEntry { id, commit });
        }
        Ok(out)
    }

    /// The commits along `from`'s first-parent line that changed
    /// `object_id`, newest first, each with the state they introduced
    /// (`None` = removed in that commit).
    pub fn object_history(
        &self,
        from: CommitId,
        object_id: ObjectId,
        limit: usize,
    ) -> Result<Vec<(CommitId, Option<ObjectStateId>)>> {
        let reader = self.reader();
        let mut out = Vec::new();
        let mut cur = Some(from);
        while let Some(id) = cur {
            if out.len() >= limit {
                break;
            }
            let commit = reader.commit(id, &self.store.caps)?;
            let state =
                reader.tree_lookup(&self.store.digest, &self.key, commit.root, object_id)?;
            let parent = commit.parents.first().copied();
            let parent_state = match parent {
                Some(p) => {
                    let pc = reader.commit(p, &self.store.caps)?;
                    reader.tree_lookup(&self.store.digest, &self.key, pc.root, object_id)?
                }
                None => None,
            };
            if state != parent_state {
                out.push((id, state));
            }
            cur = parent;
        }
        Ok(out)
    }

    // ----- workspaces ---------------------------------------------------------

    pub fn workspace(&self, id: WorkspaceId) -> Result<Workspace> {
        self.reader().workspace(id)
    }

    pub fn working_entries(&self, id: WorkspaceId) -> Result<BTreeMap<ObjectId, ObjectStateId>> {
        let ws = self.workspace(id)?;
        self.tree_entries(ws.working)
    }

    /// Creates a workspace positioned at `base` (working = base snapshot) or
    /// at the empty tree.
    pub fn create_workspace(
        &mut self,
        id: WorkspaceId,
        base: Option<CommitId>,
    ) -> Result<Workspace> {
        let working = match base {
            Some(c) => self.commit(c)?.root,
            None => {
                let empty = Node::Empty;
                let tid = empty.id(&self.store.digest, &self.key)?;
                let tx = self.store.conn.unchecked_transaction()?;
                Self::put_object_tx(
                    &tx,
                    self.repo_id.as_bytes(),
                    KIND_TREE,
                    tid.as_bytes(),
                    &empty.encode(),
                )?;
                tx.commit()?;
                tid
            }
        };
        let ws = Workspace {
            id,
            base,
            working,
            candidate: None,
            selected_bookmark: None,
            token: 1,
        };
        let n = self.store.conn.execute(
            "INSERT OR IGNORE INTO vcs_workspaces (repo_id, workspace_id, snapshot) VALUES (?1, ?2, ?3)",
            params![self.repo_id.as_bytes(), id.to_vec(), cbor::encode(&ws.to_cbor())],
        )?;
        if n == 0 {
            return Err(StoreError::Invalid(format!(
                "workspace {id} already exists"
            )));
        }
        Ok(ws)
    }

    // ----- operations ---------------------------------------------------------

    /// Runs one operation atomically: checks the expected token, applies
    /// `f`, logs before/after, bumps the token.
    fn operate<R>(
        &mut self,
        ctx: &OperationContext,
        kind: &str,
        f: impl FnOnce(&mut Executor<'_, '_, D>, &Workspace) -> Result<(Workspace, R)>,
    ) -> Result<R> {
        let repo = self.repo_id;
        let key = self.key.clone();
        let caps = self.store.caps.clone();
        let schemas = self.store.schemas.clone();
        let digest = &self.store.digest;
        let tx = self.store.conn.unchecked_transaction()?;
        let before = {
            let reader = Reader {
                conn: &tx,
                repo: repo.as_bytes(),
            };
            reader.workspace(ctx.workspace_id)?
        };
        if let Some(expected) = ctx.expected_token
            && expected != before.token
        {
            return Err(StoreError::StaleToken {
                expected,
                actual: before.token,
            });
        }
        let dup: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM vcs_operations WHERE repo_id = ?1 AND operation_id = ?2",
                params![repo.as_bytes(), ctx.operation_id.to_vec()],
                |r| r.get(0),
            )
            .optional()?;
        if dup.is_some() {
            return Err(StoreError::DuplicateOperation(ctx.operation_id));
        }
        let mut exec = Executor {
            tx: &tx,
            repo,
            key: &key,
            digest,
            caps: &caps,
            schemas: &schemas,
            pending: Pending::default(),
            visible_added: Vec::new(),
            visible_removed: Vec::new(),
            rewrites: Vec::new(),
            bookmark_writes: Vec::new(),
        };
        let (mut after, result) = f(&mut exec, &before)?;
        after.token = before.token + 1;
        after.id = before.id;
        let Executor {
            pending,
            visible_added,
            visible_removed,
            rewrites,
            bookmark_writes,
            ..
        } = exec;
        // The log names what this operation added to the store; objects it
        // only reused (most of a rebuilt tree) were added by an earlier one.
        let mut ordinal = 0i64;
        for (kind_, id, bytes) in &pending.objects {
            if !Self::put_object_tx(&tx, repo.as_bytes(), kind_, id, bytes)? {
                continue;
            }
            if *kind_ == KIND_BOOKMARK_UPDATE {
                bookmarks::index_tx(&tx, repo, id, bytes, &caps)?;
            }
            ordinal += 1;
            tx.execute(
                "INSERT INTO vcs_operation_objects (repo_id, operation_id, ordinal, kind, logical_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![repo.as_bytes(), ctx.operation_id.to_vec(), ordinal, kind_, id],
            )?;
        }
        for c in &visible_removed {
            tx.execute(
                "DELETE FROM vcs_visible WHERE repo_id = ?1 AND commit_id = ?2",
                params![repo.as_bytes(), c.to_vec()],
            )?;
        }
        for c in &visible_added {
            tx.execute(
                "INSERT OR IGNORE INTO vcs_visible (repo_id, commit_id) VALUES (?1, ?2)",
                params![repo.as_bytes(), c.to_vec()],
            )?;
        }
        for (from, to) in &rewrites {
            tx.execute(
                "INSERT OR IGNORE INTO vcs_rewrites (repo_id, operation_id, from_commit, to_commit) VALUES (?1, ?2, ?3, ?4)",
                params![repo.as_bytes(), ctx.operation_id.to_vec(), from.to_vec(), to.to_vec()],
            )?;
        }
        tx.execute(
            "UPDATE vcs_workspaces SET snapshot = ?3 WHERE repo_id = ?1 AND workspace_id = ?2",
            params![
                repo.as_bytes(),
                after.id.to_vec(),
                cbor::encode(&after.to_cbor())
            ],
        )?;
        let log = |ws: &Workspace,
                   added: &[CommitId],
                   removed: &[CommitId],
                   bookmark_writes: &[BookmarkUpdateId]| {
            cbor::encode(&Value::map([
                ("workspace", ws.to_cbor()),
                (
                    "visible_added",
                    Value::Array(added.iter().map(CommitId::to_cbor).collect()),
                ),
                (
                    "visible_removed",
                    Value::Array(removed.iter().map(CommitId::to_cbor).collect()),
                ),
                (
                    "bookmark_writes",
                    Value::Array(
                        bookmark_writes
                            .iter()
                            .map(BookmarkUpdateId::to_cbor)
                            .collect(),
                    ),
                ),
            ]))
        };
        // Redo of a later operation after an undo is a new branch of the
        // log: drop undone entries newer than the last applied one.
        tx.execute(
            "DELETE FROM vcs_operations WHERE repo_id = ?1 AND undone = 1",
            params![repo.as_bytes()],
        )?;
        tx.execute(
            "INSERT INTO vcs_operations (repo_id, operation_id, kind, reason, created_at_unix_ms, before, after, undone)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![
                repo.as_bytes(),
                ctx.operation_id.to_vec(),
                kind,
                ctx.reason,
                ctx.now_unix_ms,
                log(&before, &[], &[], &[]),
                log(&after, &visible_added, &visible_removed, &bookmark_writes),
            ],
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Replaces entries of the working tree. The candidate, if any, is kept
    /// as it was (staging freezes the snapshot to record).
    pub fn save_working(
        &mut self,
        ctx: &OperationContext,
        changes: &[WorkingChange],
    ) -> Result<Workspace> {
        self.operate(ctx, "save_working", |exec, ws| {
            let mut edits = BTreeMap::new();
            for (object_id, state) in changes {
                if let Some(s) = state {
                    // The state must exist and belong to this object.
                    let st = exec.state(*s)?;
                    if st.object_id != *object_id {
                        return Err(StoreError::Invalid(format!(
                            "state {s} belongs to {}, not {object_id}",
                            st.object_id
                        )));
                    }
                }
                edits.insert(*object_id, *state);
            }
            let root = exec.update_tree(ws.working, &edits)?;
            let mut after = ws.clone();
            after.working = root;
            Ok((after, ()))
        })?;
        self.workspace(ctx.workspace_id)
    }

    /// Freezes the current working states of `objects` on top of the base
    /// snapshot as the candidate to record next.
    pub fn select_candidate(
        &mut self,
        ctx: &OperationContext,
        objects: &[ObjectId],
    ) -> Result<Workspace> {
        self.operate(ctx, "select_candidate", |exec, ws| {
            let base = match ws.base {
                Some(b) => exec.commit(b)?.root,
                None => exec.put_tree(&BTreeMap::new())?,
            };
            let mut edits = BTreeMap::new();
            for id in objects {
                edits.insert(*id, exec.lookup(ws.working, *id)?);
            }
            let root = exec.update_tree(base, &edits)?;
            let mut after = ws.clone();
            after.candidate = Some(root);
            Ok((after, ()))
        })?;
        self.workspace(ctx.workspace_id)
    }

    pub fn clear_candidate(&mut self, ctx: &OperationContext) -> Result<Workspace> {
        self.operate(ctx, "clear_candidate", |_, ws| {
            let mut after = ws.clone();
            after.candidate = None;
            Ok((after, ()))
        })?;
        self.workspace(ctx.workspace_id)
    }

    /// Records a commit from the working tree or the candidate. The
    /// workspace base moves to the new commit; the working tree is kept, so
    /// unselected changes carry over.
    pub fn record(
        &mut self,
        ctx: &OperationContext,
        source: RecordSource,
        description: &str,
    ) -> Result<CommitId> {
        self.operate(ctx, "record", |exec, ws| {
            exec.record_commit(ctx, ws, source, description)
        })
    }

    /// Records like [`Repo::record`], then moves the workspace's selected
    /// bookmark forward to the new commit when its heads are still
    /// `expected` and the new commit descends from where it points. The
    /// commit is recorded either way; [`Advance`] tells what happened to
    /// the bookmark.
    pub fn record_advancing(
        &mut self,
        ctx: &OperationContext,
        source: RecordSource,
        description: &str,
        expected: &[BookmarkUpdateId],
    ) -> Result<(CommitId, Advance)> {
        self.operate(ctx, "record", |exec, ws| {
            let (after, id) = exec.record_commit(ctx, ws, source, description)?;
            let advance = exec.advance_selected(ctx, ws.selected_bookmark, id, expected)?;
            Ok((after, (id, advance)))
        })
    }

    /// Rewrites the workspace's base commit in place: a new commit with the
    /// same change ID and parents, the working tree as its snapshot and a
    /// new description. The old commit stays stored but leaves the visible
    /// set.
    pub fn amend(&mut self, ctx: &OperationContext, description: &str) -> Result<CommitId> {
        self.operate(ctx, "amend", |exec, ws| {
            let base = ws.base.ok_or(StoreError::NothingToRecord)?;
            let old = exec.commit(base)?;
            let commit = exec.new_commit(
                old.change_id,
                old.parents.clone(),
                ws.working,
                ctx,
                description,
            )?;
            let id = exec.put_commit(&commit)?;
            if id == base {
                return Err(StoreError::NothingToRecord);
            }
            exec.hide(base);
            exec.show(id);
            exec.rewrites.push((base, id));
            exec.follow_rewrite(ctx, base, id)?;
            let mut after = ws.clone();
            after.base = Some(id);
            Ok((after, id))
        })
    }

    /// Integrates `other` into the workspace: three-way merge of the base
    /// snapshot and `other`'s snapshot against their common ancestor. The
    /// result is a merge commit with parents `[base, other]`; objects that
    /// did not merge carry a `Conflict` state. The working tree moves to the
    /// merge result, so a working tree that differs from the base is refused
    /// rather than overwritten.
    pub fn integrate(
        &mut self,
        ctx: &OperationContext,
        other: CommitId,
    ) -> Result<IntegrateOutcome> {
        self.operate(ctx, "integrate", |exec, ws| {
            let base = ws.base.ok_or_else(|| {
                StoreError::Invalid("integrate needs a workspace with a base commit".into())
            })?;
            if base == other {
                return Err(StoreError::Invalid(
                    "cannot integrate a commit into itself".into(),
                ));
            }
            if ws.working != exec.commit(base)?.root {
                return Err(StoreError::UnrecordedChanges);
            }
            let ancestors = exec.merge_bases(base, other)?;
            let left = exec.commit(base)?.root;
            let right = exec.commit(other)?.root;
            let ancestor_roots: Vec<TreeId> = ancestors
                .iter()
                .map(|a| Ok(exec.commit(*a)?.root))
                .collect::<Result<_>>()?;
            // Only an object some side changed against some merge base can
            // merge to anything but the left state; the result is the left
            // tree with those objects updated. Unrelated histories (no base)
            // look at every object.
            let mut keys: BTreeSet<ObjectId> = BTreeSet::new();
            if ancestor_roots.is_empty() {
                keys.extend(exec.tree_entries(left)?.into_keys());
                keys.extend(exec.tree_entries(right)?.into_keys());
            } else {
                for a in &ancestor_roots {
                    let sides: &[TreeId] = if ancestor_roots.len() == 1 {
                        &[right]
                    } else {
                        &[left, right]
                    };
                    for side in sides {
                        let d = exec.diff(*a, *side)?;
                        keys.extend(d.added.iter().map(|(k, _)| *k));
                        keys.extend(d.removed.iter().map(|(k, _)| *k));
                        keys.extend(d.changed.iter().map(|(k, _, _)| *k));
                    }
                }
            }
            let mut edits = BTreeMap::new();
            let mut conflicts = Vec::new();
            for object_id in keys {
                let l = exec.lookup(left, object_id)?;
                let r = exec.lookup(right, object_id)?;
                // Base candidates for this object across all merge bases.
                let mut bases: BTreeSet<Option<ObjectStateId>> = BTreeSet::new();
                for a in &ancestor_roots {
                    bases.insert(exec.lookup(*a, object_id)?);
                }
                let base_state = if bases.len() == 1 {
                    Some(bases.into_iter().next().unwrap())
                } else {
                    None // no ancestor at all, or ambiguous
                };
                let merged = match exec.merge_object(object_id, base_state, l, r, &ancestors)? {
                    ObjectMerge::Keep(s) => s,
                    ObjectMerge::Conflict(state, conflict_id) => {
                        conflicts.push((object_id, conflict_id));
                        Some(state)
                    }
                };
                if merged != l {
                    edits.insert(object_id, merged);
                }
            }
            let root = exec.update_tree(left, &edits)?;
            let commit = exec.new_commit(
                ChangeId(exec.fresh_id(ctx, "integrate")?),
                vec![base, other],
                root,
                ctx,
                &ctx.reason,
            )?;
            let id = exec.put_commit(&commit)?;
            exec.hide(base);
            exec.hide(other);
            exec.show(id);
            let mut after = ws.clone();
            after.base = Some(id);
            after.working = root;
            after.candidate = None;
            Ok((
                after,
                IntegrateOutcome {
                    commit: id,
                    conflicts,
                },
            ))
        })
    }

    /// Replaces a conflicted object's working state with a resolved state.
    /// The caller builds the resolved state from the conflict's terms (text
    /// `resolve`, record `resolve`, or by picking one side) and stores it
    /// first; this records the choice in the working tree.
    pub fn resolve(
        &mut self,
        ctx: &OperationContext,
        object_id: ObjectId,
        resolved: ObjectStateId,
    ) -> Result<Workspace> {
        self.operate(ctx, "resolve", |exec, ws| {
            let current = exec.lookup(ws.working, object_id)?.ok_or_else(|| {
                StoreError::Invalid(format!("{object_id} is not in the working tree"))
            })?;
            let state = exec.state(current)?;
            if !matches!(state.body, ObjectBody::Conflict { .. }) {
                return Err(StoreError::Invalid(format!(
                    "{object_id} is not in conflict"
                )));
            }
            let new_state = exec.state(resolved)?;
            if new_state.object_id != object_id {
                return Err(StoreError::Invalid(
                    "resolved state belongs to another object".into(),
                ));
            }
            if matches!(new_state.body, ObjectBody::Conflict { .. }) {
                return Err(StoreError::Invalid(
                    "resolved state is itself a conflict".into(),
                ));
            }
            let root =
                exec.update_tree(ws.working, &BTreeMap::from([(object_id, Some(resolved))]))?;
            let mut after = ws.clone();
            after.working = root;
            Ok((after, ()))
        })?;
        self.workspace(ctx.workspace_id)
    }

    /// Restores `object_id` in the working tree to its state in `commit`
    /// (absent there → removed from working).
    pub fn restore(
        &mut self,
        ctx: &OperationContext,
        commit: CommitId,
        object_id: ObjectId,
    ) -> Result<Workspace> {
        self.operate(ctx, "restore", |exec, ws| {
            let source = exec.lookup(exec.commit(commit)?.root, object_id)?;
            let root = exec.update_tree(ws.working, &BTreeMap::from([(object_id, source)]))?;
            let mut after = ws.clone();
            after.working = root;
            Ok((after, ()))
        })?;
        self.workspace(ctx.workspace_id)
    }

    /// Undoes the newest applied operation on this repo. Commits stay
    /// stored; only the workspace position and the visible set move back.
    /// Bookmark updates may already be shared, so they are not removed:
    /// new updates by `author` put the fields back instead.
    pub fn undo(&mut self, author: &Actor, now_unix_ms: i64) -> Result<OperationEntry> {
        let repo = self.repo_id;
        let tx = self.store.conn.unchecked_transaction()?;
        let row: Option<OpRow> = tx
            .query_row(
                "SELECT seq, operation_id, kind, reason, created_at_unix_ms, before, after
                 FROM vcs_operations WHERE repo_id = ?1 AND undone = 0 ORDER BY seq DESC LIMIT 1",
                params![repo.as_bytes()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;
        let (seq, op_id, kind, reason, created, before, after) =
            row.ok_or(StoreError::NothingToUndo)?;
        Self::apply_log(&tx, repo, &after, &before, true)?;
        bookmarks::undo_writes_tx(
            &tx,
            repo,
            &self.key,
            &self.store.digest,
            &self.store.caps,
            &after,
            author,
            now_unix_ms,
        )?;
        tx.execute(
            "UPDATE vcs_operations SET undone = 1 WHERE seq = ?1",
            params![seq],
        )?;
        tx.commit()?;
        Ok(OperationEntry {
            seq,
            id: OperationId::from_bytes(&op_id).map_err(|e| StoreError::Invalid(e.to_string()))?,
            kind,
            reason,
            created_at_unix_ms: created,
            undone: true,
        })
    }

    /// Re-applies the oldest undone operation. Its bookmark updates are
    /// written again by `author`, as new updates.
    pub fn redo(&mut self, author: &Actor, now_unix_ms: i64) -> Result<OperationEntry> {
        let repo = self.repo_id;
        let tx = self.store.conn.unchecked_transaction()?;
        let row: Option<OpRow> = tx
            .query_row(
                "SELECT seq, operation_id, kind, reason, created_at_unix_ms, before, after
                 FROM vcs_operations WHERE repo_id = ?1 AND undone = 1 ORDER BY seq ASC LIMIT 1",
                params![repo.as_bytes()],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;
        let (seq, op_id, kind, reason, created, _before, after) =
            row.ok_or(StoreError::NothingToRedo)?;
        Self::apply_log(&tx, repo, &after, &after, false)?;
        let after = bookmarks::redo_writes_tx(
            &tx,
            repo,
            &self.key,
            &self.store.digest,
            &self.store.caps,
            &after,
            author,
            now_unix_ms,
        )?;
        // The log entry now names the updates the redo wrote, so a later
        // undo compensates for those. This is the one place an entry's
        // `after` changes after it is written.
        tx.execute(
            "UPDATE vcs_operations SET undone = 0, after = ?2 WHERE seq = ?1",
            params![seq, after],
        )?;
        tx.commit()?;
        Ok(OperationEntry {
            seq,
            id: OperationId::from_bytes(&op_id).map_err(|e| StoreError::Invalid(e.to_string()))?,
            kind,
            reason,
            created_at_unix_ms: created,
            undone: false,
        })
    }

    /// Moves the workspace to `target` and applies the visible delta
    /// recorded in `after` (inverted when undoing).
    fn apply_log(
        tx: &Transaction<'_>,
        repo: RepoId,
        after: &[u8],
        target: &[u8],
        inverting: bool,
    ) -> Result<()> {
        let bad = || StoreError::Invalid("corrupt operation log".into());
        let delta = cbor::decode(after)?;
        let target_v = cbor::decode(target)?;
        let ws = Workspace::from_cbor(target_v.get("workspace").ok_or_else(bad)?)?;
        let ids = |key: &str| -> Result<Vec<CommitId>> {
            delta
                .get(key)
                .and_then(Value::as_array)
                .ok_or_else(bad)?
                .iter()
                .map(|v| CommitId::from_cbor(v).ok_or_else(bad))
                .collect()
        };
        let (added, removed) = if inverting {
            (ids("visible_removed")?, ids("visible_added")?)
        } else {
            (ids("visible_added")?, ids("visible_removed")?)
        };
        for c in removed {
            tx.execute(
                "DELETE FROM vcs_visible WHERE repo_id = ?1 AND commit_id = ?2",
                params![repo.as_bytes(), c.to_vec()],
            )?;
        }
        for c in added {
            tx.execute(
                "INSERT OR IGNORE INTO vcs_visible (repo_id, commit_id) VALUES (?1, ?2)",
                params![repo.as_bytes(), c.to_vec()],
            )?;
        }
        tx.execute(
            "UPDATE vcs_workspaces SET snapshot = ?3 WHERE repo_id = ?1 AND workspace_id = ?2",
            params![repo.as_bytes(), ws.id.to_vec(), cbor::encode(&ws.to_cbor())],
        )?;
        Ok(())
    }

    /// Whether there is an operation to undo, and one to redo.
    pub fn can_undo_redo(&self) -> Result<(bool, bool)> {
        let any = |undone: i64| -> Result<bool> {
            Ok(self.store.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM vcs_operations WHERE repo_id = ?1 AND undone = ?2)",
                params![self.repo_id.as_bytes(), undone],
                |r| r.get(0),
            )?)
        };
        Ok((any(0)?, any(1)?))
    }

    pub fn operations(&self, limit: usize) -> Result<Vec<OperationEntry>> {
        let mut stmt = self.store.conn.prepare(
            "SELECT seq, operation_id, kind, reason, created_at_unix_ms, undone
             FROM vcs_operations WHERE repo_id = ?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.repo_id.as_bytes(), limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        rows.map(|r| {
            let (seq, id, kind, reason, created, undone) = r?;
            Ok(OperationEntry {
                seq,
                id: OperationId::from_bytes(&id).map_err(|e| StoreError::Invalid(e.to_string()))?,
                kind,
                reason,
                created_at_unix_ms: created,
                undone: undone != 0,
            })
        })
        .collect()
    }

    // ----- export / import ----------------------------------------------------

    /// Exports every immutable object (bookmark updates included) plus the
    /// visible set as one canonical CBOR archive (same realm, same IDs).
    /// Workspaces and the operation log are local and not exported.
    pub fn export_archive(&self) -> Result<Vec<u8>> {
        let mut stmt = self.store.conn.prepare(
            "SELECT kind, logical_id, bytes FROM vcs_objects WHERE repo_id = ?1 ORDER BY kind, logical_id",
        )?;
        let rows = stmt.query_map(params![self.repo_id.as_bytes()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut objects = Vec::new();
        for r in rows {
            let (kind, id, bytes) = r?;
            objects.push(Value::Array(vec![
                Value::text(kind),
                Value::Bytes(id),
                Value::Bytes(bytes),
            ]));
        }
        let visible = self.visible()?;
        // Local blobs of the repo's realm travel with the archive so binary
        // objects stay readable on the importing device.
        let realm = crate::blob_store::repo_realm(self.repo_id, &self.key);
        let mut blobs = Vec::new();
        for info in crate::blob_store::list(self.store.conn, realm.id)
            .map_err(|e| StoreError::Invalid(e.to_string()))?
        {
            let bytes = crate::blob_store::read_verified(
                self.store.conn,
                &self.store.digest,
                realm.id,
                info.blob_ref.blob_id,
            )
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
            blobs.push(Value::Array(vec![
                info.blob_ref.blob_id.to_cbor(),
                Value::Bytes(bytes),
            ]));
        }
        Ok(cbor::encode(&Value::map([
            ("format", Value::Uint(1)),
            ("repo_id", self.repo_id.to_cbor()),
            ("objects", Value::Array(objects)),
            ("blobs", Value::Array(blobs)),
            (
                "visible",
                Value::Array(visible.iter().map(CommitId::to_cbor).collect()),
            ),
        ])))
    }

    /// Imports an archive produced by `export_archive` under the same key.
    /// Every object is re-hashed before it is stored; a mismatch or an
    /// identity conflict aborts the whole import.
    pub fn import_archive(&mut self, bytes: &[u8], now_unix_ms: i64) -> Result<usize> {
        let bad = |d: &str| StoreError::Invalid(format!("archive: {d}"));
        let v = cbor::decode(bytes)?;
        if v.get("format").and_then(Value::as_u64) != Some(1) {
            return Err(bad("unsupported format"));
        }
        if v.get("repo_id").and_then(RepoId::from_cbor) != Some(self.repo_id) {
            return Err(bad("repo id mismatch"));
        }
        let objects = v
            .get("objects")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("objects missing"))?;
        let tx = self.store.conn.unchecked_transaction()?;
        let mut decoded = Vec::with_capacity(objects.len());
        for obj in objects {
            let parts = obj
                .as_array()
                .filter(|p| p.len() == 3)
                .ok_or_else(|| bad("bad object"))?;
            let kind = parts[0].as_text().ok_or_else(|| bad("bad kind"))?;
            let id = parts[1].as_bytes().ok_or_else(|| bad("bad id"))?;
            let data = parts[2].as_bytes().ok_or_else(|| bad("bad bytes"))?;
            decoded.push((kind, id, data));
        }
        let count = self.import_objects_tx(&tx, decoded.into_iter())?;
        // Tree closure of every visible commit must be present.
        let reader = Reader {
            conn: &tx,
            repo: self.repo_id.as_bytes(),
        };
        for c in v.get("visible").and_then(Value::as_array).unwrap_or(&[]) {
            let id = CommitId::from_cbor(c).ok_or_else(|| bad("bad visible id"))?;
            let commit = reader.commit(id, &self.store.caps)?;
            reader.tree_entries(&self.store.digest, &self.key, commit.root)?;
            tx.execute(
                "INSERT OR IGNORE INTO vcs_visible (repo_id, commit_id) VALUES (?1, ?2)",
                params![self.repo_id.as_bytes(), id.to_vec()],
            )?;
        }
        if let Some(blobs) = v.get("blobs").and_then(Value::as_array) {
            crate::blob_store::ensure_schema(&tx)
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
            let realm = crate::blob_store::repo_realm(self.repo_id, &self.key);
            for blob in blobs {
                let parts = blob
                    .as_array()
                    .filter(|p| p.len() == 2)
                    .ok_or_else(|| bad("bad blob"))?;
                let claimed = BlobId::from_cbor(&parts[0]).ok_or_else(|| bad("bad blob id"))?;
                let data = parts[1].as_bytes().ok_or_else(|| bad("bad blob bytes"))?;
                let (stored, _) = crate::blob_store::put_local_in(
                    &tx,
                    &self.store.digest,
                    &realm,
                    data,
                    now_unix_ms,
                )
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
                if stored.blob_id != claimed {
                    return Err(StoreError::IdentityConflict {
                        id: format!("blob:{claimed}"),
                    });
                }
            }
        }
        tx.commit()?;
        Ok(count)
    }

    /// Verifies and stores objects inside `tx`; see [`Repo::import_objects`].
    fn import_objects_tx<'o>(
        &self,
        tx: &Transaction<'_>,
        objects: impl Iterator<Item = (&'o str, &'o [u8], &'o [u8])>,
    ) -> Result<usize> {
        let bad = |d: &str| StoreError::Invalid(format!("import: {d}"));
        let mut count = 0;
        for (kind, id, data) in objects {
            let expected: Vec<u8> = match kind {
                KIND_CONTENT => content_id(&self.store.digest, &self.key, data)?.to_vec(),
                KIND_STATE => ObjectState::decode(data, &self.store.caps)?
                    .id(&self.store.digest, &self.key)?
                    .to_vec(),
                KIND_TREE => Node::decode(data)?
                    .id(&self.store.digest, &self.key)?
                    .to_vec(),
                KIND_COMMIT => Commit::decode(data, &self.store.caps)?
                    .id(&self.store.digest, &self.key)?
                    .to_vec(),
                KIND_CONFLICT => Conflict::decode(data, &self.store.caps)?
                    .id(&self.store.digest, &self.key)?
                    .to_vec(),
                KIND_BOOKMARK_UPDATE => {
                    let u = BookmarkUpdate::decode(data, &self.store.caps)?;
                    if u.repo_id != self.repo_id {
                        return Err(bad("bookmark update of another repo"));
                    }
                    u.id(&self.store.digest, &self.key)?.to_vec()
                }
                KIND_REDACTION => {
                    let r = Redaction::decode(data, &self.store.caps)?;
                    if r.repo_id != self.repo_id {
                        return Err(bad("redaction of another repo"));
                    }
                    r.id(&self.store.digest, &self.key)?.to_vec()
                }
                _ => return Err(bad("unknown object kind")),
            };
            if expected != id {
                return Err(StoreError::IdentityConflict {
                    id: format!("{kind}:{}", hex::encode(id)),
                });
            }
            let kind_static = match kind {
                KIND_CONTENT => KIND_CONTENT,
                KIND_STATE => KIND_STATE,
                KIND_TREE => KIND_TREE,
                KIND_COMMIT => KIND_COMMIT,
                KIND_BOOKMARK_UPDATE => KIND_BOOKMARK_UPDATE,
                KIND_REDACTION => KIND_REDACTION,
                _ => KIND_CONFLICT,
            };
            Self::put_object_tx(tx, self.repo_id.as_bytes(), kind_static, id, data)?;
            if kind == KIND_BOOKMARK_UPDATE {
                bookmarks::index_tx(tx, self.repo_id, id, data, &self.store.caps)?;
            }
            if kind == KIND_REDACTION {
                let r = Redaction::decode(data, &self.store.caps)?;
                retention::apply_redaction_tx(tx, self.repo_id, &r)?;
            }
            if kind == KIND_COMMIT {
                let c = Commit::decode(data, &self.store.caps)?;
                Executor::<D>::index_commit_tx(
                    tx,
                    self.repo_id,
                    CommitId::from_bytes(id).unwrap(),
                    &c,
                )?;
            }
            count += 1;
        }
        Ok(count)
    }
}

/// What every conflict an object merge records is made of, apart from its
/// driver and terms.
struct ConflictFrame {
    object_id: ObjectId,
    base: Vec<ObjectStateId>,
    sides: Vec<ObjectStateId>,
    type_hint: String,
    meta: MetadataMerge,
}

/// What one object's three-way merge produced.
enum ObjectMerge {
    Keep(Option<ObjectStateId>),
    Conflict(ObjectStateId, ConflictId),
}

/// In-transaction executor: buffers immutable writes and view changes.
struct Executor<'t, 'c, D: Digest> {
    tx: &'t Transaction<'c>,
    repo: RepoId,
    key: &'t IdKey,
    digest: &'t D,
    caps: &'t Capabilities,
    schemas: &'t SchemaRegistry,
    pending: Pending,
    visible_added: Vec<CommitId>,
    visible_removed: Vec<CommitId>,
    rewrites: Vec<(CommitId, CommitId)>,
    /// Bookmark updates written, in order; undo compensates for them.
    bookmark_writes: Vec<BookmarkUpdateId>,
}

impl<D: Digest> Executor<'_, '_, D> {
    /// The commit of a record from the working tree or the candidate, and
    /// the workspace moved onto it.
    fn record_commit(
        &mut self,
        ctx: &OperationContext,
        ws: &Workspace,
        source: RecordSource,
        description: &str,
    ) -> Result<(Workspace, CommitId)> {
        let root = match source {
            RecordSource::Working => ws.working,
            RecordSource::Candidate => ws.candidate.ok_or(StoreError::NoCandidate)?,
        };
        if let Some(base) = ws.base
            && self.commit(base)?.root == root
        {
            return Err(StoreError::NothingToRecord);
        }
        let commit = self.new_commit(
            ChangeId(self.fresh_id(ctx, "record")?),
            ws.base.into_iter().collect(),
            root,
            ctx,
            description,
        )?;
        let id = self.put_commit(&commit)?;
        if let Some(base) = ws.base {
            self.hide(base);
        }
        self.show(id);
        let mut after = ws.clone();
        after.base = Some(id);
        if source == RecordSource::Candidate {
            after.candidate = None;
        }
        Ok((after, id))
    }

    fn reader(&self) -> Reader<'_> {
        Reader {
            conn: self.tx,
            repo: self.repo.as_bytes(),
        }
    }

    /// Looks an object up in the pending writes first, then the table.
    fn object(&self, kind: &str, id: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some((_, _, bytes)) = self
            .pending
            .objects
            .iter()
            .find(|(k, i, _)| *k == kind && i == id)
        {
            return Ok(Some(bytes.clone()));
        }
        self.reader().object(kind, id)
    }

    fn commit(&self, id: CommitId) -> Result<Commit> {
        let bytes = self
            .object(KIND_COMMIT, id.as_bytes())?
            .ok_or(StoreError::UnknownCommit(id))?;
        Ok(Commit::decode(&bytes, self.caps)?)
    }

    fn state(&self, id: ObjectStateId) -> Result<ObjectState> {
        let bytes =
            self.object(KIND_STATE, id.as_bytes())?
                .ok_or_else(|| StoreError::MissingObject {
                    kind: KIND_STATE,
                    id: id.to_string(),
                })?;
        Ok(ObjectState::decode(&bytes, self.caps)?)
    }

    /// A body, or `None` when a redaction erased it; a body that is
    /// missing for any other reason is an error.
    fn content_unless_redacted(&self, id: ContentId) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.object(KIND_CONTENT, id.as_bytes())? {
            return Ok(Some(bytes));
        }
        if self.reader().is_redacted(KIND_CONTENT, id.as_bytes())? {
            return Ok(None);
        }
        Err(StoreError::MissingObject {
            kind: KIND_CONTENT,
            id: id.to_string(),
        })
    }

    fn tree_entries(&self, root: TreeId) -> Result<BTreeMap<ObjectId, ObjectStateId>> {
        let mut err = None;
        let entries = tree::walk_verified(self.digest, self.key, root, |id| {
            match self.object(KIND_TREE, id.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    None
                }
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        Ok(entries?)
    }

    /// Runs `f` with a node fetcher that sees this operation's writes.
    fn with_nodes<T, E>(
        &self,
        f: impl FnOnce(&mut dyn FnMut(&TreeId) -> Option<Vec<u8>>) -> std::result::Result<T, E>,
    ) -> Result<T>
    where
        StoreError: From<E>,
    {
        let mut err = None;
        let out = f(
            &mut |id: &TreeId| match self.object(KIND_TREE, id.as_bytes()) {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e);
                    None
                }
            },
        );
        if let Some(e) = err {
            return Err(e);
        }
        Ok(out?)
    }

    fn lookup(&self, root: TreeId, object: ObjectId) -> Result<Option<ObjectStateId>> {
        self.with_nodes(|fetch| tree::lookup(self.digest, self.key, root, &object, fetch))
    }

    fn diff(&self, before: TreeId, after: TreeId) -> Result<tree::TreeDiff> {
        self.with_nodes(|fetch| tree::diff_trees(self.digest, self.key, before, after, fetch))
    }

    /// The tree at `root` with `changes` applied, writing only the nodes on
    /// the changed paths.
    fn update_tree(
        &mut self,
        root: TreeId,
        changes: &BTreeMap<ObjectId, Option<ObjectStateId>>,
    ) -> Result<TreeId> {
        if changes.is_empty() {
            return Ok(root);
        }
        let built =
            self.with_nodes(|fetch| tree::update(self.digest, self.key, root, changes, fetch))?;
        for (id, node) in built.nodes {
            self.pending
                .objects
                .push((KIND_TREE, id.to_vec(), node.encode()));
        }
        Ok(built.root)
    }

    fn put_tree(&mut self, entries: &BTreeMap<ObjectId, ObjectStateId>) -> Result<TreeId> {
        let built = tree::build(self.digest, self.key, entries)?;
        for (id, node) in &built.nodes {
            self.pending
                .objects
                .push((KIND_TREE, id.to_vec(), node.encode()));
        }
        Ok(built.root)
    }

    fn put_content(&mut self, bytes: &[u8]) -> Result<ContentId> {
        let id = content_id(self.digest, self.key, bytes)?;
        self.pending
            .objects
            .push((KIND_CONTENT, id.to_vec(), bytes.to_vec()));
        Ok(id)
    }

    fn put_state(&mut self, state: &ObjectState) -> Result<ObjectStateId> {
        let id = state.id(self.digest, self.key)?;
        self.pending
            .objects
            .push((KIND_STATE, id.to_vec(), state.encode()?));
        Ok(id)
    }

    fn put_conflict(&mut self, conflict: &Conflict) -> Result<ConflictId> {
        let id = conflict.id(self.digest, self.key)?;
        self.pending
            .objects
            .push((KIND_CONFLICT, id.to_vec(), conflict.encode()?));
        Ok(id)
    }

    fn put_commit(&mut self, commit: &Commit) -> Result<CommitId> {
        let id = commit.id(self.digest, self.key)?;
        self.pending
            .objects
            .push((KIND_COMMIT, id.to_vec(), commit.encode()?));
        Self::index_commit_tx(self.tx, self.repo, id, commit)?;
        Ok(id)
    }

    fn index_commit_tx(
        tx: &Transaction<'_>,
        repo: RepoId,
        id: CommitId,
        commit: &Commit,
    ) -> Result<()> {
        let n = tx.execute(
            "INSERT OR IGNORE INTO vcs_commits (repo_id, commit_id, change_id, root, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                repo.as_bytes(),
                id.to_vec(),
                commit.change_id.to_vec(),
                commit.root.to_vec(),
                commit.created_at_unix_ms
            ],
        )?;
        if n == 1 {
            for (i, p) in commit.parents.iter().enumerate() {
                tx.execute(
                    "INSERT INTO vcs_commit_parents (repo_id, commit_id, ordinal, parent_id) VALUES (?1, ?2, ?3, ?4)",
                    params![repo.as_bytes(), id.to_vec(), i as i64, p.to_vec()],
                )?;
            }
        }
        Ok(())
    }

    fn new_commit(
        &self,
        change_id: ChangeId,
        parents: Vec<CommitId>,
        root: TreeId,
        ctx: &OperationContext,
        description: &str,
    ) -> Result<Commit> {
        for p in &parents {
            self.commit(*p)?;
        }
        let commit = Commit {
            format: COMMIT_FORMAT_V1,
            repo_id: self.repo,
            change_id,
            parents,
            root,
            author: ctx.actor.clone(),
            created_at_unix_ms: ctx.now_unix_ms,
            description: description.to_owned(),
            required_capabilities: vec!["core.v1".to_owned()],
        };
        commit.validate()?;
        Ok(commit)
    }

    /// A fresh 128-bit ID unique to this operation and label. The store has
    /// no CSPRNG of its own; the operation ID is the host's random input and
    /// the ID key keeps the result unpredictable to anyone without it.
    fn fresh_id(&self, ctx: &OperationContext, label: &str) -> Result<[u8; 16]> {
        let seed = Value::Array(vec![
            Value::text("fresh"),
            Value::text(label),
            ctx.operation_id.to_cbor(),
        ]);
        let h = self.digest.hmac_sha256(&self.key.0, &cbor::encode(&seed))?;
        let mut out = [0u8; 16];
        out.copy_from_slice(&h[..16]);
        Ok(out)
    }

    fn show(&mut self, c: CommitId) {
        self.visible_added.push(c);
    }

    fn hide(&mut self, c: CommitId) {
        self.visible_removed.push(c);
    }

    /// Lowest common ancestors of `a` and `b` (all of them; more than one
    /// means an ambiguous base, which the per-object merge treats as "no
    /// single base").
    fn merge_bases(&self, a: CommitId, b: CommitId) -> Result<Vec<CommitId>> {
        let reader = self.reader();
        let ancestors = |start: CommitId| -> Result<BTreeSet<CommitId>> {
            let mut seen = BTreeSet::new();
            let mut queue = VecDeque::from([start]);
            while let Some(c) = queue.pop_front() {
                if seen.insert(c) {
                    queue.extend(reader.parents(c)?);
                }
            }
            Ok(seen)
        };
        let anc_a = ancestors(a)?;
        let anc_b = ancestors(b)?;
        let common: BTreeSet<CommitId> = anc_a.intersection(&anc_b).copied().collect();
        // Keep only those with no descendant in `common`.
        let mut lowest = Vec::new();
        for c in &common {
            let mut has_child_in_common = false;
            for other in &common {
                if other != c && reader.parents(*other)?.contains(c) {
                    has_child_in_common = true;
                    break;
                }
            }
            if !has_child_in_common {
                lowest.push(*c);
            }
        }
        // A strict "no descendant" test needs the transitive closure; the
        // parent test above handles direct children. Filter transitively.
        let mut result = Vec::new();
        for c in &lowest {
            let mut dominated = false;
            for other in &lowest {
                if other != c {
                    let anc_other = ancestors(*other)?;
                    if anc_other.contains(c) {
                        dominated = true;
                        break;
                    }
                }
            }
            if !dominated {
                result.push(*c);
            }
        }
        Ok(result)
    }

    fn merge_object(
        &mut self,
        object_id: ObjectId,
        base: Option<Option<ObjectStateId>>,
        left: Option<ObjectStateId>,
        right: Option<ObjectStateId>,
        ancestors: &[CommitId],
    ) -> Result<ObjectMerge> {
        if left == right {
            return Ok(ObjectMerge::Keep(left));
        }
        if let Some(b) = base {
            if b == left {
                return Ok(ObjectMerge::Keep(right));
            }
            if b == right {
                return Ok(ObjectMerge::Keep(left));
            }
        }
        // Both sides differ from each other and from the base (or there is
        // no usable base). Try a content merge by type.
        let base_state = base.flatten();
        let ls = left.map(|s| self.state(s)).transpose()?;
        let rs = right.map(|s| self.state(s)).transpose()?;
        let bs = base_state.map(|s| self.state(s)).transpose()?;
        let base_ids: Vec<ObjectStateId> = match base {
            Some(Some(b)) => vec![b],
            Some(None) => vec![],
            None => {
                // Ambiguous or absent base: record every candidate base state.
                let mut v = Vec::new();
                for a in ancestors {
                    if let Some(s) = self.lookup(self.commit(*a)?.root, object_id)? {
                        v.push(s);
                    }
                }
                v.sort();
                v.dedup();
                v
            }
        };
        let ambiguous_base = base.is_none();
        let type_hint = ls
            .as_ref()
            .or(rs.as_ref())
            .map(|s| s.type_id.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let mut sides: Vec<ObjectStateId> = Vec::with_capacity(2);
        for side in [left, right] {
            sides.push(match side {
                Some(s) => s,
                None => {
                    let deleted = ObjectState::new(object_id, &type_hint, ObjectBody::Deleted);
                    self.put_state(&deleted)?
                }
            });
        }

        // Metadata merges key-wise beside the body; the conflict state keeps
        // the agreed keys so a resolution by content does not lose them.
        let meta = match (&ls, &rs) {
            (Some(l), Some(r)) => {
                merge_metadata(&l.metadata, &r.metadata, bs.as_ref().map(|b| &b.metadata))
            }
            (Some(s), None) | (None, Some(s)) => MetadataMerge {
                merged: s.metadata.clone(),
                conflicts: vec![],
            },
            (None, None) => MetadataMerge {
                merged: Value::Map(vec![]),
                conflicts: vec![],
            },
        };

        // The body changed on at most one side (only metadata moved on the
        // other): that body is taken whole, whatever its type.
        let one_sided_body = match (&ls, &rs) {
            (Some(l), Some(r)) if !ambiguous_base && l.type_id == r.type_id => {
                if l.body == r.body || bs.as_ref().is_some_and(|b| b.body == r.body) {
                    Some(l.body.clone())
                } else if bs.as_ref().is_some_and(|b| b.body == l.body) {
                    Some(r.body.clone())
                } else {
                    None
                }
            }
            _ => None,
        };

        let (driver, outcome) = match (&ls, &rs) {
            (Some(l), Some(_)) if one_sided_body.is_some() => {
                let body = one_sided_body.expect("checked by the guard");
                if meta.conflicts.is_empty() {
                    let state =
                        ObjectState::new(object_id, &l.type_id, body).with_metadata(meta.merged);
                    return Ok(ObjectMerge::Keep(Some(self.put_state(&state)?)));
                }
                let terms = self.metadata_terms(&meta.conflicts)?;
                match body {
                    ObjectBody::Text { content } => (TEXT_DRIVER_V1, (Some(content), terms)),
                    ObjectBody::Record { content, .. } => {
                        (RECORD_DRIVER_V1, (Some(content), terms))
                    }
                    _ => ("merge.object.v1", (None, terms)),
                }
            }
            (Some(l), Some(r)) if !ambiguous_base => {
                match (&l.body, &r.body, bs.as_ref().map(|b| &b.body)) {
                    (
                        ObjectBody::Text { content: lc },
                        ObjectBody::Text { content: rc },
                        base_body,
                    ) if l.type_id == r.type_id => {
                        // An erased base merges as no base; an erased side
                        // cannot be merged line by line.
                        let base_bytes = match base_body {
                            Some(ObjectBody::Text { content }) => {
                                self.content_unless_redacted(*content)?
                            }
                            _ => None,
                        };
                        let (Some(lb), Some(rb)) = (
                            self.content_unless_redacted(*lc)?,
                            self.content_unless_redacted(*rc)?,
                        ) else {
                            let frame = ConflictFrame {
                                object_id,
                                base: base_ids,
                                sides,
                                type_hint,
                                meta,
                            };
                            return self.whole_object_conflict(frame, &ls, &rs);
                        };
                        match merge::text::merge_text(base_bytes.as_deref(), &lb, &rb) {
                            merge::text::TextMergeOutcome::Merged(bytes) => {
                                let content = self.put_content(&bytes)?;
                                if !meta.conflicts.is_empty() {
                                    let terms = self.metadata_terms(&meta.conflicts)?;
                                    (TEXT_DRIVER_V1, (Some(content), terms))
                                } else {
                                    let state = ObjectState::new(
                                        object_id,
                                        &l.type_id,
                                        ObjectBody::Text { content },
                                    )
                                    .with_metadata(meta.merged);
                                    let id = self.put_state(&state)?;
                                    return Ok(ObjectMerge::Keep(Some(id)));
                                }
                            }
                            merge::text::TextMergeOutcome::Conflicted { merged, regions } => {
                                let merged_id = self.put_content(&merged)?;
                                let mut terms = Vec::new();
                                for r in &regions {
                                    let lc = self.put_content(&r.left)?;
                                    let rc = self.put_content(&r.right)?;
                                    terms.push(ConflictTerm {
                                        path: format!("#{}", r.index),
                                        kind: TermKind::TextRegion,
                                        candidates: vec![Some(lc), Some(rc)],
                                    });
                                }
                                terms.extend(self.metadata_terms(&meta.conflicts)?);
                                (TEXT_DRIVER_V1, (Some(merged_id), terms))
                            }
                        }
                    }
                    (
                        ObjectBody::Record {
                            content: lc,
                            schema: ls_,
                        },
                        ObjectBody::Record {
                            content: rc,
                            schema: rs_,
                        },
                        base_body,
                    ) if l.type_id == r.type_id && ls_ == rs_ => {
                        let schema = self
                            .schemas
                            .get(ls_)
                            .cloned()
                            .ok_or_else(|| StoreError::UnknownSchema(ls_.clone()))?;
                        let base_rec = match base_body {
                            Some(ObjectBody::Record { content, .. }) => self
                                .content_unless_redacted(*content)?
                                .map(|b| RecordValue::decode(&b))
                                .transpose()?,
                            _ => None,
                        };
                        let (Some(lb), Some(rb)) = (
                            self.content_unless_redacted(*lc)?,
                            self.content_unless_redacted(*rc)?,
                        ) else {
                            let frame = ConflictFrame {
                                object_id,
                                base: base_ids,
                                sides,
                                type_hint,
                                meta,
                            };
                            return self.whole_object_conflict(frame, &ls, &rs);
                        };
                        let lr = RecordValue::decode(&lb)?;
                        let rr = RecordValue::decode(&rb)?;
                        match merge::record::merge_record(&schema, base_rec.as_ref(), &lr, &rr) {
                            merge::record::RecordMergeOutcome::Merged(v) => {
                                let content = self.put_content(&v.encode()?)?;
                                if !meta.conflicts.is_empty() {
                                    let terms = self.metadata_terms(&meta.conflicts)?;
                                    (RECORD_DRIVER_V1, (Some(content), terms))
                                } else {
                                    let state = ObjectState::new(
                                        object_id,
                                        &l.type_id,
                                        ObjectBody::Record {
                                            content,
                                            schema: ls_.clone(),
                                        },
                                    )
                                    .with_metadata(meta.merged);
                                    let id = self.put_state(&state)?;
                                    return Ok(ObjectMerge::Keep(Some(id)));
                                }
                            }
                            merge::record::RecordMergeOutcome::Conflicted {
                                merged,
                                terms: rterms,
                            } => {
                                let merged_id = self.put_content(&merged.encode()?)?;
                                let mut terms = Vec::new();
                                for t in &rterms {
                                    let mut cands = Vec::new();
                                    for side in [&t.left, &t.right] {
                                        cands.push(match side {
                                            Some(v) => {
                                                Some(self.put_content(&cbor::encode(&v.to_cbor()))?)
                                            }
                                            None => None,
                                        });
                                    }
                                    terms.push(ConflictTerm {
                                        path: t.path.to_string(),
                                        kind: t.kind,
                                        candidates: cands,
                                    });
                                }
                                terms.extend(self.metadata_terms(&meta.conflicts)?);
                                (RECORD_DRIVER_V1, (Some(merged_id), terms))
                            }
                        }
                    }
                    _ => {
                        let mut terms = vec![whole_object_term(&ls, &rs)];
                        terms.extend(self.metadata_terms(&meta.conflicts)?);
                        ("merge.object.v1", (None, terms))
                    }
                }
            }
            _ => {
                let kind = if ls.is_none() || rs.is_none() {
                    TermKind::DeletedVsEdited
                } else {
                    TermKind::Value
                };
                let mut term = whole_object_term(&ls, &rs);
                term.kind = kind;
                let mut terms = vec![term];
                terms.extend(self.metadata_terms(&meta.conflicts)?);
                ("merge.object.v1", (None, terms))
            }
        };
        let (merged, terms) = outcome;
        let frame = ConflictFrame {
            object_id,
            base: base_ids,
            sides,
            type_hint,
            meta,
        };
        self.finish_conflict(frame, driver, merged, terms)
    }

    /// A conflict over the whole object, for bodies that cannot be merged
    /// by content (one of them was erased by a redaction).
    fn whole_object_conflict(
        &mut self,
        frame: ConflictFrame,
        ls: &Option<ObjectState>,
        rs: &Option<ObjectState>,
    ) -> Result<ObjectMerge> {
        let mut terms = vec![whole_object_term(ls, rs)];
        terms.extend(self.metadata_terms(&frame.meta.conflicts)?);
        self.finish_conflict(frame, "merge.object.v1", None, terms)
    }

    fn finish_conflict(
        &mut self,
        frame: ConflictFrame,
        driver: &str,
        merged: Option<ContentId>,
        terms: Vec<ConflictTerm>,
    ) -> Result<ObjectMerge> {
        let conflict = Conflict {
            format: CONFLICT_FORMAT_V1,
            object_id: frame.object_id,
            driver: driver.to_owned(),
            base: frame.base,
            sides: frame.sides,
            merged,
            terms,
        };
        let conflict_id = self.put_conflict(&conflict)?;
        let state = ObjectState::new(
            frame.object_id,
            frame.type_hint,
            ObjectBody::Conflict {
                conflict: conflict_id,
            },
        )
        .with_metadata(frame.meta.merged);
        let state_id = self.put_state(&state)?;
        Ok(ObjectMerge::Conflict(state_id, conflict_id))
    }

    /// One `Metadata` term per key both sides changed differently.
    fn metadata_terms(
        &mut self,
        conflicts: &[(String, Option<Value>, Option<Value>)],
    ) -> Result<Vec<ConflictTerm>> {
        let mut terms = Vec::with_capacity(conflicts.len());
        for (key, l, r) in conflicts {
            let mut candidates = Vec::with_capacity(2);
            for side in [l, r] {
                candidates.push(match side {
                    Some(v) => Some(self.put_content(&cbor::encode(v))?),
                    None => None,
                });
            }
            terms.push(ConflictTerm {
                path: key.clone(),
                kind: TermKind::Metadata,
                candidates,
            });
        }
        Ok(terms)
    }
}

/// A whole-object conflict term: candidates are the sides' content IDs
/// where they have one.
fn whole_object_term(l: &Option<ObjectState>, r: &Option<ObjectState>) -> ConflictTerm {
    let cid = |s: &Option<ObjectState>| -> Option<ContentId> {
        match s.as_ref().map(|s| &s.body) {
            Some(ObjectBody::Text { content }) | Some(ObjectBody::Record { content, .. }) => {
                Some(*content)
            }
            _ => None,
        }
    };
    ConflictTerm {
        path: String::new(),
        kind: TermKind::Value,
        candidates: vec![cid(l), cid(r)],
    }
}

/// The key-wise merge of two sides' metadata.
struct MetadataMerge {
    /// Every key the sides agree on, or that only one side changed.
    merged: Value,
    /// Keys both sides changed differently: (key, left value, right value),
    /// `None` where that side removed the key.
    conflicts: Vec<(String, Option<Value>, Option<Value>)>,
}

/// Metadata merge: key-wise, a key one side changed takes that side's
/// value; a key both sides changed differently is a conflict, never a
/// silent pick (a title is a path to the CLI, not decoration).
fn merge_metadata(l: &Value, r: &Value, base: Option<&Value>) -> MetadataMerge {
    let lm = l.map_entries().unwrap_or_default();
    let rm = r.map_entries().unwrap_or_default();
    let bm = base.and_then(Value::map_entries).unwrap_or_default();
    let keys: BTreeSet<&str> = lm.keys().chain(rm.keys()).copied().collect();
    let mut merged = Vec::new();
    let mut conflicts = Vec::new();
    for k in keys {
        let (lv, rv, bv) = (lm.get(k), rm.get(k), bm.get(k));
        let pick = if lv == rv || rv == bv {
            lv
        } else if lv == bv {
            rv
        } else {
            conflicts.push((
                k.to_owned(),
                lv.map(|v| (*v).clone()),
                rv.map(|v| (*v).clone()),
            ));
            continue;
        };
        if let Some(v) = pick {
            merged.push((Value::text(k), (*v).clone()));
        }
    }
    MetadataMerge {
        merged: Value::Map(merged),
        conflicts,
    }
}
