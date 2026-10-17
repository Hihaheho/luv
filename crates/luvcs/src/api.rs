//! The request/response surface every host drives luv through.
//!
//! A host translates its inputs into a [`VcsRequest`], calls [`execute`]
//! against the store database, and renders the [`VcsResponse`]. Nothing here
//! is platform-specific: the host supplies a [`HostContext`] (digest,
//! randomness, clock, device id) and the connection.
//!
//! IDs cross the boundary in their prefixed display form (`repo_…`,
//! `cmt_…`, `obj_…`). Records and record schemas cross as Eure documents
//! (see [`record_from_eure`]).

use std::collections::BTreeMap;

use base64::Engine as _;
use rusqlite::{Connection, params};

pub use crate::api_types::*;
use crate::blob_store;
use crate::bookmark::{Field, Life, SubjectState};
use crate::cbor::Value;
use crate::digest::{Digest, DigestError};
use crate::id::{
    BlobId, BookmarkId, BookmarkUpdateId, CommitId, IdKey, ObjectId, ObjectStateId, OperationId,
    RealmId, RepoId, WorkspaceId,
};
use crate::merge::RECORD_DRIVER_V2;
use crate::model::{Actor, ActorKind, BlobRef, ObjectBody, ObjectState, TermKind};
use crate::record::RecordValue;
use crate::store::{
    Advance, BookmarkEdit, BookmarkEntry, OperationContext, RecordSource, Repo, Store, StoreError,
    Workspace,
};

mod terms;

/// What a host provides for one call.
pub struct HostContext<'a> {
    pub digest: &'a dyn Digest,
    /// Fills `out` with cryptographically random bytes.
    pub random: &'a dyn Fn(&mut [u8]) -> Result<(), String>,
    pub now_unix_ms: i64,
    pub device_id: String,
    pub actor_kind: ActorKind,
}

impl HostContext<'_> {
    pub(crate) fn random_16(&self) -> Result<[u8; 16], VcsError> {
        let mut out = [0u8; 16];
        (self.random)(&mut out).map_err(|m| VcsError::new(VcsErrorCode::Host, m))?;
        Ok(out)
    }

    fn random_32(&self) -> Result<[u8; 32], VcsError> {
        let mut out = [0u8; 32];
        (self.random)(&mut out).map_err(|m| VcsError::new(VcsErrorCode::Host, m))?;
        Ok(out)
    }

    fn actor(&self) -> Actor {
        Actor {
            kind: self.actor_kind,
            device_id: self.device_id.clone(),
            description: String::new(),
        }
    }
}

/// Per-device bookkeeping the API keeps next to the store: the ID key of
/// each repo and the device's default workspace in it. The ID key is stored
/// as is, so the database file is as sensitive as the content it holds.
const LOCAL_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS vcs_local_repos (
  repo_id BLOB PRIMARY KEY,
  name TEXT NOT NULL,
  id_key BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  created_at_unix_ms INTEGER NOT NULL
) STRICT;
"#;

impl From<StoreError> for VcsError {
    fn from(e: StoreError) -> Self {
        let code = match &e {
            StoreError::UnknownRepo(_)
            | StoreError::UnknownWorkspace(_)
            | StoreError::UnknownCommit(_)
            | StoreError::MissingObject { .. }
            | StoreError::UnknownBinding(_)
            | StoreError::UnknownBookmark(_) => VcsErrorCode::NotFound,
            StoreError::StaleToken { .. } | StoreError::StaleBookmark => VcsErrorCode::Stale,
            StoreError::RepoExists(_)
            | StoreError::IdentityConflict { .. }
            | StoreError::NameTaken(_) => VcsErrorCode::Conflict,
            StoreError::Redacted { .. } => VcsErrorCode::Redacted,
            StoreError::NothingToRecord => VcsErrorCode::NothingToRecord,
            StoreError::UnrecordedChanges => VcsErrorCode::UnrecordedChanges,
            StoreError::Core(crate::model::CoreError::Unsupported { .. }) => {
                VcsErrorCode::Unsupported
            }
            StoreError::Sqlite(_) | StoreError::Digest(_) => VcsErrorCode::Internal,
            StoreError::Invalid(_)
            | StoreError::Record(_)
            | StoreError::Cbor(_)
            | StoreError::Core(_)
            | StoreError::Tree(_)
            | StoreError::NoCandidate
            | StoreError::NothingToUndo
            | StoreError::NothingToRedo
            | StoreError::Schema(_)
            | StoreError::DuplicateOperation(_)
            | StoreError::BookmarkIsDefault(_) => VcsErrorCode::Invalid,
        };
        Self::new(code, e.to_string())
    }
}

impl From<rusqlite::Error> for VcsError {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(VcsErrorCode::Internal, e.to_string())
    }
}

impl From<DigestError> for VcsError {
    fn from(e: DigestError) -> Self {
        Self::new(VcsErrorCode::Host, e.to_string())
    }
}

impl From<blob_store::BlobStoreError> for VcsError {
    fn from(e: blob_store::BlobStoreError) -> Self {
        use blob_store::BlobStoreError as E;
        let code = match &e {
            E::NotFound(..) => VcsErrorCode::NotFound,
            E::Blob(crate::blob::BlobError::Limit(_)) => VcsErrorCode::Unsupported,
            E::Blob(crate::blob::BlobError::Digest(_)) => VcsErrorCode::Host,
            E::Blob(_) => VcsErrorCode::Invalid,
            E::Sqlite(_) | E::Corrupt(..) => VcsErrorCode::Internal,
        };
        Self::new(code, e.to_string())
    }
}

fn blob_info_view(info: &blob_store::BlobInfo) -> VcsBlobInfo {
    VcsBlobInfo {
        realm_id: info.blob_ref.realm_id.to_string(),
        blob_id: info.blob_ref.blob_id.to_string(),
        byte_length: info.blob_ref.byte_length,
        raw_digest_hex: hex::encode(info.raw_digest),
        created_at_unix_ms: info.created_at_unix_ms,
    }
}

fn decode_base64(what: &str, text: &str) -> ApiResult<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| VcsError::invalid(format!("{what}: {e}")))
}

pub(crate) type ApiResult<T> = Result<T, VcsError>;

fn parse<T: std::str::FromStr<Err = crate::id::IdParseError>>(what: &str, s: &str) -> ApiResult<T> {
    s.parse::<T>()
        .map_err(|e| VcsError::invalid(format!("{what} {s:?}: {e}")))
}

pub(crate) struct LocalRepo {
    pub(crate) repo_id: RepoId,
    pub(crate) name: String,
    pub(crate) key: IdKey,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) created_at_unix_ms: i64,
}

impl LocalRepo {
    fn info(&self) -> VcsRepoInfo {
        VcsRepoInfo {
            repo_id: self.repo_id.to_string(),
            name: self.name.clone(),
            workspace_id: self.workspace_id.to_string(),
            created_at_unix_ms: self.created_at_unix_ms,
        }
    }
}

pub(crate) fn ensure_local_schema(conn: &Connection) -> ApiResult<()> {
    conn.execute_batch(LOCAL_SCHEMA_SQL)?;
    Ok(())
}

pub(crate) fn local_repos(conn: &Connection) -> ApiResult<Vec<LocalRepo>> {
    let mut stmt = conn.prepare(
        "SELECT repo_id, name, id_key, workspace_id, created_at_unix_ms FROM vcs_local_repos ORDER BY created_at_unix_ms, repo_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, Vec<u8>>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (repo, name, key, ws, created) = row?;
        let bad = |m: String| VcsError::new(VcsErrorCode::Internal, m);
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| bad("stored ID key is not 32 bytes".into()))?;
        out.push(LocalRepo {
            repo_id: RepoId::from_bytes(&repo).map_err(|e| bad(e.to_string()))?,
            name,
            key: IdKey(key),
            workspace_id: WorkspaceId::from_bytes(&ws).map_err(|e| bad(e.to_string()))?,
            created_at_unix_ms: created,
        });
    }
    Ok(out)
}

pub(crate) fn local_repo(conn: &Connection, repo_id: &str) -> ApiResult<LocalRepo> {
    let id: RepoId = parse("repo id", repo_id)?;
    local_repos(conn)?
        .into_iter()
        .find(|r| r.repo_id == id)
        .ok_or_else(|| {
            VcsError::new(
                VcsErrorCode::NotFound,
                format!("repo {id} is not on this device"),
            )
        })
}

/// Parses a record given as an Eure document.
pub fn record_from_eure(source: &str) -> ApiResult<RecordValue> {
    RecordValue::from_eure(source).map_err(|e| VcsError::invalid(e.to_string()))
}

fn cbor_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Bool(b) => J::Bool(*b),
        Value::Uint(u) => J::from(*u),
        Value::Nint(n) => J::from(*n),
        Value::Text(s) => J::from(s.clone()),
        Value::Bytes(b) => J::from(hex::encode(b)),
        Value::Array(a) => J::Array(a.iter().map(cbor_to_json).collect()),
        Value::Map(m) => J::Object(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::Text(s) => s.clone(),
                        other => format!("{other:?}"),
                    };
                    (key, cbor_to_json(v))
                })
                .collect(),
        ),
    }
}

fn metadata(title: Option<&str>) -> Value {
    match title {
        Some(t) => Value::map([("title", Value::text(t))]),
        None => Value::Map(vec![]),
    }
}

/// A bookmark token: the heads it was read at, as text the host passes back.
fn token_of(heads: &[BookmarkUpdateId]) -> String {
    heads
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_token(token: &str) -> ApiResult<Vec<BookmarkUpdateId>> {
    token
        .split(',')
        .filter(|t| !t.is_empty())
        .map(|t| parse("bookmark token", t))
        .collect()
}

fn life_view(life: Life) -> VcsBookmarkLife {
    let (kind, fused_into) = match life {
        Life::Live => (VcsBookmarkLifeKind::Live, None),
        Life::Deleted => (VcsBookmarkLifeKind::Deleted, None),
        Life::FusedInto(b) => (VcsBookmarkLifeKind::Fused, Some(b.to_string())),
    };
    VcsBookmarkLife { kind, fused_into }
}

fn parse_life(life: &VcsBookmarkLife) -> ApiResult<Life> {
    match (life.kind, &life.fused_into) {
        (VcsBookmarkLifeKind::Live, None) => Ok(Life::Live),
        (VcsBookmarkLifeKind::Deleted, None) => Ok(Life::Deleted),
        (VcsBookmarkLifeKind::Fused, Some(b)) => Ok(Life::FusedInto(parse("bookmark id", b)?)),
        _ => Err(VcsError::invalid(
            "fused_into is given exactly when the life is fused",
        )),
    }
}

fn advance_view(advance: Advance) -> VcsAdvance {
    match advance {
        Advance::Advanced(_) => VcsAdvance::Advanced,
        Advance::NoSelection => VcsAdvance::NoSelection,
        Advance::Stale => VcsAdvance::Stale,
        Advance::Unresolved => VcsAdvance::Unresolved,
        Advance::NotForward => VcsAdvance::NotForward,
    }
}

/// The default bookmark after following fused bookmarks to a live one;
/// `None` when unset, in conflict, deleted, or the chain does not end.
fn effective_default(settings: &SubjectState, entries: &[BookmarkEntry]) -> Option<BookmarkId> {
    let mut at = settings.default_bookmark.value().copied().flatten()?;
    for _ in 0..=entries.len() {
        let entry = entries.iter().find(|e| e.id == at)?;
        match entry.state.life {
            Field::Value(Life::Live) => return Some(at),
            Field::Value(Life::FusedInto(next)) => at = next,
            _ => return None,
        }
    }
    None
}

fn bookmark_entry_view(
    entry: &BookmarkEntry,
    default: Option<BookmarkId>,
    selected: Option<BookmarkId>,
) -> VcsBookmarkView {
    fn candidates<T, U>(field: &Field<T>, f: impl Fn(&T) -> U) -> Vec<U> {
        match field {
            Field::Conflict(c) => c.iter().map(|c| f(&c.value)).collect(),
            _ => Vec::new(),
        }
    }
    let s = &entry.state;
    VcsBookmarkView {
        bookmark_id: entry.id.to_string(),
        name: s.name.value().cloned(),
        target_commit_id: s.target.value().map(ToString::to_string),
        life: s.life.value().copied().map(life_view),
        name_candidates: candidates(&s.name, String::clone),
        target_candidates: candidates(&s.target, ToString::to_string),
        life_candidates: candidates(&s.life, |l| life_view(*l)),
        name_shared: entry.name_shared,
        is_default: default == Some(entry.id),
        is_selected: selected == Some(entry.id),
        token: token_of(&s.heads),
    }
}

fn title_of(state: &ObjectState) -> Option<String> {
    state
        .metadata
        .get("title")
        .and_then(Value::as_text)
        .map(str::to_owned)
}

fn actor_kind(kind: &ActorKind) -> &'static str {
    match kind {
        ActorKind::User => "user",
        ActorKind::Agent => "agent",
        ActorKind::Import => "import",
        ActorKind::System => "system",
    }
}

struct Session<'r, 's, 'c, D: Digest> {
    repo: &'r mut Repo<'s, 'c, D>,
    local: &'r LocalRepo,
    host: &'r HostContext<'r>,
}

impl<D: Digest> Session<'_, '_, '_, D> {
    /// Where a record takes its snapshot from: the working tree, or only
    /// `objects` of it (frozen as the candidate first).
    fn record_source(&mut self, objects: Option<Vec<String>>) -> ApiResult<RecordSource> {
        let Some(ids) = objects else {
            return Ok(RecordSource::Working);
        };
        let ids = ids
            .iter()
            .map(|s| parse::<ObjectId>("object id", s))
            .collect::<ApiResult<Vec<_>>>()?;
        let ctx = self.ctx_with_reason("select")?;
        self.repo.select_candidate(&ctx, &ids)?;
        Ok(RecordSource::Candidate)
    }

    fn bookmarks_view(&mut self) -> ApiResult<VcsBookmarks> {
        let selected = self.workspace()?.selected_bookmark;
        let entries = self.repo.bookmarks()?;
        let settings = self.repo.repo_settings()?;
        let effective = effective_default(&settings, &entries);
        let bookmarks = entries
            .iter()
            .map(|e| bookmark_entry_view(e, effective, selected))
            .collect();
        Ok(VcsBookmarks {
            bookmarks,
            default_bookmark_id: settings
                .default_bookmark
                .value()
                .copied()
                .flatten()
                .map(|b| b.to_string()),
            default_candidates: match &settings.default_bookmark {
                Field::Conflict(c) => c.iter().map(|c| c.value.map(|b| b.to_string())).collect(),
                _ => Vec::new(),
            },
            default_token: token_of(&settings.heads),
            selected_bookmark_id: selected.map(|b| b.to_string()),
        })
    }

    fn bookmark_view(&mut self, id: BookmarkId) -> ApiResult<VcsBookmarkView> {
        let wanted = id.to_string();
        self.bookmarks_view()?
            .bookmarks
            .into_iter()
            .find(|b| b.bookmark_id == wanted)
            .ok_or_else(|| StoreError::UnknownBookmark(id).into())
    }

    fn ctx(&self) -> ApiResult<OperationContext> {
        Ok(OperationContext {
            operation_id: OperationId(self.host.random_16()?),
            workspace_id: self.local.workspace_id,
            expected_token: None,
            actor: self.host.actor(),
            reason: String::new(),
            now_unix_ms: self.host.now_unix_ms,
        })
    }

    fn ctx_with_reason(&self, reason: &str) -> ApiResult<OperationContext> {
        let mut c = self.ctx()?;
        c.reason = reason.to_owned();
        Ok(c)
    }

    fn workspace(&mut self) -> ApiResult<Workspace> {
        match self.repo.workspace(self.local.workspace_id) {
            Ok(ws) => Ok(ws),
            Err(StoreError::UnknownWorkspace(_)) => {
                // First use on this device: position at the single head if
                // there is one, else at the empty tree.
                let heads = self.repo.visible()?;
                let base = if heads.len() == 1 {
                    Some(heads[0])
                } else {
                    None
                };
                Ok(self.repo.create_workspace(self.local.workspace_id, base)?)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn content_text(&self, id: crate::id::ContentId) -> ApiResult<Option<String>> {
        Ok(self
            .repo
            .get_content(id)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    fn realm(&self) -> crate::blob::Realm {
        blob_store::repo_realm(self.local.repo_id, &self.local.key)
    }

    fn blob_ref_view(&self, blob: &BlobRef) -> ApiResult<VcsBlobRefView> {
        let realm_id = RealmId::from_bytes(&blob.realm_id)
            .map_err(|e| VcsError::new(VcsErrorCode::Internal, e.to_string()))?;
        let blob_id = BlobId::from_bytes(&blob.blob_id)
            .map_err(|e| VcsError::new(VcsErrorCode::Internal, e.to_string()))?;
        let present = blob_store::stat(self.repo.connection(), realm_id, blob_id)?.is_some();
        Ok(VcsBlobRefView {
            realm_id: realm_id.to_string(),
            blob_id: blob_id.to_string(),
            byte_length: blob.length,
            present,
        })
    }

    fn blob_info(&self, blob_id: &str) -> ApiResult<(RealmId, BlobId, VcsBlobInfo)> {
        let realm = self.realm();
        let id: BlobId = parse("blob id", blob_id)?;
        let Some(info) = blob_store::stat(self.repo.connection(), realm.id, id)? else {
            if self.repo.is_redacted_blob(id)? {
                return Err(StoreError::Redacted {
                    kind: "blob",
                    id: id.to_string(),
                }
                .into());
            }
            return Err(VcsError::new(
                VcsErrorCode::NotFound,
                format!("blob {id} is not on this device"),
            ));
        };
        Ok((realm.id, id, blob_info_view(&info)))
    }

    /// A record, or a value inside one (a term candidate), as Eure.
    fn content_record_eure(&self, id: crate::id::ContentId) -> ApiResult<Option<String>> {
        self.repo
            .get_content(id)?
            .map(|bytes| {
                let record = RecordValue::decode(&bytes)
                    .map_err(|e| VcsError::new(VcsErrorCode::Internal, e.to_string()))?;
                Ok(record.to_eure())
            })
            .transpose()
    }

    /// A metadata candidate as text; `None` for a value that is not text.
    fn content_metadata_text(&self, id: crate::id::ContentId) -> ApiResult<Option<String>> {
        let Some(bytes) = self.repo.get_content(id)? else {
            return Ok(None);
        };
        let value = crate::cbor::decode(&bytes)
            .map_err(|e| VcsError::new(VcsErrorCode::Internal, e.to_string()))?;
        Ok(value.as_text().map(str::to_owned))
    }

    fn state_text(&self, id: ObjectStateId) -> ApiResult<Option<String>> {
        let state = self.repo.get_state(id)?;
        Ok(match state.body {
            ObjectBody::Text { content } => self.content_text(content)?,
            ObjectBody::Record { content, .. } => self.content_record_eure(content)?,
            _ => None,
        })
    }

    fn conflict_view(
        &self,
        object_id: ObjectId,
        conflict_id: crate::id::ConflictId,
    ) -> ApiResult<VcsConflictView> {
        let c = self.repo.get_conflict(conflict_id)?;
        let mut sides = Vec::with_capacity(c.sides.len());
        let mut side_titles = Vec::with_capacity(c.sides.len());
        for s in &c.sides {
            sides.push(self.state_text(*s)?);
            side_titles.push(title_of(&self.repo.get_state(*s)?));
        }
        let mut terms = Vec::with_capacity(c.terms.len());
        for t in &c.terms {
            let mut candidates = Vec::with_capacity(t.candidates.len());
            for (index, cand) in t.candidates.iter().enumerate() {
                candidates.push(match cand {
                    Some(id) if t.kind == TermKind::Metadata => self.content_metadata_text(*id)?,
                    Some(id) => {
                        let is_record = c.driver == RECORD_DRIVER_V2
                            || c.sides
                                .get(index)
                                .map(|s| self.repo.get_state(*s))
                                .transpose()?
                                .is_some_and(|s| matches!(s.body, ObjectBody::Record { .. }));
                        if is_record {
                            self.content_record_eure(*id)?
                        } else {
                            self.content_text(*id)?
                        }
                    }
                    None => None,
                });
            }
            terms.push(VcsConflictTermView {
                path: t.path.clone(),
                kind: t.kind.as_str().to_owned(),
                candidates,
            });
        }
        Ok(VcsConflictView {
            object_id: object_id.to_string(),
            conflict_id: conflict_id.to_string(),
            driver: c.driver.clone(),
            merged_record_eure: match c.merged {
                Some(m) if c.driver == RECORD_DRIVER_V2 => self.content_record_eure(m)?,
                _ => None,
            },
            merged_text: match c.merged {
                Some(m) if c.driver != RECORD_DRIVER_V2 => self.content_text(m)?,
                _ => None,
            },
            marked_text: self.marked_text(&c)?,
            sides,
            side_titles,
            terms,
        })
    }

    /// A text conflict as full text with markers around each conflicting
    /// region, merged again from its base and sides; `None` for any other
    /// conflict.
    fn marked_text(&self, c: &crate::model::Conflict) -> ApiResult<Option<String>> {
        let text = |id: &ObjectStateId| -> ApiResult<Option<Vec<u8>>> {
            Ok(match self.repo.get_state(*id)?.body {
                ObjectBody::Text { content } => self.repo.get_content(content)?,
                _ => None,
            })
        };
        if c.driver != crate::merge::TEXT_DRIVER_V1 {
            return Ok(None);
        }
        let [l, r] = c.sides.as_slice() else {
            return Ok(None);
        };
        let (Some(left), Some(right)) = (text(l)?, text(r)?) else {
            return Ok(None);
        };
        let base = match c.base.as_slice() {
            [] => None,
            [b] => text(b)?,
            _ => return Ok(None),
        };
        let outcome = crate::merge::text::merge_text(base.as_deref(), &left, &right);
        let marked = crate::merge::text::render_with_markers(&outcome);
        Ok(Some(String::from_utf8_lossy(&marked).into_owned()))
    }

    fn object_view(
        &self,
        object_id: ObjectId,
        state_id: ObjectStateId,
    ) -> ApiResult<VcsObjectView> {
        let state = self.repo.get_state(state_id)?;
        let mut view = VcsObjectView {
            object_id: object_id.to_string(),
            state_id: state_id.to_string(),
            type_id: state.type_id.clone(),
            title: title_of(&state),
            body_kind: String::new(),
            text: None,
            record_eure: None,
            schema_id: None,
            conflict: None,
            blob: None,
            metadata_json: cbor_to_json(&state.metadata).to_string(),
            redacted: false,
        };
        match &state.body {
            ObjectBody::Text { content } => {
                view.body_kind = "text".into();
                view.text = self.content_text(*content)?;
                view.redacted = self.repo.is_redacted_content(*content)?;
            }
            ObjectBody::Record { content, schema } => {
                view.body_kind = "record".into();
                view.schema_id = Some(schema.to_string());
                view.record_eure = self.content_record_eure(*content)?;
                view.redacted = self.repo.is_redacted_content(*content)?;
            }
            ObjectBody::Binary { blob } => {
                view.body_kind = "binary".into();
                let blob = self.blob_ref_view(blob)?;
                view.redacted = !blob.present
                    && self
                        .repo
                        .is_redacted_blob(parse("blob id", &blob.blob_id)?)?;
                view.blob = Some(blob);
            }
            ObjectBody::Conflict { conflict } => {
                view.body_kind = "conflict".into();
                view.conflict = Some(self.conflict_view(object_id, *conflict)?);
            }
            ObjectBody::Deleted => view.body_kind = "deleted".into(),
        }
        Ok(view)
    }

    fn commit_view(&self, id: CommitId, heads: &[CommitId]) -> ApiResult<VcsCommitView> {
        let c = self.repo.commit(id)?;
        let before = match c.parents.first() {
            Some(p) => Some(self.repo.commit(*p)?.root),
            None => None,
        };
        let diff = self.repo.diff(before, c.root)?;
        let mut changed: Vec<String> = diff
            .added
            .iter()
            .map(|(o, _)| o.to_string())
            .chain(diff.changed.iter().map(|(o, _, _)| o.to_string()))
            .chain(diff.removed.iter().map(|(o, _)| o.to_string()))
            .collect();
        changed.sort();
        Ok(VcsCommitView {
            commit_id: id.to_string(),
            change_id: c.change_id.to_string(),
            parents: c.parents.iter().map(ToString::to_string).collect(),
            description: c.description.clone(),
            author_kind: actor_kind(&c.author.kind).to_owned(),
            author_device: c.author.device_id.clone(),
            created_at_unix_ms: c.created_at_unix_ms,
            changed_objects: changed,
            is_head: heads.contains(&id),
        })
    }

    fn status(&mut self) -> ApiResult<VcsStatus> {
        let ws = self.workspace()?;
        let mut changes = Vec::new();
        for (object_id, kind) in working_changes(self.repo, &ws)? {
            let state = self
                .repo
                .lookup(ws.working, object_id)?
                .map(|s| self.repo.get_state(s))
                .transpose()?;
            changes.push(VcsChange {
                object_id: object_id.to_string(),
                kind: kind.to_owned(),
                type_id: state
                    .as_ref()
                    .map(|s| s.type_id.clone())
                    .unwrap_or_default(),
                title: state.as_ref().and_then(title_of),
                conflicted: matches!(
                    state.as_ref().map(|s| &s.body),
                    Some(ObjectBody::Conflict { .. })
                ),
            });
        }
        // Conflicts are a property of the working tree, not of the diff: an
        // integrate leaves them in the tree with nothing "changed" yet. Only
        // an object some stored conflict is about can hold one, so those are
        // looked up instead of reading every state of the tree.
        let mut conflicted = std::collections::BTreeSet::new();
        for id in self.repo.object_ids("conflict")? {
            let id = crate::id::ConflictId::from_bytes(&id)
                .map_err(|e| VcsError::new(VcsErrorCode::Internal, e.to_string()))?;
            conflicted.insert(self.repo.get_conflict(id)?.object_id);
        }
        let mut conflicts = Vec::new();
        for object_id in conflicted {
            let Some(state_id) = self.repo.lookup(ws.working, object_id)? else {
                continue;
            };
            if let ObjectBody::Conflict { conflict } = self.repo.get_state(state_id)?.body {
                conflicts.push(self.conflict_view(object_id, conflict)?);
            }
        }
        let (can_undo, can_redo) = self.repo.can_undo_redo()?;
        Ok(VcsStatus {
            repo_id: self.local.repo_id.to_string(),
            workspace_id: self.local.workspace_id.to_string(),
            base_commit_id: ws.base.map(|c| c.to_string()),
            token: ws.token,
            changes,
            conflicts,
            heads: self
                .repo
                .visible()?
                .iter()
                .map(ToString::to_string)
                .collect(),
            can_undo,
            can_redo,
        })
    }

    fn save(&mut self, object_id: Option<&str>, state_id: ObjectStateId) -> ApiResult<ObjectId> {
        let object_id = match object_id {
            Some(s) => parse("object id", s)?,
            None => ObjectId(self.host.random_16()?),
        };
        self.workspace()?;
        let ctx = self.ctx_with_reason("save")?;
        self.repo
            .save_working(&ctx, &[(object_id, Some(state_id))])?;
        Ok(object_id)
    }

    /// The tree of `commit_id`, or of the working tree.
    fn root_at(&mut self, commit_id: Option<&str>) -> ApiResult<crate::id::TreeId> {
        Ok(match commit_id {
            Some(c) => self.repo.commit(parse("commit id", c)?)?.root,
            None => self.workspace()?.working,
        })
    }

    fn entries_at(
        &mut self,
        commit_id: Option<&str>,
    ) -> ApiResult<BTreeMap<ObjectId, ObjectStateId>> {
        let root = self.root_at(commit_id)?;
        Ok(self.repo.tree_entries(root)?)
    }

    /// The working state of `id`, which must be in the working tree.
    fn working_state(&mut self, id: ObjectId) -> ApiResult<ObjectStateId> {
        let ws = self.workspace()?;
        self.repo.lookup(ws.working, id)?.ok_or_else(|| {
            VcsError::new(
                VcsErrorCode::NotFound,
                format!("object {id} is not in the working tree"),
            )
        })
    }

    fn run(&mut self, request: VcsRequest) -> ApiResult<VcsResponse> {
        Ok(match request {
            VcsRequest::CreateRepo { .. } | VcsRequest::ListRepos | VcsRequest::Import { .. } => {
                unreachable!("handled by execute")
            }
            VcsRequest::Status { .. } => VcsResponse::Status {
                status: self.status()?,
            },
            VcsRequest::SaveText {
                object_id,
                type_id,
                text,
                title,
                ..
            } => {
                let state = self.repo.put_text_state(
                    match &object_id {
                        Some(s) => parse("object id", s)?,
                        None => ObjectId(self.host.random_16()?),
                    },
                    &type_id,
                    text.as_bytes(),
                    metadata(title.as_deref()),
                )?;
                // put_text_state fixes the object id inside the state, so
                // the id chosen above must be the one saved.
                let state_obj = self.repo.get_state(state)?;
                let id = self.save(Some(&state_obj.object_id.to_string()), state)?;
                VcsResponse::Object {
                    object: self.object_view(id, state)?,
                }
            }
            VcsRequest::SaveRecord {
                object_id,
                type_id,
                schema_id,
                record_eure,
                title,
                ..
            } => {
                let record = record_from_eure(&record_eure)?;
                let schema = parse("schema id", &schema_id)?;
                let oid = match &object_id {
                    Some(s) => parse("object id", s)?,
                    None => ObjectId(self.host.random_16()?),
                };
                let state = self.repo.put_record_state(
                    oid,
                    &type_id,
                    schema,
                    &record,
                    metadata(title.as_deref()),
                )?;
                let id = self.save(Some(&oid.to_string()), state)?;
                VcsResponse::Object {
                    object: self.object_view(id, state)?,
                }
            }
            VcsRequest::SaveBinary {
                object_id,
                type_id,
                data_base64,
                title,
                ..
            } => {
                let bytes = decode_base64("data", &data_base64)?;
                let realm = self.realm();
                let (blob_ref, _) = blob_store::put_local(
                    self.repo.connection(),
                    self.host.digest,
                    &realm,
                    &bytes,
                    self.host.now_unix_ms,
                )?;
                let oid = match &object_id {
                    Some(s) => parse("object id", s)?,
                    None => ObjectId(self.host.random_16()?),
                };
                let state = ObjectState::new(
                    oid,
                    &type_id,
                    ObjectBody::Binary {
                        blob: BlobRef {
                            realm_id: blob_ref.realm_id.to_vec(),
                            blob_id: blob_ref.blob_id.to_vec(),
                            length: blob_ref.byte_length,
                        },
                    },
                )
                .with_metadata(metadata(title.as_deref()));
                let state_id = self.repo.put_state(&state)?;
                let id = self.save(Some(&oid.to_string()), state_id)?;
                VcsResponse::Object {
                    object: self.object_view(id, state_id)?,
                }
            }
            VcsRequest::PutSchema { schema_eure, .. } => {
                let value = record_from_eure(&schema_eure)?;
                let id = self.repo.put_schema(&value)?;
                VcsResponse::Schema {
                    schema_id: id.to_string(),
                    schema_eure: value.to_eure(),
                }
            }
            VcsRequest::ReadSchema { schema_id, .. } => {
                let id = parse("schema id", &schema_id)?;
                let eure = self.content_record_eure(id)?.ok_or_else(|| {
                    VcsError::new(
                        VcsErrorCode::NotFound,
                        format!("schema {schema_id} is not here"),
                    )
                })?;
                VcsResponse::Schema {
                    schema_id,
                    schema_eure: eure,
                }
            }
            VcsRequest::PutBlob { data_base64, .. } => {
                let bytes = decode_base64("data", &data_base64)?;
                let realm = self.realm();
                let (blob_ref, _) = blob_store::put_local(
                    self.repo.connection(),
                    self.host.digest,
                    &realm,
                    &bytes,
                    self.host.now_unix_ms,
                )?;
                blob_store::pin(self.repo.connection(), realm.id, blob_ref.blob_id)?;
                let (_, _, blob) = self.blob_info(&blob_ref.blob_id.to_string())?;
                VcsResponse::Blob { blob }
            }
            VcsRequest::ReadBlob {
                blob_id,
                offset,
                length,
                ..
            } => {
                let (realm_id, id, blob) = self.blob_info(&blob_id)?;
                let conn = self.repo.connection();
                let whole = offset.is_none() && length.is_none();
                let offset = offset.unwrap_or(0);
                let bytes = if whole {
                    blob_store::read_verified(conn, self.host.digest, realm_id, id)?
                } else {
                    let length = length.unwrap_or(blob.byte_length.saturating_sub(offset));
                    blob_store::read_range(conn, realm_id, id, offset, length)?
                };
                VcsResponse::BlobBytes {
                    blob,
                    offset,
                    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                }
            }
            VcsRequest::ListBlobs { .. } => VcsResponse::Blobs {
                blobs: blob_store::list(self.repo.connection(), self.realm().id)?
                    .iter()
                    .map(blob_info_view)
                    .collect(),
            },
            VcsRequest::DeleteObject { object_id, .. } => {
                let id: ObjectId = parse("object id", &object_id)?;
                self.workspace()?;
                let ctx = self.ctx_with_reason("delete")?;
                self.repo.save_working(&ctx, &[(id, None)])?;
                VcsResponse::Ok
            }
            VcsRequest::SetTitle {
                object_id, title, ..
            } => {
                let id: ObjectId = parse("object id", &object_id)?;
                let current = self.working_state(id)?;
                let mut state = self.repo.get_state(current)?;
                match state.body {
                    ObjectBody::Deleted => {
                        return Err(VcsError::new(
                            VcsErrorCode::NotFound,
                            format!("object {id} is deleted"),
                        ));
                    }
                    // Its title belongs to the conflict until it is resolved.
                    ObjectBody::Conflict { .. } => {
                        return Err(VcsError::new(
                            VcsErrorCode::Conflict,
                            format!("object {id} is in conflict; resolve it first"),
                        ));
                    }
                    _ => {}
                }
                let mut entries: Vec<(Value, Value)> = state
                    .metadata
                    .map_entries()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|(k, _)| *k != "title")
                    .map(|(k, v)| (Value::text(k), v.clone()))
                    .collect();
                if let Some(t) = title {
                    entries.push((Value::text("title"), Value::text(t)));
                }
                state.metadata = Value::Map(entries);
                let state_id = self.repo.put_state(&state)?;
                let id = self.save(Some(&object_id), state_id)?;
                VcsResponse::Object {
                    object: self.object_view(id, state_id)?,
                }
            }
            VcsRequest::ReadObject {
                object_id,
                commit_id,
                ..
            } => {
                let id: ObjectId = parse("object id", &object_id)?;
                let root = self.root_at(commit_id.as_deref())?;
                let state = self.repo.lookup(root, id)?.ok_or_else(|| {
                    VcsError::new(
                        VcsErrorCode::NotFound,
                        format!("object {id} is not in this snapshot"),
                    )
                })?;
                VcsResponse::Object {
                    object: self.object_view(id, state)?,
                }
            }
            VcsRequest::ListObjects { commit_id, .. } => {
                let entries = self.entries_at(commit_id.as_deref())?;
                let mut objects = Vec::with_capacity(entries.len());
                for (id, state) in entries {
                    objects.push(self.object_view(id, state)?);
                }
                VcsResponse::Objects { objects }
            }
            VcsRequest::Record {
                description,
                objects,
                ..
            } => {
                self.workspace()?;
                let source = self.record_source(objects)?;
                let ctx = self.ctx_with_reason(&description)?;
                let id = self.repo.record(&ctx, source, &description)?;
                let heads = self.repo.visible()?;
                VcsResponse::Commit {
                    commit: self.commit_view(id, &heads)?,
                }
            }
            VcsRequest::Amend { description, .. } => {
                self.workspace()?;
                let ctx = self.ctx_with_reason(&description)?;
                let id = self.repo.amend(&ctx, &description)?;
                let heads = self.repo.visible()?;
                VcsResponse::Commit {
                    commit: self.commit_view(id, &heads)?,
                }
            }
            VcsRequest::Undo { .. } => {
                let ctx = self.ctx_with_reason("undo")?;
                let op = self.repo.undo(&ctx.actor, ctx.now_unix_ms)?;
                VcsResponse::Operation {
                    operation: operation_view(&op),
                }
            }
            VcsRequest::Redo { .. } => {
                let ctx = self.ctx_with_reason("redo")?;
                let op = self.repo.redo(&ctx.actor, ctx.now_unix_ms)?;
                VcsResponse::Operation {
                    operation: operation_view(&op),
                }
            }
            VcsRequest::Restore {
                commit_id,
                object_id,
                ..
            } => {
                self.workspace()?;
                let ctx = self.ctx_with_reason("restore")?;
                self.repo.restore(
                    &ctx,
                    parse("commit id", &commit_id)?,
                    parse("object id", &object_id)?,
                )?;
                VcsResponse::Ok
            }
            VcsRequest::Integrate { commit_id, .. } => {
                self.workspace()?;
                let ctx = self.ctx_with_reason("integrate")?;
                let out = self.repo.integrate(&ctx, parse("commit id", &commit_id)?)?;
                let mut conflicts = Vec::with_capacity(out.conflicts.len());
                for (o, c) in &out.conflicts {
                    conflicts.push(self.conflict_view(*o, *c)?);
                }
                VcsResponse::Integrated {
                    commit_id: out.commit.to_string(),
                    conflicts,
                }
            }
            VcsRequest::Resolve {
                object_id,
                resolution,
                ..
            } => {
                let id: ObjectId = parse("object id", &object_id)?;
                let current = self.working_state(id)?;
                let state = self.repo.get_state(current)?;
                let ObjectBody::Conflict { conflict } = state.body else {
                    return Err(VcsError::invalid(format!("object {id} is not in conflict")));
                };
                let conflict = self.repo.get_conflict(conflict)?;
                // Given content keeps the metadata the sides agree on (the
                // conflict state carries it); a key in conflict needs a value.
                let resolved_metadata = |title: Option<String>| -> ApiResult<Value> {
                    for t in conflict
                        .terms
                        .iter()
                        .filter(|t| t.kind == TermKind::Metadata)
                    {
                        if t.path != "title" {
                            return Err(VcsError::invalid(format!(
                                "metadata {:?} is in conflict and cannot be given; take a side",
                                t.path
                            )));
                        }
                        if title.is_none() {
                            return Err(VcsError::invalid(
                                "the title is in conflict; give a title or take a side",
                            ));
                        }
                    }
                    let mut entries: Vec<(Value, Value)> = state
                        .metadata
                        .map_entries()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|(k, _)| !(*k == "title" && title.is_some()))
                        .map(|(k, v)| (Value::text(k), v.clone()))
                        .collect();
                    if let Some(t) = title {
                        entries.push((Value::text("title"), Value::text(t)));
                    }
                    Ok(Value::Map(entries))
                };
                let resolved = match resolution {
                    VcsResolution::Terms {
                        conflict_id,
                        choices,
                    } => {
                        let ObjectBody::Conflict { conflict: current } = state.body else {
                            unreachable!("checked above");
                        };
                        if parse::<crate::id::ConflictId>("conflict id", &conflict_id)? != current {
                            return Err(VcsError::new(
                                VcsErrorCode::Stale,
                                format!("object {id} is in conflict {current} now"),
                            ));
                        }
                        terms::resolve(self.repo, id, &state, &conflict, &choices)?
                    }
                    VcsResolution::Side { index } => *conflict
                        .sides
                        .get(index as usize)
                        .ok_or_else(|| VcsError::invalid("no such side"))?,
                    VcsResolution::Text { text, title } => self.repo.put_text_state(
                        id,
                        &state.type_id,
                        text.as_bytes(),
                        resolved_metadata(title)?,
                    )?,
                    VcsResolution::Record { record_eure, title } => {
                        let record = record_from_eure(&record_eure)?;
                        let schema = conflict
                            .sides
                            .iter()
                            .find_map(|s| match self.repo.get_state(*s).ok()?.body {
                                ObjectBody::Record { schema, .. } => Some(schema),
                                _ => None,
                            })
                            .ok_or_else(|| VcsError::invalid("conflict is not over a record"))?;
                        self.repo.put_record_state(
                            id,
                            &state.type_id,
                            schema,
                            &record,
                            resolved_metadata(title)?,
                        )?
                    }
                };
                let ctx = self.ctx_with_reason("resolve")?;
                self.repo.resolve(&ctx, id, resolved)?;
                VcsResponse::Object {
                    object: self.object_view(id, resolved)?,
                }
            }
            VcsRequest::Log {
                from_commit_id,
                limit,
                ..
            } => {
                let heads = self.repo.visible()?;
                let from = match from_commit_id {
                    Some(c) => vec![parse("commit id", &c)?],
                    None => heads.clone(),
                };
                let mut seen = std::collections::BTreeSet::new();
                let mut entries = Vec::new();
                for start in from {
                    for e in self.repo.history(start, limit as usize)? {
                        if seen.insert(e.id) {
                            entries.push(e);
                        }
                    }
                }
                entries.sort_by(|a, b| {
                    b.commit
                        .created_at_unix_ms
                        .cmp(&a.commit.created_at_unix_ms)
                        .then_with(|| a.id.cmp(&b.id))
                });
                entries.truncate(limit as usize);
                let mut commits = Vec::with_capacity(entries.len());
                for e in entries {
                    commits.push(self.commit_view(e.id, &heads)?);
                }
                VcsResponse::Commits { commits }
            }
            VcsRequest::Show { commit_id, .. } => {
                let heads = self.repo.visible()?;
                VcsResponse::Commit {
                    commit: self.commit_view(parse("commit id", &commit_id)?, &heads)?,
                }
            }
            VcsRequest::Heads { .. } => VcsResponse::Heads {
                heads: self
                    .repo
                    .visible()?
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            },
            VcsRequest::Operations { limit, .. } => VcsResponse::Operations {
                operations: self
                    .repo
                    .operations(limit as usize)?
                    .iter()
                    .map(operation_view)
                    .collect(),
            },
            VcsRequest::ListBookmarks { .. } => VcsResponse::Bookmarks {
                bookmarks: self.bookmarks_view()?,
            },
            VcsRequest::CreateBookmark {
                name, commit_id, ..
            } => {
                self.workspace()?;
                let id = BookmarkId(self.host.random_16()?);
                let ctx = self.ctx_with_reason("bookmark create")?;
                self.repo
                    .create_bookmark(&ctx, id, &name, parse("commit id", &commit_id)?)?;
                VcsResponse::Bookmark {
                    bookmark: self.bookmark_view(id)?,
                }
            }
            VcsRequest::EditBookmark {
                bookmark_id,
                token,
                name,
                commit_id,
                life,
                new_default,
                ..
            } => {
                self.workspace()?;
                let id: BookmarkId = parse("bookmark id", &bookmark_id)?;
                let edit = BookmarkEdit {
                    life: life.as_ref().map(parse_life).transpose()?,
                    name,
                    target: commit_id.map(|c| parse("commit id", &c)).transpose()?,
                    new_default: new_default
                        .map(|d| match d {
                            VcsNewDefault::Bookmark { bookmark_id } => {
                                parse("bookmark id", &bookmark_id).map(Some)
                            }
                            VcsNewDefault::Unset => Ok(None),
                        })
                        .transpose()?,
                };
                let ctx = self.ctx_with_reason("bookmark edit")?;
                self.repo
                    .edit_bookmark(&ctx, id, &parse_token(&token)?, edit)?;
                VcsResponse::Bookmark {
                    bookmark: self.bookmark_view(id)?,
                }
            }
            VcsRequest::SetDefaultBookmark {
                token, bookmark_id, ..
            } => {
                self.workspace()?;
                let default = bookmark_id.map(|b| parse("bookmark id", &b)).transpose()?;
                let ctx = self.ctx_with_reason("bookmark default")?;
                self.repo
                    .set_default_bookmark(&ctx, &parse_token(&token)?, default)?;
                VcsResponse::Bookmarks {
                    bookmarks: self.bookmarks_view()?,
                }
            }
            VcsRequest::SelectBookmark { bookmark_id, .. } => {
                self.workspace()?;
                let selected = bookmark_id.map(|b| parse("bookmark id", &b)).transpose()?;
                let ctx = self.ctx_with_reason("bookmark select")?;
                self.repo.select_bookmark(&ctx, selected)?;
                VcsResponse::Bookmarks {
                    bookmarks: self.bookmarks_view()?,
                }
            }
            VcsRequest::RecordAdvancing {
                description,
                objects,
                token,
                ..
            } => {
                self.workspace()?;
                let source = self.record_source(objects)?;
                let ctx = self.ctx_with_reason(&description)?;
                let (id, advance) = self.repo.record_advancing(
                    &ctx,
                    source,
                    &description,
                    &parse_token(&token)?,
                )?;
                let heads = self.repo.visible()?;
                VcsResponse::Recorded {
                    commit: self.commit_view(id, &heads)?,
                    advance: advance_view(advance),
                }
            }
            VcsRequest::Purge {
                object_ids,
                blob_ids,
                reason,
                ..
            } => {
                let objects = object_ids
                    .iter()
                    .map(|s| parse::<ObjectId>("object id", s))
                    .collect::<ApiResult<Vec<_>>>()?;
                let blobs = blob_ids
                    .iter()
                    .map(|s| parse::<BlobId>("blob id", s))
                    .collect::<ApiResult<Vec<_>>>()?;
                // The workspace is checked like any head: create it first.
                self.workspace()?;
                let out = self.repo.purge(
                    &objects,
                    &blobs,
                    &self.host.actor(),
                    self.host.now_unix_ms,
                    &reason,
                )?;
                VcsResponse::Purged {
                    redaction_ids: out.redactions.iter().map(ToString::to_string).collect(),
                    contents: out.contents as u32,
                    blobs: out.blobs as u32,
                    kept_shared: out.kept_shared as u32,
                }
            }
            VcsRequest::Gc {
                keep_operations, ..
            } => {
                let out = self
                    .repo
                    .collect_garbage(keep_operations.map(|n| n as usize))?;
                VcsResponse::Collected {
                    objects: out.objects as u32,
                    blobs: out.blobs as u32,
                    operations: out.operations as u32,
                }
            }
            VcsRequest::Export { .. } => {
                let bytes = self.repo.export_archive()?;
                let working = self.workspace()?.working;
                let objects = self.repo.tree_entries(working)?.len();
                VcsResponse::Archive {
                    repo_id: self.local.repo_id.to_string(),
                    archive_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    id_key_base64: base64::engine::general_purpose::STANDARD
                        .encode(self.local.key.0),
                    objects: objects as u32,
                }
            }
        })
    }
}

fn operation_view(op: &crate::store::OperationEntry) -> VcsOperationView {
    VcsOperationView {
        seq: op.seq,
        operation_id: op.id.to_string(),
        kind: op.kind.clone(),
        reason: op.reason.clone(),
        created_at_unix_ms: op.created_at_unix_ms,
        undone: op.undone,
    }
}

fn repo_id_of(request: &VcsRequest) -> Option<&str> {
    match request {
        VcsRequest::CreateRepo { .. } | VcsRequest::ListRepos | VcsRequest::Import { .. } => None,
        VcsRequest::Status { repo_id }
        | VcsRequest::SaveText { repo_id, .. }
        | VcsRequest::SaveRecord { repo_id, .. }
        | VcsRequest::SaveBinary { repo_id, .. }
        | VcsRequest::PutBlob { repo_id, .. }
        | VcsRequest::PutSchema { repo_id, .. }
        | VcsRequest::ReadSchema { repo_id, .. }
        | VcsRequest::ReadBlob { repo_id, .. }
        | VcsRequest::ListBlobs { repo_id }
        | VcsRequest::DeleteObject { repo_id, .. }
        | VcsRequest::SetTitle { repo_id, .. }
        | VcsRequest::ReadObject { repo_id, .. }
        | VcsRequest::ListObjects { repo_id, .. }
        | VcsRequest::Record { repo_id, .. }
        | VcsRequest::Amend { repo_id, .. }
        | VcsRequest::Undo { repo_id }
        | VcsRequest::Redo { repo_id }
        | VcsRequest::Restore { repo_id, .. }
        | VcsRequest::Integrate { repo_id, .. }
        | VcsRequest::Resolve { repo_id, .. }
        | VcsRequest::Log { repo_id, .. }
        | VcsRequest::Show { repo_id, .. }
        | VcsRequest::Heads { repo_id }
        | VcsRequest::Operations { repo_id, .. }
        | VcsRequest::ListBookmarks { repo_id }
        | VcsRequest::CreateBookmark { repo_id, .. }
        | VcsRequest::EditBookmark { repo_id, .. }
        | VcsRequest::SetDefaultBookmark { repo_id, .. }
        | VcsRequest::SelectBookmark { repo_id, .. }
        | VcsRequest::RecordAdvancing { repo_id, .. }
        | VcsRequest::Purge { repo_id, .. }
        | VcsRequest::Gc { repo_id, .. }
        | VcsRequest::Export { repo_id } => Some(repo_id),
    }
}

pub(crate) fn insert_local_repo(conn: &Connection, local: &LocalRepo) -> ApiResult<()> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO vcs_local_repos (repo_id, name, id_key, workspace_id, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            local.repo_id.to_vec(),
            local.name,
            local.key.0.to_vec(),
            local.workspace_id.to_vec(),
            local.created_at_unix_ms,
        ],
    )?;
    if n == 0 {
        return Err(VcsError::new(
            VcsErrorCode::Conflict,
            format!("repo {} is already on this device", local.repo_id),
        ));
    }
    Ok(())
}

fn create_local_repo(
    conn: &Connection,
    host: &HostContext<'_>,
    name: &str,
) -> ApiResult<LocalRepo> {
    let local = LocalRepo {
        repo_id: RepoId(host.random_16()?),
        name: name.to_owned(),
        key: IdKey(host.random_32()?),
        workspace_id: WorkspaceId(host.random_16()?),
        created_at_unix_ms: host.now_unix_ms,
    };
    insert_local_repo(conn, &local)?;
    Ok(local)
}

/// Runs one request against the store database. Errors come back as
/// [`VcsResponse::Error`], never as a host-level failure, so every host
/// renders them the same way.
pub fn execute(conn: &Connection, host: &HostContext<'_>, request: VcsRequest) -> VcsResponse {
    match execute_inner(conn, host, request) {
        Ok(r) => r,
        Err(error) => VcsResponse::Error { error },
    }
}

fn execute_inner(
    conn: &Connection,
    host: &HostContext<'_>,
    request: VcsRequest,
) -> ApiResult<VcsResponse> {
    ensure_local_schema(conn)?;
    let mut store = Store::open(conn, host.digest)?;
    match &request {
        VcsRequest::CreateRepo { name } => {
            let tx = conn.unchecked_transaction()?;
            let local = create_local_repo(&tx, host, name)?;
            store.create_repo(local.repo_id, host.now_unix_ms)?;
            tx.commit()?;
            return Ok(VcsResponse::Repo { repo: local.info() });
        }
        VcsRequest::ListRepos => {
            return Ok(VcsResponse::Repos {
                repos: local_repos(conn)?.iter().map(LocalRepo::info).collect(),
            });
        }
        VcsRequest::Import {
            name,
            archive_base64,
            id_key_base64,
        } => {
            let b64 = base64::engine::general_purpose::STANDARD;
            let bytes = b64
                .decode(archive_base64)
                .map_err(|e| VcsError::invalid(format!("archive: {e}")))?;
            let key: [u8; 32] = b64
                .decode(id_key_base64)
                .map_err(|e| VcsError::invalid(format!("id key: {e}")))?
                .try_into()
                .map_err(|_| VcsError::invalid("id key must be 32 bytes"))?;
            let repo_id = crate::cbor::decode(&bytes)
                .ok()
                .and_then(|v| v.get("repo_id").and_then(RepoId::from_cbor))
                .ok_or_else(|| VcsError::invalid("archive: repo id missing"))?;
            if local_repos(conn)?.iter().any(|r| r.repo_id == repo_id) {
                return Err(VcsError::new(
                    VcsErrorCode::Conflict,
                    format!("repo {repo_id} is already on this device"),
                ));
            }
            let local = LocalRepo {
                repo_id,
                name: name.clone(),
                key: IdKey(key),
                workspace_id: WorkspaceId(host.random_16()?),
                created_at_unix_ms: host.now_unix_ms,
            };
            // The store row may be left over from an import that failed
            // before its local row was written; the archive import itself
            // is one transaction.
            match store.create_repo(local.repo_id, host.now_unix_ms) {
                Ok(()) | Err(StoreError::RepoExists(_)) => {}
                Err(e) => return Err(e.into()),
            }
            let mut repo = store.repo(local.repo_id, local.key.clone())?;
            let objects = repo.import_archive(&bytes, host.now_unix_ms)?;
            insert_local_repo(conn, &local)?;
            return Ok(VcsResponse::Imported {
                repo: local.info(),
                objects: objects as u32,
            });
        }
        _ => {}
    }
    let local = local_repo(conn, repo_id_of(&request).expect("repo request"))?;
    let mut repo = store.repo(local.repo_id, local.key.clone())?;
    let mut session = Session {
        repo: &mut repo,
        local: &local,
        host,
    };
    session.run(request)
}

/// Objects that changed between the workspace base and its working tree:
/// the "unsaved / unrecorded" set a UI shows.
fn working_changes<D: Digest>(
    repo: &Repo<'_, '_, D>,
    workspace: &Workspace,
) -> Result<Vec<(ObjectId, &'static str)>, StoreError> {
    let base = match workspace.base {
        Some(c) => Some(repo.commit(c)?.root),
        None => None,
    };
    let diff = repo.diff(base, workspace.working)?;
    let mut out: Vec<(ObjectId, &'static str)> = diff
        .added
        .iter()
        .map(|(o, _)| (*o, "added"))
        .chain(diff.changed.iter().map(|(o, _, _)| (*o, "changed")))
        .chain(diff.removed.iter().map(|(o, _)| (*o, "removed")))
        .collect();
    out.sort();
    Ok(out)
}

#[cfg(all(test, feature = "rust-digest"))]
mod tests {
    use super::*;
    use crate::digest::RustDigest;
    use crate::model::{CONFLICT_FORMAT_V1, Conflict, ConflictTerm};

    #[test]
    fn non_record_driver_keeps_merged_text() {
        let conn = Connection::open_in_memory().unwrap();
        let host = HostContext {
            digest: &RustDigest,
            random: &|out| {
                out.fill(1);
                Ok(())
            },
            now_unix_ms: 0,
            device_id: "test".into(),
            actor_kind: ActorKind::User,
        };
        ensure_local_schema(&conn).unwrap();
        let local = create_local_repo(&conn, &host, "test").unwrap();
        let mut store = Store::open(&conn, &RustDigest).unwrap();
        store.create_repo(local.repo_id, 0).unwrap();
        let mut repo = store.repo(local.repo_id, local.key.clone()).unwrap();
        let object_id = ObjectId([2; 16]);
        let side = repo
            .put_text_state(object_id, "text", b"side", Value::Map(vec![]))
            .unwrap();
        let merged = repo.put_content(b"provisional text").unwrap();
        let conflict = Conflict {
            format: CONFLICT_FORMAT_V1,
            object_id,
            driver: "merge.object.v1".into(),
            base: vec![],
            sides: vec![side, side],
            merged: Some(merged),
            terms: vec![ConflictTerm {
                path: String::new(),
                kind: TermKind::Value,
                candidates: vec![Some(merged); 2],
            }],
        };
        let id = conflict.id(&RustDigest, &local.key).unwrap();
        repo.import_objects(&[("conflict".into(), id.to_vec(), conflict.encode().unwrap())])
            .unwrap();
        let session = Session {
            repo: &mut repo,
            local: &local,
            host: &host,
        };
        let view = session.conflict_view(object_id, id).unwrap();
        assert_eq!(view.merged_text.as_deref(), Some("provisional text"));
        assert_eq!(view.merged_record_eure, None);
    }
}
