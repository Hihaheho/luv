//! Remotes: where devices exchange history.
//!
//! A remote is an append-only, ordered log plus a blob store (see
//! `luvcs-protocol`). [`Remote`] is the client side of it; this module has
//! three implementations:
//!
//! - [`MemoryRemote`]: in process, for tests and embedding.
//! - `DirRemote` (native only): a directory several devices can reach. Appends are
//!   atomic and ordered without a lock: an entry file is written whole, then
//!   published with a hard link whose name is its index, so two writers can
//!   never both take the same index.
//! - `ProcessRemote` (native only): a child process that speaks the protocol as one JSON
//!   document per line. This is how another program (a service, a different
//!   storage) becomes a remote.
//!
//! [`serve`] is the other direction: it answers protocol messages from a
//! reader with any [`Remote`], which makes a directory a process remote and
//! documents the protocol by example.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::sync::Mutex;

use base64::Engine as _;
use luvcs_protocol::{
    Entry, EntryBody, MAX_READ_ENTRIES, PROTOCOL_VERSION, RemoteLimits, Request, Response,
    error_code,
};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("remote I/O: {0}")]
    Io(String),
    #[error("remote protocol: {0}")]
    Protocol(String),
    #[error("remote refused ({code}): {message}")]
    Refused { code: String, message: String },
}

impl From<std::io::Error> for RemoteError {
    fn from(e: std::io::Error) -> Self {
        RemoteError::Io(e.to_string())
    }
}

impl RemoteError {
    /// The host may retry the sync operation after a delay; no busy retry loop
    /// is imposed on synchronous or browser hosts.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Refused { code, .. } if error_code::is_retryable(code))
    }
}

pub fn validate_limits(limits: RemoteLimits) -> RemoteResult<()> {
    if limits.max_entry_bytes == 0
        || limits.max_blob_chunk_bytes == 0
        || limits.max_append_entries == 0
    {
        return Err(RemoteError::Protocol(
            "remote limits must be positive".into(),
        ));
    }
    Ok(())
}

/// Validates an entire append before any of it is published.
pub fn check_append(entries: &[EntryBody], limits: RemoteLimits) -> RemoteResult<()> {
    validate_limits(limits)?;
    let too_large = || RemoteError::Refused {
        code: error_code::TOO_LARGE.into(),
        message: "append exceeds advertised entry size or count".into(),
    };
    if entries.len() > limits.max_append_entries as usize {
        return Err(too_large());
    }
    for entry in entries {
        if let EntryBody::Objects { objects, .. } = entry {
            let mut size = 0u64;
            for object in objects {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&object.bytes_base64)
                    .map_err(|e| RemoteError::Refused {
                        code: error_code::INVALID.into(),
                        message: e.to_string(),
                    })?;
                size += bytes.len() as u64;
                if size > u64::from(limits.max_entry_bytes) {
                    return Err(too_large());
                }
            }
        }
    }
    Ok(())
}

pub type RemoteResult<T> = Result<T, RemoteError>;

/// The client side of a remote.
pub trait Remote {
    fn limits(&self) -> RemoteLimits {
        RemoteLimits::default()
    }

    /// Index of the last entry; 0 for an empty log.
    fn head(&self) -> RemoteResult<u64>;
    /// Entries with an index greater than `after`, at most `limit`, in order.
    fn read(&self, after: u64, limit: u32) -> RemoteResult<Vec<Entry>>;
    /// Appends `entries` atomically and in order; returns the new head.
    /// Implementations must validate against `limits()` before writing (see
    /// [`check_append`]). `serve` delegates validation to this method.
    fn append(&self, entries: &[EntryBody]) -> RemoteResult<u64>;
    fn put_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
        bytes: &[u8],
    ) -> RemoteResult<()>;
    fn get_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
    ) -> RemoteResult<Option<Vec<u8>>>;
}

fn check_chunk(bytes: &[u8], limits: RemoteLimits) -> RemoteResult<()> {
    validate_limits(limits)?;
    if bytes.len() > limits.max_blob_chunk_bytes as usize {
        return Err(RemoteError::Refused {
            code: "too_large".into(),
            message: format!(
                "blob chunk of {} bytes exceeds limit of {} bytes",
                bytes.len(),
                limits.max_blob_chunk_bytes
            ),
        });
    }
    Ok(())
}

/// Ids are used as file names: letters, digits, `_` and `-` only.
#[cfg(not(target_arch = "wasm32"))]
fn check_name(what: &str, name: &str) -> RemoteResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(RemoteError::Refused {
            code: "invalid".into(),
            message: format!("{what} {name:?} is not a valid id"),
        })
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemoryState {
    log: Vec<EntryBody>,
    blobs: BTreeMap<(String, String, u32), Vec<u8>>,
}

/// A remote that lives in memory. Cloning is not provided: share it by
/// reference.
#[derive(Default)]
pub struct MemoryRemote {
    state: Mutex<MemoryState>,
}

impl MemoryRemote {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Remote for MemoryRemote {
    fn head(&self) -> RemoteResult<u64> {
        Ok(self.state.lock().unwrap().log.len() as u64)
    }

    fn read(&self, after: u64, limit: u32) -> RemoteResult<Vec<Entry>> {
        let state = self.state.lock().unwrap();
        Ok(state
            .log
            .iter()
            .enumerate()
            .skip(usize::try_from(after).unwrap_or(usize::MAX))
            .take(limit.min(MAX_READ_ENTRIES) as usize)
            .map(|(i, body)| Entry {
                index: i as u64 + 1,
                body: body.clone(),
            })
            .collect())
    }

    fn append(&self, entries: &[EntryBody]) -> RemoteResult<u64> {
        check_append(entries, self.limits())?;
        let mut state = self.state.lock().unwrap();
        state.log.extend_from_slice(entries);
        Ok(state.log.len() as u64)
    }

    fn put_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
        bytes: &[u8],
    ) -> RemoteResult<()> {
        check_chunk(bytes, self.limits())?;
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert((repo_id.into(), blob_id.into(), index), bytes.to_vec());
        Ok(())
    }

    fn get_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
    ) -> RemoteResult<Option<Vec<u8>>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .blobs
            .get(&(repo_id.into(), blob_id.into(), index))
            .cloned())
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{DirRemote, ProcessRemote};

// ---------------------------------------------------------------------------
// Serving
// ---------------------------------------------------------------------------

fn serve_one(remote: &dyn Remote, request: Request, backend: &str) -> RemoteResult<Response> {
    Ok(match request {
        Request::Hello { protocol, .. } => {
            if protocol != PROTOCOL_VERSION {
                return Err(RemoteError::Refused {
                    code: "unsupported_protocol".into(),
                    message: format!("this server speaks protocol {PROTOCOL_VERSION}"),
                });
            }
            validate_limits(remote.limits())?;
            Response::Welcome {
                protocol: PROTOCOL_VERSION,
                backend: backend.to_owned(),
                head: remote.head()?,
                limits: remote.limits(),
            }
        }
        Request::Head => Response::Head {
            head: remote.head()?,
        },
        Request::Read { after, limit } => Response::Entries {
            entries: remote.read(after, limit)?,
            head: remote.head()?,
        },
        Request::Append { entries } => Response::Appended {
            head: remote.append(&entries)?,
        },
        Request::PutBlobChunk {
            repo_id,
            blob_id,
            index,
            data_base64,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .map_err(|e| RemoteError::Refused {
                    code: "invalid".into(),
                    message: format!("blob chunk: {e}"),
                })?;
            check_chunk(&bytes, remote.limits())?;
            remote.put_blob_chunk(&repo_id, &blob_id, index, &bytes)?;
            Response::Ok
        }
        Request::GetBlobChunk {
            repo_id,
            blob_id,
            index,
        } => Response::BlobChunk {
            data_base64: remote
                .get_blob_chunk(&repo_id, &blob_id, index)?
                .map(|b| base64::engine::general_purpose::STANDARD.encode(b)),
        },
    })
}

/// Answers protocol messages (one JSON document per line) from `input` on
/// `output` until `input` ends, using `remote` as the storage.
pub fn serve(
    remote: &dyn Remote,
    backend: &str,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Err(e) => Response::Error {
                code: "invalid".into(),
                message: format!("bad request: {e}"),
            },
            Ok(request) => match serve_one(remote, request, backend) {
                Ok(r) => r,
                Err(RemoteError::Refused { code, message }) => Response::Error { code, message },
                Err(e) => Response::Error {
                    code: "internal".into(),
                    message: e.to_string(),
                },
            },
        };
        let mut text = serde_json::to_string(&response).expect("responses serialize");
        text.push('\n');
        output.write_all(text.as_bytes())?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::path::Path;

    fn heads(n: u32) -> EntryBody {
        EntryBody::Heads {
            repo_id: "repo_a".into(),
            commit_ids: vec![format!("cmt_{n}")],
            refs: vec![],
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_new_dir_remote_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("remote");
        let remote = DirRemote::open(&root).unwrap();
        remote.append(&[heads(1)]).unwrap();
        remote.put_blob_chunk("repo_a", "blob_a", 0, b"x").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&root.join("log")), 0o700);
        assert_eq!(mode(&root.join("blobs/repo_a/blob_a")), 0o700);
        let log = std::fs::read_dir(root.join("log"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(mode(&log.path()), 0o600);
        let chunk = root.join("blobs/repo_a/blob_a/0000000000");
        assert_eq!(mode(&chunk), 0o600);
    }

    /// The contract every remote must meet.
    fn contract(remote: &dyn Remote) {
        assert_eq!(remote.head().unwrap(), 0);
        assert!(remote.read(0, 10).unwrap().is_empty());

        assert_eq!(remote.append(&[heads(1), heads(2)]).unwrap(), 2);
        assert_eq!(remote.append(&[heads(3)]).unwrap(), 3);
        assert_eq!(remote.head().unwrap(), 3);
        assert!(remote.read(0, 0).unwrap().is_empty());

        let all = remote.read(0, 10).unwrap();
        assert_eq!(
            all.iter().map(|e| e.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(all[2].body, heads(3));
        // Reading resumes after an index, inside a multi-entry append too.
        let tail = remote.read(1, 10).unwrap();
        assert_eq!(tail.iter().map(|e| e.index).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(remote.read(2, 1).unwrap().len(), 1);
        assert!(remote.read(3, 10).unwrap().is_empty());

        assert_eq!(remote.get_blob_chunk("repo_a", "blob_x", 0).unwrap(), None);
        remote
            .put_blob_chunk("repo_a", "blob_x", 0, b"abc")
            .unwrap();
        assert_eq!(
            remote.get_blob_chunk("repo_a", "blob_x", 0).unwrap(),
            Some(b"abc".to_vec())
        );
    }

    #[test]
    fn serve_returns_append_validation_errors_without_writing() {
        let remote = MemoryRemote::new();
        for body in [
            EntryBody::Objects {
                repo_id: "repo_a".into(),
                objects: vec![luvcs_protocol::WireObject {
                    kind: "content".into(),
                    logical_id_base64: "".into(),
                    bytes_base64: "invalid!".into(),
                }],
            },
            EntryBody::Objects {
                repo_id: "repo_a".into(),
                objects: vec![luvcs_protocol::WireObject {
                    kind: "content".into(),
                    logical_id_base64: "".into(),
                    bytes_base64: base64::engine::general_purpose::STANDARD.encode(vec![
                        0;
                        luvcs_protocol::MAX_ENTRY_BYTES
                            + 1
                    ]),
                }],
            },
        ] {
            let input = serde_json::to_string(&Request::Append {
                entries: vec![heads(0), body],
            })
            .unwrap();
            let mut output = Vec::new();
            serve(&remote, "memory", input.as_bytes(), &mut output).unwrap();
            assert!(
                matches!(serde_json::from_slice::<Response>(&output).unwrap(), Response::Error { code, .. } if code == error_code::INVALID || code == error_code::TOO_LARGE)
            );
            assert_eq!(remote.head().unwrap(), 0);
        }
    }

    #[test]
    fn append_checks_are_atomic_and_include_the_exact_boundary() {
        let remote = MemoryRemote::new();
        let object = |length| EntryBody::Objects {
            repo_id: "repo_a".into(),
            objects: vec![luvcs_protocol::WireObject {
                kind: "content".into(),
                logical_id_base64: "".into(),
                bytes_base64: base64::engine::general_purpose::STANDARD.encode(vec![0; length]),
            }],
        };
        let max = luvcs_protocol::MAX_ENTRY_BYTES;
        assert!(remote.append(&[heads(0), object(max + 1)]).is_err());
        assert_eq!(remote.head().unwrap(), 0);
        assert!(
            remote
                .append(&vec![
                    heads(0);
                    luvcs_protocol::MAX_APPEND_ENTRIES as usize + 1
                ])
                .is_err()
        );
        assert_eq!(remote.head().unwrap(), 0);
        assert_eq!(remote.append(&[object(max)]).unwrap(), 1);
    }

    #[test]
    fn memory_remote_meets_the_contract() {
        contract(&MemoryRemote::new());
    }

    #[test]
    fn dir_remote_meets_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        contract(&DirRemote::open(dir.path()).unwrap());
    }

    #[test]
    fn dir_remote_is_shared_between_handles() {
        let dir = tempfile::tempdir().unwrap();
        let a = DirRemote::open(dir.path()).unwrap();
        let b = DirRemote::open(dir.path()).unwrap();
        a.append(&[heads(1)]).unwrap();
        assert_eq!(b.append(&[heads(2)]).unwrap(), 2);
        assert_eq!(a.head().unwrap(), 2);
        assert_eq!(a.read(0, 10).unwrap()[1].body, heads(2));
    }

    #[test]
    fn dir_remote_refuses_ids_that_are_paths() {
        let dir = tempfile::tempdir().unwrap();
        let remote = DirRemote::open(dir.path()).unwrap();
        for bad in ["../x", "a/b", "", ".", "a b"] {
            assert!(
                remote.put_blob_chunk(bad, "blob_x", 0, b"x").is_err(),
                "{bad}"
            );
            assert!(remote.get_blob_chunk("repo_a", bad, 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn oversized_chunks_are_refused() {
        let remote = MemoryRemote::new();
        let big = vec![0u8; luvcs_protocol::MAX_BLOB_CHUNK_BYTES + 1];
        assert!(remote.put_blob_chunk("repo_a", "blob_x", 0, &big).is_err());
    }

    #[test]
    fn serve_answers_the_protocol_over_lines() {
        let remote = MemoryRemote::new();
        let input = [
            r#"{"type":"hello","protocol":1,"client":"test"}"#,
            r#"{"type":"append","entries":[{"type":"heads","repo_id":"repo_a","commit_ids":["cmt_1"],"refs":[]}]}"#,
            r#"{"type":"read","after":0,"limit":10}"#,
            r#"{"type":"hello","protocol":99,"client":"test"}"#,
            "not json",
        ]
        .join("\n");
        let mut out = Vec::new();
        serve(&remote, "memory", input.as_bytes(), &mut out).unwrap();
        let replies: Vec<Response> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(matches!(&replies[0], Response::Welcome { backend, .. } if backend == "memory"));
        assert_eq!(replies[1], Response::Appended { head: 1 });
        assert!(
            matches!(&replies[2], Response::Entries { entries, head: 1 } if entries.len() == 1)
        );
        assert!(
            matches!(&replies[3], Response::Error { code, .. } if code == "unsupported_protocol")
        );
        assert!(matches!(&replies[4], Response::Error { code, .. } if code == "invalid"));
    }
}
