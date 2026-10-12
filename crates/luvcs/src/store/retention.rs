//! Retention: what a repo's history reaches, what may be erased from it,
//! and what local garbage may be collected.
//!
//! - [`Repo::reach`] walks commits, trees, states and conflicts down to the
//!   content and blobs they name. Push sends what the heads and bookmarks
//!   reach; garbage collection keeps what anything local still reaches.
//! - [`Repo::purge`] erases the bodies an object ever had (see
//!   [`crate::redaction`]): the IDs stay, the bytes go, here and on every
//!   device and remote the redaction reaches.
//! - [`Repo::collect_garbage`] deletes objects that nothing reaches and
//!   that never left this device. An object that went through a remote is
//!   kept: another device may still build on it and send it back.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{OptionalExtension as _, Transaction, params};

use super::{
    KIND_BOOKMARK_UPDATE, KIND_COMMIT, KIND_CONFLICT, KIND_CONTENT, KIND_REDACTION, KIND_STATE,
    KIND_TREE, Reader, Repo, Result, StoreError, Workspace,
};
use crate::bookmark::BookmarkUpdate;
use crate::cbor::{self, Value};
use crate::digest::Digest;
use crate::id::{BlobId, CommitId, ContentId, ObjectId, RedactionId, RepoId, TreeId};
use crate::model::{Actor, Capabilities, Commit, Conflict, ObjectBody, ObjectState};
use crate::redaction::{MAX_REDACTED_IDS, Redaction};
use crate::tree::Node;

/// What a walk reached: objects by `(kind, logical id)`, and blob IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reach {
    pub objects: BTreeSet<(&'static str, Vec<u8>)>,
    pub blobs: BTreeSet<BlobId>,
}

/// Where a walk starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roots {
    pub commits: Vec<CommitId>,
    pub trees: Vec<TreeId>,
}

/// What a purge erased.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeOutcome {
    /// One per [`MAX_REDACTED_IDS`] erased IDs; empty when there was nothing
    /// left to erase.
    pub redactions: Vec<RedactionId>,
    pub contents: usize,
    pub blobs: usize,
    /// Bodies of the purged objects that another object also has (the same
    /// bytes), kept so that object stays readable.
    pub kept_shared: usize,
}

/// What a garbage collection removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcOutcome {
    pub objects: usize,
    pub blobs: usize,
    pub operations: usize,
}

/// Whether a table exists: the sync and blob tables are created by their
/// modules on first use, and a store that never synced has none.
fn has_table(tx: &rusqlite::Connection, name: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |r| r.get(0),
    )?)
}

/// The content IDs and blob IDs a state or conflict names.
fn state_refs(state: &ObjectState, contents: &mut Vec<ContentId>, blobs: &mut Vec<BlobId>) {
    match &state.body {
        ObjectBody::Text { content } | ObjectBody::Record { content, .. } => {
            contents.push(*content)
        }
        ObjectBody::Binary { blob } => {
            if let Ok(id) = BlobId::from_bytes(&blob.blob_id) {
                blobs.push(id);
            }
        }
        ObjectBody::Conflict { .. } | ObjectBody::Deleted => {}
    }
}

fn conflict_contents(conflict: &Conflict) -> impl Iterator<Item = ContentId> + '_ {
    conflict.merged.into_iter().chain(
        conflict
            .terms
            .iter()
            .flat_map(|t| t.candidates.iter().flatten().copied()),
    )
}

impl Reader<'_> {
    /// Walks from `roots`. `stop(kind, id)` says an object and everything
    /// under it is known to be elsewhere (on the remote, for a push): it is
    /// neither included nor descended into. Content is not fetched, so a
    /// redacted body is reached like any other.
    pub(super) fn reach(
        &self,
        caps: &Capabilities,
        roots: &Roots,
        stop: &mut dyn FnMut(&'static str, &[u8]) -> Result<bool>,
    ) -> Result<Reach> {
        let mut reach = Reach::default();
        let mut work: Vec<(&'static str, Vec<u8>)> = roots
            .commits
            .iter()
            .map(|c| (KIND_COMMIT, c.to_vec()))
            .chain(roots.trees.iter().map(|t| (KIND_TREE, t.to_vec())))
            .collect();
        while let Some((kind, id)) = work.pop() {
            if reach.objects.contains(&(kind, id.clone())) || stop(kind, &id)? {
                continue;
            }
            reach.objects.insert((kind, id.clone()));
            if kind == KIND_CONTENT {
                continue;
            }
            let bytes = self
                .object(kind, &id)?
                .ok_or_else(|| StoreError::MissingObject {
                    kind,
                    id: hex::encode(&id),
                })?;
            match kind {
                KIND_COMMIT => {
                    let c = Commit::decode(&bytes, caps)?;
                    work.extend(c.parents.iter().map(|p| (KIND_COMMIT, p.to_vec())));
                    work.push((KIND_TREE, c.root.to_vec()));
                }
                KIND_TREE => match Node::decode(&bytes)? {
                    Node::Empty => {}
                    Node::Leaf { state, .. } => work.push((KIND_STATE, state.to_vec())),
                    Node::Branch { left, right, .. } => {
                        work.push((KIND_TREE, left.to_vec()));
                        work.push((KIND_TREE, right.to_vec()));
                    }
                },
                KIND_STATE => {
                    let state = ObjectState::decode(&bytes, caps)?;
                    let (mut contents, mut blobs) = (Vec::new(), Vec::new());
                    state_refs(&state, &mut contents, &mut blobs);
                    work.extend(contents.iter().map(|c| (KIND_CONTENT, c.to_vec())));
                    reach.blobs.extend(blobs);
                    if let ObjectBody::Conflict { conflict } = state.body {
                        work.push((KIND_CONFLICT, conflict.to_vec()));
                    }
                }
                KIND_CONFLICT => {
                    let c = Conflict::decode(&bytes, caps)?;
                    work.extend(
                        c.base
                            .iter()
                            .chain(&c.sides)
                            .map(|s| (KIND_STATE, s.to_vec())),
                    );
                    work.extend(conflict_contents(&c).map(|c| (KIND_CONTENT, c.to_vec())));
                }
                _ => unreachable!("walks only reach history kinds"),
            }
        }
        Ok(reach)
    }

    /// Every commit a stored bookmark update targets, among those present:
    /// a bookmark's history needs them all, or a device that takes it in
    /// leaves the update (and everything after it) out.
    pub(super) fn bookmark_targets(&self, caps: &Capabilities) -> Result<Vec<CommitId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT bytes FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2")?;
        let rows = stmt.query_map(params![self.repo, KIND_BOOKMARK_UPDATE], |r| {
            r.get::<_, Vec<u8>>(0)
        })?;
        let mut out = BTreeSet::new();
        for row in rows {
            if let Some(t) = BookmarkUpdate::decode(&row?, caps)?.assigns.target
                && self.object(KIND_COMMIT, t.commit.as_bytes())?.is_some()
            {
                out.insert(t.commit);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Whether `id` of `kind` was erased by a redaction and is absent.
    pub(super) fn is_redacted(&self, kind: &str, id: &[u8]) -> Result<bool> {
        let named: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM vcs_redacted WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                params![self.repo, kind, id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(named.is_some() && self.object(kind, id)?.is_none())
    }

    fn workspaces(&self) -> Result<Vec<Workspace>> {
        let mut stmt = self
            .conn
            .prepare("SELECT snapshot FROM vcs_workspaces WHERE repo_id = ?1")?;
        let rows = stmt.query_map(params![self.repo], |r| r.get::<_, Vec<u8>>(0))?;
        rows.map(|r| Workspace::from_cbor(&cbor::decode(&r?)?))
            .collect()
    }
}

/// Stores a redaction's index rows and erases what it names, inside `tx`:
/// the content rows, the local blob, and the sync bookkeeping that says a
/// remote holds them (the remote is asked to drop them too, so a body
/// written again later is sent again).
pub(super) fn apply_redaction_tx(
    tx: &Transaction<'_>,
    repo: RepoId,
    redaction: &Redaction,
) -> Result<()> {
    let r = repo.as_bytes();
    let sync = has_table(tx, "vcs_sync_pushed")?;
    for c in &redaction.contents {
        tx.execute(
            "INSERT OR IGNORE INTO vcs_redacted (repo_id, kind, logical_id) VALUES (?1, ?2, ?3)",
            params![r, KIND_CONTENT, c.to_vec()],
        )?;
        tx.execute(
            "DELETE FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
            params![r, KIND_CONTENT, c.to_vec()],
        )?;
        if sync {
            tx.execute(
                "DELETE FROM vcs_sync_pushed WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                params![r, KIND_CONTENT, c.to_vec()],
            )?;
        }
    }
    let blobs = has_table(tx, "vcs_blobs")?;
    for b in &redaction.blobs {
        tx.execute(
            "INSERT OR IGNORE INTO vcs_redacted (repo_id, kind, logical_id) VALUES (?1, 'blob', ?2)",
            params![r, b.to_vec()],
        )?;
        if blobs {
            for table in ["vcs_blob_chunks", "vcs_blobs", "vcs_blob_pins"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE realm_id = ?1 AND blob_id = ?2"),
                    params![r, b.to_vec()],
                )?;
            }
        }
        if sync {
            for table in ["vcs_sync_blobs", "vcs_sync_wanted"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE repo_id = ?1 AND blob_id = ?2"),
                    params![r, b.to_vec()],
                )?;
            }
        }
    }
    Ok(())
}

impl<D: Digest> Repo<'_, '_, D> {
    /// What `roots` reach, all the way down.
    pub fn reach(&self, roots: &Roots) -> Result<Reach> {
        self.reader()
            .reach(&self.store.caps, roots, &mut |_, _| Ok(false))
    }

    /// What `roots` reach, not descending into what `stop` says is
    /// elsewhere already.
    pub fn reach_until(
        &self,
        roots: &Roots,
        stop: &mut dyn FnMut(&'static str, &[u8]) -> Result<bool>,
    ) -> Result<Reach> {
        self.reader().reach(&self.store.caps, roots, stop)
    }

    /// The commits whose history is shared: the heads and every commit a
    /// bookmark update names.
    pub fn shared_roots(&self) -> Result<Roots> {
        let reader = self.reader();
        let mut commits: BTreeSet<CommitId> = reader.visible()?;
        commits.extend(reader.bookmark_targets(&self.store.caps)?);
        Ok(Roots {
            commits: commits.into_iter().collect(),
            trees: Vec::new(),
        })
    }

    /// Logical IDs of every stored object of `kind` (bookmark updates and
    /// redactions travel whole, not by reach).
    pub fn object_ids(&self, kind: &str) -> Result<Vec<Vec<u8>>> {
        let mut stmt = self.store.conn.prepare(
            "SELECT logical_id FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2 ORDER BY logical_id",
        )?;
        let rows = stmt.query_map(params![self.repo_id.as_bytes(), kind], |r| {
            r.get::<_, Vec<u8>>(0)
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether a body was erased by a redaction (and not written again).
    pub fn is_redacted_content(&self, id: ContentId) -> Result<bool> {
        self.reader().is_redacted(KIND_CONTENT, id.as_bytes())
    }

    /// Whether a blob was erased by a redaction.
    pub fn is_redacted_blob(&self, id: BlobId) -> Result<bool> {
        let named: Option<i64> = self
            .store
            .conn
            .query_row(
                "SELECT 1 FROM vcs_redacted WHERE repo_id = ?1 AND kind = 'blob' AND logical_id = ?2",
                params![self.repo_id.as_bytes(), id.to_vec()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(named.is_some())
    }

    /// Bodies and blobs a redaction erased that are here again, written
    /// anew. A walk that stops at what a remote has misses them: the state
    /// (or the whole tree) that names one may be exactly the one the
    /// remote already holds.
    pub fn written_again(&self) -> Result<(Vec<ContentId>, Vec<BlobId>)> {
        let conn = self.store.conn;
        let repo = self.repo_id.as_bytes();
        let mut stmt = conn.prepare(
            "SELECT r.logical_id FROM vcs_redacted r JOIN vcs_objects o
               ON o.repo_id = r.repo_id AND o.kind = r.kind AND o.logical_id = r.logical_id
             WHERE r.repo_id = ?1 AND r.kind = ?2 ORDER BY r.logical_id",
        )?;
        let rows = stmt.query_map(params![repo, KIND_CONTENT], |r| r.get::<_, Vec<u8>>(0))?;
        let mut contents = Vec::new();
        for row in rows {
            contents.push(
                ContentId::from_bytes(&row?).map_err(|e| StoreError::Invalid(e.to_string()))?,
            );
        }
        let mut blobs = Vec::new();
        if has_table(conn, "vcs_blobs")? {
            let mut stmt = conn.prepare(
                "SELECT r.logical_id FROM vcs_redacted r JOIN vcs_blobs b
                   ON b.realm_id = r.repo_id AND b.blob_id = r.logical_id
                 WHERE r.repo_id = ?1 AND r.kind = 'blob' ORDER BY r.logical_id",
            )?;
            let rows = stmt.query_map(params![repo], |r| r.get::<_, Vec<u8>>(0))?;
            for row in rows {
                blobs.push(
                    BlobId::from_bytes(&row?).map_err(|e| StoreError::Invalid(e.to_string()))?,
                );
            }
        }
        Ok((contents, blobs))
    }

    /// A stored redaction.
    pub fn redaction(&self, id: RedactionId) -> Result<Redaction> {
        let bytes = self
            .reader()
            .object(KIND_REDACTION, id.as_bytes())?
            .ok_or_else(|| StoreError::MissingObject {
                kind: KIND_REDACTION,
                id: id.to_string(),
            })?;
        Ok(Redaction::decode(&bytes, &self.store.caps)?)
    }

    /// Erases every body `objects` ever had in this store (text, record
    /// and blob, conflict terms included) and the host blobs `blobs`, by
    /// writing redactions. Each object must be deleted or absent in every
    /// head and every workspace: a purge removes history, not the present.
    ///
    /// A body another object also has is kept (and counted), so that
    /// object stays readable. Metadata (titles), commit descriptions and
    /// IDs stay. There is no undo: the bytes are gone.
    pub fn purge(
        &mut self,
        objects: &[ObjectId],
        blobs: &[BlobId],
        author: &Actor,
        now_unix_ms: i64,
        reason: &str,
    ) -> Result<PurgeOutcome> {
        let caps = self.store.caps.clone();
        let targets: BTreeSet<ObjectId> = objects.iter().copied().collect();
        let tx = self.store.conn.unchecked_transaction()?;
        let reader = Reader {
            conn: &tx,
            repo: self.repo_id.as_bytes(),
        };

        // Still live somewhere: refuse rather than break the present.
        let mut trees: Vec<(String, TreeId)> = Vec::new();
        for c in reader.visible()? {
            trees.push((format!("head {c}"), reader.commit(c, &caps)?.root));
        }
        for ws in reader.workspaces()? {
            trees.push((format!("workspace {}", ws.id), ws.working));
            if let Some(c) = ws.candidate {
                trees.push((format!("workspace {} candidate", ws.id), c));
            }
        }
        for (place, root) in &trees {
            for o in &targets {
                if let Some(s) = reader.tree_lookup(&self.store.digest, &self.key, *root, *o)?
                    && !matches!(reader.state(s, &caps)?.body, ObjectBody::Deleted)
                {
                    return Err(StoreError::Invalid(format!(
                        "{o} is not deleted in {place}; delete it and record before purging"
                    )));
                }
            }
        }

        // Every body each side names: the purged objects', everyone else's.
        let (mut mine_c, mut mine_b) = (BTreeSet::new(), BTreeSet::new());
        let (mut other_c, mut other_b) = (BTreeSet::new(), BTreeSet::new());
        mine_b.extend(blobs.iter().copied());
        {
            let mut stmt = tx.prepare(
                "SELECT kind, bytes FROM vcs_objects WHERE repo_id = ?1 AND kind IN (?2, ?3)",
            )?;
            let rows = stmt.query_map(
                params![self.repo_id.as_bytes(), KIND_STATE, KIND_CONFLICT],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )?;
            for row in rows {
                let (kind, bytes) = row?;
                let (mut contents, mut bs) = (Vec::new(), Vec::new());
                let owner = if kind == KIND_STATE {
                    let state = ObjectState::decode(&bytes, &caps)?;
                    state_refs(&state, &mut contents, &mut bs);
                    state.object_id
                } else {
                    let conflict = Conflict::decode(&bytes, &caps)?;
                    contents.extend(conflict_contents(&conflict));
                    conflict.object_id
                };
                if targets.contains(&owner) {
                    mine_c.extend(contents);
                    mine_b.extend(bs);
                } else {
                    other_c.extend(contents);
                    other_b.extend(bs);
                }
            }
        }
        let kept_shared =
            mine_c.intersection(&other_c).count() + mine_b.intersection(&other_b).count();
        // Already erased and not written again: nothing left to say.
        let mut contents = Vec::new();
        for c in mine_c.difference(&other_c) {
            if !reader.is_redacted(KIND_CONTENT, c.as_bytes())? {
                contents.push(*c);
            }
        }
        let blob_present = has_table(&tx, "vcs_blobs")?;
        let mut erase_blobs = Vec::new();
        for b in mine_b.difference(&other_b) {
            let named: bool = tx.query_row(
                "SELECT count(*) > 0 FROM vcs_redacted WHERE repo_id = ?1 AND kind = 'blob' AND logical_id = ?2",
                params![self.repo_id.as_bytes(), b.to_vec()],
                |r| r.get(0),
            )?;
            let present = blob_present
                && tx.query_row(
                    "SELECT count(*) > 0 FROM vcs_blobs WHERE realm_id = ?1 AND blob_id = ?2",
                    params![self.repo_id.as_bytes(), b.to_vec()],
                    |r| r.get(0),
                )?;
            if !named || present {
                erase_blobs.push(*b);
            }
        }

        let mut outcome = PurgeOutcome {
            contents: contents.len(),
            blobs: erase_blobs.len(),
            kept_shared,
            ..PurgeOutcome::default()
        };
        // Contents first, then blobs, split so each redaction fits an entry.
        let ids: Vec<(Option<ContentId>, Option<BlobId>)> = contents
            .into_iter()
            .map(|c| (Some(c), None))
            .chain(erase_blobs.into_iter().map(|b| (None, Some(b))))
            .collect();
        for chunk in ids.chunks(MAX_REDACTED_IDS) {
            let redaction = Redaction::new(
                self.repo_id,
                chunk.iter().filter_map(|(c, _)| *c),
                chunk.iter().filter_map(|(_, b)| *b),
                author.clone(),
                now_unix_ms,
                reason,
            );
            let id = redaction.id(&self.store.digest, &self.key)?;
            Self::put_object_tx(
                &tx,
                self.repo_id.as_bytes(),
                KIND_REDACTION,
                id.as_bytes(),
                &redaction.encode()?,
            )?;
            apply_redaction_tx(&tx, self.repo_id, &redaction)?;
            outcome.redactions.push(id);
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// Deletes objects and blobs that nothing local reaches and that never
    /// went through a remote. With `keep_operations`, first forgets all but
    /// that many of the newest operations, so undo cannot go further back
    /// and what only those operations reached becomes garbage.
    ///
    /// Kept: the heads, every commit a bookmark update names, every
    /// workspace (base, working tree, candidate), every remaining
    /// operation's before and after, bookmark updates, redactions, pinned
    /// blobs, and anything pushed to or pulled from a remote.
    pub fn collect_garbage(&mut self, keep_operations: Option<usize>) -> Result<GcOutcome> {
        let caps = self.store.caps.clone();
        let repo = self.repo_id.as_bytes();
        let tx = self.store.conn.unchecked_transaction()?;
        let mut outcome = GcOutcome::default();

        if let Some(keep) = keep_operations {
            outcome.operations = tx.execute(
                "DELETE FROM vcs_operations WHERE repo_id = ?1 AND seq NOT IN (
                   SELECT seq FROM vcs_operations WHERE repo_id = ?1 ORDER BY seq DESC LIMIT ?2)",
                params![repo, keep as i64],
            )?;
            for table in ["vcs_operation_objects", "vcs_rewrites"] {
                tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE repo_id = ?1 AND operation_id NOT IN (
                           SELECT operation_id FROM vcs_operations WHERE repo_id = ?1)"
                    ),
                    params![repo],
                )?;
            }
        }

        let reader = Reader { conn: &tx, repo };
        let mut roots = Roots::default();
        roots.commits.extend(reader.visible()?);
        roots.commits.extend(reader.bookmark_targets(&caps)?);
        let keep_workspace = |ws: &Workspace, roots: &mut Roots| {
            roots.commits.extend(ws.base);
            roots.trees.push(ws.working);
            roots.trees.extend(ws.candidate);
        };
        for ws in reader.workspaces()? {
            keep_workspace(&ws, &mut roots);
        }
        {
            let bad = || StoreError::Invalid("corrupt operation log".into());
            let mut stmt =
                tx.prepare("SELECT before, after FROM vcs_operations WHERE repo_id = ?1")?;
            let rows = stmt.query_map(params![repo], |r| {
                Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (before, after) = row?;
                for log in [before, after] {
                    let v = cbor::decode(&log)?;
                    keep_workspace(
                        &Workspace::from_cbor(v.get("workspace").ok_or_else(bad)?)?,
                        &mut roots,
                    );
                    for key in ["visible_added", "visible_removed"] {
                        for c in v.get(key).and_then(Value::as_array).unwrap_or(&[]) {
                            roots.commits.push(CommitId::from_cbor(c).ok_or_else(bad)?);
                        }
                    }
                }
            }
        }
        let reach = reader.reach(&caps, &roots, &mut |_, _| Ok(false))?;

        let sync = has_table(&tx, "vcs_sync_pushed")?;
        let mut garbage: Vec<(String, Vec<u8>)> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT kind, logical_id FROM vcs_objects WHERE repo_id = ?1 AND kind IN (?2, ?3, ?4, ?5, ?6)",
            )?;
            let rows = stmt.query_map(
                params![
                    repo,
                    KIND_CONTENT,
                    KIND_STATE,
                    KIND_TREE,
                    KIND_COMMIT,
                    KIND_CONFLICT
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )?;
            let kinds: BTreeMap<&str, &'static str> = [
                KIND_CONTENT,
                KIND_STATE,
                KIND_TREE,
                KIND_COMMIT,
                KIND_CONFLICT,
            ]
            .into_iter()
            .map(|k| (k, k))
            .collect();
            for row in rows {
                let (kind, id) = row?;
                let kind_static = kinds[kind.as_str()];
                if !reach.objects.contains(&(kind_static, id.clone())) {
                    garbage.push((kind, id));
                }
            }
        }
        for (kind, id) in garbage {
            let shared = sync
                && tx.query_row(
                    "SELECT count(*) > 0 FROM vcs_sync_pushed WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                    params![repo, kind, id],
                    |r| r.get::<_, bool>(0),
                )?;
            if shared {
                continue;
            }
            tx.execute(
                "DELETE FROM vcs_objects WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                params![repo, kind, id],
            )?;
            tx.execute(
                "DELETE FROM vcs_operation_objects WHERE repo_id = ?1 AND kind = ?2 AND logical_id = ?3",
                params![repo, kind, id],
            )?;
            if kind == KIND_COMMIT {
                tx.execute(
                    "DELETE FROM vcs_commits WHERE repo_id = ?1 AND commit_id = ?2",
                    params![repo, id],
                )?;
                tx.execute(
                    "DELETE FROM vcs_commit_parents WHERE repo_id = ?1 AND commit_id = ?2",
                    params![repo, id],
                )?;
                tx.execute(
                    "DELETE FROM vcs_rewrites WHERE repo_id = ?1 AND (from_commit = ?2 OR to_commit = ?2)",
                    params![repo, id],
                )?;
            }
            outcome.objects += 1;
        }

        if has_table(&tx, "vcs_blobs")? {
            let synced = has_table(&tx, "vcs_sync_blobs")?;
            let local: Vec<Vec<u8>> = {
                let mut stmt = tx.prepare(
                    "SELECT blob_id FROM vcs_blobs WHERE realm_id = ?1
                     AND blob_id NOT IN (SELECT blob_id FROM vcs_blob_pins WHERE realm_id = ?1)",
                )?;
                let rows = stmt.query_map(params![repo], |r| r.get::<_, Vec<u8>>(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for id in local {
                let reached = BlobId::from_bytes(&id).is_ok_and(|b| reach.blobs.contains(&b));
                let shared = synced
                    && tx.query_row(
                        "SELECT count(*) > 0 FROM vcs_sync_blobs WHERE repo_id = ?1 AND blob_id = ?2",
                        params![repo, id],
                        |r| r.get::<_, bool>(0),
                    )?;
                if reached || shared {
                    continue;
                }
                for table in ["vcs_blob_chunks", "vcs_blobs"] {
                    tx.execute(
                        &format!("DELETE FROM {table} WHERE realm_id = ?1 AND blob_id = ?2"),
                        params![repo, id],
                    )?;
                }
                outcome.blobs += 1;
            }
        }
        tx.commit()?;
        Ok(outcome)
    }
}
