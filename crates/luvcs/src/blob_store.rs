//! Blob storage in the store database: exact bytes kept in segment-sized
//! chunks, keyed by realm and [`BlobId`].
//!
//! A realm is a repo: `RealmId` is the repo's id bytes and the realm ID key
//! is the repo's ID key (the HMAC domains differ, so blob IDs and content
//! IDs never collide). Dedup therefore stays inside one repo.

use rusqlite::{Connection, OptionalExtension, params};

use crate::blob::{BlobError, BlobRef, MAX_BLOB_BYTES, Realm, SEGMENT_SIZE, blob_id, raw_digest};
use crate::digest::{Digest, Hash32};
use crate::id::{BlobId, IdKey, RealmId, RepoId};

/// The realm of a repo (v1: realm == repo).
pub fn repo_realm(repo_id: RepoId, key: &IdKey) -> Realm {
    Realm {
        id: RealmId(repo_id.0),
        id_key: key.clone(),
    }
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS vcs_blobs (
  realm_id BLOB NOT NULL,
  blob_id BLOB NOT NULL,
  byte_length INTEGER NOT NULL,
  raw_digest BLOB NOT NULL,
  created_at_unix_ms INTEGER NOT NULL,
  PRIMARY KEY (realm_id, blob_id)
) STRICT;
CREATE TABLE IF NOT EXISTS vcs_blob_chunks (
  realm_id BLOB NOT NULL,
  blob_id BLOB NOT NULL,
  chunk_index INTEGER NOT NULL,
  bytes BLOB NOT NULL,
  PRIMARY KEY (realm_id, blob_id, chunk_index)
) STRICT;
"#;

/// Local chunk size; equal to the transfer segment size so a segment is
/// one chunk read.
pub const CHUNK_SIZE: u64 = SEGMENT_SIZE;

#[derive(Debug, thiserror::Error)]
pub enum BlobStoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("blob {0} is not in realm {1}")]
    NotFound(BlobId, RealmId),
    #[error("stored blob {0} is corrupt: {1}")]
    Corrupt(BlobId, String),
}

impl From<crate::digest::DigestError> for BlobStoreError {
    fn from(error: crate::digest::DigestError) -> Self {
        BlobStoreError::Blob(error.into())
    }
}

pub type Result<T> = std::result::Result<T, BlobStoreError>;

/// What the store knows about one blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    pub blob_ref: BlobRef,
    pub raw_digest: Hash32,
    pub created_at_unix_ms: i64,
}

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(())
}

/// Stores exact bytes under their keyed ID. Idempotent: the same bytes in
/// the same realm return the same reference and `false`.
pub fn put_local<D: Digest + ?Sized>(
    conn: &Connection,
    digest: &D,
    realm: &Realm,
    bytes: &[u8],
    now_unix_ms: i64,
) -> Result<(BlobRef, bool)> {
    ensure_schema(conn)?;
    let tx = conn.unchecked_transaction()?;
    let out = put_local_in(&tx, digest, realm, bytes, now_unix_ms)?;
    tx.commit()?;
    Ok(out)
}

/// [`put_local`] inside the caller's transaction (schema already ensured).
pub fn put_local_in<D: Digest + ?Sized>(
    tx: &Connection,
    digest: &D,
    realm: &Realm,
    bytes: &[u8],
    now_unix_ms: i64,
) -> Result<(BlobRef, bool)> {
    if bytes.len() as u64 > MAX_BLOB_BYTES {
        return Err(BlobError::Limit(format!(
            "blob length {} exceeds the limit {MAX_BLOB_BYTES}",
            bytes.len()
        ))
        .into());
    }
    let raw_digest = raw_digest(digest, bytes)?;
    let id = blob_id(digest, realm, bytes.len() as u64, &raw_digest)?;
    let blob_ref = BlobRef {
        realm_id: realm.id,
        blob_id: id,
        byte_length: bytes.len() as u64,
    };
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO vcs_blobs (realm_id, blob_id, byte_length, raw_digest, created_at_unix_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            realm.id.to_vec(),
            id.to_vec(),
            bytes.len() as i64,
            raw_digest.to_vec(),
            now_unix_ms
        ],
    )?;
    if inserted == 1 {
        for (index, chunk) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
            tx.execute(
                "INSERT INTO vcs_blob_chunks (realm_id, blob_id, chunk_index, bytes) VALUES (?1, ?2, ?3, ?4)",
                params![realm.id.to_vec(), id.to_vec(), index as i64, chunk],
            )?;
        }
    }
    Ok((blob_ref, inserted == 1))
}

pub fn stat(conn: &Connection, realm_id: RealmId, blob_id: BlobId) -> Result<Option<BlobInfo>> {
    ensure_schema(conn)?;
    let row = conn
        .query_row(
            "SELECT byte_length, raw_digest, created_at_unix_ms FROM vcs_blobs WHERE realm_id = ?1 AND blob_id = ?2",
            params![realm_id.to_vec(), blob_id.to_vec()],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(len, digest, created)| {
        Ok(BlobInfo {
            blob_ref: BlobRef {
                realm_id,
                blob_id,
                byte_length: u64::try_from(len)
                    .map_err(|_| BlobStoreError::Corrupt(blob_id, "negative length".into()))?,
            },
            raw_digest: digest
                .as_slice()
                .try_into()
                .map_err(|_| BlobStoreError::Corrupt(blob_id, "bad digest".into()))?,
            created_at_unix_ms: created,
        })
    })
    .transpose()
}

pub fn list(conn: &Connection, realm_id: RealmId) -> Result<Vec<BlobInfo>> {
    ensure_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT blob_id, byte_length, raw_digest, created_at_unix_ms FROM vcs_blobs WHERE realm_id = ?1 ORDER BY created_at_unix_ms, blob_id",
    )?;
    let rows = stmt.query_map(params![realm_id.to_vec()], |r| {
        Ok((
            r.get::<_, Vec<u8>>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, len, digest, created) = row?;
        let blob_id = BlobId::from_bytes(&id)
            .map_err(|e| BlobStoreError::Corrupt(BlobId([0; 32]), e.to_string()))?;
        out.push(BlobInfo {
            blob_ref: BlobRef {
                realm_id,
                blob_id,
                byte_length: len as u64,
            },
            raw_digest: digest
                .as_slice()
                .try_into()
                .map_err(|_| BlobStoreError::Corrupt(blob_id, "bad digest".into()))?,
            created_at_unix_ms: created,
        });
    }
    Ok(out)
}

/// Reads `[offset, offset + length)` of a blob, clamped to its end. A
/// range read is not whole-blob verified; [`read_verified`] is.
pub fn read_range(
    conn: &Connection,
    realm_id: RealmId,
    blob_id: BlobId,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    let info = stat(conn, realm_id, blob_id)?.ok_or(BlobStoreError::NotFound(blob_id, realm_id))?;
    let end = offset.saturating_add(length).min(info.blob_ref.byte_length);
    if offset >= end {
        return Ok(Vec::new());
    }
    let first = offset / CHUNK_SIZE;
    let last = (end - 1) / CHUNK_SIZE;
    let mut stmt = conn.prepare(
        "SELECT chunk_index, bytes FROM vcs_blob_chunks WHERE realm_id = ?1 AND blob_id = ?2 AND chunk_index BETWEEN ?3 AND ?4 ORDER BY chunk_index",
    )?;
    let rows = stmt.query_map(
        params![
            realm_id.to_vec(),
            blob_id.to_vec(),
            first as i64,
            last as i64
        ],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
    )?;
    let mut out = Vec::with_capacity((end - offset) as usize);
    for (expected, row) in (first..).zip(rows) {
        let (index, bytes) = row?;
        if index as u64 != expected {
            return Err(BlobStoreError::Corrupt(
                blob_id,
                format!("chunk {expected} missing"),
            ));
        }
        let chunk_start = expected * CHUNK_SIZE;
        let from = offset.saturating_sub(chunk_start) as usize;
        let to = (end - chunk_start).min(bytes.len() as u64) as usize;
        if from > to {
            return Err(BlobStoreError::Corrupt(
                blob_id,
                format!("chunk {expected} short"),
            ));
        }
        out.extend_from_slice(&bytes[from..to]);
    }
    if out.len() as u64 != end - offset {
        return Err(BlobStoreError::Corrupt(
            blob_id,
            "chunks do not cover the blob".into(),
        ));
    }
    Ok(out)
}

/// Reads the whole blob and checks it against its recorded digest.
pub fn read_verified<D: Digest + ?Sized>(
    conn: &Connection,
    digest: &D,
    realm_id: RealmId,
    blob_id: BlobId,
) -> Result<Vec<u8>> {
    let info = stat(conn, realm_id, blob_id)?.ok_or(BlobStoreError::NotFound(blob_id, realm_id))?;
    let bytes = read_range(conn, realm_id, blob_id, 0, info.blob_ref.byte_length)?;
    if raw_digest(digest, &bytes)? != info.raw_digest {
        return Err(BlobStoreError::Corrupt(blob_id, "digest mismatch".into()));
    }
    Ok(bytes)
}

/// Removes a blob's local bytes. Holds and retention are a later stage;
/// callers decide when a blob is unreferenced.
pub fn delete_local(conn: &Connection, realm_id: RealmId, blob_id: BlobId) -> Result<bool> {
    ensure_schema(conn)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM vcs_blob_chunks WHERE realm_id = ?1 AND blob_id = ?2",
        params![realm_id.to_vec(), blob_id.to_vec()],
    )?;
    let n = tx.execute(
        "DELETE FROM vcs_blobs WHERE realm_id = ?1 AND blob_id = ?2",
        params![realm_id.to_vec(), blob_id.to_vec()],
    )?;
    tx.commit()?;
    Ok(n == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::RustDigest;

    fn realm() -> Realm {
        Realm {
            id: RealmId([1; 16]),
            id_key: IdKey([2; 32]),
        }
    }

    #[test]
    fn put_is_idempotent_and_reads_back_verified() {
        let conn = Connection::open_in_memory().unwrap();
        let d = RustDigest;
        let bytes: Vec<u8> = (0..(CHUNK_SIZE + 100)).map(|i| (i % 253) as u8).collect();
        let (r1, fresh) = put_local(&conn, &d, &realm(), &bytes, 1).unwrap();
        assert!(fresh);
        let (r2, again) = put_local(&conn, &d, &realm(), &bytes, 2).unwrap();
        assert!(!again);
        assert_eq!(r1, r2);
        assert_eq!(r1.byte_length, bytes.len() as u64);
        let info = stat(&conn, realm().id, r1.blob_id).unwrap().unwrap();
        assert_eq!(info.created_at_unix_ms, 1);
        assert_eq!(
            read_verified(&conn, &d, realm().id, r1.blob_id).unwrap(),
            bytes
        );
        // Range reads across the chunk boundary.
        let mid = read_range(&conn, realm().id, r1.blob_id, CHUNK_SIZE - 3, 6).unwrap();
        assert_eq!(
            mid,
            &bytes[(CHUNK_SIZE - 3) as usize..(CHUNK_SIZE + 3) as usize]
        );
        assert_eq!(
            read_range(&conn, realm().id, r1.blob_id, 10_000_000, 5).unwrap(),
            b""
        );
        assert_eq!(list(&conn, realm().id).unwrap().len(), 1);

        // The same bytes in another realm are a different blob.
        let other = Realm {
            id: RealmId([9; 16]),
            id_key: IdKey([3; 32]),
        };
        let (r3, _) = put_local(&conn, &d, &other, &bytes, 3).unwrap();
        assert_ne!(r3.blob_id, r1.blob_id);

        assert!(delete_local(&conn, realm().id, r1.blob_id).unwrap());
        assert!(!delete_local(&conn, realm().id, r1.blob_id).unwrap());
        assert!(matches!(
            read_verified(&conn, &d, realm().id, r1.blob_id),
            Err(BlobStoreError::NotFound(..))
        ));
    }

    #[test]
    fn corrupt_chunks_are_detected() {
        let conn = Connection::open_in_memory().unwrap();
        let d = RustDigest;
        let (r, _) = put_local(&conn, &d, &realm(), b"hello world", 1).unwrap();
        conn.execute(
            "UPDATE vcs_blob_chunks SET bytes = X'00' WHERE blob_id = ?1",
            params![r.blob_id.to_vec()],
        )
        .unwrap();
        assert!(matches!(
            read_verified(&conn, &d, realm().id, r.blob_id),
            Err(BlobStoreError::Corrupt(..))
        ));
        let (e, _) = put_local(&conn, &d, &realm(), b"", 1).unwrap();
        assert_eq!(
            read_verified(&conn, &d, realm().id, e.blob_id).unwrap(),
            b""
        );
    }
}
