//! `luv bookmark …`: names for commits, over the request API. Every rule
//! (who wins, what conflicts) is in luvcs; this module finds bookmarks by
//! name, prints them, and never picks a candidate on its own.

use clap::Subcommand;
use luvcs::api::{
    VcsAdvance, VcsBookmarkLife, VcsBookmarkLifeKind, VcsBookmarkView, VcsBookmarks, VcsNewDefault,
    VcsRequest, VcsResponse,
};

use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum BookmarkCommands {
    /// List bookmarks: `*` marks the default, `@` the selected one
    List {
        /// Also list deleted and fused bookmarks
        #[arg(long)]
        all: bool,
    },
    /// Name a commit (default: the last commit)
    Create {
        name: String,
        /// cmt_…, or a bookmark name
        #[arg(long)]
        commit: Option<String>,
    },
    /// Point a bookmark at another commit (default: the last commit)
    Move {
        name: String,
        /// cmt_…, or a bookmark name
        #[arg(long)]
        commit: Option<String>,
    },
    /// Give a bookmark another name; it stays the same bookmark
    Rename { name: String, new_name: String },
    /// Delete a bookmark (the commits stay)
    Delete {
        name: String,
        /// The default from now on, when deleting the default
        #[arg(long, conflicts_with = "clear_default")]
        new_default: Option<String>,
        /// Leave the repo without a default, when deleting the default
        #[arg(long)]
        clear_default: bool,
    },
    /// Merge a bookmark into another, which carries on for it (the way to
    /// settle two bookmarks of the same name)
    Fuse {
        name: String,
        #[arg(long)]
        into: String,
    },
    /// Select the bookmark `luv commit` moves forward on this device
    Select {
        #[arg(required_unless_present = "none")]
        name: Option<String>,
        /// Select none
        #[arg(long, conflicts_with = "name")]
        none: bool,
    },
    /// Set the repo's default bookmark
    Default {
        #[arg(required_unless_present = "clear")]
        name: Option<String>,
        /// Leave the repo without a default
        #[arg(long, conflicts_with = "name")]
        clear: bool,
    },
    /// Settle a bookmark in conflict by choosing a value for each field in
    /// conflict (see `luv bookmark list`)
    Resolve {
        /// The bookmark (bmk_… when its name is in conflict)
        name: String,
        /// Where it points: cmt_…, or a bookmark name
        #[arg(long)]
        commit: Option<String>,
        /// What it is called
        #[arg(long)]
        rename: Option<String>,
        /// Keep it, against a concurrent delete or fuse
        #[arg(long, conflicts_with = "delete")]
        keep: bool,
        /// Delete it, against a concurrent edit
        #[arg(long)]
        delete: bool,
    },
}

fn live() -> VcsBookmarkLife {
    VcsBookmarkLife {
        kind: VcsBookmarkLifeKind::Live,
        fused_into: None,
    }
}

fn deleted() -> VcsBookmarkLife {
    VcsBookmarkLife {
        kind: VcsBookmarkLifeKind::Deleted,
        fused_into: None,
    }
}

/// Deleted or fused (not in conflict about it).
fn is_gone(b: &VcsBookmarkView) -> bool {
    b.life
        .as_ref()
        .is_some_and(|l| l.kind != VcsBookmarkLifeKind::Live)
}

fn label(b: &VcsBookmarkView) -> String {
    match &b.name {
        Some(name) => name.clone(),
        None => format!("{{{}}}", b.name_candidates.join(" | ")),
    }
}

/// One `luv bookmark list` line.
fn line(b: &VcsBookmarkView) -> String {
    let marks = format!(
        "{}{}",
        if b.is_default { '*' } else { ' ' },
        if b.is_selected { '@' } else { ' ' }
    );
    let target = match &b.target_commit_id {
        Some(c) => c.clone(),
        None => format!("{{{}}}", b.target_candidates.join(" | ")),
    };
    let mut notes = Vec::new();
    match &b.life {
        Some(l) if l.kind == VcsBookmarkLifeKind::Deleted => notes.push("deleted".to_owned()),
        Some(l) if l.kind == VcsBookmarkLifeKind::Fused => notes.push(format!(
            "fused into {}",
            l.fused_into.as_deref().unwrap_or("?")
        )),
        Some(_) => {}
        None => notes.push("deleted on one side, edited on the other".to_owned()),
    }
    if b.name.is_none() {
        notes.push("name in conflict".to_owned());
    }
    if b.target_commit_id.is_none() {
        notes.push("target in conflict".to_owned());
    }
    if b.name_shared {
        notes.push("another bookmark has this name".to_owned());
    }
    let notes = if notes.is_empty() {
        String::new()
    } else {
        format!("  ({})", notes.join("; "))
    };
    format!("{marks} {} -> {target}  {}{notes}", label(b), b.bookmark_id)
}

impl Ctx<'_> {
    pub(crate) fn bookmarks(&self) -> anyhow::Result<VcsBookmarks> {
        match self.call(VcsRequest::ListBookmarks {
            repo_id: self.repo_id.clone(),
        })? {
            VcsResponse::Bookmarks { bookmarks } => Ok(bookmarks),
            other => anyhow::bail!("unexpected response to bookmark list: {other:?}"),
        }
    }

    /// The one bookmark `name` means: a `bmk_…` ID, or a name no other
    /// bookmark that is not deleted or fused goes by. Never guesses.
    fn find_bookmark<'b>(
        &self,
        all: &'b VcsBookmarks,
        name: &str,
    ) -> anyhow::Result<&'b VcsBookmarkView> {
        if name.starts_with("bmk_") {
            return all
                .bookmarks
                .iter()
                .find(|b| b.bookmark_id == name)
                .ok_or_else(|| anyhow::anyhow!("no bookmark {name}"));
        }
        let found: Vec<&VcsBookmarkView> = all
            .bookmarks
            .iter()
            .filter(|b| !is_gone(b))
            .filter(|b| {
                b.name.as_deref() == Some(name) || b.name_candidates.iter().any(|n| n == name)
            })
            .collect();
        match found.as_slice() {
            [one] => Ok(one),
            [] => anyhow::bail!("no bookmark named {name:?}"),
            many => anyhow::bail!(
                "several bookmarks are named {name:?} ({}); use the bmk_… ID, and settle it with `luv bookmark rename` or `fuse`",
                many.iter()
                    .map(|b| b.bookmark_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// The commit `rev` names: a `cmt_…` as is, or where a bookmark points.
    pub(crate) fn rev(&self, rev: &str) -> anyhow::Result<String> {
        if rev.starts_with("cmt_") {
            return Ok(rev.to_owned());
        }
        let all = self.bookmarks()?;
        let b = self.find_bookmark(&all, rev)?;
        b.target_commit_id.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "{rev} points at several commits ({}); choose one with `luv bookmark resolve {rev} --commit …`",
                b.target_candidates.join(", ")
            )
        })
    }

    fn base_commit(&self) -> anyhow::Result<String> {
        let VcsResponse::Status { status } = self.call(VcsRequest::Status {
            repo_id: self.repo_id.clone(),
        })?
        else {
            anyhow::bail!("unexpected response to status");
        };
        status
            .base_commit_id
            .ok_or_else(|| anyhow::anyhow!("no commits yet: commit first, or pass --commit"))
    }

    fn commit_or_base(&self, commit: Option<String>) -> anyhow::Result<String> {
        match commit {
            Some(rev) => self.rev(&rev),
            None => self.base_commit(),
        }
    }

    /// The selected bookmark's name and token, for a commit that moves it.
    pub(crate) fn selected_bookmark(&self) -> anyhow::Result<Option<(String, String)>> {
        let all = self.bookmarks()?;
        Ok(all
            .bookmarks
            .iter()
            .find(|b| b.is_selected)
            .map(|b| (label(b), b.token.clone())))
    }

    pub(crate) fn report_advance(&self, name: &str, advance: VcsAdvance) {
        let why = match advance {
            VcsAdvance::Advanced => {
                println!("moved {name} forward");
                return;
            }
            VcsAdvance::NoSelection => return,
            VcsAdvance::Stale => "it changed meanwhile",
            VcsAdvance::Unresolved => {
                "it is in conflict, deleted or fused; `luv bookmark resolve` it, or stop moving it with `luv bookmark select --none`"
            }
            VcsAdvance::NotForward => "the commit does not descend from where it points",
        };
        println!("{name} stays where it was: {why} (see `luv bookmark list`)");
    }

    fn edit_bookmark(
        &self,
        b: &VcsBookmarkView,
        name: Option<String>,
        commit_id: Option<String>,
        life: Option<VcsBookmarkLife>,
        new_default: Option<VcsNewDefault>,
    ) -> anyhow::Result<VcsBookmarkView> {
        match self.call(VcsRequest::EditBookmark {
            repo_id: self.repo_id.clone(),
            bookmark_id: b.bookmark_id.clone(),
            token: b.token.clone(),
            name,
            commit_id,
            life,
            new_default,
        })? {
            VcsResponse::Bookmark { bookmark } => Ok(bookmark),
            other => anyhow::bail!("unexpected response to bookmark edit: {other:?}"),
        }
    }

    pub(crate) fn bookmark(&self, command: BookmarkCommands) -> anyhow::Result<()> {
        match command {
            BookmarkCommands::List { all } => {
                let listed = self.bookmarks()?;
                let mut shown = 0;
                for b in listed.bookmarks.iter().filter(|b| all || !is_gone(b)) {
                    println!("{}", line(b));
                    shown += 1;
                }
                if shown == 0 {
                    println!("no bookmarks");
                }
                if !listed.default_candidates.is_empty() {
                    let names: Vec<String> = listed
                        .default_candidates
                        .iter()
                        .map(|c| c.clone().unwrap_or_else(|| "(none)".into()))
                        .collect();
                    println!(
                        "the default is in conflict: {}; choose with `luv bookmark default`",
                        names.join(" | ")
                    );
                }
            }
            BookmarkCommands::Create { name, commit } => {
                let commit_id = self.commit_or_base(commit)?;
                let VcsResponse::Bookmark { bookmark } = self.call(VcsRequest::CreateBookmark {
                    repo_id: self.repo_id.clone(),
                    name,
                    commit_id,
                })?
                else {
                    anyhow::bail!("unexpected response to bookmark create");
                };
                println!("{}", line(&bookmark));
            }
            BookmarkCommands::Move { name, commit } => {
                let commit_id = self.commit_or_base(commit)?;
                let all = self.bookmarks()?;
                let b = self.find_bookmark(&all, &name)?;
                println!(
                    "{}",
                    line(&self.edit_bookmark(b, None, Some(commit_id), None, None)?)
                );
            }
            BookmarkCommands::Rename { name, new_name } => {
                let all = self.bookmarks()?;
                let b = self.find_bookmark(&all, &name)?;
                println!(
                    "{}",
                    line(&self.edit_bookmark(b, Some(new_name), None, None, None)?)
                );
            }
            BookmarkCommands::Delete {
                name,
                new_default,
                clear_default,
            } => {
                let all = self.bookmarks()?;
                let b = self.find_bookmark(&all, &name)?;
                let new_default = match (new_default, clear_default) {
                    (Some(d), _) => Some(VcsNewDefault::Bookmark {
                        bookmark_id: self.find_bookmark(&all, &d)?.bookmark_id.clone(),
                    }),
                    (None, true) => Some(VcsNewDefault::Unset),
                    (None, false) => None,
                };
                if new_default.is_none()
                    && all.default_bookmark_id.as_deref() == Some(b.bookmark_id.as_str())
                {
                    anyhow::bail!(
                        "{name} is the default; pass --new-default <name> or --clear-default"
                    );
                }
                self.edit_bookmark(b, None, None, Some(deleted()), new_default)?;
                println!("deleted {name}");
            }
            BookmarkCommands::Fuse { name, into } => {
                let all = self.bookmarks()?;
                let b = self.find_bookmark(&all, &name)?;
                let target = self.find_bookmark(&all, &into)?;
                let life = VcsBookmarkLife {
                    kind: VcsBookmarkLifeKind::Fused,
                    fused_into: Some(target.bookmark_id.clone()),
                };
                self.edit_bookmark(b, None, None, Some(life), None)?;
                println!("fused {name} into {into}");
            }
            BookmarkCommands::Select { name, .. } => {
                let bookmark_id = match name {
                    Some(n) => Some(
                        self.find_bookmark(&self.bookmarks()?, &n)?
                            .bookmark_id
                            .clone(),
                    ),
                    None => None,
                };
                self.call(VcsRequest::SelectBookmark {
                    repo_id: self.repo_id.clone(),
                    bookmark_id,
                })?;
                println!("selected");
            }
            BookmarkCommands::Default { name, .. } => {
                let all = self.bookmarks()?;
                let bookmark_id = match name {
                    Some(n) => Some(self.find_bookmark(&all, &n)?.bookmark_id.clone()),
                    None => None,
                };
                self.call(VcsRequest::SetDefaultBookmark {
                    repo_id: self.repo_id.clone(),
                    token: all.default_token.clone(),
                    bookmark_id,
                })?;
                println!("default set");
            }
            BookmarkCommands::Resolve {
                name,
                commit,
                rename,
                keep,
                delete,
            } => {
                let all = self.bookmarks()?;
                let b = self.find_bookmark(&all, &name)?;
                let life = match (keep, delete) {
                    (true, _) => Some(live()),
                    (_, true) => Some(deleted()),
                    _ => None,
                };
                let commit_id = commit.map(|c| self.rev(&c)).transpose()?;
                if life.is_none() && commit_id.is_none() && rename.is_none() {
                    anyhow::bail!("choose something: --commit, --rename, --keep or --delete");
                }
                println!(
                    "{}",
                    line(&self.edit_bookmark(b, rename, commit_id, life, None)?)
                );
            }
        }
        Ok(())
    }
}
