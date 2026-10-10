//! Remotes: where devices exchange history.
//!
//! A remote is an append-only, ordered log plus a blob store (see
//! `luvcs-protocol`). [`Remote`] is the client side of it; this module has
//! three implementations:
//!
//! - [`MemoryRemote`]: in process, for tests and embedding.
//! - [`DirRemote`]: a directory several devices can reach. Appends are
//!   atomic and ordered without a lock: an entry file is written whole, then
//!   published with a hard link whose name is its index, so two writers can
//!   never both take the same index.
//! - [`ProcessRemote`]: a child process that speaks the protocol as one JSON
//!   document per line. This is how another program (a service, a different
//!   storage) becomes a remote.
//!
//! [`serve`] is the other direction: it answers protocol messages from a
//! reader with any [`Remote`], which makes a directory a process remote and
//! documents the protocol by example.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use base64::Engine as _;
use luvcs_protocol::{
    Entry, EntryBody, MAX_BLOB_CHUNK_BYTES, MAX_READ_ENTRIES, PROTOCOL_VERSION, Request, Response,
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

pub type RemoteResult<T> = Result<T, RemoteError>;

/// The client side of a remote.
pub trait Remote {
    /// Index of the last entry; 0 for an empty log.
    fn head(&self) -> RemoteResult<u64>;
    /// Entries with an index greater than `after`, at most `limit`, in order.
    fn read(&self, after: u64, limit: u32) -> RemoteResult<Vec<Entry>>;
    /// Appends `entries` atomically and in order; returns the new head.
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

fn check_chunk(bytes: &[u8]) -> RemoteResult<()> {
    if bytes.len() > MAX_BLOB_CHUNK_BYTES {
        return Err(RemoteError::Refused {
            code: "too_large".into(),
            message: format!("blob chunk of {} bytes", bytes.len()),
        });
    }
    Ok(())
}

/// Ids are used as file names: letters, digits, `_` and `-` only.
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
        check_chunk(bytes)?;
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

// ---------------------------------------------------------------------------
// Directory
// ---------------------------------------------------------------------------

/// A remote in a directory:
///
/// ```text
/// log/<first index, 20 digits>.json   a JSON array of entry bodies
/// blobs/<repo id>/<blob id>/<chunk, 10 digits>
/// ```
///
/// A log file holds the entries from its name's index up to the index before
/// the next file's name, so a reader never opens a file it can skip.
pub struct DirRemote {
    root: PathBuf,
}

/// Retries of an append that lost the race for an index.
const APPEND_ATTEMPTS: usize = 64;

/// Creates `path` and its missing parents readable by the owner only. The
/// log carries each repo's ID key, so it is as sensitive as the store. A
/// directory that already exists keeps the mode its owner gave it.
fn create_private_dir_all(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Writes `bytes` to a new file readable by the owner only.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

impl DirRemote {
    pub fn open(root: impl Into<PathBuf>) -> RemoteResult<Self> {
        let root = root.into();
        create_private_dir_all(&root.join("log"))?;
        create_private_dir_all(&root.join("blobs"))?;
        create_private_dir_all(&root.join("tmp"))?;
        Ok(Self { root })
    }

    /// `(first index, path)` of every log file, ascending.
    fn log_files(&self) -> RemoteResult<Vec<(u64, PathBuf)>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.root.join("log"))? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let Ok(first) = stem.parse::<u64>() else {
                continue;
            };
            files.push((first, entry.path()));
        }
        files.sort();
        Ok(files)
    }

    fn read_file(path: &Path) -> RemoteResult<Vec<EntryBody>> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| RemoteError::Protocol(format!("{}: {e}", path.display())))
    }

    fn blob_path(&self, repo_id: &str, blob_id: &str, index: u32) -> RemoteResult<PathBuf> {
        check_name("repo id", repo_id)?;
        check_name("blob id", blob_id)?;
        Ok(self
            .root
            .join("blobs")
            .join(repo_id)
            .join(blob_id)
            .join(format!("{index:010}")))
    }

    fn tmp_path(&self) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.root.join("tmp").join(format!(
            "{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

impl Remote for DirRemote {
    fn head(&self) -> RemoteResult<u64> {
        match self.log_files()?.last() {
            None => Ok(0),
            Some((first, path)) => Ok(first - 1 + Self::read_file(path)?.len() as u64),
        }
    }

    fn read(&self, after: u64, limit: u32) -> RemoteResult<Vec<Entry>> {
        let limit = limit.min(MAX_READ_ENTRIES) as usize;
        let files = self.log_files()?;
        let mut out = Vec::new();
        for (i, (first, path)) in files.iter().enumerate() {
            // Entries of this file end before the next file begins.
            if let Some((next, _)) = files.get(i + 1)
                && *next <= after + 1
            {
                continue;
            }
            for (offset, body) in Self::read_file(path)?.into_iter().enumerate() {
                let index = first + offset as u64;
                if index > after {
                    out.push(Entry { index, body });
                    if out.len() == limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    fn append(&self, entries: &[EntryBody]) -> RemoteResult<u64> {
        if entries.is_empty() {
            return self.head();
        }
        let tmp = self.tmp_path();
        write_private(
            &tmp,
            &serde_json::to_vec(entries).map_err(|e| RemoteError::Protocol(e.to_string()))?,
        )?;
        let result = (|| {
            for _ in 0..APPEND_ATTEMPTS {
                let first = self.head()? + 1;
                let target = self.root.join("log").join(format!("{first:020}.json"));
                match std::fs::hard_link(&tmp, &target) {
                    Ok(()) => return Ok(first - 1 + entries.len() as u64),
                    // Another writer took this index: look again.
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(RemoteError::from(e)),
                }
            }
            Err(RemoteError::Io("log is too contended to append".into()))
        })();
        let _ = std::fs::remove_file(&tmp);
        result
    }

    fn put_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
        bytes: &[u8],
    ) -> RemoteResult<()> {
        check_chunk(bytes)?;
        let path = self.blob_path(repo_id, blob_id, index)?;
        create_private_dir_all(path.parent().expect("chunk has a parent"))?;
        let tmp = self.tmp_path();
        write_private(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn get_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
    ) -> RemoteResult<Option<Vec<u8>>> {
        match std::fs::read(self.blob_path(repo_id, blob_id, index)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Process
// ---------------------------------------------------------------------------

struct Pipe {
    child: Child,
    /// `None` once closed, which is how a server is told to exit.
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

/// A remote served by a child process, one JSON document per line in each
/// direction. The command is split on whitespace (no quoting): wrap anything
/// more complex in a script.
///
/// Dropping the remote closes the server's stdin and waits up to five
/// seconds for it to exit, then kills it. A server should therefore exit when
/// its input ends, as [`serve`] does.
pub struct ProcessRemote {
    pipe: Mutex<Pipe>,
    backend: String,
}

impl ProcessRemote {
    pub fn spawn(command: &str) -> RemoteResult<Self> {
        let mut parts = command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| RemoteError::Io("remote command is empty".into()))?;
        let mut child = Command::new(program)
            .args(parts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| RemoteError::Io(format!("cannot start {program:?}: {e}")))?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut remote = Self {
            pipe: Mutex::new(Pipe {
                child,
                stdin: Some(stdin),
                stdout,
            }),
            backend: String::new(),
        };
        match remote.call(&Request::Hello {
            protocol: PROTOCOL_VERSION,
            client: format!("luvcs/{}", env!("CARGO_PKG_VERSION")),
        })? {
            Response::Welcome {
                protocol, backend, ..
            } if protocol == PROTOCOL_VERSION => {
                remote.backend = backend;
                Ok(remote)
            }
            Response::Welcome { protocol, .. } => Err(RemoteError::Protocol(format!(
                "remote speaks protocol {protocol}, this client {PROTOCOL_VERSION}"
            ))),
            other => Err(RemoteError::Protocol(format!(
                "expected a welcome, got {other:?}"
            ))),
        }
    }

    /// What the remote called itself in its welcome.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    fn call(&self, request: &Request) -> RemoteResult<Response> {
        let mut pipe = self.pipe.lock().unwrap();
        let mut line =
            serde_json::to_string(request).map_err(|e| RemoteError::Protocol(e.to_string()))?;
        line.push('\n');
        let stdin = pipe
            .stdin
            .as_mut()
            .ok_or_else(|| RemoteError::Io("remote connection is closed".into()))?;
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        let mut reply = String::new();
        if pipe.stdout.read_line(&mut reply)? == 0 {
            return Err(RemoteError::Io("remote closed the connection".into()));
        }
        match serde_json::from_str::<Response>(&reply)
            .map_err(|e| RemoteError::Protocol(format!("bad reply: {e}")))?
        {
            Response::Error { code, message } => Err(RemoteError::Refused { code, message }),
            other => Ok(other),
        }
    }
}

impl Drop for ProcessRemote {
    fn drop(&mut self) {
        if let Ok(pipe) = self.pipe.get_mut() {
            // Closing stdin tells a well-behaved server to exit; a server
            // that ignores it is not waited for forever.
            drop(pipe.stdin.take());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match pipe.child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if std::time::Instant::now() >= deadline => {
                        let _ = pipe.child.kill();
                        let _ = pipe.child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }
        }
    }
}

fn unexpected(what: &str, got: Response) -> RemoteError {
    RemoteError::Protocol(format!("expected {what}, got {got:?}"))
}

impl Remote for ProcessRemote {
    fn head(&self) -> RemoteResult<u64> {
        match self.call(&Request::Head)? {
            Response::Head { head } => Ok(head),
            other => Err(unexpected("a head", other)),
        }
    }

    fn read(&self, after: u64, limit: u32) -> RemoteResult<Vec<Entry>> {
        match self.call(&Request::Read { after, limit })? {
            Response::Entries { entries, .. } => Ok(entries),
            other => Err(unexpected("entries", other)),
        }
    }

    fn append(&self, entries: &[EntryBody]) -> RemoteResult<u64> {
        match self.call(&Request::Append {
            entries: entries.to_vec(),
        })? {
            Response::Appended { head } => Ok(head),
            other => Err(unexpected("an append receipt", other)),
        }
    }

    fn put_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
        bytes: &[u8],
    ) -> RemoteResult<()> {
        check_chunk(bytes)?;
        match self.call(&Request::PutBlobChunk {
            repo_id: repo_id.into(),
            blob_id: blob_id.into(),
            index,
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })? {
            Response::Ok => Ok(()),
            other => Err(unexpected("ok", other)),
        }
    }

    fn get_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
    ) -> RemoteResult<Option<Vec<u8>>> {
        match self.call(&Request::GetBlobChunk {
            repo_id: repo_id.into(),
            blob_id: blob_id.into(),
            index,
        })? {
            Response::BlobChunk { data_base64: None } => Ok(None),
            Response::BlobChunk {
                data_base64: Some(text),
            } => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(|e| RemoteError::Protocol(format!("blob chunk: {e}")))?;
                check_chunk(&bytes)?;
                Ok(Some(bytes))
            }
            other => Err(unexpected("a blob chunk", other)),
        }
    }
}

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
            Response::Welcome {
                protocol: PROTOCOL_VERSION,
                backend: backend.to_owned(),
                head: remote.head()?,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let big = vec![0u8; MAX_BLOB_CHUNK_BYTES + 1];
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
