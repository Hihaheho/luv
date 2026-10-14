//! The default backend: one SQLite file and a device id under `.luv/` in the
//! work tree.

use std::path::{Path, PathBuf};

use luvcs::api::{HostContext, VcsRepoInfo, VcsRequest, VcsResponse, execute};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::remote::Remote;
use luvcs::sync::{self, PullReport, PushReport};
use rusqlite::Connection;

/// Directory of the store inside a work tree.
const STORE_DIR: &str = ".luv";
const STORE_FILE: &str = "store.sqlite";
const DEVICE_ID_FILE: &str = "device-id";

pub struct Local {
    conn: Connection,
    device_id: String,
}

impl Local {
    /// Opens (creating on first use) the store of the work tree at `root`.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let dir = root.join(STORE_DIR);
        create_private_dir(&dir)?;
        let path = dir.join(STORE_FILE);
        ensure_private_file(&path)?;
        let conn = Connection::open(&path)
            .map_err(|e| anyhow::anyhow!("failed to open the store at {}: {e}", path.display()))?;
        Ok(Self {
            conn,
            device_id: device_id(&dir)?,
        })
    }

    fn host(&self) -> HostContext<'_> {
        HostContext {
            digest: &RustDigest,
            random: &random_bytes,
            now_unix_ms: now_unix_ms(),
            device_id: self.device_id.clone(),
            actor_kind: ActorKind::User,
        }
    }

    pub fn execute(&self, request: VcsRequest) -> VcsResponse {
        execute(&self.conn, &self.host(), request)
    }

    /// The one repo of this store called `name`, if there is one.
    pub fn find_repo(&self, name: &str) -> anyhow::Result<Option<String>> {
        let repos = match self.execute(VcsRequest::ListRepos) {
            VcsResponse::Repos { repos } => repos,
            VcsResponse::Error { error } => anyhow::bail!("{}: {}", error.code, error.message),
            other => anyhow::bail!("unexpected response: {other:?}"),
        };
        if !repos.iter().any(|r| r.name == name) {
            return Ok(None);
        }
        // Several repos of that name is an error that lists their ids.
        match_repo_name(&repos, name)
            .map(Some)
            .map_err(anyhow::Error::msg)
    }

    pub fn push(
        &self,
        remote: &dyn Remote,
        remote_name: &str,
        repo_id: &str,
    ) -> anyhow::Result<PushReport> {
        Ok(sync::push(
            &self.conn,
            &self.host(),
            remote,
            remote_name,
            repo_id,
        )?)
    }

    pub fn pull(&self, remote: &dyn Remote, remote_name: &str) -> anyhow::Result<PullReport> {
        Ok(sync::pull(&self.conn, &self.host(), remote, remote_name)?)
    }

    /// Resolves a repo argument (an id or a unique name) to its id.
    pub fn resolve_repo(&self, arg: &str) -> anyhow::Result<String> {
        if arg.starts_with("repo_") {
            return Ok(arg.to_owned());
        }
        let repos = match self.execute(VcsRequest::ListRepos) {
            VcsResponse::Repos { repos } => repos,
            VcsResponse::Error { error } => anyhow::bail!("{}: {}", error.code, error.message),
            other => anyhow::bail!("unexpected response: {other:?}"),
        };
        match_repo_name(&repos, arg).map_err(anyhow::Error::msg)
    }
}

/// Picks the one repo named `name`; ids are returned as given.
pub fn match_repo_name(repos: &[VcsRepoInfo], name: &str) -> Result<String, String> {
    let matches: Vec<&VcsRepoInfo> = repos.iter().filter(|r| r.name == name).collect();
    match matches.as_slice() {
        [one] => Ok(one.repo_id.clone()),
        [] => Err(format!(
            "no repo named {name:?} in this store. Run `luv init` first, or pass a repo id (repo_…)."
        )),
        several => Err(format!(
            "{} repos are named {name:?}; pass one of their ids instead: {}",
            several.len(),
            several
                .iter()
                .map(|r| r.repo_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The store holds repo ID keys in the clear, so only the owner may enter.
fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Creates the file if needed and makes it owner-only, so it stays private
/// when copied or backed up without its directory. SQLite gives its journal
/// and WAL files the permissions of the main file.
fn ensure_private_file(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    Ok(())
}

fn random_bytes(out: &mut [u8]) -> Result<(), String> {
    getrandom::fill(out).map_err(|e| e.to_string())
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// This device's actor id: generated once and kept next to the store.
fn device_id(dir: &Path) -> anyhow::Result<String> {
    let path: PathBuf = dir.join(DEVICE_ID_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_owned());
        }
    }
    let mut raw = [0u8; 16];
    random_bytes(&mut raw).map_err(anyhow::Error::msg)?;
    let id = format!("cli-{}", hex::encode(raw));
    std::fs::write(&path, &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = device_id(dir.path()).unwrap();
        assert!(first.starts_with("cli-"));
        assert_eq!(device_id(dir.path()).unwrap(), first);
    }

    #[cfg(unix)]
    #[test]
    fn store_is_owner_only_even_if_it_was_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let store = root.path().join(STORE_DIR).join(STORE_FILE);

        drop(Local::open(root.path()).unwrap());
        assert_eq!(mode(&store), 0o600);
        assert_eq!(mode(&root.path().join(STORE_DIR)), 0o700);

        // A store left world-readable by an earlier version is tightened.
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(Local::open(root.path()).unwrap());
        assert_eq!(mode(&store), 0o600);
    }

    #[test]
    fn repo_names_resolve_uniquely() {
        let info = |id: &str, name: &str| VcsRepoInfo {
            repo_id: id.into(),
            name: name.into(),
            workspace_id: "ws_x".into(),
            created_at_unix_ms: 0,
        };
        let repos = [
            info("repo_a", "notes"),
            info("repo_b", "notes"),
            info("repo_c", "docs"),
        ];
        assert_eq!(match_repo_name(&repos, "docs").unwrap(), "repo_c");
        assert!(match_repo_name(&repos, "notes").is_err());
        assert!(match_repo_name(&repos, "missing").is_err());
    }
}
