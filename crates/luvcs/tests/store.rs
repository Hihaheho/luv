//! Local core behaviour: record / amend / undo / integrate / resolve /
//! export-import, and the crash-atomicity of operations.

#![cfg(feature = "store")]

use std::collections::BTreeMap;

use luvcs::cbor::Value;
use luvcs::digest::RustDigest;
use luvcs::id::{IdKey, ObjectId, OperationId, RepoId, WorkspaceId};
use luvcs::merge::text::{RegionChoice, resolve as resolve_text};
use luvcs::model::{Actor, ActorKind, ObjectBody, TermKind};
use luvcs::record::{FieldPolicy, RecordSchema, RecordValue};
use luvcs::store::{OperationContext, RecordSource, SchemaRegistry, Store, StoreError};
use rusqlite::Connection;

const REPO: RepoId = RepoId([1; 16]);
const WS: WorkspaceId = WorkspaceId([2; 16]);
const NOTE: ObjectId = ObjectId([0x10; 16]);
const OTHER: ObjectId = ObjectId([0x20; 16]);

fn key() -> IdKey {
    IdKey([7; 32])
}

fn store(conn: &Connection) -> Store<'_, RustDigest> {
    let mut s = Store::open(conn, RustDigest).unwrap().with_schemas(
        SchemaRegistry::default().register(
            RecordSchema::new("note.v1")
                .field("body", FieldPolicy::Text)
                .field("tags", FieldPolicy::Set),
        ),
    );
    s.create_repo(REPO, 1).unwrap();
    s
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        device_id: "test".into(),
        description: String::new(),
    }
}

struct Ctx {
    n: u64,
}

impl Ctx {
    fn new() -> Self {
        Self { n: 0 }
    }

    fn next(&mut self, ws: WorkspaceId, expected_token: Option<u64>) -> OperationContext {
        self.n += 1;
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&self.n.to_be_bytes());
        OperationContext {
            operation_id: OperationId(id),
            workspace_id: ws,
            expected_token,
            actor: actor(),
            reason: format!("op {}", self.n),
            now_unix_ms: 1_000 + self.n as i64,
        }
    }
}

#[test]
fn record_amend_history_and_object_history() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();

    let s1 = repo
        .put_text_state(
            NOTE,
            "note",
            b"v1\n",
            Value::map([("title", Value::text("n"))]),
        )
        .unwrap();
    let ws = repo
        .save_working(&ctx.next(WS, Some(1)), &[(NOTE, Some(s1))])
        .unwrap();
    assert_eq!(ws.token, 2);
    let c1 = repo
        .record(&ctx.next(WS, Some(2)), RecordSource::Working, "first")
        .unwrap();
    assert_eq!(repo.visible().unwrap(), vec![c1]);
    assert_eq!(repo.workspace(WS).unwrap().base, Some(c1));

    // Recording again with nothing changed is refused.
    assert!(matches!(
        repo.record(&ctx.next(WS, None), RecordSource::Working, "again"),
        Err(StoreError::NothingToRecord)
    ));

    let s2 = repo
        .put_text_state(NOTE, "note", b"v2\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(s2))])
        .unwrap();
    let c2 = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "second")
        .unwrap();
    assert_eq!(repo.visible().unwrap(), vec![c2]);

    // Amend keeps the change id, hides the old commit.
    let s3 = repo
        .put_text_state(NOTE, "note", b"v2 fixed\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(s3))])
        .unwrap();
    let c2b = repo.amend(&ctx.next(WS, None), "second (amended)").unwrap();
    assert_ne!(c2, c2b);
    let (m2, m2b) = (repo.commit(c2).unwrap(), repo.commit(c2b).unwrap());
    assert_eq!(m2.change_id, m2b.change_id);
    assert_eq!(m2b.parents, vec![c1]);
    assert_eq!(repo.visible().unwrap(), vec![c2b]);
    assert_eq!(repo.commits_of_change(m2.change_id).unwrap(), vec![c2, c2b]);
    // The old commit is still stored, not deleted.
    assert!(repo.has_commit(c2).unwrap());

    let hist = repo.history(c2b, 10).unwrap();
    assert_eq!(hist.iter().map(|e| e.id).collect::<Vec<_>>(), vec![c2b, c1]);
    let oh = repo.object_history(c2b, NOTE, 10).unwrap();
    assert_eq!(oh, vec![(c2b, Some(s3)), (c1, Some(s1))]);
    assert_eq!(
        repo.get_content(match repo.get_state(s3).unwrap().body {
            ObjectBody::Text { content } => content,
            _ => panic!(),
        })
        .unwrap()
        .unwrap(),
        b"v2 fixed\n"
    );
}

#[test]
fn stale_token_and_duplicate_operation_are_refused_atomically() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let s1 = repo
        .put_text_state(NOTE, "note", b"a", Value::Map(vec![]))
        .unwrap();
    let err = repo
        .save_working(&ctx.next(WS, Some(99)), &[(NOTE, Some(s1))])
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::StaleToken {
            expected: 99,
            actual: 1
        }
    ));
    assert!(repo.working_entries(WS).unwrap().is_empty());
    assert!(repo.operations(10).unwrap().is_empty());

    let op = ctx.next(WS, None);
    repo.save_working(&op, &[(NOTE, Some(s1))]).unwrap();
    assert!(matches!(
        repo.save_working(&op, &[(NOTE, None)]),
        Err(StoreError::DuplicateOperation(_))
    ));
    assert_eq!(repo.working_entries(WS).unwrap().len(), 1);

    // A state of another object cannot be filed under this object.
    let wrong = repo
        .put_text_state(OTHER, "note", b"b", Value::Map(vec![]))
        .unwrap();
    assert!(matches!(
        repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(wrong))]),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn candidate_records_a_subset_and_keeps_the_rest_working() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let a1 = repo
        .put_text_state(NOTE, "note", b"a1", Value::Map(vec![]))
        .unwrap();
    let b1 = repo
        .put_text_state(OTHER, "note", b"b1", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(a1)), (OTHER, Some(b1))])
        .unwrap();
    repo.select_candidate(&ctx.next(WS, None), &[NOTE]).unwrap();
    // Editing after staging does not change the candidate.
    let a2 = repo
        .put_text_state(NOTE, "note", b"a2", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(a2))])
        .unwrap();
    let c = repo
        .record(&ctx.next(WS, None), RecordSource::Candidate, "only a1")
        .unwrap();
    let snap = repo.snapshot(c).unwrap();
    assert_eq!(snap, BTreeMap::from([(NOTE, a1)]));
    let ws = repo.workspace(WS).unwrap();
    assert_eq!(ws.candidate, None);
    assert_eq!(
        repo.working_entries(WS).unwrap(),
        BTreeMap::from([(NOTE, a2), (OTHER, b1)])
    );
    assert!(matches!(
        repo.record(&ctx.next(WS, None), RecordSource::Candidate, "none"),
        Err(StoreError::NoCandidate)
    ));
}

#[test]
fn undo_and_redo_move_the_workspace_and_visible_set_without_deleting_commits() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let s1 = repo
        .put_text_state(NOTE, "note", b"1", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(s1))])
        .unwrap();
    let c1 = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "1")
        .unwrap();
    let s2 = repo
        .put_text_state(NOTE, "note", b"2", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(s2))])
        .unwrap();
    let c2 = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "2")
        .unwrap();
    assert_eq!(repo.visible().unwrap(), vec![c2]);

    let undone = repo.undo(&actor(), 0).unwrap();
    assert_eq!(undone.kind, "record");
    assert_eq!(repo.visible().unwrap(), vec![c1]);
    let ws = repo.workspace(WS).unwrap();
    assert_eq!(ws.base, Some(c1));
    assert_eq!(
        repo.working_entries(WS).unwrap(),
        BTreeMap::from([(NOTE, s2)])
    );
    assert!(repo.has_commit(c2).unwrap());

    repo.undo(&actor(), 0).unwrap(); // save_working of s2
    assert_eq!(
        repo.working_entries(WS).unwrap(),
        BTreeMap::from([(NOTE, s1)])
    );

    repo.redo(&actor(), 0).unwrap();
    assert_eq!(
        repo.working_entries(WS).unwrap(),
        BTreeMap::from([(NOTE, s2)])
    );
    repo.redo(&actor(), 0).unwrap();
    assert_eq!(repo.visible().unwrap(), vec![c2]);
    assert!(matches!(
        repo.redo(&actor(), 0),
        Err(StoreError::NothingToRedo)
    ));

    // A new operation after an undo discards the undone branch.
    repo.undo(&actor(), 0).unwrap();
    let s3 = repo
        .put_text_state(NOTE, "note", b"3", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(s3))])
        .unwrap();
    assert!(matches!(
        repo.redo(&actor(), 0),
        Err(StoreError::NothingToRedo)
    ));
    let ops = repo.operations(10).unwrap();
    assert!(ops.iter().all(|o| !o.undone));
}

#[test]
fn integrate_merges_text_and_records_and_stores_structured_conflicts() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let ws2 = WorkspaceId([3; 16]);

    // Base: a text note and a record note.
    let t0 = repo
        .put_text_state(NOTE, "note", b"a\nb\nc\n", Value::Map(vec![]))
        .unwrap();
    let rec0 = RecordValue::object([
        ("title", RecordValue::text("T")),
        ("body", RecordValue::text("x\ny\n")),
        (
            "tags",
            RecordValue::collection([("t1", RecordValue::text("one"))]),
        ),
    ]);
    let r0 = repo
        .put_record_state(OTHER, "note", "note.v1", &rec0, Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(t0)), (OTHER, Some(r0))])
        .unwrap();
    let base = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "base")
        .unwrap();

    // Left (WS): change line a, add tag t2.
    let t_l = repo
        .put_text_state(NOTE, "note", b"A\nb\nc\n", Value::Map(vec![]))
        .unwrap();
    let rec_l = RecordValue::object([
        ("title", RecordValue::text("T")),
        ("body", RecordValue::text("x\ny\n")),
        (
            "tags",
            RecordValue::collection([
                ("t1", RecordValue::text("one")),
                ("t2", RecordValue::text("two")),
            ]),
        ),
    ]);
    let r_l = repo
        .put_record_state(OTHER, "note", "note.v1", &rec_l, Value::Map(vec![]))
        .unwrap();
    repo.save_working(
        &ctx.next(WS, None),
        &[(NOTE, Some(t_l)), (OTHER, Some(r_l))],
    )
    .unwrap();
    let left = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "left")
        .unwrap();

    // Right (ws2 from base): change line c, change title.
    repo.create_workspace(ws2, Some(base)).unwrap();
    let t_r = repo
        .put_text_state(NOTE, "note", b"a\nb\nC\n", Value::Map(vec![]))
        .unwrap();
    let rec_r = RecordValue::object([
        ("title", RecordValue::text("T2")),
        ("body", RecordValue::text("x\ny\n")),
        (
            "tags",
            RecordValue::collection([("t1", RecordValue::text("one"))]),
        ),
    ]);
    let r_r = repo
        .put_record_state(OTHER, "note", "note.v1", &rec_r, Value::Map(vec![]))
        .unwrap();
    repo.save_working(
        &ctx.next(ws2, None),
        &[(NOTE, Some(t_r)), (OTHER, Some(r_r))],
    )
    .unwrap();
    let right = repo
        .record(&ctx.next(ws2, None), RecordSource::Working, "right")
        .unwrap();

    let outcome = repo.integrate(&ctx.next(WS, None), right).unwrap();
    assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
    let merge = repo.commit(outcome.commit).unwrap();
    assert_eq!(merge.parents, vec![left, right]);
    assert_eq!(repo.visible().unwrap(), vec![outcome.commit]);
    let snap = repo.snapshot(outcome.commit).unwrap();
    let text = match repo.get_state(snap[&NOTE]).unwrap().body {
        ObjectBody::Text { content } => repo.get_content(content).unwrap().unwrap(),
        b => panic!("{b:?}"),
    };
    assert_eq!(text, b"A\nb\nC\n");
    let record = match repo.get_state(snap[&OTHER]).unwrap().body {
        ObjectBody::Record { content, .. } => {
            RecordValue::decode(&repo.get_content(content).unwrap().unwrap()).unwrap()
        }
        b => panic!("{b:?}"),
    };
    assert_eq!(
        record.at(&luvcs::record::FieldPath::parse("title")),
        Some(&RecordValue::text("T2"))
    );
    assert_eq!(
        record
            .at(&luvcs::record::FieldPath::parse("tags"))
            .unwrap()
            .as_collection()
            .unwrap()
            .len(),
        2
    );

    // Now a real conflict: both sides edit the same line.
    let t_l2 = repo
        .put_text_state(NOTE, "note", b"A\nB-left\nC\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(t_l2))])
        .unwrap();
    let left2 = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "left2")
        .unwrap();
    let ws3 = WorkspaceId([4; 16]);
    repo.create_workspace(ws3, Some(outcome.commit)).unwrap();
    let t_r2 = repo
        .put_text_state(NOTE, "note", b"A\nB-right\nC\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(ws3, None), &[(NOTE, Some(t_r2))])
        .unwrap();
    let right2 = repo
        .record(&ctx.next(ws3, None), RecordSource::Working, "right2")
        .unwrap();

    let outcome2 = repo.integrate(&ctx.next(WS, None), right2).unwrap();
    assert_eq!(outcome2.conflicts.len(), 1);
    let (obj, conflict_id) = outcome2.conflicts[0];
    assert_eq!(obj, NOTE);
    let conflict = repo.get_conflict(conflict_id).unwrap();
    assert_eq!(conflict.driver, "merge.text.v1");
    assert_eq!(conflict.sides, vec![t_l2, t_r2]);
    assert_eq!(conflict.terms.len(), 1);
    assert_eq!(conflict.terms[0].kind, TermKind::TextRegion);
    let merge2 = repo.commit(outcome2.commit).unwrap();
    assert_eq!(merge2.parents, vec![left2, right2]);
    let snap2 = repo.snapshot(outcome2.commit).unwrap();
    assert!(matches!(
        repo.get_state(snap2[&NOTE]).unwrap().body,
        ObjectBody::Conflict { conflict } if conflict == conflict_id
    ));

    // Resolve by taking the right side via the text driver's resolve.
    let base_text = b"A\nb\nC\n";
    let out =
        luvcs::merge::text::merge_text(Some(base_text), b"A\nB-left\nC\n", b"A\nB-right\nC\n");
    let resolved_bytes = match &out {
        luvcs::merge::text::TextMergeOutcome::Conflicted { merged, regions } => {
            resolve_text(merged, regions, &[RegionChoice::Right]).unwrap()
        }
        _ => panic!("expected conflict"),
    };
    assert_eq!(resolved_bytes, b"A\nB-right\nC\n");
    let resolved = repo
        .put_text_state(NOTE, "note", &resolved_bytes, Value::Map(vec![]))
        .unwrap();
    // Cannot resolve with a state of another object or a non-conflict object.
    assert!(repo.resolve(&ctx.next(WS, None), OTHER, resolved).is_err());
    repo.resolve(&ctx.next(WS, None), NOTE, resolved).unwrap();
    let c = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "resolved")
        .unwrap();
    let snap3 = repo.snapshot(c).unwrap();
    assert_eq!(snap3[&NOTE], resolved);
    assert_eq!(repo.commit(c).unwrap().parents, vec![outcome2.commit]);
}

#[test]
fn integrate_refuses_unrecorded_changes_and_keeps_them() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let t0 = repo
        .put_text_state(NOTE, "note", b"x\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(t0))])
        .unwrap();
    let base = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "base")
        .unwrap();
    let ws2 = WorkspaceId([3; 16]);
    repo.create_workspace(ws2, Some(base)).unwrap();
    let t_r = repo
        .put_text_state(OTHER, "note", b"right\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(ws2, None), &[(OTHER, Some(t_r))])
        .unwrap();
    let right = repo
        .record(&ctx.next(ws2, None), RecordSource::Working, "right")
        .unwrap();

    let dirty = repo
        .put_text_state(NOTE, "note", b"unrecorded\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(dirty))])
        .unwrap();
    let err = repo.integrate(&ctx.next(WS, None), right).unwrap_err();
    assert!(matches!(err, StoreError::UnrecordedChanges), "{err:?}");
    assert_eq!(repo.working_entries(WS).unwrap()[&NOTE], dirty);
    assert_eq!(repo.workspace(WS).unwrap().base, Some(base));

    // Once recorded, the same integrate goes through.
    repo.record(&ctx.next(WS, None), RecordSource::Working, "left")
        .unwrap();
    let out = repo.integrate(&ctx.next(WS, None), right).unwrap();
    assert!(out.conflicts.is_empty());
    let working = repo.working_entries(WS).unwrap();
    assert_eq!(working[&NOTE], dirty);
    assert_eq!(working[&OTHER], t_r);
}

#[test]
fn whole_object_conflict_still_reports_a_title_conflict() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let title = |t: &str| Value::map([("title", Value::text(t)), ("kept", Value::text("k"))]);
    let t0 = repo
        .put_text_state(NOTE, "note", b"x\n", title("n.md"))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(t0))])
        .unwrap();
    let base = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "base")
        .unwrap();
    // The type changes on one side, so no content merge applies.
    let tl = repo
        .put_text_state(NOTE, "note", b"left\n", title("a.md"))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(tl))])
        .unwrap();
    repo.record(&ctx.next(WS, None), RecordSource::Working, "left")
        .unwrap();
    let ws2 = WorkspaceId([3; 16]);
    repo.create_workspace(ws2, Some(base)).unwrap();
    let tr = repo
        .put_text_state(NOTE, "memo", b"right\n", title("b.md"))
        .unwrap();
    repo.save_working(&ctx.next(ws2, None), &[(NOTE, Some(tr))])
        .unwrap();
    let right = repo
        .record(&ctx.next(ws2, None), RecordSource::Working, "right")
        .unwrap();

    let out = repo.integrate(&ctx.next(WS, None), right).unwrap();
    assert_eq!(out.conflicts.len(), 1);
    let conflict = repo.get_conflict(out.conflicts[0].1).unwrap();
    assert_eq!(conflict.driver, "merge.object.v1");
    let kinds: Vec<(TermKind, &str)> = conflict
        .terms
        .iter()
        .map(|t| (t.kind, t.path.as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec![(TermKind::Value, ""), (TermKind::Metadata, "title")]
    );
    // The conflict state keeps the agreed key and drops the disputed one.
    let working = repo.working_entries(WS).unwrap()[&NOTE];
    let metadata = repo.get_state(working).unwrap().metadata;
    assert_eq!(metadata.get("kept").and_then(Value::as_text), Some("k"));
    assert_eq!(metadata.get("title"), None);
}

#[test]
fn deleted_vs_edited_is_a_conflict_and_restore_brings_old_content_back() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let t0 = repo
        .put_text_state(NOTE, "note", b"x\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(t0))])
        .unwrap();
    let base = repo
        .record(&ctx.next(WS, None), RecordSource::Working, "base")
        .unwrap();
    // Left deletes, right edits.
    repo.save_working(&ctx.next(WS, None), &[(NOTE, None)])
        .unwrap();
    repo.record(&ctx.next(WS, None), RecordSource::Working, "delete")
        .unwrap();
    let ws2 = WorkspaceId([3; 16]);
    repo.create_workspace(ws2, Some(base)).unwrap();
    let t1 = repo
        .put_text_state(NOTE, "note", b"x edited\n", Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(ws2, None), &[(NOTE, Some(t1))])
        .unwrap();
    let right = repo
        .record(&ctx.next(ws2, None), RecordSource::Working, "edit")
        .unwrap();
    let out = repo.integrate(&ctx.next(WS, None), right).unwrap();
    assert_eq!(out.conflicts.len(), 1);
    let conflict = repo.get_conflict(out.conflicts[0].1).unwrap();
    assert_eq!(conflict.terms[0].kind, TermKind::DeletedVsEdited);
    assert_eq!(conflict.base, vec![t0]);
    // The deleted side is a real Deleted state.
    assert!(matches!(
        repo.get_state(conflict.sides[0]).unwrap().body,
        ObjectBody::Deleted
    ));
    assert_eq!(conflict.sides[1], t1);

    // Restore the base version into working, then record.
    repo.restore(&ctx.next(WS, None), base, NOTE).unwrap();
    assert_eq!(repo.working_entries(WS).unwrap()[&NOTE], t0);
}

#[test]
fn export_import_round_trips_and_verifies_ids() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = store(&conn);
    let mut ctx = Ctx::new();
    let archive = {
        let mut repo = store.repo(REPO, key()).unwrap();
        repo.create_workspace(WS, None).unwrap();
        let s = repo
            .put_text_state(NOTE, "note", b"exported\n", Value::Map(vec![]))
            .unwrap();
        repo.save_working(&ctx.next(WS, None), &[(NOTE, Some(s))])
            .unwrap();
        let c = repo
            .record(&ctx.next(WS, None), RecordSource::Working, "c")
            .unwrap();
        repo.set_ref("main", c).unwrap();
        repo.export_archive().unwrap()
    };

    let other_conn = Connection::open_in_memory().unwrap();
    let mut other = Store::open(&other_conn, RustDigest).unwrap();
    other.create_repo(REPO, 1).unwrap();
    let mut repo = other.repo(REPO, key()).unwrap();
    let n = repo.import_archive(&archive, 1).unwrap();
    assert!(n >= 4);
    let visible = repo.visible().unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(repo.refs().unwrap()["main"], visible[0]);
    let snap = repo.snapshot(visible[0]).unwrap();
    assert_eq!(snap.len(), 1);
    // Importing twice is idempotent.
    repo.import_archive(&archive, 1).unwrap();

    // Under a different key every ID fails verification.
    let third_conn = Connection::open_in_memory().unwrap();
    let mut third = Store::open(&third_conn, RustDigest).unwrap();
    third.create_repo(REPO, 1).unwrap();
    let mut repo3 = third.repo(REPO, IdKey([8; 32])).unwrap();
    assert!(matches!(
        repo3.import_archive(&archive, 1),
        Err(StoreError::IdentityConflict { .. })
    ));
    assert!(repo3.visible().unwrap().is_empty());
}

#[test]
fn newer_store_schema_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    {
        let _ = Store::open(&conn, RustDigest).unwrap();
    }
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE vcs_meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL) STRICT;
         INSERT INTO vcs_meta VALUES ('schema_version', '999');",
    )
    .unwrap();
    assert!(matches!(
        Store::open(&conn, RustDigest),
        Err(StoreError::Invalid(_))
    ));
}
