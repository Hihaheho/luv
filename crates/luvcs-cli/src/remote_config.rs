//! The remote of a work tree: where `luv push` and `luv pull` go.
//!
//! It is kept in `.luv/remote.eure`, next to the store and never in
//! `Luv.eure`: a work tree can come from someone else, and a remote may name
//! a command, which must not run just because a file was checked out.

use std::path::{Path, PathBuf};

use luvcs::remote::{DirRemote, ProcessRemote, Remote};
use luvcs_protocol::{RemoteConfig, RemoteSpec};

pub const REMOTE_FILE: &str = "remote.eure";

fn file(root: &Path) -> PathBuf {
    root.join(".luv").join(REMOTE_FILE)
}

pub fn load(root: &Path) -> anyhow::Result<Option<RemoteConfig>> {
    let path = file(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let config: RemoteConfig = crate::eure_file::read(&text, &path)?;
    config
        .spec()
        .map_err(|m| anyhow::anyhow!("{}: {m}", path.display()))?;
    Ok(Some(config))
}

pub fn render(config: &RemoteConfig) -> String {
    let quote = |s: &str| serde_json::to_string(s).expect("string serializes");
    let mut out =
        String::from("// Where `luv push` and `luv pull` go. Set with `luv remote set`.\n");
    if let Some(path) = &config.path {
        out.push_str(&format!("path = {}\n", quote(path)));
    }
    if let Some(command) = &config.command {
        out.push_str(&format!("command = {}\n", quote(command)));
    }
    out
}

pub fn save(root: &Path, config: &RemoteConfig) -> anyhow::Result<()> {
    config.spec().map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(root.join(".luv"))?;
    std::fs::write(file(root), render(config))?;
    Ok(())
}

/// An open remote and the name the store keeps its bookkeeping under.
pub struct Connected {
    pub remote: Box<dyn Remote>,
    pub name: String,
}

pub fn connect(root: &Path) -> anyhow::Result<Connected> {
    let config = load(root)?.ok_or_else(|| {
        anyhow::anyhow!("no remote: run `luv remote set --path <dir>` or `--command <cmd>`")
    })?;
    Ok(match config.spec().map_err(anyhow::Error::msg)? {
        RemoteSpec::Path(path) => {
            let resolved = root.join(&path);
            let remote = DirRemote::open(&resolved)?;
            let canonical = std::fs::canonicalize(&resolved).unwrap_or(resolved);
            Connected {
                remote: Box::new(remote),
                name: format!("path:{}", canonical.display()),
            }
        }
        RemoteSpec::Command(command) => Connected {
            remote: Box::new(ProcessRemote::spawn(&command)?),
            name: format!("command:{command}"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        let config = RemoteConfig {
            path: Some("../shared".into()),
            command: None,
        };
        save(dir.path(), &config).unwrap();
        assert_eq!(load(dir.path()).unwrap(), Some(config));

        let both = RemoteConfig {
            path: Some("a".into()),
            command: Some("b".into()),
        };
        assert!(save(dir.path(), &both).is_err());
        std::fs::write(file(dir.path()), "path = \"a\"\ncommand = \"b\"\n").unwrap();
        assert!(
            load(dir.path()).is_err(),
            "a hand-edited file is checked too"
        );
    }
}
