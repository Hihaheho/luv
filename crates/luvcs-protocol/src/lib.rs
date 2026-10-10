//! The wire contract between luvcs and a remote.
//!
//! A remote is an append-only, ordered **log** of entries plus a **blob
//! store**. Nothing in it needs to understand the content: objects are
//! immutable and content-addressed, so appending the same object twice is
//! harmless, and a puller re-hashes everything it takes in. Heads are
//! announced, not decided: every device folds the announcements it reads
//! into its own set of heads.
//!
//! A host that wants to be a remote implements the log and blob operations
//! of [`Request`] and answers with [`Response`]. Over a process boundary the
//! messages are one JSON document per line.
//!
//! IDs are in their prefixed display form (`repo_…`, `cmt_…`, `blob_…`);
//! byte strings are standard base64.

use serde::{Deserialize, Serialize};

/// Version of this contract. A remote refuses a client it cannot serve.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest number of bytes of object payload one [`EntryBody::Objects`]
/// carries; a writer splits bigger sets over several entries.
pub const MAX_ENTRY_BYTES: usize = 1024 * 1024;
/// Largest blob chunk, in bytes.
pub const MAX_BLOB_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Most entries one [`Request::Read`] may ask for.
pub const MAX_READ_ENTRIES: u32 = 256;

/// Default maximum number of entries in one atomic append.
pub const MAX_APPEND_ENTRIES: u32 = MAX_READ_ENTRIES;

/// Write limits. Byte counts exclude JSON/base64 overhead: entry bytes are
/// the sum of decoded object payloads in an Objects entry. Other entry kinds
/// have no byte limit in v1. Limits must be positive and stable for a log's
/// lifetime, since blob chunk indices depend on the chunk size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteLimits {
    pub max_entry_bytes: u32,
    pub max_blob_chunk_bytes: u32,
    pub max_append_entries: u32,
}

impl Default for RemoteLimits {
    fn default() -> Self {
        Self {
            max_entry_bytes: MAX_ENTRY_BYTES as u32,
            max_blob_chunk_bytes: MAX_BLOB_CHUNK_BYTES as u32,
            max_append_entries: MAX_APPEND_ENTRIES,
        }
    }
}

/// Standard error vocabulary. Unknown codes must be surfaced to the user,
/// never automatically retried. Locked/unavailable mean the request made no
/// changes and can be retried after a host-selected delay. Transport failures
/// have an unknown outcome and are not classified as safe request retries.
pub mod error_code {
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const LOCKED: &str = "locked";
    pub const TOO_LARGE: &str = "too_large";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const INVALID: &str = "invalid";
    pub const UNSUPPORTED_PROTOCOL: &str = "unsupported_protocol";
    pub const INTERNAL: &str = "internal";

    pub fn is_retryable(code: &str) -> bool {
        matches!(code, LOCKED | UNAVAILABLE)
    }
}

/// How to reach a remote. Exactly one of the two fields is set.
///
/// In `.luv/remote.eure`:
///
/// ```eure
/// path = "../shared"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "config", derive(eure::FromEure))]
#[cfg_attr(
    feature = "config",
    eure(crate = ::eure::document, rename_all = "kebab-case")
)]
pub struct RemoteConfig {
    /// A directory several devices can reach (a synced folder, a mount).
    #[cfg_attr(feature = "config", eure(default))]
    pub path: Option<String>,
    /// A command that serves this protocol on its stdin and stdout.
    #[cfg_attr(feature = "config", eure(default))]
    pub command: Option<String>,
}

/// A [`RemoteConfig`] that has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSpec {
    Path(String),
    Command(String),
}

impl RemoteConfig {
    pub fn spec(&self) -> Result<RemoteSpec, String> {
        match (&self.path, &self.command) {
            (Some(p), None) if !p.trim().is_empty() => Ok(RemoteSpec::Path(p.clone())),
            (None, Some(c)) if !c.trim().is_empty() => Ok(RemoteSpec::Command(c.clone())),
            (None, None) => Err("remote needs `path` or `command`".into()),
            (Some(_), Some(_)) => Err("remote takes `path` or `command`, not both".into()),
            _ => Err("remote `path` / `command` must not be empty".into()),
        }
    }
}

/// An object as it travels: its kind, its logical ID and its exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireObject {
    pub kind: String,
    pub logical_id_base64: String,
    pub bytes_base64: String,
}

/// A named reference to a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRef {
    pub name: String,
    pub commit_id: String,
}

/// What a client appends to the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryBody {
    /// A repo exists. `id_key_base64` is the key every ID of the repo is an
    /// HMAC under; whoever reads the log can verify objects with it, so a
    /// remote must keep the log as private as the content itself.
    RepoCreated {
        repo_id: String,
        name: String,
        id_key_base64: String,
        created_at_unix_ms: i64,
    },
    /// Immutable objects of a repo.
    Objects {
        repo_id: String,
        objects: Vec<WireObject>,
    },
    /// The announcer's heads and refs. Every commit named here is in an
    /// earlier entry of the log.
    Heads {
        repo_id: String,
        commit_ids: Vec<String>,
        refs: Vec<WireRef>,
    },
    /// The bytes of a blob are in the blob store (all chunks).
    Blob {
        repo_id: String,
        blob_id: String,
        byte_length: u64,
        raw_digest_hex: String,
        chunk_count: u32,
    },
}

/// A log entry as a reader sees it: the position the remote gave it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 1-based, dense, assigned by the remote.
    pub index: u64,
    pub body: EntryBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: u32,
        client: String,
    },
    /// The index of the last entry (0 for an empty log).
    Head,
    /// Entries with an index greater than `after`, at most `limit`.
    Read {
        after: u64,
        limit: u32,
    },
    /// Appends all `entries` atomically and in order.
    Append {
        entries: Vec<EntryBody>,
    },
    PutBlobChunk {
        repo_id: String,
        blob_id: String,
        index: u32,
        data_base64: String,
    },
    GetBlobChunk {
        repo_id: String,
        blob_id: String,
        index: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Welcome {
        protocol: u32,
        backend: String,
        head: u64,
        /// Additive v1 capability: old peers omit this and use defaults.
        #[serde(default)]
        limits: RemoteLimits,
    },
    Head {
        head: u64,
    },
    Entries {
        entries: Vec<Entry>,
        head: u64,
    },
    Appended {
        head: u64,
    },
    Ok,
    /// `None` when the chunk is not in the store.
    BlobChunk {
        data_base64: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_welcome_uses_default_limits() {
        let response: Response =
            serde_json::from_str(r#"{"type":"welcome","protocol":1,"backend":"old","head":0}"#)
                .unwrap();
        assert!(
            matches!(response, Response::Welcome { limits, .. } if limits == RemoteLimits::default())
        );
        let current = Response::Welcome {
            protocol: PROTOCOL_VERSION,
            backend: "new".into(),
            head: 0,
            limits: RemoteLimits {
                max_entry_bytes: 32,
                max_blob_chunk_bytes: 16,
                max_append_entries: 1,
            },
        };
        assert_eq!(
            serde_json::from_str::<Response>(&serde_json::to_string(&current).unwrap()).unwrap(),
            current
        );
    }

    #[test]
    fn only_explicit_temporary_refusals_can_be_retried() {
        for code in [error_code::LOCKED, error_code::UNAVAILABLE] {
            assert!(error_code::is_retryable(code));
        }
        for code in [
            error_code::UNAUTHORIZED,
            error_code::TOO_LARGE,
            error_code::INVALID,
            error_code::INTERNAL,
            error_code::UNSUPPORTED_PROTOCOL,
            "future_code",
        ] {
            assert!(!error_code::is_retryable(code));
        }
    }

    #[test]
    fn messages_round_trip_as_tagged_json() {
        let request = Request::Append {
            entries: vec![
                EntryBody::RepoCreated {
                    repo_id: "repo_a".into(),
                    name: "notes".into(),
                    id_key_base64: "AAAA".into(),
                    created_at_unix_ms: 1,
                },
                EntryBody::Heads {
                    repo_id: "repo_a".into(),
                    commit_ids: vec!["cmt_x".into()],
                    refs: vec![WireRef {
                        name: "main".into(),
                        commit_id: "cmt_x".into(),
                    }],
                },
            ],
        };
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.starts_with(r#"{"type":"append""#), "{text}");
        assert_eq!(serde_json::from_str::<Request>(&text).unwrap(), request);

        let response = Response::BlobChunk { data_base64: None };
        let text = serde_json::to_string(&response).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&text).unwrap(), response);
    }

    #[test]
    fn unknown_message_types_are_refused() {
        assert!(serde_json::from_str::<Request>(r#"{"type":"drop_everything"}"#).is_err());
    }

    #[test]
    fn remote_config_needs_exactly_one_target() {
        let cfg = |path: Option<&str>, command: Option<&str>| RemoteConfig {
            path: path.map(Into::into),
            command: command.map(Into::into),
        };
        assert_eq!(
            cfg(Some("../x"), None).spec(),
            Ok(RemoteSpec::Path("../x".into()))
        );
        assert_eq!(
            cfg(None, Some("my-remote serve")).spec(),
            Ok(RemoteSpec::Command("my-remote serve".into()))
        );
        assert!(cfg(None, None).spec().is_err());
        assert!(cfg(Some("a"), Some("b")).spec().is_err());
        assert!(cfg(Some("  "), None).spec().is_err());
    }
}
