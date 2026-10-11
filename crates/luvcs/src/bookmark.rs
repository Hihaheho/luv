//! Bookmarks: names that point at commits, kept as a DAG of updates.
//!
//! A bookmark, like the repo's settings, is a small set of fields whose
//! history is a DAG of immutable [`BookmarkUpdate`]s. An update names the
//! updates it has seen (`predecessors`) and assigns some fields. The state
//! is a function of the set of updates alone, never of arrival order or of
//! which side is "left": for each field, the candidates are the latest
//! updates that assign it (no other update assigning it descends from
//! them). One distinct value is the field's value; more is a conflict that
//! keeps every candidate. Fields merge independently, so a rename and a
//! move made concurrently both land, while two moves conflict unless both
//! moved forward along one line of history (then the furthest one wins).
//!
//! Deletion is the `life` field: every edit assigns `live` and a delete
//! assigns `deleted`, so a delete concurrent with an edit is a conflict on
//! `life`, as deleting and editing an object is.

use std::collections::{BTreeMap, BTreeSet};

use crate::cbor::{self, Value};
use crate::digest::{Digest, DigestError};
use crate::id::{BookmarkId, BookmarkUpdateId, CommitId, Domain, IdKey, RepoId, keyed_id};
use crate::model::{
    Actor, Capabilities, CoreError, Fields, caps_value, check_support, validate_capabilities,
};

pub const BOOKMARK_UPDATE_FORMAT_V1: u32 = 1;
/// The capability every bookmark update requires.
pub const BOOKMARK_CAPABILITY_V1: &str = "bookmark.v1";
pub const MAX_PREDECESSORS: usize = 256;
pub const MAX_BOOKMARK_NAME_BYTES: usize = 255;

const KIND: &str = "bookmark_update";

/// What an update is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Subject {
    Bookmark(BookmarkId),
    /// The repo-wide settings (the default bookmark).
    Repo,
}

/// Whether a bookmark exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Life {
    Live,
    Deleted,
    /// Merged into another bookmark, which carries on for it.
    FusedInto(BookmarkId),
}

/// Where a bookmark points, and where it pointed when the writer moved it
/// (`None` when the writer saw no single target, as at creation or over a
/// conflict). `from` only decides whether a move went forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub commit: CommitId,
    pub from: Option<CommitId>,
}

/// The fields one update assigns; `None` leaves a field as it was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assigns {
    pub life: Option<Life>,
    pub name: Option<String>,
    pub target: Option<Target>,
    /// Repo settings only; `Some(None)` clears the default.
    pub default_bookmark: Option<Option<BookmarkId>>,
}

impl Assigns {
    fn is_empty(&self) -> bool {
        self.life.is_none()
            && self.name.is_none()
            && self.target.is_none()
            && self.default_bookmark.is_none()
    }
}

/// One immutable step in the history of a bookmark or of the repo settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkUpdate {
    pub format: u32,
    pub repo_id: RepoId,
    pub subject: Subject,
    /// The updates the writer had seen and supersedes: sorted, unique.
    pub predecessors: Vec<BookmarkUpdateId>,
    pub assigns: Assigns,
    pub author: Actor,
    pub created_at_unix_ms: i64,
    pub required_capabilities: Vec<String>,
}

/// Whether `name` can name a bookmark: 1 to 255 bytes, no control
/// characters, no leading or trailing whitespace.
pub fn validate_name(name: &str) -> Result<(), CoreError> {
    if name.is_empty() || name.len() > MAX_BOOKMARK_NAME_BYTES {
        return Err(CoreError::invalid(KIND, "name must be 1 to 255 bytes"));
    }
    if name.chars().any(char::is_control) {
        return Err(CoreError::invalid(
            KIND,
            "name contains a control character",
        ));
    }
    if name.trim() != name {
        return Err(CoreError::invalid(
            KIND,
            "name has leading or trailing whitespace",
        ));
    }
    Ok(())
}

impl BookmarkUpdate {
    /// A version-1 update.
    pub fn new(
        repo_id: RepoId,
        subject: Subject,
        predecessors: impl IntoIterator<Item = BookmarkUpdateId>,
        assigns: Assigns,
        author: Actor,
        created_at_unix_ms: i64,
    ) -> Self {
        let predecessors: BTreeSet<BookmarkUpdateId> = predecessors.into_iter().collect();
        Self {
            format: BOOKMARK_UPDATE_FORMAT_V1,
            repo_id,
            subject,
            predecessors: predecessors.into_iter().collect(),
            assigns,
            author,
            created_at_unix_ms,
            required_capabilities: vec![BOOKMARK_CAPABILITY_V1.to_owned()],
        }
    }

    pub fn to_cbor(&self) -> Value {
        let a = &self.assigns;
        let mut assigns = Vec::new();
        if let Some(life) = &a.life {
            let v = match life {
                Life::Live => Value::text("live"),
                Life::Deleted => Value::text("deleted"),
                Life::FusedInto(into) => Value::map([("fused_into", into.to_cbor())]),
            };
            assigns.push((Value::text("life"), v));
        }
        if let Some(name) = &a.name {
            assigns.push((Value::text("name"), Value::text(name.clone())));
        }
        if let Some(t) = &a.target {
            let v = Value::map([
                ("commit", t.commit.to_cbor()),
                (
                    "from",
                    t.from.as_ref().map_or(Value::Null, CommitId::to_cbor),
                ),
            ]);
            assigns.push((Value::text("target"), v));
        }
        if let Some(d) = &a.default_bookmark {
            let v = d.as_ref().map_or(Value::Null, BookmarkId::to_cbor);
            assigns.push((Value::text("default_bookmark"), v));
        }
        let subject = match &self.subject {
            Subject::Bookmark(id) => id.to_cbor(),
            Subject::Repo => Value::Null,
        };
        Value::map([
            ("format", Value::Uint(u64::from(self.format))),
            ("repo_id", self.repo_id.to_cbor()),
            ("bookmark_id", subject),
            (
                "predecessors",
                Value::Array(
                    self.predecessors
                        .iter()
                        .map(BookmarkUpdateId::to_cbor)
                        .collect(),
                ),
            ),
            ("assigns", Value::Map(assigns)),
            ("author", self.author.to_cbor()),
            ("created_at_unix_ms", Value::int(self.created_at_unix_ms)),
            (
                "required_capabilities",
                caps_value(&self.required_capabilities),
            ),
        ])
    }

    pub fn encode(&self) -> Result<Vec<u8>, CoreError> {
        self.validate()?;
        Ok(cbor::encode(&self.to_cbor()))
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.format != BOOKMARK_UPDATE_FORMAT_V1 {
            return Err(CoreError::invalid(KIND, "format is not 1"));
        }
        if self.predecessors.len() > MAX_PREDECESSORS {
            return Err(CoreError::invalid(KIND, "more than 256 predecessors"));
        }
        if self.predecessors.windows(2).any(|p| p[0] >= p[1]) {
            return Err(CoreError::invalid(
                KIND,
                "predecessors must be sorted and unique",
            ));
        }
        let a = &self.assigns;
        if a.is_empty() {
            return Err(CoreError::invalid(KIND, "assigns nothing"));
        }
        match self.subject {
            Subject::Bookmark(id) => {
                if a.default_bookmark.is_some() {
                    return Err(CoreError::invalid(
                        KIND,
                        "a bookmark has no default_bookmark",
                    ));
                }
                if a.life == Some(Life::FusedInto(id)) {
                    return Err(CoreError::invalid(KIND, "fused into itself"));
                }
                // The first update creates the bookmark whole.
                if self.predecessors.is_empty()
                    && (a.life != Some(Life::Live) || a.name.is_none() || a.target.is_none())
                {
                    return Err(CoreError::invalid(
                        KIND,
                        "a first update must assign live, name and target",
                    ));
                }
            }
            Subject::Repo => {
                if a.life.is_some() || a.name.is_some() || a.target.is_some() {
                    return Err(CoreError::invalid(
                        KIND,
                        "repo settings have only default_bookmark",
                    ));
                }
            }
        }
        if let Some(name) = &a.name {
            validate_name(name)?;
        }
        validate_capabilities(KIND, &self.required_capabilities)
    }

    pub fn decode(bytes: &[u8], caps: &Capabilities) -> Result<Self, CoreError> {
        Self::from_cbor(&cbor::decode(bytes)?, caps)
    }

    pub fn from_cbor(value: &Value, caps: &Capabilities) -> Result<Self, CoreError> {
        let f = Fields::new(
            KIND,
            value,
            &[
                "format",
                "repo_id",
                "bookmark_id",
                "predecessors",
                "assigns",
                "author",
                "created_at_unix_ms",
                "required_capabilities",
            ],
        )?;
        let format = f.u32("format")?;
        let required_capabilities = f.capabilities()?;
        check_support(
            KIND,
            format,
            BOOKMARK_UPDATE_FORMAT_V1,
            &required_capabilities,
            caps,
        )?;
        let subject = match f.get("bookmark_id")? {
            Value::Null => Subject::Repo,
            v => Subject::Bookmark(
                BookmarkId::from_cbor(v)
                    .ok_or_else(|| CoreError::invalid(KIND, "bookmark_id is not a valid id"))?,
            ),
        };
        let assigns = assigns_from_cbor(f.get("assigns")?)?;
        let update = Self {
            format,
            repo_id: f.id("repo_id", RepoId::from_cbor)?,
            subject,
            predecessors: f.ids("predecessors", BookmarkUpdateId::from_cbor)?,
            assigns,
            author: Actor::from_cbor(f.get("author")?)?,
            created_at_unix_ms: f.i64("created_at_unix_ms")?,
            required_capabilities,
        };
        update.validate()?;
        Ok(update)
    }

    pub fn id<D: Digest>(&self, digest: &D, key: &IdKey) -> Result<BookmarkUpdateId, DigestError> {
        Ok(BookmarkUpdateId(keyed_id(
            digest,
            key,
            Domain::BookmarkUpdate,
            self.format,
            self.to_cbor(),
        )?))
    }
}

fn assigns_from_cbor(value: &Value) -> Result<Assigns, CoreError> {
    const AKIND: &str = "bookmark_update.assigns";
    // Only to refuse unknown keys: every key here is optional, so the
    // fields are read with `Value::get` below.
    Fields::new(
        AKIND,
        value,
        &["life", "name", "target", "default_bookmark"],
    )?;
    let bad = |what: &str| CoreError::invalid(AKIND, format!("{what} is malformed"));
    let life = value
        .get("life")
        .map(|v| match v.as_text() {
            Some("live") => Ok(Life::Live),
            Some("deleted") => Ok(Life::Deleted),
            Some(_) => Err(bad("life")),
            None => {
                let f = Fields::new(AKIND, v, &["fused_into"])?;
                Ok(Life::FusedInto(f.id("fused_into", BookmarkId::from_cbor)?))
            }
        })
        .transpose()?;
    let name = value
        .get("name")
        .map(|v| v.as_text().map(str::to_owned).ok_or_else(|| bad("name")))
        .transpose()?;
    let target = value
        .get("target")
        .map(|v| -> Result<Target, CoreError> {
            let f = Fields::new(AKIND, v, &["commit", "from"])?;
            Ok(Target {
                commit: f.id("commit", CommitId::from_cbor)?,
                from: match f.get("from")? {
                    Value::Null => None,
                    v => Some(CommitId::from_cbor(v).ok_or_else(|| bad("target.from"))?),
                },
            })
        })
        .transpose()?;
    let default_bookmark = value
        .get("default_bookmark")
        .map(|v| match v {
            Value::Null => Ok(None),
            v => BookmarkId::from_cbor(v)
                .map(Some)
                .ok_or_else(|| bad("default_bookmark")),
        })
        .transpose()?;
    Ok(Assigns {
        life,
        name,
        target,
        default_bookmark,
    })
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// One field of a resolved subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    /// No update assigns it (repo settings never set).
    Unset,
    Value(T),
    /// Two or more distinct values, in canonical order, each with the
    /// latest updates that assigned it.
    Conflict(Vec<Candidate<T>>),
}

impl<T> Field<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Field::Value(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_conflict(&self) -> bool {
        matches!(self, Field::Conflict(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate<T> {
    pub value: T,
    pub updates: Vec<BookmarkUpdateId>,
}

/// A bookmark or the repo settings as its updates leave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectState {
    /// The updates no other update supersedes, sorted. A writer names them
    /// as its predecessors, and a host can compare them to tell whether a
    /// view is still current.
    pub heads: Vec<BookmarkUpdateId>,
    pub life: Field<Life>,
    pub name: Field<String>,
    pub target: Field<CommitId>,
    pub default_bookmark: Field<Option<BookmarkId>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError<E> {
    #[error("update {0} names predecessor {1}, which is missing")]
    MissingPredecessor(BookmarkUpdateId, BookmarkUpdateId),
    #[error("updates of different subjects or repos were resolved together")]
    MixedSubjects,
    #[error("{0}")]
    Ancestry(E),
}

/// The state of one subject from all of its updates. `is_ancestor(a, b)`
/// tells whether commit `a` is `b` or an ancestor of it; it is asked only
/// to decide whether concurrent moves all went forward.
pub fn resolve<E>(
    updates: &BTreeMap<BookmarkUpdateId, BookmarkUpdate>,
    mut is_ancestor: impl FnMut(CommitId, CommitId) -> Result<bool, E>,
) -> Result<SubjectState, ResolveError<E>> {
    let mut first: Option<(RepoId, Subject)> = None;
    let mut superseded = BTreeSet::new();
    for (id, u) in updates {
        match first {
            None => first = Some((u.repo_id, u.subject)),
            Some(s) if s != (u.repo_id, u.subject) => return Err(ResolveError::MixedSubjects),
            Some(_) => {}
        }
        for p in &u.predecessors {
            if !updates.contains_key(p) {
                return Err(ResolveError::MissingPredecessor(*id, *p));
            }
            superseded.insert(*p);
        }
    }
    let heads: Vec<BookmarkUpdateId> = updates
        .keys()
        .filter(|id| !superseded.contains(id))
        .copied()
        .collect();
    let dag = Dag {
        updates,
        heads: &heads,
    };

    let life = group(dag.latest(|a| a.life.is_some()), updates, |a| a.life);
    let name = group(dag.latest(|a| a.name.is_some()), updates, |a| {
        a.name.clone()
    });
    let default_bookmark = group(dag.latest(|a| a.default_bookmark.is_some()), updates, |a| {
        a.default_bookmark
    });
    let target_writers = dag.latest(|a| a.target.is_some());
    let mut target = group(target_writers.clone(), updates, |a| {
        a.target.map(|t| t.commit)
    });
    if let Field::Conflict(candidates) = &target
        && let Some(tip) = forward_tip(&target_writers, candidates, updates, &mut is_ancestor)
            .map_err(ResolveError::Ancestry)?
    {
        target = Field::Value(tip);
    }
    Ok(SubjectState {
        heads,
        life,
        name,
        target,
        default_bookmark,
    })
}

struct Dag<'a> {
    updates: &'a BTreeMap<BookmarkUpdateId, BookmarkUpdate>,
    heads: &'a [BookmarkUpdateId],
}

impl Dag<'_> {
    /// The latest updates that `assigns` a field: reachable from the heads
    /// without passing another update that assigns it, and not an ancestor
    /// of another such update.
    fn latest(&self, assigns: impl Fn(&Assigns) -> bool) -> Vec<BookmarkUpdateId> {
        let mut found = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<BookmarkUpdateId> = self.heads.to_vec();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let u = &self.updates[&id];
            if assigns(&u.assigns) {
                found.insert(id);
            } else {
                stack.extend(u.predecessors.iter().copied());
            }
        }
        // A writer found along one path may still be behind another.
        let mut behind = BTreeSet::new();
        for id in &found {
            self.ancestors_into(*id, &mut behind);
        }
        found.difference(&behind).copied().collect()
    }

    /// Every strict ancestor of `id`, added to `out`.
    fn ancestors_into(&self, id: BookmarkUpdateId, out: &mut BTreeSet<BookmarkUpdateId>) {
        let mut stack: Vec<BookmarkUpdateId> = self.updates[&id].predecessors.clone();
        while let Some(p) = stack.pop() {
            if out.insert(p) {
                stack.extend(self.updates[&p].predecessors.iter().copied());
            }
        }
    }
}

/// Groups the writers' values: none is `Unset`, one distinct value is the
/// value, more is a conflict in the value's order.
fn group<T: Ord + Clone>(
    writers: Vec<BookmarkUpdateId>,
    updates: &BTreeMap<BookmarkUpdateId, BookmarkUpdate>,
    value: impl Fn(&Assigns) -> Option<T>,
) -> Field<T> {
    let mut by_value: BTreeMap<T, Vec<BookmarkUpdateId>> = BTreeMap::new();
    for id in writers {
        if let Some(v) = value(&updates[&id].assigns) {
            by_value.entry(v).or_default().push(id);
        }
    }
    match by_value.len() {
        0 => Field::Unset,
        1 => Field::Value(by_value.into_keys().next().expect("one entry")),
        _ => Field::Conflict(
            by_value
                .into_iter()
                .map(|(value, updates)| Candidate { value, updates })
                .collect(),
        ),
    }
}

/// When every concurrent move went forward from where its writer saw the
/// bookmark and the targets lie on one line of history, the furthest
/// target: nobody meant to go back, so the moves agree.
fn forward_tip<E>(
    writers: &[BookmarkUpdateId],
    candidates: &[Candidate<CommitId>],
    updates: &BTreeMap<BookmarkUpdateId, BookmarkUpdate>,
    is_ancestor: &mut impl FnMut(CommitId, CommitId) -> Result<bool, E>,
) -> Result<Option<CommitId>, E> {
    for id in writers {
        let Some(t) = updates[id].assigns.target else {
            continue;
        };
        let Some(from) = t.from else {
            return Ok(None);
        };
        if !is_ancestor(from, t.commit)? {
            return Ok(None);
        }
    }
    'tip: for tip in candidates {
        for other in candidates {
            if other.value != tip.value && !is_ancestor(other.value, tip.value)? {
                continue 'tip;
            }
        }
        return Ok(Some(tip.value));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::RustDigest;
    use crate::model::ActorKind;

    const REPO: RepoId = RepoId([1; 16]);
    const B: BookmarkId = BookmarkId([2; 16]);
    const OTHER: BookmarkId = BookmarkId([3; 16]);

    fn c(n: u8) -> CommitId {
        CommitId([n; 32])
    }

    /// History: 1 ← 2 ← 3, and 1 ← 20 (a side line).
    fn is_ancestor(a: CommitId, b: CommitId) -> Result<bool, ()> {
        let line = |x: u8| {
            if x >= 20 {
                vec![1, x]
            } else {
                (1..=x).collect()
            }
        };
        Ok(line(b.0[0]).contains(&a.0[0]))
    }

    #[derive(Default)]
    struct History {
        updates: BTreeMap<BookmarkUpdateId, BookmarkUpdate>,
        clock: i64,
    }

    impl History {
        fn add(
            &mut self,
            subject: Subject,
            preds: &[BookmarkUpdateId],
            a: Assigns,
        ) -> BookmarkUpdateId {
            self.clock += 1;
            let u = BookmarkUpdate::new(
                REPO,
                subject,
                preds.iter().copied(),
                a,
                Actor {
                    kind: ActorKind::User,
                    device_id: "d".into(),
                    description: String::new(),
                },
                self.clock,
            );
            let bytes = u.encode().unwrap();
            assert_eq!(
                BookmarkUpdate::decode(&bytes, &Capabilities::v1()).unwrap(),
                u
            );
            let id = u.id(&RustDigest, &IdKey([9; 32])).unwrap();
            self.updates.insert(id, u);
            id
        }

        fn create(&mut self, name: &str, at: u8) -> BookmarkUpdateId {
            self.add(
                Subject::Bookmark(B),
                &[],
                Assigns {
                    life: Some(Life::Live),
                    name: Some(name.into()),
                    target: Some(Target {
                        commit: c(at),
                        from: None,
                    }),
                    ..Assigns::default()
                },
            )
        }

        fn rename(&mut self, preds: &[BookmarkUpdateId], name: &str) -> BookmarkUpdateId {
            self.add(
                Subject::Bookmark(B),
                preds,
                Assigns {
                    life: Some(Life::Live),
                    name: Some(name.into()),
                    ..Assigns::default()
                },
            )
        }

        fn move_(&mut self, preds: &[BookmarkUpdateId], from: u8, to: u8) -> BookmarkUpdateId {
            self.add(
                Subject::Bookmark(B),
                preds,
                Assigns {
                    life: Some(Life::Live),
                    target: Some(Target {
                        commit: c(to),
                        from: Some(c(from)),
                    }),
                    ..Assigns::default()
                },
            )
        }

        fn delete(&mut self, preds: &[BookmarkUpdateId]) -> BookmarkUpdateId {
            self.add(
                Subject::Bookmark(B),
                preds,
                Assigns {
                    life: Some(Life::Deleted),
                    ..Assigns::default()
                },
            )
        }

        fn state(&self) -> SubjectState {
            resolve(&self.updates, is_ancestor).unwrap()
        }
    }

    #[test]
    fn a_created_bookmark_has_its_fields() {
        let mut h = History::default();
        let u = h.create("main", 1);
        let s = h.state();
        assert_eq!(s.heads, vec![u]);
        assert_eq!(s.life, Field::Value(Life::Live));
        assert_eq!(s.name, Field::Value("main".to_owned()));
        assert_eq!(s.target, Field::Value(c(1)));
        assert_eq!(s.default_bookmark, Field::Unset);
    }

    #[test]
    fn concurrent_rename_and_move_both_land() {
        let mut h = History::default();
        let root = h.create("main", 1);
        let r = h.rename(&[root], "trunk");
        let m = h.move_(&[root], 1, 2);
        let s = h.state();
        assert_eq!(
            s.heads,
            BTreeSet::from([r, m]).into_iter().collect::<Vec<_>>()
        );
        assert_eq!(s.name, Field::Value("trunk".to_owned()));
        assert_eq!(s.target, Field::Value(c(2)));
        assert_eq!(s.life, Field::Value(Life::Live));
    }

    #[test]
    fn diverging_moves_conflict_and_a_later_rename_keeps_the_conflict() {
        let mut h = History::default();
        let root = h.create("main", 1);
        let a = h.move_(&[root], 1, 2);
        let b = h.move_(&[root], 1, 20);
        let s = h.state();
        assert_eq!(
            s.target,
            Field::Conflict(vec![
                Candidate {
                    value: c(2),
                    updates: vec![a]
                },
                Candidate {
                    value: c(20),
                    updates: vec![b]
                },
            ])
        );
        // A rename that has seen both does not decide where it points.
        h.rename(&[a, b], "trunk");
        let s = h.state();
        assert_eq!(s.name, Field::Value("trunk".to_owned()));
        assert!(s.target.is_conflict());
        // Choosing a target does.
        let heads = s.heads.clone();
        h.add(
            Subject::Bookmark(B),
            &heads,
            Assigns {
                life: Some(Life::Live),
                target: Some(Target {
                    commit: c(20),
                    from: None,
                }),
                ..Assigns::default()
            },
        );
        assert_eq!(h.state().target, Field::Value(c(20)));
    }

    #[test]
    fn forward_moves_on_one_line_take_the_furthest() {
        let mut h = History::default();
        let root = h.create("main", 1);
        h.move_(&[root], 1, 2);
        h.move_(&[root], 1, 3);
        assert_eq!(h.state().target, Field::Value(c(3)));
    }

    #[test]
    fn a_move_back_concurrent_with_a_move_forward_conflicts() {
        let mut h = History::default();
        let root = h.create("main", 2);
        h.move_(&[root], 2, 3);
        h.move_(&[root], 2, 1);
        assert!(h.state().target.is_conflict());
    }

    #[test]
    fn delete_concurrent_with_rename_conflicts_on_life() {
        let mut h = History::default();
        let root = h.create("main", 1);
        let r = h.rename(&[root], "trunk");
        let d = h.delete(&[root]);
        let s = h.state();
        assert_eq!(
            s.life,
            Field::Conflict(vec![
                Candidate {
                    value: Life::Live,
                    updates: vec![r]
                },
                Candidate {
                    value: Life::Deleted,
                    updates: vec![d]
                },
            ])
        );
        assert_eq!(s.name, Field::Value("trunk".to_owned()));
        assert_eq!(s.target, Field::Value(c(1)));
    }

    #[test]
    fn fusing_is_a_life_value_that_round_trips_and_conflicts_with_an_edit() {
        let mut h = History::default();
        let root = h.create("main", 1);
        // `add` encodes and decodes every update, `fused_into` included.
        let fused = h.add(
            Subject::Bookmark(B),
            &[root],
            Assigns {
                life: Some(Life::FusedInto(OTHER)),
                ..Assigns::default()
            },
        );
        assert_eq!(h.state().life, Field::Value(Life::FusedInto(OTHER)));
        // A move made without seeing the fuse keeps the bookmark alive.
        let moved = h.move_(&[root], 1, 2);
        let s = h.state();
        assert_eq!(
            s.life,
            Field::Conflict(vec![
                Candidate {
                    value: Life::Live,
                    updates: vec![moved]
                },
                Candidate {
                    value: Life::FusedInto(OTHER),
                    updates: vec![fused]
                },
            ])
        );
        assert_eq!(s.target, Field::Value(c(2)));
    }

    #[test]
    fn the_latest_writer_wins_even_when_it_restores_an_older_value() {
        // root names it "n"; p renames to "m"; x (after p) renames back to
        // "n"; y (after p) moves; z (after root only) moves forward.
        let mut h = History::default();
        let root = h.create("n", 1);
        let p = h.rename(&[root], "m");
        h.rename(&[p], "n");
        h.move_(&[p], 1, 2);
        h.move_(&[root], 1, 3);
        let s = h.state();
        assert_eq!(s.name, Field::Value("n".to_owned()));
        assert_eq!(s.target, Field::Value(c(3)));
        assert_eq!(s.heads.len(), 3);
    }

    #[test]
    fn repo_settings_start_unset_and_concurrent_defaults_conflict() {
        let mut h = History::default();
        let state = resolve(&h.updates, is_ancestor).unwrap();
        assert_eq!(state.default_bookmark, Field::Unset);
        assert!(state.heads.is_empty());
        let set = |d| Assigns {
            default_bookmark: Some(d),
            ..Assigns::default()
        };
        let first = h.add(Subject::Repo, &[], set(Some(B)));
        let other = h.add(Subject::Repo, &[first], set(Some(OTHER)));
        let cleared = h.add(Subject::Repo, &[first], set(None));
        assert_eq!(
            h.state().default_bookmark,
            Field::Conflict(vec![
                Candidate {
                    value: None,
                    updates: vec![cleared]
                },
                Candidate {
                    value: Some(OTHER),
                    updates: vec![other]
                },
            ])
        );
    }

    #[test]
    fn invalid_updates_are_refused() {
        let actor = Actor {
            kind: ActorKind::User,
            device_id: "d".into(),
            description: String::new(),
        };
        let update = |subject, preds: Vec<BookmarkUpdateId>, a| {
            BookmarkUpdate::new(REPO, subject, preds, a, actor.clone(), 0)
        };
        let some_pred = vec![BookmarkUpdateId([5; 32])];
        let rename = |n: &str| Assigns {
            name: Some(n.into()),
            ..Assigns::default()
        };
        // A first update must create the bookmark whole.
        assert!(
            update(Subject::Bookmark(B), vec![], rename("x"))
                .validate()
                .is_err()
        );
        assert!(
            update(Subject::Bookmark(B), some_pred.clone(), Assigns::default())
                .validate()
                .is_err()
        );
        for bad in ["", " lead", "trail ", "a\nb", &"x".repeat(256)] {
            assert!(
                update(Subject::Bookmark(B), some_pred.clone(), rename(bad))
                    .validate()
                    .is_err(),
                "{bad:?}"
            );
        }
        assert!(
            update(
                Subject::Bookmark(B),
                some_pred.clone(),
                rename("feature/x y")
            )
            .validate()
            .is_ok()
        );
        assert!(
            update(Subject::Repo, vec![], rename("x"))
                .validate()
                .is_err()
        );
        let fuse_self = Assigns {
            life: Some(Life::FusedInto(B)),
            ..Assigns::default()
        };
        assert!(
            update(Subject::Bookmark(B), some_pred.clone(), fuse_self)
                .validate()
                .is_err()
        );
        // A reader without bookmark.v1 refuses the update.
        let ok = update(Subject::Bookmark(B), some_pred, rename("x"));
        let bytes = ok.encode().unwrap();
        assert!(matches!(
            BookmarkUpdate::decode(&bytes, &Capabilities::default().with("core.v1")),
            Err(CoreError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_missing_predecessor_or_a_mixed_set_is_an_error() {
        let mut h = History::default();
        let root = h.create("main", 1);
        let m = h.move_(&[root], 1, 2);
        let mut partial = h.updates.clone();
        partial.remove(&root);
        assert_eq!(
            resolve(&partial, is_ancestor),
            Err(ResolveError::MissingPredecessor(m, root))
        );
        h.add(
            Subject::Repo,
            &[],
            Assigns {
                default_bookmark: Some(None),
                ..Assigns::default()
            },
        );
        assert_eq!(
            resolve(&h.updates, is_ancestor),
            Err(ResolveError::MixedSubjects)
        );
    }
}
