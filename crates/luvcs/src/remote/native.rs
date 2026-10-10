//! File and process transports, available only on native targets.
use super::*;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

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
        if limit == 0 {
            return Ok(Vec::new());
        }
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
        check_append(entries, self.limits())?;
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
            Err(RemoteError::Refused {
                code: error_code::LOCKED.into(),
                message: "log is too contended to append".into(),
            })
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
        check_chunk(bytes, self.limits())?;
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
    limits: RemoteLimits,
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
            limits: RemoteLimits::default(),
        };
        match remote.call(&Request::Hello {
            protocol: PROTOCOL_VERSION,
            client: format!("luvcs/{}", env!("CARGO_PKG_VERSION")),
        })? {
            Response::Welcome {
                protocol,
                backend,
                limits,
                ..
            } if protocol == PROTOCOL_VERSION => {
                validate_limits(limits)?;
                remote.limits = limits;
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
    fn limits(&self) -> RemoteLimits {
        self.limits
    }
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
        check_append(entries, self.limits)?;
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
        check_chunk(bytes, self.limits())?;
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
                check_chunk(&bytes, self.limits())?;
                Ok(Some(bytes))
            }
            other => Err(unexpected("a blob chunk", other)),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn received_chunks_respect_negotiated_limit() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("chunks.sh");
        for size in [16, 17] {
            let welcome = serde_json::to_string(&Response::Welcome {
                protocol: PROTOCOL_VERSION,
                backend: "small".into(),
                head: 0,
                limits: RemoteLimits {
                    max_blob_chunk_bytes: 16,
                    ..RemoteLimits::default()
                },
            })
            .unwrap();
            let reply = serde_json::to_string(&Response::BlobChunk {
                data_base64: Some(base64::engine::general_purpose::STANDARD.encode(vec![0; size])),
            })
            .unwrap();
            std::fs::write(&script, format!(
                "read -r hello\nprintf '%s\\n' '{welcome}'\nread -r request\nprintf '%s\\n' '{reply}'\nwhile read -r rest; do :; done\n"
            )).unwrap();
            let remote = ProcessRemote::spawn(&format!("/bin/sh {}", script.display())).unwrap();
            let result = remote.get_blob_chunk("repo_a", "blob_a", 0);
            if size == 16 {
                assert_eq!(result.unwrap(), Some(vec![0; size]));
            } else {
                assert!(
                    matches!(result, Err(RemoteError::Refused { code, .. }) if code == error_code::TOO_LARGE)
                );
            }
        }
    }

    #[test]
    fn process_welcome_negotiates_limits_and_checks_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("remote.sh");
        for (reply, expected) in [
            (
                r#"{"type":"welcome","protocol":1,"backend":"old","head":0}"#,
                Some(RemoteLimits::default()),
            ),
            (
                r#"{"type":"welcome","protocol":1,"backend":"small","head":0,"limits":{"max_entry_bytes":32,"max_blob_chunk_bytes":16,"max_append_entries":1}}"#,
                Some(RemoteLimits {
                    max_entry_bytes: 32,
                    max_blob_chunk_bytes: 16,
                    max_append_entries: 1,
                }),
            ),
            (
                r#"{"type":"welcome","protocol":99,"backend":"future","head":0}"#,
                None,
            ),
            (
                r#"{"type":"welcome","protocol":1,"backend":"bad","head":0,"limits":{"max_entry_bytes":0,"max_blob_chunk_bytes":16,"max_append_entries":1}}"#,
                None,
            ),
        ] {
            std::fs::write(
                &script,
                format!(
                    "read -r hello\nprintf '%s\\n' '{reply}'\nwhile read -r rest; do :; done\n"
                ),
            )
            .unwrap();
            let result = ProcessRemote::spawn(&format!("/bin/sh {}", script.display()));
            match expected {
                Some(limits) => assert_eq!(result.unwrap().limits(), limits),
                None => assert!(matches!(result, Err(RemoteError::Protocol(_)))),
            }
        }
    }
}
