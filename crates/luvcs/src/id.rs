//! Typed identifiers.
//!
//! Random IDs (repo, object, change, operation, workspace, realm) are
//! 128-bit values the host draws from its CSPRNG. Content IDs (content,
//! state, tree, commit, conflict, blob) are 256-bit HMAC-SHA256 outputs
//! keyed by the repo's stable ID key over the canonical CBOR of
//! `[domain, format_version, payload]`. Display form is a type prefix plus
//! base64url without padding; the wire form is the raw bytes.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Why a textual ID did not parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdParseError {
    #[error("expected prefix {expected} on {input}")]
    Prefix {
        expected: &'static str,
        input: String,
    },
    #[error("invalid base64url in {0}")]
    Encoding(String),
    #[error("{0} has {1} bytes, expected {2}")]
    Length(String, usize, usize),
}

#[doc(hidden)]
pub fn encode_base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[doc(hidden)]
pub fn decode_base64url(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Defines a fixed-length ID type: `fixed_id!(Name, byte_len, "prefix_")`.
/// The type carries the raw bytes, parses and displays its prefixed
/// base64url form, converts to and from canonical CBOR bytes and
/// serializes as its display form.
#[macro_export]
macro_rules! fixed_id {
    ($(#[$doc:meta])* $name:ident, $len:expr, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub [u8; $len]);

        impl $name {
            pub const LEN: usize = $len;
            pub const PREFIX: &'static str = $prefix;

            pub fn from_bytes(bytes: &[u8]) -> Result<Self, $crate::id::IdParseError> {
                let arr: [u8; $len] = bytes.try_into().map_err(|_| {
                    $crate::id::IdParseError::Length(stringify!($name).to_owned(), bytes.len(), $len)
                })?;
                Ok(Self(arr))
            }

            pub fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn to_vec(&self) -> Vec<u8> {
                self.0.to_vec()
            }

            pub fn to_cbor(&self) -> $crate::cbor::Value {
                $crate::cbor::Value::Bytes(self.0.to_vec())
            }

            pub fn from_cbor(value: &$crate::cbor::Value) -> Option<Self> {
                value.as_bytes().and_then(|b| Self::from_bytes(b).ok())
            }

            pub fn parse(text: &str) -> Result<Self, $crate::id::IdParseError> {
                let body = text.strip_prefix($prefix).ok_or_else(|| $crate::id::IdParseError::Prefix {
                    expected: $prefix,
                    input: text.to_owned(),
                })?;
                let bytes = $crate::id::decode_base64url(body)
                    .ok_or_else(|| $crate::id::IdParseError::Encoding(text.to_owned()))?;
                Self::from_bytes(&bytes)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str($prefix)?;
                f.write_str(&$crate::id::encode_base64url(&self.0))
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self)
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::id::IdParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }

        impl $crate::__serde::Serialize for $name {
            fn serialize<S: $crate::__serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.to_string())
            }
        }

        impl<'de> $crate::__serde::Deserialize<'de> for $name {
            fn deserialize<D: $crate::__serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <String as $crate::__serde::Deserialize>::deserialize(d)?;
                Self::parse(&s).map_err($crate::__serde::de::Error::custom)
            }
        }
    };
}

fixed_id!(
    /// Random 128-bit ID of a blob realm: the dedup and key scope of blobs.
    /// A realm has its own stable ID key.
    RealmId, 16, "realm_"
);
fixed_id!(
    /// Keyed ID of exact blob bytes inside a realm.
    BlobId, 32, "blob_"
);

/// A repo's stable ID key: keys the blob IDs and content IDs of the repo.
/// It is never rotated.
#[derive(Clone, PartialEq, Eq)]
pub struct IdKey(pub [u8; 32]);

impl std::fmt::Debug for IdKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IdKey(..)")
    }
}

use crate::cbor::{self, Value};
use crate::digest::{Digest, DigestError};

fixed_id!(
    /// Random 128-bit repository ID.
    RepoId, 16, "repo_"
);
fixed_id!(
    /// Random 128-bit ID of a logical object inside a repo; survives renames.
    ObjectId, 16, "obj_"
);
fixed_id!(
    /// Random 128-bit ID shared by every rewrite of the same change.
    ChangeId, 16, "chg_"
);
fixed_id!(
    /// Random 128-bit ID of one atomic local operation.
    OperationId, 16, "op_"
);
fixed_id!(
    /// Random 128-bit ID of a workspace.
    WorkspaceId, 16, "ws_"
);
fixed_id!(
    /// Keyed content ID of exact bytes.
    ContentId, 32, "cnt_"
);
fixed_id!(
    /// Keyed ID of an `ObjectState`.
    ObjectStateId, 32, "st_"
);
fixed_id!(
    /// Keyed ID of a snapshot tree root.
    TreeId, 32, "tree_"
);
fixed_id!(
    /// Keyed ID of a commit.
    CommitId, 32, "cmt_"
);
fixed_id!(
    /// Keyed ID of a stored conflict value.
    ConflictId, 32, "cfl_"
);

/// Domain separation for keyed IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    Content,
    State,
    Tree,
    Commit,
    Conflict,
}

impl Domain {
    pub fn as_str(self) -> &'static str {
        match self {
            Domain::Content => "content",
            Domain::State => "state",
            Domain::Tree => "tree",
            Domain::Commit => "commit",
            Domain::Conflict => "conflict",
        }
    }
}

/// `HMAC-SHA256(key, cbor([domain, format_version, payload]))`.
pub fn keyed_id<D: Digest>(
    digest: &D,
    key: &IdKey,
    domain: Domain,
    format_version: u32,
    payload: Value,
) -> Result<[u8; 32], DigestError> {
    let envelope = Value::Array(vec![
        Value::text(domain.as_str()),
        Value::Uint(u64::from(format_version)),
        payload,
    ]);
    digest.hmac_sha256(&key.0, &cbor::encode(&envelope))
}

/// Content ID of exact bytes.
pub fn content_id<D: Digest>(
    digest: &D,
    key: &IdKey,
    bytes: &[u8],
) -> Result<ContentId, DigestError> {
    Ok(ContentId(keyed_id(
        digest,
        key,
        Domain::Content,
        crate::model::CONTENT_FORMAT_V1,
        Value::Bytes(bytes.to_vec()),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::RustDigest;

    #[test]
    fn display_roundtrip() {
        let id = ObjectId([7; 16]);
        let text = id.to_string();
        assert!(text.starts_with("obj_"));
        assert_eq!(ObjectId::parse(&text).unwrap(), id);
        assert_eq!(
            CommitId::parse(&text).unwrap_err(),
            IdParseError::Prefix {
                expected: "cmt_",
                input: text.clone()
            }
        );
        assert!(matches!(
            ContentId::parse("cnt_AAAA").unwrap_err(),
            IdParseError::Length(..)
        ));
        assert!(matches!(
            ObjectId::parse("obj_!!").unwrap_err(),
            IdParseError::Encoding(_)
        ));
    }

    #[test]
    fn content_id_is_keyed_and_domain_separated() {
        let k1 = IdKey([1; 32]);
        let k2 = IdKey([2; 32]);
        let a = content_id(&RustDigest, &k1, b"hello").unwrap();
        assert_eq!(a, content_id(&RustDigest, &k1, b"hello").unwrap());
        assert_ne!(a, content_id(&RustDigest, &k2, b"hello").unwrap());
        assert_ne!(a, content_id(&RustDigest, &k1, b"hellp").unwrap());
        let as_state = keyed_id(
            &RustDigest,
            &k1,
            Domain::State,
            1,
            Value::Bytes(b"hello".to_vec()),
        )
        .unwrap();
        assert_ne!(a.0, as_state);
    }
}
