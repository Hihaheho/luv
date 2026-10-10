//! Blob identity: realms, keyed blob IDs and semantic blob references.
//!
//! A blob is exact bytes. Its [`BlobId`] is keyed by the realm's ID key over
//! the byte length and the SHA-256 of the bytes, so equal bytes dedup inside
//! one realm and IDs never collide across realms. How blob bytes are kept
//! (chunked in the local store, framed for a remote) is the store's and the
//! backend's business, not this module's.

use crate::cbor::{self, Value};
use crate::digest::{Digest, DigestError, Hash32};
use crate::id::{BlobId, IdKey, RealmId};

/// Plaintext bytes per stored chunk; only the last chunk is shorter.
pub const SEGMENT_SIZE: u64 = 4 * 1024 * 1024;
/// Product policy limit on one blob.
pub const MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;

/// Why a blob operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("{0}")]
    Limit(String),
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// A length or digest the caller expected does not match the bytes.
    #[error("{0}")]
    Mismatch(String),
    #[error(transparent)]
    Digest(#[from] DigestError),
}

/// A blob realm: the scope of dedup and of the blob ID key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Realm {
    pub id: RealmId,
    pub id_key: IdKey,
}

/// `SHA-256(exact_raw_bytes)`.
pub fn raw_digest<D: Digest + ?Sized>(digest: &D, bytes: &[u8]) -> Result<Hash32, DigestError> {
    digest.sha256(bytes)
}

/// `HMAC-SHA256(realm_id_key, cbor(["luvcs/blob/id", 1, byte_length, raw_digest]))`.
pub fn blob_id<D: Digest + ?Sized>(
    digest: &D,
    realm: &Realm,
    byte_length: u64,
    raw_digest: &Hash32,
) -> Result<BlobId, DigestError> {
    let envelope = Value::Array(vec![
        Value::text("luvcs/blob/id"),
        Value::Uint(1),
        Value::Uint(byte_length),
        Value::Bytes(raw_digest.to_vec()),
    ]);
    Ok(BlobId(
        digest.hmac_sha256(&realm.id_key.0, &cbor::encode(&envelope))?,
    ))
}

/// A semantic reference to blob bytes: realm, keyed ID and exact length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BlobRef {
    pub realm_id: RealmId,
    pub blob_id: BlobId,
    pub byte_length: u64,
}

impl BlobRef {
    pub fn to_cbor(&self) -> Value {
        Value::map([
            ("realm_id", self.realm_id.to_cbor()),
            ("blob_id", self.blob_id.to_cbor()),
            ("byte_length", Value::Uint(self.byte_length)),
        ])
    }

    pub fn from_cbor(value: &Value) -> Option<Self> {
        Some(Self {
            realm_id: RealmId::from_cbor(value.get("realm_id")?)?,
            blob_id: BlobId::from_cbor(value.get("blob_id")?)?,
            byte_length: value.get("byte_length")?.as_u64()?,
        })
    }
}
