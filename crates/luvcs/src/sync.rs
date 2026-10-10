//! Push and pull between a store and a [`Remote`].
//!
//! The remote is an ordered log (see `luvcs-protocol`). **Push** appends
//! what the remote has not seen: the repo's creation, its immutable
//! objects, its blobs and the device's current heads and refs. **Pull**
//! reads the entries after the device's cursor and takes them in:
//! objects are re-hashed under the repo's ID key before they are stored,
//! heads are folded into the device's visible set the same way a local
//! integrate would, and blobs are checked against the IDs they were
//! announced under.
//!
//! Nothing here can conflict: objects are immutable and content-addressed,
//! so taking the same object twice changes nothing, and two devices that
//! push concurrently simply end up with both heads. A pull never moves a
//! device's working tree; integrating another head is a separate, explicit
//! operation (`VcsRequest::Integrate`).
//!
//! `remote_name` identifies the remote in the store's bookkeeping (its
//! cursor and what it is known to hold); use a stable string per remote.

use base64::Engine as _;
use luvcs_protocol::{
    EntryBody, MAX_BLOB_CHUNK_BYTES, MAX_ENTRY_BYTES, MAX_READ_ENTRIES, WireObject, WireRef,
};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::api::{HostContext, LocalRepo, VcsError, ensure_local_schema, insert_local_repo};
use crate::blob::{BlobError, MAX_BLOB_BYTES, SEGMENT_SIZE, blob_id, raw_digest};
use crate::blob_store::{self, BlobStoreError};
use crate::id::{BlobId, CommitId, IdKey, RealmId, RepoId, WorkspaceId};
use crate::remote::{Remote, RemoteError};
use crate::store::{Store, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error("{}: {}", .0.code, .0.message)]
    Api(VcsError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Blob(#[from] BlobStoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// The remote's log says something this device cannot accept.
    #[error("remote log: {0}")]
    Log(String),
}

impl From<VcsError> for SyncError {
    fn from(e: VcsError) -> Self {
        SyncError::Api(e)
    }
}

pub type Result<T> = std::result::Result<T, SyncError>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushReport {
    pub objects: usize,
    pub blobs: usize,
    pub heads_announced: bool,
    /// The remote's head after the push.
    pub head: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullReport {
    pub entries: usize,
    /// Repos that were new to this device.
    pub repos: Vec<String>,
    pub objects: usize,
    pub blobs: usize,
    /// The remote's head the device is now at.
    pub head: u64,
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS vcs_sync_cursor (
  remote TEXT PRIMARY KEY NOT NULL,
  last_index INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_sync_repos (
  remote TEXT NOT NULL,
  repo_id BLOB NOT NULL,
  PRIMARY KEY (remote, repo_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_sync_pushed (
  remote TEXT NOT NULL,
  repo_id BLOB NOT NULL,
  kind TEXT NOT NULL,
  logical_id BLOB NOT NULL,
  PRIMARY KEY (remote, repo_id, kind, logical_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_sync_blobs (
  remote TEXT NOT NULL,
  repo_id BLOB NOT NULL,
  blob_id BLOB NOT NULL,
  PRIMARY KEY (remote, repo_id, blob_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_sync_wanted (
  remote TEXT NOT NULL,
  repo_id BLOB NOT NULL,
  blob_id BLOB NOT NULL,
  byte_length INTEGER NOT NULL,
  raw_digest_hex TEXT NOT NULL,
  chunk_count INTEGER NOT NULL,
  PRIMARY KEY (remote, repo_id, blob_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_sync_heads (
  remote TEXT NOT NULL,
  repo_id BLOB NOT NULL,
  announced TEXT NOT NULL,
  PRIMARY KEY (remote, repo_id)
) STRICT;
"#;

fn ensure_schema(conn: &Connection) -> Result<()> {
    ensure_local_schema(conn)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn log_err(what: &str, detail: impl std::fmt::Display) -> SyncError {
    SyncError::Log(format!("{what}: {detail}"))
}

fn parse_repo(text: &str) -> Result<RepoId> {
    RepoId::parse(text).map_err(|e| log_err("repo id", e))
}

fn parse_commit(text: &str) -> Result<CommitId> {
    CommitId::parse(text).map_err(|e| log_err("commit id", e))
}

fn parse_blob(text: &str) -> Result<BlobId> {
    BlobId::parse(text).map_err(|e| log_err("blob id", e))
}

fn decode(what: &str, text: &str) -> Result<Vec<u8>> {
    B64.decode(text).map_err(|e| log_err(what, e))
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/// Appends to `remote` what it does not have of repo `repo_id`.
pub fn push(
    conn: &Connection,
    host: &HostContext<'_>,
    remote: &dyn Remote,
    remote_name: &str,
    repo_id: &str,
) -> Result<PushReport> {
    ensure_schema(conn)?;
    blob_store::ensure_schema(conn)?;
    let local = crate::api::local_repo(conn, repo_id)?;
    let rid = local.repo_id;
    let rid_text = rid.to_string();
    let mut store = Store::open(conn, host.digest)?;
    let repo = store.repo(rid, local.key.clone())?;

    let mut bodies: Vec<EntryBody> = Vec::new();
    let mut report = PushReport::default();

    // 1. The repo itself.
    let known: bool = conn.query_row(
        "SELECT count(*) > 0 FROM vcs_sync_repos WHERE remote = ?1 AND repo_id = ?2",
        params![remote_name, rid.to_vec()],
        |r| r.get(0),
    )?;
    if !known {
        bodies.push(EntryBody::RepoCreated {
            repo_id: rid_text.clone(),
            name: local.name.clone(),
            id_key_base64: B64.encode(local.key.0),
            created_at_unix_ms: local.created_at_unix_ms,
        });
    }

    // 2. Objects the remote has not seen, in entries of bounded size.
    let mut pushed: Vec<(String, Vec<u8>)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT o.kind, o.logical_id, o.bytes FROM vcs_objects o
             WHERE o.repo_id = ?1 AND NOT EXISTS (
               SELECT 1 FROM vcs_sync_pushed p
               WHERE p.remote = ?2 AND p.repo_id = o.repo_id
                 AND p.kind = o.kind AND p.logical_id = o.logical_id)
             ORDER BY o.kind, o.logical_id",
        )?;
        let rows = stmt.query_map(params![rid.to_vec(), remote_name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        let mut group: Vec<WireObject> = Vec::new();
        let mut group_bytes = 0usize;
        for row in rows {
            let (kind, logical_id, bytes) = row?;
            if group_bytes + bytes.len() > MAX_ENTRY_BYTES && !group.is_empty() {
                bodies.push(EntryBody::Objects {
                    repo_id: rid_text.clone(),
                    objects: std::mem::take(&mut group),
                });
                group_bytes = 0;
            }
            group_bytes += bytes.len();
            group.push(WireObject {
                kind: kind.clone(),
                logical_id_base64: B64.encode(&logical_id),
                bytes_base64: B64.encode(&bytes),
            });
            pushed.push((kind, logical_id));
        }
        if !group.is_empty() {
            bodies.push(EntryBody::Objects {
                repo_id: rid_text.clone(),
                objects: group,
            });
        }
        report.objects = pushed.len();
    }

    // 3. Blobs: the chunks go to the blob store first, so no entry ever
    //    names bytes the remote does not have.
    let realm_id = RealmId(rid.0);
    let mut uploaded: Vec<BlobId> = Vec::new();
    for info in blob_store::list(conn, realm_id)? {
        let id = info.blob_ref.blob_id;
        let known: bool = conn.query_row(
            "SELECT count(*) > 0 FROM vcs_sync_blobs WHERE remote = ?1 AND repo_id = ?2 AND blob_id = ?3",
            params![remote_name, rid.to_vec(), id.to_vec()],
            |r| r.get(0),
        )?;
        if known {
            continue;
        }
        let length = info.blob_ref.byte_length;
        let chunk = SEGMENT_SIZE.min(MAX_BLOB_CHUNK_BYTES as u64);
        let count = length.div_ceil(chunk);
        for index in 0..count {
            let offset = index * chunk;
            let bytes =
                blob_store::read_range(conn, realm_id, id, offset, chunk.min(length - offset))?;
            remote.put_blob_chunk(
                &rid_text,
                &id.to_string(),
                u32::try_from(index).map_err(|_| log_err("blob", "too many chunks"))?,
                &bytes,
            )?;
        }
        bodies.push(EntryBody::Blob {
            repo_id: rid_text.clone(),
            blob_id: id.to_string(),
            byte_length: length,
            raw_digest_hex: hex::encode(info.raw_digest),
            chunk_count: count as u32,
        });
        uploaded.push(id);
    }
    report.blobs = uploaded.len();

    // 4. The device's heads and refs, when they changed.
    let visible = repo.visible()?;
    let refs = repo.refs()?;
    let announcement = announcement_text(&visible, &refs);
    let last: Option<String> = conn
        .query_row(
            "SELECT announced FROM vcs_sync_heads WHERE remote = ?1 AND repo_id = ?2",
            params![remote_name, rid.to_vec()],
            |r| r.get(0),
        )
        .optional()?;
    let has_heads = !(visible.is_empty() && refs.is_empty());
    if has_heads && last.as_deref() != Some(announcement.as_str()) {
        bodies.push(EntryBody::Heads {
            repo_id: rid_text.clone(),
            commit_ids: visible.iter().map(ToString::to_string).collect(),
            refs: refs
                .iter()
                .map(|(name, id)| WireRef {
                    name: name.clone(),
                    commit_id: id.to_string(),
                })
                .collect(),
        });
        report.heads_announced = true;
    }

    report.head = if bodies.is_empty() {
        remote.head()?
    } else {
        remote.append(&bodies)?
    };

    // Only now does the device record what the remote holds.
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO vcs_sync_repos (remote, repo_id) VALUES (?1, ?2)",
        params![remote_name, rid.to_vec()],
    )?;
    for (kind, logical_id) in &pushed {
        tx.execute(
            "INSERT OR IGNORE INTO vcs_sync_pushed (remote, repo_id, kind, logical_id) VALUES (?1, ?2, ?3, ?4)",
            params![remote_name, rid.to_vec(), kind, logical_id],
        )?;
    }
    for id in &uploaded {
        tx.execute(
            "INSERT OR IGNORE INTO vcs_sync_blobs (remote, repo_id, blob_id) VALUES (?1, ?2, ?3)",
            params![remote_name, rid.to_vec(), id.to_vec()],
        )?;
    }
    if report.heads_announced {
        tx.execute(
            "INSERT INTO vcs_sync_heads (remote, repo_id, announced) VALUES (?1, ?2, ?3)
             ON CONFLICT (remote, repo_id) DO UPDATE SET announced = excluded.announced",
            params![remote_name, rid.to_vec(), announcement],
        )?;
    }
    tx.commit()?;
    Ok(report)
}

/// A canonical text of the heads and refs, to tell whether they changed.
fn announcement_text(
    visible: &[CommitId],
    refs: &std::collections::BTreeMap<String, CommitId>,
) -> String {
    let mut heads: Vec<String> = visible.iter().map(ToString::to_string).collect();
    heads.sort();
    let refs: Vec<String> = refs.iter().map(|(n, c)| format!("{n}={c}")).collect();
    format!("{}|{}", heads.join(","), refs.join(","))
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

/// A blob the log announced that this device does not have yet. Kept in the
/// store until it arrives, so a failed download is retried by the next pull
/// even though the log entry that announced it has been read.
struct PendingBlob {
    repo_id: RepoId,
    blob_id: BlobId,
    byte_length: u64,
    raw_digest_hex: String,
    chunk_count: u32,
}

/// Takes in every entry of `remote` after this device's cursor.
pub fn pull(
    conn: &Connection,
    host: &HostContext<'_>,
    remote: &dyn Remote,
    remote_name: &str,
) -> Result<PullReport> {
    ensure_schema(conn)?;
    blob_store::ensure_schema(conn)?;
    let mut cursor: u64 = conn
        .query_row(
            "SELECT last_index FROM vcs_sync_cursor WHERE remote = ?1",
            params![remote_name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map_or(0, |v| v as u64);
    let mut report = PullReport {
        head: cursor,
        ..PullReport::default()
    };

    loop {
        let entries = remote.read(cursor, MAX_READ_ENTRIES)?;
        if entries.is_empty() {
            break;
        }
        for entry in entries {
            if entry.index != cursor + 1 {
                return Err(log_err(
                    "entry index",
                    format!("expected {}, got {}", cursor + 1, entry.index),
                ));
            }
            apply(conn, host, remote_name, &entry.body, &mut report)?;
            cursor = entry.index;
            conn.execute(
                "INSERT INTO vcs_sync_cursor (remote, last_index) VALUES (?1, ?2)
                 ON CONFLICT (remote) DO UPDATE SET last_index = excluded.last_index",
                params![remote_name, cursor as i64],
            )?;
            report.entries += 1;
            report.head = cursor;
        }
    }

    // Every blob still wanted, including those of earlier pulls. One that
    // fails does not keep the others from arriving; the first error is
    // returned and the rest is tried again next time.
    let mut first_error = None;
    for blob in wanted_blobs(conn, remote_name)? {
        match download_blob(conn, host, remote, &blob) {
            Ok(downloaded) => {
                conn.execute(
                    "DELETE FROM vcs_sync_wanted WHERE remote = ?1 AND repo_id = ?2 AND blob_id = ?3",
                    params![remote_name, blob.repo_id.to_vec(), blob.blob_id.to_vec()],
                )?;
                if downloaded {
                    report.blobs += 1;
                }
            }
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

fn apply(
    conn: &Connection,
    host: &HostContext<'_>,
    remote_name: &str,
    body: &EntryBody,
    report: &mut PullReport,
) -> Result<()> {
    match body {
        EntryBody::RepoCreated {
            repo_id,
            name,
            id_key_base64,
            created_at_unix_ms,
        } => {
            let repo_id = parse_repo(repo_id)?;
            let key: [u8; 32] = decode("id key", id_key_base64)?
                .try_into()
                .map_err(|_| log_err("id key", "must be 32 bytes"))?;
            let existing = crate::api::local_repos(conn)?
                .into_iter()
                .find(|r| r.repo_id == repo_id);
            let tx = conn.unchecked_transaction()?;
            match existing {
                Some(local) if local.key.0 != key => {
                    return Err(log_err(
                        "repo",
                        format!("{repo_id} has a different ID key on this device"),
                    ));
                }
                Some(_) => {}
                None => {
                    insert_local_repo(
                        &tx,
                        &LocalRepo {
                            repo_id,
                            name: name.clone(),
                            key: IdKey(key),
                            workspace_id: WorkspaceId(host.random_16()?),
                            created_at_unix_ms: *created_at_unix_ms,
                        },
                    )?;
                    let mut store = Store::open(&tx, host.digest)?;
                    match store.create_repo(repo_id, *created_at_unix_ms) {
                        Ok(()) | Err(StoreError::RepoExists(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                    report.repos.push(repo_id.to_string());
                }
            }
            tx.execute(
                "INSERT OR IGNORE INTO vcs_sync_repos (remote, repo_id) VALUES (?1, ?2)",
                params![remote_name, repo_id.to_vec()],
            )?;
            tx.commit()?;
        }
        EntryBody::Objects { repo_id, objects } => {
            let repo_id = parse_repo(repo_id)?;
            let local = local_repo(conn, repo_id)?;
            let mut decoded = Vec::with_capacity(objects.len());
            for o in objects {
                decoded.push((
                    o.kind.clone(),
                    decode("object id", &o.logical_id_base64)?,
                    decode("object bytes", &o.bytes_base64)?,
                ));
            }
            let mut store = Store::open(conn, host.digest)?;
            let mut repo = store.repo(repo_id, local.key)?;
            // Re-hashes every object under the repo's key; one bad object
            // refuses the entry and the cursor stays where it was.
            report.objects += repo.import_objects(&decoded)?;
            let tx = conn.unchecked_transaction()?;
            for (kind, logical_id, _) in &decoded {
                tx.execute(
                    "INSERT OR IGNORE INTO vcs_sync_pushed (remote, repo_id, kind, logical_id) VALUES (?1, ?2, ?3, ?4)",
                    params![remote_name, repo_id.to_vec(), kind, logical_id],
                )?;
            }
            tx.commit()?;
        }
        EntryBody::Heads {
            repo_id,
            commit_ids,
            refs,
        } => {
            let repo_id = parse_repo(repo_id)?;
            let local = local_repo(conn, repo_id)?;
            let mut named = Vec::with_capacity(refs.len());
            for r in refs {
                named.push((r.name.clone(), parse_commit(&r.commit_id)?));
            }
            let mut store = Store::open(conn, host.digest)?;
            let mut repo = store.repo(repo_id, local.key)?;
            for (i, commit) in commit_ids.iter().enumerate() {
                let commit = parse_commit(commit)?;
                repo.merge_visible(commit, if i == 0 { &named } else { &[] })?;
            }
        }
        EntryBody::Blob {
            repo_id,
            blob_id,
            byte_length,
            raw_digest_hex,
            chunk_count,
        } => {
            let repo_id = parse_repo(repo_id)?;
            let blob_id = parse_blob(blob_id)?;
            local_repo(conn, repo_id)?;
            conn.execute(
                "INSERT OR IGNORE INTO vcs_sync_blobs (remote, repo_id, blob_id) VALUES (?1, ?2, ?3)",
                params![remote_name, repo_id.to_vec(), blob_id.to_vec()],
            )?;
            if blob_store::stat(conn, RealmId(repo_id.0), blob_id)?.is_none() {
                conn.execute(
                    "INSERT OR IGNORE INTO vcs_sync_wanted
                       (remote, repo_id, blob_id, byte_length, raw_digest_hex, chunk_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        remote_name,
                        repo_id.to_vec(),
                        blob_id.to_vec(),
                        *byte_length as i64,
                        raw_digest_hex,
                        i64::from(*chunk_count)
                    ],
                )?;
            }
        }
    }
    Ok(())
}

fn wanted_blobs(conn: &Connection, remote_name: &str) -> Result<Vec<PendingBlob>> {
    let mut stmt = conn.prepare(
        "SELECT repo_id, blob_id, byte_length, raw_digest_hex, chunk_count
         FROM vcs_sync_wanted WHERE remote = ?1 ORDER BY repo_id, blob_id",
    )?;
    let rows = stmt.query_map(params![remote_name], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (repo_id, blob_id, byte_length, raw_digest_hex, chunk_count) = row?;
        out.push(PendingBlob {
            repo_id: RepoId::from_bytes(&repo_id).map_err(|e| log_err("wanted blob", e))?,
            blob_id: BlobId::from_bytes(&blob_id).map_err(|e| log_err("wanted blob", e))?,
            byte_length: u64::try_from(byte_length)
                .map_err(|_| log_err("wanted blob", "negative length"))?,
            raw_digest_hex,
            chunk_count: u32::try_from(chunk_count)
                .map_err(|_| log_err("wanted blob", "bad chunk count"))?,
        });
    }
    Ok(out)
}

fn local_repo(conn: &Connection, repo_id: RepoId) -> Result<LocalRepo> {
    crate::api::local_repos(conn)?
        .into_iter()
        .find(|r| r.repo_id == repo_id)
        .ok_or_else(|| {
            log_err(
                "entry",
                format!("names repo {repo_id}, which was never created"),
            )
        })
}

/// Downloads one announced blob and stores it only if its bytes hash to the
/// ID it was announced under. `false` when it arrived by another entry.
fn download_blob(
    conn: &Connection,
    host: &HostContext<'_>,
    remote: &dyn Remote,
    blob: &PendingBlob,
) -> Result<bool> {
    if blob.byte_length > MAX_BLOB_BYTES {
        return Err(log_err(
            "blob",
            format!("{} bytes exceed the limit", blob.byte_length),
        ));
    }
    let local = local_repo(conn, blob.repo_id)?;
    let realm = blob_store::repo_realm(blob.repo_id, &local.key);
    if blob_store::stat(conn, realm.id, blob.blob_id)?.is_some() {
        return Ok(false);
    }
    let mut bytes = Vec::with_capacity(blob.byte_length as usize);
    for index in 0..blob.chunk_count {
        let chunk = remote
            .get_blob_chunk(&blob.repo_id.to_string(), &blob.blob_id.to_string(), index)?
            .ok_or_else(|| log_err("blob", format!("{} chunk {index} is missing", blob.blob_id)))?;
        bytes.extend_from_slice(&chunk);
        if bytes.len() as u64 > blob.byte_length {
            return Err(log_err(
                "blob",
                format!("{} is longer than announced", blob.blob_id),
            ));
        }
    }
    let digest = raw_digest(host.digest, &bytes).map_err(BlobStoreError::from)?;
    let id = blob_id(host.digest, &realm, bytes.len() as u64, &digest)
        .map_err(BlobError::from)
        .map_err(BlobStoreError::from)?;
    if bytes.len() as u64 != blob.byte_length
        || id != blob.blob_id
        || hex::encode(digest) != blob.raw_digest_hex
    {
        return Err(log_err(
            "blob",
            format!(
                "{} does not hash to the ID it was announced under",
                blob.blob_id
            ),
        ));
    }
    blob_store::put_local(conn, host.digest, &realm, &bytes, host.now_unix_ms)?;
    Ok(true)
}
