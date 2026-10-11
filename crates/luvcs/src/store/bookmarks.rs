//! Bookmarks in the store. Updates are immutable objects like commits (so
//! push, pull, export and import carry them unchanged), indexed by subject;
//! a subject's state is resolved from its updates whenever it is read.
//!
//! An update whose predecessors or target commit have not arrived yet is
//! left out until they do, so a partial pull never shows a half history.

use super::{
    Executor, KIND_BOOKMARK_UPDATE, KIND_COMMIT, OperationContext, Reader, Repo, Result,
    StoreError, Workspace,
};
use crate::bookmark::{
    self, Assigns, BookmarkUpdate, Field, Life, ResolveError, Subject, SubjectState, Target,
};
use crate::cbor::{self, Value};
use crate::digest::Digest;
use crate::id::{BookmarkId, BookmarkUpdateId, CommitId, IdKey, RepoId};
use crate::model::{Actor, Capabilities};
use rusqlite::{Transaction, params};
use std::collections::{BTreeMap, BTreeSet};

/// One bookmark as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkEntry {
    pub id: BookmarkId,
    pub state: SubjectState,
    /// Another bookmark that is not deleted or fused may go by the same
    /// name (made concurrently on two devices, say): the name alone does
    /// not pick this bookmark.
    pub name_shared: bool,
}

/// A change to one bookmark. `name` or `target` alone keep it alive;
/// `life` deletes, fuses or revives it, and also settles a `life` conflict.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookmarkEdit {
    pub life: Option<Life>,
    pub name: Option<String>,
    pub target: Option<CommitId>,
    /// A new default for the repo, written in the same operation; needed
    /// when deleting the bookmark that is the default.
    pub new_default: Option<Option<BookmarkId>>,
}

/// What a record did to the selected bookmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Advance {
    /// Moved to the new commit by this update.
    Advanced(BookmarkUpdateId),
    /// The workspace has no selected bookmark.
    NoSelection,
    /// The bookmark changed since the caller read it.
    Stale,
    /// Its life or target is in conflict, or it is deleted or fused.
    Unresolved,
    /// The new commit does not descend from where it points.
    NotForward,
}

fn subject_key(subject: Subject) -> Vec<u8> {
    match subject {
        Subject::Bookmark(id) => id.to_vec(),
        Subject::Repo => Vec::new(),
    }
}

/// Indexes a stored update by its subject.
pub(super) fn index_tx(
    tx: &Transaction<'_>,
    repo: RepoId,
    id: &[u8],
    bytes: &[u8],
    caps: &Capabilities,
) -> Result<()> {
    let update = BookmarkUpdate::decode(bytes, caps)?;
    tx.execute(
        "INSERT OR IGNORE INTO vcs_bookmark_updates (repo_id, update_id, subject) VALUES (?1, ?2, ?3)",
        params![repo.as_bytes(), id, subject_key(update.subject)],
    )?;
    Ok(())
}

/// Stores and indexes an update outside an operation (undo and redo).
fn put_update_tx<D: Digest>(
    tx: &Transaction<'_>,
    key: &IdKey,
    digest: &D,
    caps: &Capabilities,
    update: &BookmarkUpdate,
) -> Result<BookmarkUpdateId> {
    let id = update.id(digest, key)?;
    let bytes = update.encode()?;
    Repo::<D>::put_object_tx(
        tx,
        update.repo_id.as_bytes(),
        KIND_BOOKMARK_UPDATE,
        id.as_bytes(),
        &bytes,
    )?;
    index_tx(tx, update.repo_id, id.as_bytes(), &bytes, caps)?;
    Ok(id)
}

fn resolve_err(e: ResolveError<StoreError>) -> StoreError {
    match e {
        ResolveError::Ancestry(e) => e,
        e => StoreError::Invalid(e.to_string()),
    }
}

/// The values a field may be restored to: its value, or every candidate.
fn values<T: Clone>(field: &Field<T>) -> Vec<T> {
    match field {
        Field::Unset => Vec::new(),
        Field::Value(v) => vec![v.clone()],
        Field::Conflict(c) => c.iter().map(|c| c.value.clone()).collect(),
    }
}

/// Deleted or fused: no longer goes by its name.
fn is_gone(state: &SubjectState) -> bool {
    matches!(
        state.life,
        Field::Value(Life::Deleted) | Field::Value(Life::FusedInto(_))
    )
}

fn sorted(ids: &[BookmarkUpdateId]) -> Vec<BookmarkUpdateId> {
    let set: BTreeSet<BookmarkUpdateId> = ids.iter().copied().collect();
    set.into_iter().collect()
}

impl Reader<'_> {
    fn subject_updates(
        &self,
        subject: Subject,
        caps: &Capabilities,
    ) -> Result<BTreeMap<BookmarkUpdateId, BookmarkUpdate>> {
        let mut stmt = self.conn.prepare(
            "SELECT b.update_id, o.bytes FROM vcs_bookmark_updates b
             JOIN vcs_objects o ON o.repo_id = b.repo_id AND o.kind = ?3 AND o.logical_id = b.update_id
             WHERE b.repo_id = ?1 AND b.subject = ?2",
        )?;
        let rows = stmt.query_map(
            params![self.repo, subject_key(subject), KIND_BOOKMARK_UPDATE],
            |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
        )?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (id, bytes) = row?;
            let id = BookmarkUpdateId::from_bytes(&id)
                .map_err(|e| StoreError::Invalid(e.to_string()))?;
            out.insert(id, BookmarkUpdate::decode(&bytes, caps)?);
        }
        Ok(out)
    }

    fn bookmark_ids(&self) -> Result<Vec<BookmarkId>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT subject FROM vcs_bookmark_updates WHERE repo_id = ?1 AND length(subject) > 0 ORDER BY subject",
        )?;
        let rows = stmt.query_map(params![self.repo], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| BookmarkId::from_bytes(&r?).map_err(|e| StoreError::Invalid(e.to_string())))
            .collect()
    }

    /// The updates whose whole history and target commits are here.
    fn complete(
        &self,
        mut updates: BTreeMap<BookmarkUpdateId, BookmarkUpdate>,
    ) -> Result<BTreeMap<BookmarkUpdateId, BookmarkUpdate>> {
        let mut missing_commit = BTreeSet::new();
        for (id, u) in &updates {
            if let Some(t) = u.assigns.target
                && self.object(KIND_COMMIT, t.commit.as_bytes())?.is_none()
            {
                missing_commit.insert(*id);
            }
        }
        updates.retain(|id, _| !missing_commit.contains(id));
        loop {
            let incomplete: Vec<BookmarkUpdateId> = updates
                .iter()
                .filter(|(_, u)| u.predecessors.iter().any(|p| !updates.contains_key(p)))
                .map(|(id, _)| *id)
                .collect();
            if incomplete.is_empty() {
                return Ok(updates);
            }
            for id in incomplete {
                updates.remove(&id);
            }
        }
    }

    fn subject_state(&self, subject: Subject, caps: &Capabilities) -> Result<SubjectState> {
        let updates = self.complete(self.subject_updates(subject, caps)?)?;
        bookmark::resolve(&updates, |a, b| self.is_ancestor(a, b)).map_err(resolve_err)
    }

    fn bookmark_entries(&self, caps: &Capabilities) -> Result<Vec<BookmarkEntry>> {
        let mut states = Vec::new();
        for id in self.bookmark_ids()? {
            let state = self.subject_state(Subject::Bookmark(id), caps)?;
            if !state.heads.is_empty() {
                states.push((id, state));
            }
        }
        let mut by_name: BTreeMap<String, usize> = BTreeMap::new();
        for (_, s) in states.iter().filter(|(_, s)| !is_gone(s)) {
            for n in values(&s.name) {
                *by_name.entry(n).or_default() += 1;
            }
        }
        Ok(states
            .into_iter()
            .map(|(id, state)| {
                let name_shared = !is_gone(&state)
                    && values(&state.name)
                        .iter()
                        .any(|n| by_name.get(n).copied().unwrap_or(0) > 1);
                BookmarkEntry {
                    id,
                    state,
                    name_shared,
                }
            })
            .collect())
    }
}

impl<D: Digest> Repo<'_, '_, D> {
    /// Every bookmark that has arrived, deleted and fused ones included.
    pub fn bookmarks(&self) -> Result<Vec<BookmarkEntry>> {
        self.reader().bookmark_entries(&self.store.caps)
    }

    pub fn bookmark(&self, id: BookmarkId) -> Result<SubjectState> {
        let state = self
            .reader()
            .subject_state(Subject::Bookmark(id), &self.store.caps)?;
        if state.heads.is_empty() {
            return Err(StoreError::UnknownBookmark(id));
        }
        Ok(state)
    }

    /// The repo settings; never-set fields are `Unset`.
    pub fn repo_settings(&self) -> Result<SubjectState> {
        self.reader().subject_state(Subject::Repo, &self.store.caps)
    }

    /// Creates bookmark `id` named `name` at `target`.
    pub fn create_bookmark(
        &mut self,
        ctx: &OperationContext,
        id: BookmarkId,
        name: &str,
        target: CommitId,
    ) -> Result<BookmarkUpdateId> {
        self.operate(ctx, "bookmark_create", |exec, ws| {
            if !exec.subject_state(Subject::Bookmark(id))?.heads.is_empty() {
                return Err(StoreError::Invalid(format!("bookmark {id} already exists")));
            }
            bookmark::validate_name(name)?;
            exec.check_name_free(name, id)?;
            exec.commit(target)?;
            let update = exec.write_update(
                ctx,
                Subject::Bookmark(id),
                Vec::new(),
                Assigns {
                    life: Some(Life::Live),
                    name: Some(name.to_owned()),
                    target: Some(Target {
                        commit: target,
                        from: None,
                    }),
                    default_bookmark: None,
                },
            )?;
            Ok((ws.clone(), update))
        })
    }

    /// Changes bookmark `id`, whose heads the caller read as `expected`.
    /// Resolving a conflict is an edit too: it assigns the chosen value
    /// over every head the caller considered.
    pub fn edit_bookmark(
        &mut self,
        ctx: &OperationContext,
        id: BookmarkId,
        expected: &[BookmarkUpdateId],
        edit: BookmarkEdit,
    ) -> Result<BookmarkUpdateId> {
        self.operate(ctx, "bookmark_edit", |exec, ws| {
            let state = exec.subject_state(Subject::Bookmark(id))?;
            if state.heads.is_empty() {
                return Err(StoreError::UnknownBookmark(id));
            }
            if state.heads != sorted(expected) {
                return Err(StoreError::StaleBookmark);
            }
            let life = edit
                .life
                .or((edit.name.is_some() || edit.target.is_some()).then_some(Life::Live));
            if let Some(Life::FusedInto(into)) = life {
                let other = exec.subject_state(Subject::Bookmark(into))?;
                if other.heads.is_empty() || other.life != Field::Value(Life::Live) {
                    return Err(StoreError::Invalid(format!(
                        "bookmark {into} is not a live bookmark to fuse into"
                    )));
                }
            }
            match &edit.name {
                Some(n) => {
                    bookmark::validate_name(n)?;
                    exec.check_name_free(n, id)?;
                }
                // Reviving keeps the name, which someone may have taken.
                None if life == Some(Life::Live) && is_gone(&state) => {
                    for n in values(&state.name) {
                        exec.check_name_free(&n, id)?;
                    }
                }
                None => {}
            }
            let target = match edit.target {
                Some(commit) => {
                    exec.commit(commit)?;
                    Some(Target {
                        commit,
                        from: state.target.value().copied(),
                    })
                }
                None => None,
            };
            let update = exec.write_update(
                ctx,
                Subject::Bookmark(id),
                state.heads.clone(),
                Assigns {
                    life,
                    name: edit.name,
                    target,
                    default_bookmark: None,
                },
            )?;
            let settings = exec.subject_state(Subject::Repo)?;
            match edit.new_default {
                Some(default) => {
                    if default == Some(id) && life == Some(Life::Deleted) {
                        return Err(StoreError::Invalid(
                            "a deleted bookmark cannot be the default".into(),
                        ));
                    }
                    if let Some(d) = default.filter(|d| *d != id) {
                        exec.check_live(d)?;
                    }
                    exec.write_update(
                        ctx,
                        Subject::Repo,
                        settings.heads,
                        Assigns {
                            default_bookmark: Some(default),
                            ..Assigns::default()
                        },
                    )?;
                }
                // A fused bookmark's default follows it to where it went.
                None if life == Some(Life::Deleted)
                    && settings.default_bookmark == Field::Value(Some(id)) =>
                {
                    return Err(StoreError::BookmarkIsDefault(id));
                }
                None => {}
            }
            Ok((ws.clone(), update))
        })
    }

    /// Sets (or with `None` clears) the repo's default bookmark; the
    /// caller read the settings' heads as `expected`.
    pub fn set_default_bookmark(
        &mut self,
        ctx: &OperationContext,
        expected: &[BookmarkUpdateId],
        default: Option<BookmarkId>,
    ) -> Result<BookmarkUpdateId> {
        self.operate(ctx, "bookmark_default", |exec, ws| {
            let settings = exec.subject_state(Subject::Repo)?;
            if settings.heads != sorted(expected) {
                return Err(StoreError::StaleBookmark);
            }
            if let Some(d) = default {
                exec.check_live(d)?;
            }
            let update = exec.write_update(
                ctx,
                Subject::Repo,
                settings.heads,
                Assigns {
                    default_bookmark: Some(default),
                    ..Assigns::default()
                },
            )?;
            Ok((ws.clone(), update))
        })
    }

    /// Selects the bookmark records in this workspace move forward, or
    /// with `None` selects none. Local to the workspace; never shared.
    pub fn select_bookmark(
        &mut self,
        ctx: &OperationContext,
        id: Option<BookmarkId>,
    ) -> Result<Workspace> {
        self.operate(ctx, "bookmark_select", |exec, ws| {
            if let Some(id) = id {
                exec.check_live(id)?;
            }
            let mut after = ws.clone();
            after.selected_bookmark = id;
            Ok((after, ()))
        })?;
        self.workspace(ctx.workspace_id)
    }
}

impl<D: Digest> Executor<'_, '_, D> {
    fn subject_state(&self, subject: Subject) -> Result<SubjectState> {
        self.reader().subject_state(subject, self.caps)
    }

    fn write_update(
        &mut self,
        ctx: &OperationContext,
        subject: Subject,
        predecessors: Vec<BookmarkUpdateId>,
        assigns: Assigns,
    ) -> Result<BookmarkUpdateId> {
        let update = BookmarkUpdate::new(
            self.repo,
            subject,
            predecessors,
            assigns,
            ctx.actor.clone(),
            ctx.now_unix_ms,
        );
        let id = update.id(self.digest, self.key)?;
        self.pending
            .objects
            .push((KIND_BOOKMARK_UPDATE, id.to_vec(), update.encode()?));
        self.bookmark_writes.push(id);
        Ok(id)
    }

    /// Refuses `name` when a bookmark other than `except` that is not
    /// deleted or fused goes by it (or has it as a candidate).
    fn check_name_free(&self, name: &str, except: BookmarkId) -> Result<()> {
        for entry in self.reader().bookmark_entries(self.caps)? {
            if entry.id != except
                && !is_gone(&entry.state)
                && values(&entry.state.name).iter().any(|n| n == name)
            {
                return Err(StoreError::NameTaken(name.to_owned()));
            }
        }
        Ok(())
    }

    fn check_live(&self, id: BookmarkId) -> Result<()> {
        let state = self.subject_state(Subject::Bookmark(id))?;
        if state.heads.is_empty() {
            return Err(StoreError::UnknownBookmark(id));
        }
        if state.life != Field::Value(Life::Live) {
            return Err(StoreError::Invalid(format!(
                "bookmark {id} is deleted, fused or in conflict"
            )));
        }
        Ok(())
    }

    /// Moves the selected bookmark forward to `new`; see
    /// [`Repo::record_advancing`].
    pub(super) fn advance_selected(
        &mut self,
        ctx: &OperationContext,
        selected: Option<BookmarkId>,
        new: CommitId,
        expected: &[BookmarkUpdateId],
    ) -> Result<Advance> {
        let Some(id) = selected else {
            return Ok(Advance::NoSelection);
        };
        let state = self.subject_state(Subject::Bookmark(id))?;
        if state.heads != sorted(expected) {
            return Ok(Advance::Stale);
        }
        let (Field::Value(Life::Live), Field::Value(at)) = (&state.life, &state.target) else {
            return Ok(Advance::Unresolved);
        };
        let at = *at;
        if !self.reader().is_ancestor(at, new)? {
            return Ok(Advance::NotForward);
        }
        let update = self.write_update(
            ctx,
            Subject::Bookmark(id),
            state.heads,
            Assigns {
                life: Some(Life::Live),
                target: Some(Target {
                    commit: new,
                    from: Some(at),
                }),
                ..Assigns::default()
            },
        )?;
        Ok(Advance::Advanced(update))
    }

    /// After `from` is rewritten as `to`, moves every live bookmark that
    /// points at `from` (and is not in conflict) along.
    pub(super) fn follow_rewrite(
        &mut self,
        ctx: &OperationContext,
        from: CommitId,
        to: CommitId,
    ) -> Result<()> {
        for entry in self.reader().bookmark_entries(self.caps)? {
            if entry.state.life == Field::Value(Life::Live)
                && entry.state.target == Field::Value(from)
            {
                self.write_update(
                    ctx,
                    Subject::Bookmark(entry.id),
                    entry.state.heads,
                    Assigns {
                        life: Some(Life::Live),
                        target: Some(Target {
                            commit: to,
                            from: Some(from),
                        }),
                        ..Assigns::default()
                    },
                )?;
            }
        }
        Ok(())
    }
}

/// The `i`th restored value; a field with fewer values repeats its last.
fn nth<T: Clone>(values: &[T], i: usize) -> Option<T> {
    values.get(i.min(values.len().saturating_sub(1))).cloned()
}

fn logged_writes(log: &Value) -> Result<Vec<BookmarkUpdateId>> {
    let Some(items) = log.get("bookmark_writes").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|v| {
            BookmarkUpdateId::from_cbor(v)
                .ok_or_else(|| StoreError::Invalid("corrupt operation log".into()))
        })
        .collect()
}

fn logged_update(
    reader: &Reader<'_>,
    id: BookmarkUpdateId,
    caps: &Capabilities,
) -> Result<BookmarkUpdate> {
    let bytes = reader
        .object(KIND_BOOKMARK_UPDATE, id.as_bytes())?
        .ok_or_else(|| StoreError::MissingObject {
            kind: KIND_BOOKMARK_UPDATE,
            id: id.to_string(),
        })?;
    Ok(BookmarkUpdate::decode(&bytes, caps)?)
}

/// Undoes the bookmark updates an operation wrote (newest first) with new
/// updates over the current heads that put back what each field was
/// before. A field that was in conflict comes back in conflict: one
/// sibling update per candidate.
#[allow(clippy::too_many_arguments)]
pub(super) fn undo_writes_tx<D: Digest>(
    tx: &Transaction<'_>,
    repo: RepoId,
    key: &IdKey,
    digest: &D,
    caps: &Capabilities,
    after: &[u8],
    author: &Actor,
    now_unix_ms: i64,
) -> Result<()> {
    let reader = Reader {
        conn: tx,
        repo: repo.as_bytes(),
    };
    for id in logged_writes(&cbor::decode(after)?)?.into_iter().rev() {
        let undone = logged_update(&reader, id, caps)?;
        let subject = undone.subject;
        let prior = if undone.predecessors.is_empty() {
            None
        } else {
            let all = reader.subject_updates(subject, caps)?;
            let mut keep = BTreeSet::new();
            let mut stack = undone.predecessors.clone();
            while let Some(p) = stack.pop() {
                if keep.insert(p)
                    && let Some(u) = all.get(&p)
                {
                    stack.extend(u.predecessors.iter().copied());
                }
            }
            let before: BTreeMap<_, _> = all
                .into_iter()
                .filter(|(id, _)| keep.contains(id))
                .collect();
            Some(bookmark::resolve(&before, |a, b| reader.is_ancestor(a, b)).map_err(resolve_err)?)
        };
        let a = &undone.assigns;
        let life: Vec<Life> = match (&prior, a.life) {
            (_, None) => Vec::new(),
            // Undoing a creation deletes.
            (None, Some(_)) => vec![Life::Deleted],
            (Some(p), Some(_)) => values(&p.life),
        };
        let pick = |field: Option<&Field<String>>| field.map(values).unwrap_or_default();
        let name = if a.name.is_some() {
            pick(prior.as_ref().map(|p| &p.name))
        } else {
            Vec::new()
        };
        let target: Vec<CommitId> = match (&prior, a.target) {
            (Some(p), Some(_)) => values(&p.target),
            _ => Vec::new(),
        };
        let default: Vec<Option<BookmarkId>> = match (&prior, a.default_bookmark) {
            (_, None) => Vec::new(),
            (Some(p), Some(_)) if p.default_bookmark != Field::Unset => values(&p.default_bookmark),
            (_, Some(_)) => vec![None],
        };
        let k = [life.len(), name.len(), target.len(), default.len()]
            .into_iter()
            .max()
            .unwrap_or(0);
        if k == 0 {
            continue;
        }
        let current = reader.subject_state(subject, caps)?;
        for i in 0..k {
            let assigns = Assigns {
                life: nth(&life, i),
                name: nth(&name, i),
                // Restored candidates must not fast-forward into one.
                target: nth(&target, i).map(|commit| Target {
                    commit,
                    from: if k == 1 {
                        current.target.value().copied()
                    } else {
                        None
                    },
                }),
                default_bookmark: nth(&default, i),
            };
            let update = BookmarkUpdate::new(
                repo,
                subject,
                current.heads.clone(),
                assigns,
                author.clone(),
                now_unix_ms,
            );
            put_update_tx(tx, key, digest, caps, &update)?;
        }
    }
    Ok(())
}

/// Writes an operation's bookmark updates again, as new updates over the
/// current heads, and returns the operation's `after` log naming them.
#[allow(clippy::too_many_arguments)]
pub(super) fn redo_writes_tx<D: Digest>(
    tx: &Transaction<'_>,
    repo: RepoId,
    key: &IdKey,
    digest: &D,
    caps: &Capabilities,
    after: &[u8],
    author: &Actor,
    now_unix_ms: i64,
) -> Result<Vec<u8>> {
    let reader = Reader {
        conn: tx,
        repo: repo.as_bytes(),
    };
    let log = cbor::decode(after)?;
    let writes = logged_writes(&log)?;
    if writes.is_empty() {
        return Ok(after.to_vec());
    }
    let mut rewritten = Vec::with_capacity(writes.len());
    for id in writes {
        let original = logged_update(&reader, id, caps)?;
        let current = reader.subject_state(original.subject, caps)?;
        let mut assigns = original.assigns.clone();
        if let Some(t) = &mut assigns.target {
            t.from = current.target.value().copied();
        }
        let update = BookmarkUpdate::new(
            repo,
            original.subject,
            current.heads,
            assigns,
            author.clone(),
            now_unix_ms,
        );
        rewritten.push(put_update_tx(tx, key, digest, caps, &update)?);
    }
    let Value::Map(entries) = log else {
        return Err(StoreError::Invalid("corrupt operation log".into()));
    };
    let entries = entries
        .into_iter()
        .map(|(k, v)| {
            if k.as_text() == Some("bookmark_writes") {
                (
                    k,
                    Value::Array(rewritten.iter().map(BookmarkUpdateId::to_cbor).collect()),
                )
            } else {
                (k, v)
            }
        })
        .collect();
    Ok(cbor::encode(&Value::Map(entries)))
}
