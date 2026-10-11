//! Bookmarks in the store: operations, records that move them, undo and
//! redo, and updates arriving from elsewhere.

#![cfg(feature = "store")]

use luvcs::bookmark::{Assigns, BookmarkUpdate, Field, Life, Subject, Target};
use luvcs::cbor::Value;
use luvcs::digest::RustDigest;
use luvcs::id::{
    BookmarkId, BookmarkUpdateId, CommitId, IdKey, ObjectId, OperationId, RepoId, WorkspaceId,
};
use luvcs::model::{Actor, ActorKind};
use luvcs::store::{
    Advance, BookmarkEdit, OperationContext, RecordSource, Repo, Store, StoreError,
};
use rusqlite::Connection;

const REPO: RepoId = RepoId([1; 16]);
const WS: WorkspaceId = WorkspaceId([2; 16]);
const NOTE: ObjectId = ObjectId([0x10; 16]);
const MAIN: BookmarkId = BookmarkId([0xA1; 16]);
const DEV: BookmarkId = BookmarkId([0xA2; 16]);

fn key() -> IdKey {
    IdKey([7; 32])
}

fn actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        device_id: "test".into(),
        description: String::new(),
    }
}

#[derive(Default)]
struct Ctx {
    n: u64,
}

impl Ctx {
    fn next(&mut self) -> OperationContext {
        self.n += 1;
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&self.n.to_be_bytes());
        OperationContext {
            operation_id: OperationId(id),
            workspace_id: WS,
            expected_token: None,
            actor: actor(),
            reason: String::new(),
            now_unix_ms: 1_000 + self.n as i64,
        }
    }
}

fn open(conn: &Connection) -> Store<'_, RustDigest> {
    let mut s = Store::open(conn, RustDigest).unwrap();
    s.create_repo(REPO, 1).unwrap();
    s
}

/// Saves `text` and records it.
fn commit(repo: &mut Repo<'_, '_, RustDigest>, ctx: &mut Ctx, text: &str) -> CommitId {
    let s = repo
        .put_text_state(NOTE, "note", text.as_bytes(), Value::Map(vec![]))
        .unwrap();
    repo.save_working(&ctx.next(), &[(NOTE, Some(s))]).unwrap();
    repo.record(&ctx.next(), RecordSource::Working, text)
        .unwrap()
}

/// An update made elsewhere, taken in as replication would.
fn arrive(
    repo: &mut Repo<'_, '_, RustDigest>,
    subject: Subject,
    preds: &[BookmarkUpdateId],
    assigns: Assigns,
    at: i64,
) -> BookmarkUpdateId {
    let u = BookmarkUpdate::new(REPO, subject, preds.iter().copied(), assigns, actor(), at);
    let id = u.id(&RustDigest, &key()).unwrap();
    repo.import_objects(&[("bookmark_update".into(), id.to_vec(), u.encode().unwrap())])
        .unwrap();
    id
}

fn moved_to(c: CommitId, from: Option<CommitId>) -> Assigns {
    Assigns {
        life: Some(Life::Live),
        target: Some(Target { commit: c, from }),
        ..Assigns::default()
    }
}

#[test]
fn create_rename_move_and_stale_heads() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    let c2 = commit(&mut repo, &mut ctx, "2");

    let created = repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    let state = repo.bookmark(MAIN).unwrap();
    assert_eq!(state.heads, vec![created]);
    assert_eq!(state.name, Field::Value("main".into()));
    assert_eq!(state.target, Field::Value(c1));

    let renamed = repo
        .edit_bookmark(
            &ctx.next(),
            MAIN,
            &[created],
            BookmarkEdit {
                name: Some("trunk".into()),
                target: Some(c2),
                ..BookmarkEdit::default()
            },
        )
        .unwrap();
    let state = repo.bookmark(MAIN).unwrap();
    assert_eq!(state.heads, vec![renamed]);
    assert_eq!(state.name, Field::Value("trunk".into()));
    assert_eq!(state.target, Field::Value(c2));

    // An edit from a view that saw only `created` is refused.
    let stale = repo.edit_bookmark(
        &ctx.next(),
        MAIN,
        &[created],
        BookmarkEdit {
            target: Some(c1),
            ..BookmarkEdit::default()
        },
    );
    assert!(matches!(stale, Err(StoreError::StaleBookmark)), "{stale:?}");
    assert!(matches!(
        repo.bookmark(DEV),
        Err(StoreError::UnknownBookmark(_))
    ));
}

#[test]
fn names_are_unique_locally_and_shared_names_are_flagged() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    assert!(matches!(
        repo.create_bookmark(&ctx.next(), DEV, "main", c1),
        Err(StoreError::NameTaken(_))
    ));

    // Another device made its own "main" without seeing ours.
    arrive(
        &mut repo,
        Subject::Bookmark(DEV),
        &[],
        Assigns {
            life: Some(Life::Live),
            name: Some("main".into()),
            target: Some(Target {
                commit: c1,
                from: None,
            }),
            ..Assigns::default()
        },
        5,
    );
    let entries = repo.bookmarks().unwrap();
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|e| e.name_shared), "{entries:?}");

    // Fusing one into the other settles it.
    let heads = repo.bookmark(DEV).unwrap().heads;
    repo.edit_bookmark(
        &ctx.next(),
        DEV,
        &heads,
        BookmarkEdit {
            life: Some(Life::FusedInto(MAIN)),
            ..BookmarkEdit::default()
        },
    )
    .unwrap();
    let entries = repo.bookmarks().unwrap();
    assert!(entries.iter().all(|e| !e.name_shared), "{entries:?}");
    assert_eq!(
        repo.bookmark(DEV).unwrap().life,
        Field::Value(Life::FusedInto(MAIN))
    );
}

#[test]
fn record_advances_the_selected_bookmark_only_when_current_and_forward() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    let save = |repo: &mut Repo<'_, '_, RustDigest>, ctx: &mut Ctx, text: &str| {
        let s = repo
            .put_text_state(NOTE, "note", text.as_bytes(), Value::Map(vec![]))
            .unwrap();
        repo.save_working(&ctx.next(), &[(NOTE, Some(s))]).unwrap();
    };

    // Nothing selected: the commit is recorded, nothing moves.
    save(&mut repo, &mut ctx, "2");
    let heads = repo.bookmark(MAIN).unwrap().heads;
    let (c2, adv) = repo
        .record_advancing(&ctx.next(), RecordSource::Working, "2", &heads)
        .unwrap();
    assert_eq!(adv, Advance::NoSelection);
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(c1));

    repo.select_bookmark(&ctx.next(), Some(MAIN)).unwrap();
    save(&mut repo, &mut ctx, "3");
    let (c3, adv) = repo
        .record_advancing(&ctx.next(), RecordSource::Working, "3", &heads)
        .unwrap();
    assert!(matches!(adv, Advance::Advanced(_)), "{adv:?}");
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(c3));
    assert_ne!(c2, c3);

    // The same stale heads again: recorded, not moved.
    save(&mut repo, &mut ctx, "4");
    let (c4, adv) = repo
        .record_advancing(&ctx.next(), RecordSource::Working, "4", &heads)
        .unwrap();
    assert_eq!(adv, Advance::Stale);
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(c3));
    assert_eq!(repo.workspace(WS).unwrap().base, Some(c4));
}

#[test]
fn amend_moves_bookmarks_on_the_rewritten_commit() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    let amended = repo.amend(&ctx.next(), "reworded").unwrap();
    assert_ne!(amended, c1);
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(amended));
}

#[test]
fn undo_puts_fields_back_with_new_updates_and_redo_reapplies() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    let c2 = commit(&mut repo, &mut ctx, "2");
    let created = repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    let moved = repo
        .edit_bookmark(
            &ctx.next(),
            MAIN,
            &[created],
            BookmarkEdit {
                target: Some(c2),
                ..BookmarkEdit::default()
            },
        )
        .unwrap();

    repo.undo(&actor(), 50).unwrap();
    let state = repo.bookmark(MAIN).unwrap();
    assert_eq!(state.target, Field::Value(c1));
    // Not removed: a new update on top of the move.
    assert_eq!(state.heads.len(), 1);
    assert_ne!(state.heads, vec![created]);
    assert!(
        repo.object_bytes("bookmark_update", moved.as_bytes())
            .unwrap()
            .is_some()
    );

    repo.redo(&actor(), 51).unwrap();
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(c2));

    // Undoing the move and then the creation deletes the bookmark.
    repo.undo(&actor(), 52).unwrap();
    repo.undo(&actor(), 53).unwrap();
    assert_eq!(
        repo.bookmark(MAIN).unwrap().life,
        Field::Value(Life::Deleted)
    );
}

#[test]
fn undoing_a_resolution_brings_the_conflict_back() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    let c2 = commit(&mut repo, &mut ctx, "2");
    let created = repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    // Two devices moved it forward onto different lines.
    arrive(
        &mut repo,
        Subject::Bookmark(MAIN),
        &[created],
        moved_to(c2, Some(c1)),
        5,
    );
    let side = {
        let ws2 = WorkspaceId([3; 16]);
        repo.create_workspace(ws2, Some(c1)).unwrap();
        let s = repo
            .put_text_state(NOTE, "note", b"side", Value::Map(vec![]))
            .unwrap();
        let mut c = ctx.next();
        c.workspace_id = ws2;
        repo.save_working(&c, &[(NOTE, Some(s))]).unwrap();
        let mut c = ctx.next();
        c.workspace_id = ws2;
        repo.record(&c, RecordSource::Working, "side").unwrap()
    };
    arrive(
        &mut repo,
        Subject::Bookmark(MAIN),
        &[created],
        moved_to(side, Some(c1)),
        6,
    );
    let state = repo.bookmark(MAIN).unwrap();
    assert!(state.target.is_conflict(), "{state:?}");

    repo.edit_bookmark(
        &ctx.next(),
        MAIN,
        &state.heads,
        BookmarkEdit {
            target: Some(c2),
            ..BookmarkEdit::default()
        },
    )
    .unwrap();
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(c2));

    repo.undo(&actor(), 60).unwrap();
    let back = repo.bookmark(MAIN).unwrap();
    match back.target {
        Field::Conflict(c) => {
            let values: Vec<CommitId> = c.iter().map(|c| c.value).collect();
            let mut expected = vec![c2, side];
            expected.sort();
            assert_eq!(values, expected);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn deleting_the_default_needs_a_new_default() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    let created = repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    assert_eq!(repo.repo_settings().unwrap().default_bookmark, Field::Unset);
    repo.set_default_bookmark(&ctx.next(), &[], Some(MAIN))
        .unwrap();
    assert_eq!(
        repo.repo_settings().unwrap().default_bookmark,
        Field::Value(Some(MAIN))
    );
    let delete = |new_default| BookmarkEdit {
        life: Some(Life::Deleted),
        new_default,
        ..BookmarkEdit::default()
    };
    assert!(matches!(
        repo.edit_bookmark(&ctx.next(), MAIN, &[created], delete(None)),
        Err(StoreError::BookmarkIsDefault(_))
    ));
    repo.edit_bookmark(&ctx.next(), MAIN, &[created], delete(Some(None)))
        .unwrap();
    assert_eq!(
        repo.bookmark(MAIN).unwrap().life,
        Field::Value(Life::Deleted)
    );
    assert_eq!(
        repo.repo_settings().unwrap().default_bookmark,
        Field::Value(None)
    );
}

#[test]
fn selection_is_part_of_the_workspace_and_undo_restores_it() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    repo.create_bookmark(&ctx.next(), MAIN, "main", c1).unwrap();
    assert!(matches!(
        repo.select_bookmark(&ctx.next(), Some(DEV)),
        Err(StoreError::UnknownBookmark(_))
    ));
    let ws = repo.select_bookmark(&ctx.next(), Some(MAIN)).unwrap();
    assert_eq!(ws.selected_bookmark, Some(MAIN));
    repo.undo(&actor(), 9).unwrap();
    assert_eq!(repo.workspace(WS).unwrap().selected_bookmark, None);
}

#[test]
fn updates_wait_for_their_history_and_commits_to_arrive() {
    let conn = Connection::open_in_memory().unwrap();
    let mut store = open(&conn);
    let mut ctx = Ctx::default();
    let mut repo = store.repo(REPO, key()).unwrap();
    repo.create_workspace(WS, None).unwrap();
    let c1 = commit(&mut repo, &mut ctx, "1");
    let root = BookmarkUpdate::new(
        REPO,
        Subject::Bookmark(MAIN),
        [],
        Assigns {
            life: Some(Life::Live),
            name: Some("main".into()),
            target: Some(Target {
                commit: c1,
                from: None,
            }),
            ..Assigns::default()
        },
        actor(),
        1,
    );
    let root_id = root.id(&RustDigest, &key()).unwrap();
    // The move arrives before the creation it follows.
    let rename = Assigns {
        life: Some(Life::Live),
        name: Some("trunk".into()),
        ..Assigns::default()
    };
    arrive(&mut repo, Subject::Bookmark(MAIN), &[root_id], rename, 2);
    assert!(matches!(
        repo.bookmark(MAIN),
        Err(StoreError::UnknownBookmark(_))
    ));
    repo.import_objects(&[(
        "bookmark_update".into(),
        root_id.to_vec(),
        root.encode().unwrap(),
    )])
    .unwrap();
    assert_eq!(
        repo.bookmark(MAIN).unwrap().name,
        Field::Value("trunk".into())
    );

    // A move to a commit that has not arrived waits for it.
    let unknown = CommitId([0xEE; 32]);
    let heads = repo.bookmark(MAIN).unwrap().heads;
    arrive(
        &mut repo,
        Subject::Bookmark(MAIN),
        &heads,
        moved_to(unknown, Some(c1)),
        3,
    );
    assert_eq!(repo.bookmark(MAIN).unwrap().target, Field::Value(c1));
}
