//! `luv`: git / jj style commands over a directory of files, on top of the
//! luvcs request API (`luvcs::api`).
//!
//! A directory is a luv work tree when it holds `Luv.eure`, which names the
//! repo and **allowlists** the files luv may see (`include` globs). A file
//! that no glob matches does not exist for luv: it is never saved, listed or
//! deleted. A file in the work tree is one object whose `title` is its
//! `/`-separated path relative to `Luv.eure`; the VCS rules (commit, log,
//! restore) stay in `luvcs`, this module only maps files to objects and
//! back.

mod local;
mod remote_config;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use clap::{Parser, Subcommand};
use eure::FromEure;
use globset::{Glob, GlobSet, GlobSetBuilder};
use luvcs::api::{VcsObjectView, VcsRequest, VcsResponse};

use local::Local;

/// Name of the config file that marks a work tree.
pub const CONFIG_FILE: &str = "Luv.eure";
/// Type id of every object luv saves.
const FILE_TYPE: &str = "luv.file";
const DEFAULT_LOG_LIMIT: u32 = 20;

/// `Luv.eure`.
#[derive(Debug, Clone, FromEure)]
#[eure(crate = ::eure::document, rename_all = "kebab-case")]
pub struct LuvConfig {
    /// Name of the repo this directory is the work tree of.
    pub repo: String,
    /// Globs (relative to this file) of the files luv tracks. Nothing else
    /// is tracked; an empty list tracks nothing.
    #[eure(default)]
    pub include: Vec<String>,
}

impl LuvConfig {
    pub fn parse(content: &str, path: &Path) -> anyhow::Result<Self> {
        eure::parse_content(content, path.to_path_buf())
            .map_err(|m| anyhow::anyhow!("invalid {}: {m}", path.display()))
    }

    fn render(&self) -> String {
        let globs = serde_json::to_string(&self.include).expect("array serializes");
        format!(
            "// Files luv tracks. Anything not matched here is ignored.\nrepo = {}\ninclude = {globs}\n",
            serde_json::to_string(&self.repo).expect("string serializes"),
        )
    }

    fn matcher(&self) -> anyhow::Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.include {
            let glob = globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow::anyhow!("bad include glob {pattern:?}: {e}"))?;
            builder.add(Glob::clone(&glob));
        }
        Ok(builder.build()?)
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "luv",
    version,
    about = "Version control for a directory of files"
)]
pub struct LuvArgs {
    #[command(subcommand)]
    pub command: LuvCommands,
}

#[derive(Subcommand, Debug)]
pub enum LuvCommands {
    /// Turn this directory into a work tree: writes Luv.eure and creates the repo
    Init {
        /// Repo name (defaults to the directory name)
        #[arg(long)]
        repo: Option<String>,
        /// Allowlisted globs, e.g. "**/*.md" (repeatable). Without any,
        /// Luv.eure tracks nothing until you edit it.
        #[arg(long = "include")]
        include: Vec<String>,
    },
    /// Show files added, changed or removed since the last commit
    Status,
    /// Show line changes of text files since the last commit
    Diff {
        /// Only these paths
        paths: Vec<String>,
    },
    /// Record the allowlisted files as a commit
    Commit {
        /// Commit description
        #[arg(short = 'm', long = "message")]
        message: String,
        /// Only these paths; the rest stays uncommitted
        paths: Vec<String>,
    },
    /// List commits, newest first
    Log {
        /// Maximum number of commits
        #[arg(short = 'n', long, default_value_t = DEFAULT_LOG_LIMIT)]
        limit: u32,
    },
    /// Write files back to disk as saved (discarding edits on disk), or as
    /// they were in a commit
    Restore {
        /// Take the files from this commit (cmt_…) instead of the last commit
        #[arg(long)]
        commit: Option<String>,
        /// Only these paths (default: every tracked file)
        paths: Vec<String>,
    },
    /// Choose where `push` and `pull` go
    Remote {
        #[command(subcommand)]
        command: RemoteCommands,
    },
    /// Send this repo's history to the remote
    Push,
    /// Take in what the remote has; does not touch the files on disk
    Pull,
    /// Serve a directory as a remote on stdin and stdout, the reference
    /// implementation of the protocol a `remote set --command` program speaks
    #[command(hide = true)]
    Serve {
        /// The directory that holds the log and the blobs
        #[arg(long)]
        path: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum RemoteCommands {
    /// Set the remote: a directory several devices reach, or a command that
    /// speaks the luvcs protocol on stdin and stdout
    Set {
        /// A shared directory (relative paths are from the work tree root)
        #[arg(long, conflicts_with = "command", required_unless_present = "command")]
        path: Option<String>,
        /// A program to run for every push and pull, split on spaces
        #[arg(long)]
        command: Option<String>,
    },
    /// Print the remote
    Show,
}

fn main() -> anyhow::Result<()> {
    run(LuvArgs::parse())
}

fn run(args: LuvArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    match args.command {
        LuvCommands::Init { repo, include } => return init(&cwd, repo, include),
        LuvCommands::Remote { command } => return remote_command(&cwd, command),
        LuvCommands::Pull => return pull(&cwd),
        LuvCommands::Serve { path } => return serve(&path),
        _ => {}
    }
    let tree = WorkTree::discover(&cwd)?;
    let device = Local::open(&tree.root)?;
    let repo_id = device.resolve_repo(&tree.config.repo)?;
    let ctx = Ctx {
        device: &device,
        tree: &tree,
        repo_id,
    };
    match args.command {
        LuvCommands::Init { .. }
        | LuvCommands::Remote { .. }
        | LuvCommands::Pull
        | LuvCommands::Serve { .. } => unreachable!(),
        LuvCommands::Push => ctx.push(),
        LuvCommands::Status => ctx.status(),
        LuvCommands::Diff { paths } => ctx.diff(&paths),
        LuvCommands::Commit { message, paths } => ctx.commit(&message, &paths),
        LuvCommands::Log { limit } => ctx.log(limit),
        LuvCommands::Restore { commit, paths } => ctx.restore(commit, &paths),
    }
}

fn init(cwd: &Path, repo: Option<String>, include: Vec<String>) -> anyhow::Result<()> {
    let config_path = cwd.join(CONFIG_FILE);
    if config_path.exists() {
        anyhow::bail!("{} already exists", config_path.display());
    }
    let name = match repo {
        Some(name) => name,
        None => cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow::anyhow!("pass --repo: this directory has no name"))?,
    };
    let config = LuvConfig {
        repo: name.clone(),
        include,
    };
    config.matcher()?; // reject a bad glob before anything is created
    let device = Local::open(cwd)?;
    // A repo of this name that `luv pull` brought in is attached to rather
    // than created again: this is how a second device starts.
    let attached = device.find_repo(&name)?.is_some();
    if !attached {
        match device.execute(VcsRequest::CreateRepo { name: name.clone() }) {
            VcsResponse::Repo { .. } => {}
            VcsResponse::Error { error } => anyhow::bail!("{}: {}", error.code, error.message),
            other => anyhow::bail!("unexpected response: {other:?}"),
        }
    }
    std::fs::write(&config_path, config.render())?;
    if attached {
        println!(
            "Attached to repo {name} and wrote {}; `luv restore` writes its files.",
            config_path.display()
        );
    } else {
        println!("Created repo {name} and {}", config_path.display());
    }
    if config.include.is_empty() {
        println!("include is empty: edit {CONFIG_FILE} to allowlist the files to track.");
    }
    Ok(())
}

/// The directory whose `.luv` holds the store: the nearest ancestor with a
/// `Luv.eure` or a `.luv`, else the current directory.
fn store_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(CONFIG_FILE).is_file() || dir.join(".luv").is_dir())
        .unwrap_or(cwd)
        .to_path_buf()
}

fn remote_command(cwd: &Path, command: RemoteCommands) -> anyhow::Result<()> {
    let root = store_root(cwd);
    match command {
        RemoteCommands::Set { path, command } => {
            let config = luvcs_protocol::RemoteConfig { path, command };
            remote_config::save(&root, &config)?;
            println!("remote set");
            Ok(())
        }
        RemoteCommands::Show => {
            match remote_config::load(&root)? {
                Some(config) => print!("{}", remote_config::render(&config)),
                None => println!("no remote"),
            }
            Ok(())
        }
    }
}

fn pull(cwd: &Path) -> anyhow::Result<()> {
    let root = store_root(cwd);
    let connected = remote_config::connect(&root)?;
    let device = Local::open(&root)?;
    let report = device.pull(connected.remote.as_ref(), &connected.name)?;
    if report.entries == 0 && report.blobs == 0 {
        println!("up to date");
        return Ok(());
    }
    for repo in &report.repos {
        println!("new repo {repo}");
    }
    println!(
        "took in {} entries: {} objects, {} blobs",
        report.entries, report.objects, report.blobs
    );
    println!("Your files are unchanged; see `luv log` for the heads.");
    Ok(())
}

fn serve(path: &Path) -> anyhow::Result<()> {
    let remote = luvcs::remote::DirRemote::open(path)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    luvcs::remote::serve(&remote, "luv-dir", stdin.lock(), stdout.lock())?;
    Ok(())
}

struct WorkTree {
    root: PathBuf,
    config: LuvConfig,
    matcher: GlobSet,
}

impl WorkTree {
    /// The nearest directory at or above `start` that holds `Luv.eure`.
    fn discover(start: &Path) -> anyhow::Result<Self> {
        for dir in start.ancestors() {
            let path = dir.join(CONFIG_FILE);
            if path.is_file() {
                let config = LuvConfig::parse(&std::fs::read_to_string(&path)?, &path)?;
                let matcher = config.matcher()?;
                return Ok(Self {
                    root: dir.to_path_buf(),
                    config,
                    matcher,
                });
            }
        }
        anyhow::bail!("no {CONFIG_FILE} here or above: run `luv init` first")
    }

    fn allows(&self, rel: &str) -> bool {
        is_safe_rel(rel) && self.matcher.is_match(rel)
    }

    /// The allowlisted files on disk: relative path → bytes. Symlinks are
    /// never followed (they are reported and skipped).
    fn scan(&self) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
        let mut found = BTreeMap::new();
        let mut folded: BTreeMap<String, String> = BTreeMap::new();
        let mut stack = vec![(self.root.clone(), String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let name = entry.file_name().into_string().map_err(|n| {
                    anyhow::anyhow!("{} has a name that is not UTF-8: {n:?}", dir.display())
                })?;
                if is_reserved_name(&name) {
                    continue;
                }
                let rel = format!("{prefix}{name}");
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    if self.allows(&rel) {
                        eprintln!("skipping symlink {rel}: luv does not follow symlinks");
                    }
                    continue;
                }
                if kind.is_dir() {
                    stack.push((entry.path(), format!("{rel}/")));
                } else if kind.is_file() && self.allows(&rel) {
                    if let Some(other) = folded.insert(rel.to_lowercase(), rel.clone()) {
                        anyhow::bail!(
                            "{other} and {rel} differ only in case; they would collide on a case-insensitive host"
                        );
                    }
                    found.insert(rel, std::fs::read(entry.path())?);
                }
            }
        }
        Ok(found)
    }

    /// Writes `rel` under the work tree root. A title comes from repo
    /// content (possibly fetched from someone else), so it must stay inside
    /// the root: no absolute path, no `..`, and no symlinked directory on the way.
    fn write(&self, rel: &str, bytes: &[u8]) -> anyhow::Result<()> {
        if !is_safe_rel(rel) {
            anyhow::bail!("{rel:?} is not a safe relative path; refusing to write it");
        }
        let mut path = self.root.clone();
        let parts: Vec<&str> = rel.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            path.push(part);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    anyhow::bail!(
                        "{rel}: {} is a symlink; refusing to write through it",
                        path.display()
                    )
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if i + 1 < parts.len() {
                        std::fs::create_dir(&path)?;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        std::fs::write(&path, bytes)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Added,
    Modified,
    Removed,
}

impl Kind {
    fn letter(self) -> char {
        match self {
            Kind::Added => 'A',
            Kind::Modified => 'M',
            Kind::Removed => 'D',
        }
    }
}

struct Change {
    path: String,
    kind: Kind,
    /// Existing object (None for an added file).
    object: Option<VcsObjectView>,
    /// Bytes on disk (None for a removed file).
    disk: Option<Vec<u8>>,
}

struct Ctx<'a> {
    device: &'a Local,
    tree: &'a WorkTree,
    repo_id: String,
}

fn text_of(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    (!text.contains('\0')).then_some(text)
}

impl Ctx<'_> {
    fn call(&self, request: VcsRequest) -> anyhow::Result<VcsResponse> {
        match self.device.execute(request) {
            VcsResponse::Error { error } => anyhow::bail!("{}: {}", error.code, error.message),
            other => Ok(other),
        }
    }

    /// The saved (working tree) objects whose titles the allowlist covers.
    fn saved(&self) -> anyhow::Result<BTreeMap<String, VcsObjectView>> {
        let VcsResponse::Objects { objects } = self.call(VcsRequest::ListObjects {
            repo_id: self.repo_id.clone(),
            commit_id: None,
        })?
        else {
            anyhow::bail!("unexpected response to list");
        };
        let mut by_path = BTreeMap::new();
        for object in objects {
            let Some(title) = object.title.clone() else {
                continue;
            };
            if object.body_kind == "deleted" || !self.tree.allows(&title) {
                continue;
            }
            if by_path.insert(title.clone(), object).is_some() {
                anyhow::bail!("two objects of the repo are titled {title:?}");
            }
        }
        Ok(by_path)
    }

    fn same_content(&self, object: &VcsObjectView, bytes: &[u8]) -> anyhow::Result<bool> {
        Ok(match object.body_kind.as_str() {
            "text" => text_of(bytes).is_some_and(|t| object.text.as_deref() == Some(t)),
            "binary" => match &object.blob {
                Some(blob) if blob.byte_length == bytes.len() as u64 && blob.present => {
                    self.read_blob(&blob.blob_id)? == bytes
                }
                _ => false,
            },
            _ => false,
        })
    }

    fn read_blob(&self, blob_id: &str) -> anyhow::Result<Vec<u8>> {
        let VcsResponse::BlobBytes { data_base64, .. } = self.call(VcsRequest::ReadBlob {
            repo_id: self.repo_id.clone(),
            blob_id: blob_id.to_owned(),
            offset: None,
            length: None,
        })?
        else {
            anyhow::bail!("unexpected response to blob read");
        };
        Ok(base64::engine::general_purpose::STANDARD.decode(data_base64)?)
    }

    /// Disk against the saved state, limited to `only` when non-empty.
    fn changes(&self, only: &[String]) -> anyhow::Result<Vec<Change>> {
        let only: BTreeSet<String> = only.iter().map(|p| normalize(p)).collect();
        let wanted = |path: &str| only.is_empty() || only.contains(path);
        for path in &only {
            if !self.tree.allows(path) {
                anyhow::bail!("{path} is not allowlisted in {CONFIG_FILE}");
            }
        }
        let disk = self.tree.scan()?;
        let mut saved = self.saved()?;
        let mut changes = Vec::new();
        for (path, bytes) in disk {
            if !wanted(&path) {
                continue;
            }
            match saved.remove(&path) {
                None => changes.push(Change {
                    path,
                    kind: Kind::Added,
                    object: None,
                    disk: Some(bytes),
                }),
                Some(object) => {
                    if !self.same_content(&object, &bytes)? {
                        changes.push(Change {
                            path,
                            kind: Kind::Modified,
                            object: Some(object),
                            disk: Some(bytes),
                        });
                    }
                }
            }
        }
        for (path, object) in saved {
            if wanted(&path) {
                changes.push(Change {
                    path,
                    kind: Kind::Removed,
                    object: Some(object),
                    disk: None,
                });
            }
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    fn status(&self) -> anyhow::Result<()> {
        let VcsResponse::Status { status } = self.call(VcsRequest::Status {
            repo_id: self.repo_id.clone(),
        })?
        else {
            anyhow::bail!("unexpected response to status");
        };
        println!(
            "repo {} @ {}",
            self.tree.config.repo,
            status.base_commit_id.as_deref().unwrap_or("(no commits)")
        );
        if self.tree.config.include.is_empty() {
            println!("include is empty in {CONFIG_FILE}: nothing is tracked");
        }
        let changes = self.changes(&[])?;
        for change in &changes {
            println!("{} {}", change.kind.letter(), change.path);
        }
        for conflict in &status.conflicts {
            println!("C {}", conflict.object_id);
        }
        if changes.is_empty() && status.conflicts.is_empty() {
            println!("nothing to commit");
        }
        Ok(())
    }

    fn diff(&self, paths: &[String]) -> anyhow::Result<()> {
        for change in self.changes(paths)? {
            let old = change.object.as_ref().and_then(|o| o.text.clone());
            let new = change.disk.as_deref().and_then(text_of).map(str::to_owned);
            println!("--- {}\n+++ {}", change.path, change.path);
            match (&change.object, &change.disk) {
                (Some(o), _) if o.body_kind == "binary" => println!("(binary file changed)"),
                (_, Some(bytes)) if text_of(bytes).is_none() => println!("(binary file changed)"),
                _ => {
                    let text = similar::TextDiff::from_lines(
                        old.as_deref().unwrap_or(""),
                        new.as_deref().unwrap_or(""),
                    );
                    print!("{}", text.unified_diff().context_radius(3));
                }
            }
        }
        Ok(())
    }

    fn commit(&self, message: &str, paths: &[String]) -> anyhow::Result<()> {
        let changes = self.changes(paths)?;
        let mut selected = Vec::new();
        for change in &changes {
            let object_id = match (&change.kind, &change.disk) {
                (Kind::Removed, _) => {
                    let id = change
                        .object
                        .as_ref()
                        .expect("removed has an object")
                        .object_id
                        .clone();
                    self.call(VcsRequest::DeleteObject {
                        repo_id: self.repo_id.clone(),
                        object_id: id.clone(),
                    })?;
                    id
                }
                (_, Some(bytes)) => self.save(change, bytes)?,
                (_, None) => unreachable!("added or modified has bytes"),
            };
            selected.push(object_id);
        }
        // With paths, record only those objects; without, the whole working
        // tree (which includes anything saved outside the allowlist).
        let objects = (!paths.is_empty()).then_some(selected);
        let VcsResponse::Commit { commit } = self.call(VcsRequest::Record {
            repo_id: self.repo_id.clone(),
            description: message.to_owned(),
            objects,
        })?
        else {
            anyhow::bail!("unexpected response to commit");
        };
        println!("[{}] {message}", commit.commit_id);
        for change in &changes {
            println!("{} {}", change.kind.letter(), change.path);
        }
        Ok(())
    }

    fn save(&self, change: &Change, bytes: &[u8]) -> anyhow::Result<String> {
        let object_id = change.object.as_ref().map(|o| o.object_id.clone());
        let title = Some(change.path.clone());
        let response = match text_of(bytes) {
            Some(text) => self.call(VcsRequest::SaveText {
                repo_id: self.repo_id.clone(),
                object_id,
                type_id: FILE_TYPE.to_owned(),
                text: text.to_owned(),
                title,
            })?,
            None => self.call(VcsRequest::SaveBinary {
                repo_id: self.repo_id.clone(),
                object_id,
                type_id: FILE_TYPE.to_owned(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                title,
            })?,
        };
        match response {
            VcsResponse::Object { object } => Ok(object.object_id),
            other => anyhow::bail!("unexpected response to save: {other:?}"),
        }
    }

    fn push(&self) -> anyhow::Result<()> {
        let connected = remote_config::connect(&self.tree.root)?;
        let report = self
            .device
            .push(connected.remote.as_ref(), &connected.name, &self.repo_id)?;
        println!(
            "pushed {} objects, {} blobs{}",
            report.objects,
            report.blobs,
            if report.heads_announced {
                ", heads announced"
            } else {
                ""
            }
        );
        Ok(())
    }

    fn log(&self, limit: u32) -> anyhow::Result<()> {
        let VcsResponse::Commits { commits } = self.call(VcsRequest::Log {
            repo_id: self.repo_id.clone(),
            from_commit_id: None,
            limit,
        })?
        else {
            anyhow::bail!("unexpected response to log");
        };
        for commit in commits {
            println!(
                "{}{} {}",
                commit.commit_id,
                if commit.is_head { " (head)" } else { "" },
                commit.description
            );
        }
        Ok(())
    }

    fn restore(&self, commit: Option<String>, paths: &[String]) -> anyhow::Result<()> {
        let only: BTreeSet<String> = paths.iter().map(|p| normalize(p)).collect();
        for path in &only {
            if !self.tree.allows(path) {
                anyhow::bail!("{path} is not allowlisted in {CONFIG_FILE}");
            }
        }
        let wanted = |path: &str| only.is_empty() || only.contains(path);
        // Titles of the objects at the source: the commit, or the saved state.
        let VcsResponse::Objects { objects } = self.call(VcsRequest::ListObjects {
            repo_id: self.repo_id.clone(),
            commit_id: commit.clone(),
        })?
        else {
            anyhow::bail!("unexpected response to list");
        };
        let mut restored = 0;
        for object in objects {
            let Some(title) = object.title.clone() else {
                continue;
            };
            if object.body_kind == "deleted" || !self.tree.allows(&title) || !wanted(&title) {
                continue;
            }
            if let Some(commit_id) = &commit {
                self.call(VcsRequest::Restore {
                    repo_id: self.repo_id.clone(),
                    commit_id: commit_id.clone(),
                    object_id: object.object_id.clone(),
                })?;
            }
            let bytes = match object.body_kind.as_str() {
                "text" => object.text.clone().unwrap_or_default().into_bytes(),
                "binary" => {
                    let blob = object
                        .blob
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("{title}: binary object has no blob"))?;
                    if !blob.present {
                        anyhow::bail!(
                            "{title}: the bytes are not on this device yet (fetch first)"
                        );
                    }
                    self.read_blob(&blob.blob_id)?
                }
                other => anyhow::bail!("{title}: cannot restore a {other} object"),
            };
            self.tree.write(&title, &bytes)?;
            println!("restored {title}");
            restored += 1;
        }
        if restored == 0 {
            println!("nothing to restore");
        }
        Ok(())
    }
}

/// Whether `rel` is a plain `/`-separated path below the work tree root.
fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.contains('\\')
        && !rel.contains('\0')
        && !Path::new(rel).is_absolute()
        && !rel.starts_with('/')
        && rel.split('/').all(|c| {
            !c.is_empty() && c != "." && c != ".." && !c.contains(':') && !is_reserved_name(c)
        })
}

/// Names that belong to luv or to another VCS, at any depth: `.luv` holds the
/// store and the remote (which may name a command), and `.git` / `.jj` can
/// hold hooks. Repo content, which may come from someone else, must never be
/// able to write them, and they are never tracked. Compared case-insensitively
/// because the work tree may sit on a case-insensitive file system.
fn is_reserved_name(name: &str) -> bool {
    [".luv", ".git", ".jj"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// A path argument as the allowlist sees it: `/`-separated, no `./`.
fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches("./").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(include: &[&str]) -> WorkTree {
        let config = LuvConfig {
            repo: "r".into(),
            include: include.iter().map(|s| s.to_string()).collect(),
        };
        let matcher = config.matcher().unwrap();
        WorkTree {
            root: PathBuf::new(),
            config,
            matcher,
        }
    }

    #[test]
    fn reserved_names_are_never_allowed_at_any_depth() {
        let t = tree(&["**"]);
        for path in [
            ".luv/remote.eure",
            ".luv/store.sqlite",
            "sub/.luv/remote.eure",
            ".git/hooks/pre-commit",
            "a/.GIT/config",
            ".Jj/x",
            ".luv",
        ] {
            assert!(!t.allows(path), "{path} must not be allowed");
        }
        assert!(t.allows("notes/a.md"));
        assert!(t.allows(".gitignore"), "only the exact names are reserved");
    }

    #[test]
    fn write_refuses_reserved_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = tree(&["**"]);
        t.root = dir.path().to_path_buf();
        for path in [".luv/remote.eure", ".git/hooks/pre-commit", "x/.jj/y"] {
            assert!(t.write(path, b"x").is_err(), "{path}");
        }
        assert!(!dir.path().join(".luv").exists());
        t.write("ok/a.md", b"x").unwrap();
    }

    #[test]
    fn allowlist_matches_only_listed_globs() {
        let t = tree(&["*.md", "docs/**/*.txt"]);
        assert!(t.allows("a.md"));
        assert!(t.allows("docs/x/y.txt"));
        assert!(!t.allows("sub/a.md"), "* does not cross directories");
        assert!(!t.allows("a.rs"));
        assert!(
            !tree(&[]).allows("a.md"),
            "an empty allowlist tracks nothing"
        );
    }

    #[test]
    fn unsafe_titles_are_never_allowed() {
        let t = tree(&["**/*.md", "**"]);
        for bad in [
            "../x.md",
            "a/../../x.md",
            "/etc/x.md",
            "a//b.md",
            "./a.md",
            "C:/x.md",
            "a\\b.md",
        ] {
            assert!(!t.allows(bad), "{bad}");
            assert!(t.write(bad, b"x").is_err(), "{bad}");
        }
        assert!(t.allows("a/b.md"));
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_symlinked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let mut t = tree(&["**/*.md"]);
        t.root = dir.path().to_path_buf();
        assert!(t.write("link/x.md", b"x").is_err());
        assert!(!outside.path().join("x.md").exists());
        t.write("ok/x.md", b"x").unwrap();
    }

    #[test]
    fn config_round_trips_through_eure() {
        let config = LuvConfig {
            repo: "notes".into(),
            include: vec!["**/*.md".into(), "img/*.png".into()],
        };
        let parsed = LuvConfig::parse(&config.render(), Path::new(CONFIG_FILE)).unwrap();
        assert_eq!(parsed.repo, "notes");
        assert_eq!(parsed.include, config.include);
        assert!(LuvConfig::parse("include = []\n", Path::new(CONFIG_FILE)).is_err());
    }

    #[test]
    fn text_detection_rejects_nul_and_invalid_utf8() {
        assert_eq!(text_of(b"hi"), Some("hi"));
        assert_eq!(text_of(b"a\0b"), None);
        assert_eq!(text_of(&[0xff, 0xfe]), None);
    }
}
