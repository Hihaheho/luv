//! What push sends, what a purge erases, what garbage collection removes.

#![cfg(feature = "store")]

use std::cell::Cell;
use std::rc::Rc;

use base64::Engine as _;
use luvcs::api::{HostContext, VcsErrorCode, VcsObjectView, VcsRequest, VcsResponse, execute};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::remote::{DirRemote, MemoryRemote, Remote, RemoteError};
use luvcs::sync::{PushReport, SyncError, pull, push};
use luvcs_protocol::EntryBody;
use rusqlite::{Connection, params};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

type Random = Box<dyn Fn(&mut [u8]) -> Result<(), String>>;

struct Device {
    conn: Connection,
    seed: Rc<Cell<u8>>,
    random: Random,
    clock: Cell<i64>,
}

impl Device {
    fn new(seed: u8) -> Self {
        let counter = Rc::new(Cell::new(seed));
        let c = counter.clone();
        Self {
            conn: Connection::open_in_memory().unwrap(),
            seed: counter,
            random: Box::new(move |out: &mut [u8]| {
                let n = c.get().wrapping_add(1);
                c.set(n);
                out.fill(n);
                Ok(())
            }),
            clock: Cell::new(1_700_000_000_000),
        }
    }

    fn ctx(&self) -> HostContext<'_> {
        self.clock.set(self.clock.get() + 1);
        HostContext {
            digest: &RustDigest,
            random: &self.random,
            now_unix_ms: self.clock.get(),
            device_id: format!("dev-{}", self.seed.get()),
            actor_kind: ActorKind::User,
        }
    }

    fn try_run(&self, request: VcsRequest) -> VcsResponse {
        execute(&self.conn, &self.ctx(), request)
    }

    fn run(&self, request: VcsRequest) -> VcsResponse {
        let r = self.try_run(request);
        if let VcsResponse::Error { error } = &r {
            panic!("{}: {}", error.code, error.message);
        }
        r
    }

    fn create(&self, name: &str) -> String {
        match self.run(VcsRequest::CreateRepo { name: name.into() }) {
            VcsResponse::Repo { repo } => repo.repo_id,
            other => panic!("{other:?}"),
        }
    }

    fn save_text(&self, repo: &str, object: Option<&str>, text: &str) -> String {
        match self.run(VcsRequest::SaveText {
            repo_id: repo.into(),
            object_id: object.map(Into::into),
            type_id: "doc".into(),
            text: text.into(),
            title: Some("a.md".into()),
        }) {
            VcsResponse::Object { object } => object.object_id,
            other => panic!("{other:?}"),
        }
    }

    fn delete(&self, repo: &str, object: &str) {
        self.run(VcsRequest::DeleteObject {
            repo_id: repo.into(),
            object_id: object.into(),
        });
    }

    fn record(&self, repo: &str, message: &str) -> String {
        match self.run(VcsRequest::Record {
            repo_id: repo.into(),
            description: message.into(),
            objects: None,
        }) {
            VcsResponse::Commit { commit } => commit.commit_id,
            other => panic!("{other:?}"),
        }
    }

    fn object_at(&self, repo: &str, object: &str, commit: Option<&str>) -> VcsObjectView {
        match self.run(VcsRequest::ReadObject {
            repo_id: repo.into(),
            object_id: object.into(),
            commit_id: commit.map(Into::into),
        }) {
            VcsResponse::Object { object } => object,
            other => panic!("{other:?}"),
        }
    }

    fn purge(&self, repo: &str, objects: &[&str], blobs: &[&str]) -> VcsResponse {
        self.try_run(VcsRequest::Purge {
            repo_id: repo.into(),
            object_ids: objects.iter().map(|s| s.to_string()).collect(),
            blob_ids: blobs.iter().map(|s| s.to_string()).collect(),
            reason: "erasure request".into(),
        })
    }

    fn gc(&self, repo: &str, keep_operations: Option<u32>) -> (u32, u32, u32) {
        match self.run(VcsRequest::Gc {
            repo_id: repo.into(),
            keep_operations,
        }) {
            VcsResponse::Collected {
                objects,
                blobs,
                operations,
            } => (objects, blobs, operations),
            other => panic!("{other:?}"),
        }
    }

    fn push(&self, remote: &dyn Remote, repo: &str) -> PushReport {
        push(&self.conn, &self.ctx(), remote, "r", repo).unwrap()
    }

    fn pull(&self, remote: &dyn Remote) {
        pull(&self.conn, &self.ctx(), remote, "r").unwrap();
    }

    /// Whether the store holds a body with exactly these bytes.
    fn holds(&self, bytes: &[u8]) -> bool {
        self.conn
            .query_row(
                "SELECT count(*) > 0 FROM vcs_objects WHERE kind = 'content' AND bytes = ?1",
                params![bytes],
                |r| r.get(0),
            )
            .unwrap()
    }
}

/// Whether any object in the remote's log carries these exact bytes.
fn log_holds(remote: &dyn Remote, bytes: &[u8]) -> bool {
    remote.read(0, 256).unwrap().iter().any(|e| match &e.body {
        EntryBody::Objects { objects, .. } => objects
            .iter()
            .any(|o| B64.decode(&o.bytes_base64).unwrap() == bytes),
        _ => false,
    })
}

#[test]
fn push_sends_the_shared_history_and_nothing_else() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "draft one\n");
    a.save_text(&repo, Some(&object), "kept\n");
    a.record(&repo, "first");
    a.save_text(&repo, Some(&object), "amended away\n");
    a.record(&repo, "second");
    a.save_text(&repo, Some(&object), "second, amended\n");
    a.run(VcsRequest::Amend {
        repo_id: repo.clone(),
        description: "second".into(),
    });
    a.save_text(&repo, Some(&object), "not recorded\n");
    a.push(&remote, &repo);

    assert!(log_holds(&remote, b"kept\n"));
    assert!(log_holds(&remote, b"second, amended\n"));
    for local in [&b"draft one\n"[..], b"amended away\n", b"not recorded\n"] {
        assert!(a.holds(local));
        assert!(!log_holds(&remote, local));
    }

    // A later push sends only what is new, and a second device gets a
    // complete history from it.
    a.save_text(&repo, Some(&object), "third\n");
    let third = a.record(&repo, "third");
    let report = a.push(&remote, &repo);
    assert!(report.objects > 0 && report.objects < 10, "{report:?}");
    let b = Device::new(100);
    b.pull(&remote);
    assert_eq!(
        b.object_at(&repo, &object, Some(&third)).text.as_deref(),
        Some("third\n")
    );
}

#[test]
fn a_bookmark_on_a_hidden_commit_brings_that_commit() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "on the bookmark\n");
    let first = a.record(&repo, "first");
    a.run(VcsRequest::CreateBookmark {
        repo_id: repo.clone(),
        name: "release".into(),
        commit_id: first.clone(),
    });
    // Amending moves the bookmark along, but its first update still names
    // the amended-away commit: without it a puller would leave the whole
    // bookmark out.
    a.save_text(&repo, Some(&object), "amended\n");
    let amended = match a.run(VcsRequest::Amend {
        repo_id: repo.clone(),
        description: "first".into(),
    }) {
        VcsResponse::Commit { commit } => commit.commit_id,
        other => panic!("{other:?}"),
    };
    assert_eq!(remote.head().unwrap(), 0);
    a.push(&remote, &repo);
    let b = Device::new(100);
    b.pull(&remote);
    match b.run(VcsRequest::ListBookmarks {
        repo_id: repo.clone(),
    }) {
        VcsResponse::Bookmarks { bookmarks } => {
            assert_eq!(bookmarks.bookmarks.len(), 1);
            assert_eq!(
                bookmarks.bookmarks[0].target_commit_id.as_deref(),
                Some(amended.as_str())
            );
        }
        other => panic!("{other:?}"),
    }
    match b.run(VcsRequest::Heads {
        repo_id: repo.clone(),
    }) {
        VcsResponse::Heads { heads } => assert_eq!(heads, vec![amended]),
        other => panic!("{other:?}"),
    }
}

fn purge_reaches_every_device_and_the_remote(remote: &dyn Remote) {
    let a = Device::new(0);
    let repo = a.create("crm");
    let customer = a.save_text(&repo, None, "name: Alice\n");
    a.save_text(&repo, Some(&customer), "name: Alice\nphone: 555\n");
    let old = a.record(&repo, "add Alice");
    let other = a.save_text(&repo, None, "unrelated\n");
    a.record(&repo, "unrelated");
    a.push(remote, &repo);
    let b = Device::new(100);
    b.pull(remote);
    assert!(b.holds(b"name: Alice\nphone: 555\n"));

    // Live objects are refused.
    match a.purge(&repo, &[&customer], &[]) {
        VcsResponse::Error { error } => {
            assert_eq!(error.code, VcsErrorCode::Invalid);
            assert!(error.message.contains("not deleted"), "{}", error.message);
        }
        other => panic!("{other:?}"),
    }

    a.delete(&repo, &customer);
    a.record(&repo, "forget Alice");
    match a.purge(&repo, &[&customer], &[]) {
        VcsResponse::Purged {
            redaction_ids,
            contents,
            kept_shared,
            ..
        } => {
            assert_eq!(redaction_ids.len(), 1);
            // The pushed version; the draft "name: Alice\n" never left a's
            // working tree but is erased too.
            assert_eq!((contents, kept_shared), (2, 0));
        }
        other => panic!("{other:?}"),
    }
    assert!(!a.holds(b"name: Alice\nphone: 555\n"));
    let view = a.object_at(&repo, &customer, Some(&old));
    assert!(view.redacted);
    assert_eq!(view.text, None);
    assert_eq!(view.title.as_deref(), Some("a.md"));
    // Other objects are untouched.
    assert_eq!(
        a.object_at(&repo, &other, None).text.as_deref(),
        Some("unrelated\n")
    );
    // Purging again finds nothing left.
    match a.purge(&repo, &[&customer], &[]) {
        VcsResponse::Purged {
            redaction_ids,
            contents,
            ..
        } => assert_eq!((redaction_ids.len(), contents), (0, 0)),
        other => panic!("{other:?}"),
    }

    assert!(log_holds(remote, b"name: Alice\nphone: 555\n"));
    let report = a.push(remote, &repo);
    assert_eq!(report.erased, 1);
    assert!(!log_holds(remote, b"name: Alice\nphone: 555\n"));
    assert!(log_holds(remote, b"unrelated\n"));

    // A device that had the bytes drops them when it pulls; its next push
    // asks the remote again, which changes nothing.
    b.pull(remote);
    assert!(!b.holds(b"name: Alice\nphone: 555\n"));
    assert!(b.object_at(&repo, &customer, Some(&old)).redacted);
    assert_eq!(b.push(remote, &repo).erased, 1);
    assert_eq!(b.push(remote, &repo).erased, 0);

    // A device that starts now reads the whole history without the bytes.
    let c = Device::new(200);
    c.pull(remote);
    let view = c.object_at(&repo, &customer, Some(&old));
    assert!(view.redacted);
    assert_eq!(view.text, None);
}

#[test]
fn purge_reaches_every_device_and_a_memory_remote() {
    purge_reaches_every_device_and_the_remote(&MemoryRemote::new());
}

#[test]
fn purge_reaches_every_device_and_a_directory_remote() {
    let dir = tempfile::tempdir().unwrap();
    purge_reaches_every_device_and_the_remote(&DirRemote::open(dir.path()).unwrap());
}

#[test]
fn a_body_another_object_shares_is_kept() {
    let a = Device::new(0);
    let repo = a.create("notes");
    let one = a.save_text(&repo, None, "same\n");
    let two = a.save_text(&repo, None, "same\n");
    a.record(&repo, "two copies");
    a.delete(&repo, &one);
    a.record(&repo, "drop one");
    match a.purge(&repo, &[&one], &[]) {
        VcsResponse::Purged {
            redaction_ids,
            contents,
            kept_shared,
            ..
        } => assert_eq!((redaction_ids.len(), contents, kept_shared), (0, 0, 1)),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        a.object_at(&repo, &two, None).text.as_deref(),
        Some("same\n")
    );
}

#[test]
fn a_purge_erases_a_record_and_keeps_its_schema() {
    let a = Device::new(0);
    let repo = a.create("notes");
    let schema_id = match a.run(VcsRequest::PutSchema {
        repo_id: repo.clone(),
        schema_eure: "secret = `text`\n".into(),
    }) {
        VcsResponse::Schema { schema_id, .. } => schema_id,
        other => panic!("{other:?}"),
    };
    let save = || match a.run(VcsRequest::SaveRecord {
        repo_id: repo.clone(),
        object_id: None,
        type_id: "note".into(),
        schema_id: schema_id.clone(),
        record_eure: "secret = \"s3cr3t\"\n".into(),
        title: None,
    }) {
        VcsResponse::Object { object } => object.object_id,
        other => panic!("{other:?}"),
    };
    let id = save();
    let commit = a.record(&repo, "a record");
    a.delete(&repo, &id);
    a.record(&repo, "drop it");
    match a.purge(&repo, &[&id], &[]) {
        VcsResponse::Purged { contents, .. } => assert_eq!(contents, 1),
        other => panic!("{other:?}"),
    }
    let old = a.object_at(&repo, &id, Some(&commit));
    assert!(old.redacted);
    assert_eq!(old.record_eure, None);
    // The schema is no body of the purged record: it stays, and serves the
    // next record.
    match a.run(VcsRequest::ReadSchema {
        repo_id: repo.clone(),
        schema_id: schema_id.clone(),
    }) {
        VcsResponse::Schema { .. } => {}
        other => panic!("{other:?}"),
    }
    save();
}

#[test]
fn bytes_written_again_after_a_purge_travel_again() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("crm");
    let customer = a.save_text(&repo, None, "name: Alice\n");
    a.record(&repo, "add");
    a.push(&remote, &repo);
    let b = Device::new(100);
    b.pull(&remote);
    a.delete(&repo, &customer);
    a.record(&repo, "forget");
    a.purge(&repo, &[&customer], &[]);
    a.push(&remote, &repo);

    // Alice comes back as a new customer with the same text: new data.
    let again = a.save_text(&repo, None, "name: Alice\n");
    let back = a.record(&repo, "Alice is back");
    a.push(&remote, &repo);
    assert!(log_holds(&remote, b"name: Alice\n"));
    b.pull(&remote);
    assert_eq!(
        b.object_at(&repo, &again, Some(&back)).text.as_deref(),
        Some("name: Alice\n")
    );
    // A device pulling the whole log, erasure included, ends the same way.
    let c = Device::new(200);
    c.pull(&remote);
    assert_eq!(
        c.object_at(&repo, &again, Some(&back)).text.as_deref(),
        Some("name: Alice\n")
    );
    // And b repeating the erase later does not take it away again.
    b.push(&remote, &repo);
    assert!(log_holds(&remote, b"name: Alice\n"));
}

#[test]
fn erased_bytes_repeated_in_a_draft_stay_on_the_device() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("crm");
    let customer = a.save_text(&repo, None, "name: Alice\n");
    a.record(&repo, "add");
    a.push(&remote, &repo);
    a.delete(&repo, &customer);
    a.record(&repo, "forget");
    a.purge(&repo, &[&customer], &[]);
    a.push(&remote, &repo);
    a.save_text(&repo, None, "name: Alice\n");
    a.push(&remote, &repo);
    assert!(!log_holds(&remote, b"name: Alice\n"));
}

#[test]
fn the_same_object_written_again_with_the_same_bytes_travels_again() {
    // A host that derives object IDs from its own keys writes the very
    // state it wrote before: the remote already has that state, not the
    // body under it any more.
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("crm");
    let customer = a.save_text(&repo, None, "name: Alice\n");
    a.record(&repo, "add");
    a.push(&remote, &repo);
    a.delete(&repo, &customer);
    a.record(&repo, "forget");
    a.purge(&repo, &[&customer], &[]);
    a.push(&remote, &repo);
    a.save_text(&repo, Some(&customer), "name: Alice\n");
    let back = a.record(&repo, "back");
    a.push(&remote, &repo);
    let c = Device::new(200);
    c.pull(&remote);
    let view = c.object_at(&repo, &customer, Some(&back));
    assert_eq!(view.text.as_deref(), Some("name: Alice\n"));
    assert!(!view.redacted);
}

#[test]
fn an_edit_concurrent_with_a_purge_still_integrates() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("crm");
    let customer = a.save_text(&repo, None, "name: Alice\n");
    a.record(&repo, "add");
    a.push(&remote, &repo);
    let b = Device::new(100);
    b.pull(&remote);

    // a deletes and purges; b, unaware, edits.
    a.delete(&repo, &customer);
    a.record(&repo, "forget");
    a.purge(&repo, &[&customer], &[]);
    a.push(&remote, &repo);
    b.save_text(&repo, Some(&customer), "name: Alice\nnote: called\n");
    b.record(&repo, "edit");
    b.pull(&remote);

    let heads = match b.run(VcsRequest::Heads {
        repo_id: repo.clone(),
    }) {
        VcsResponse::Heads { heads } => heads,
        other => panic!("{other:?}"),
    };
    let status = match b.run(VcsRequest::Status {
        repo_id: repo.clone(),
    }) {
        VcsResponse::Status { status } => status,
        other => panic!("{other:?}"),
    };
    let theirs = heads
        .iter()
        .find(|h| Some(h.as_str()) != status.base_commit_id.as_deref())
        .unwrap();
    match b.run(VcsRequest::Integrate {
        repo_id: repo.clone(),
        commit_id: theirs.clone(),
    }) {
        VcsResponse::Integrated { conflicts, .. } => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].terms[0].kind, "deleted_vs_edited");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn pinned_blobs_travel_and_a_purge_erases_them() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("files");
    let blob = match a.run(VcsRequest::PutBlob {
        repo_id: repo.clone(),
        data_base64: B64.encode(b"scan of a passport"),
    }) {
        VcsResponse::Blob { blob } => blob.blob_id,
        other => panic!("{other:?}"),
    };
    a.save_text(&repo, None, &format!("attachment: {blob}\n"));
    a.record(&repo, "attach");
    assert_eq!(a.push(&remote, &repo).blobs, 1);
    assert_eq!(a.gc(&repo, Some(0)).1, 0);
    let b = Device::new(100);
    b.pull(&remote);
    let read = |d: &Device| {
        d.try_run(VcsRequest::ReadBlob {
            repo_id: repo.clone(),
            blob_id: blob.clone(),
            offset: None,
            length: None,
        })
    };
    assert!(matches!(read(&b), VcsResponse::BlobBytes { .. }));

    match a.purge(&repo, &[], &[&blob]) {
        VcsResponse::Purged { blobs, .. } => assert_eq!(blobs, 1),
        other => panic!("{other:?}"),
    }
    a.push(&remote, &repo);
    b.pull(&remote);
    for d in [&a, &b] {
        match read(d) {
            VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Redacted),
            other => panic!("{other:?}"),
        }
    }
    let c = Device::new(200);
    c.pull(&remote);
    assert!(matches!(read(&c), VcsResponse::Error { .. }));
}

#[test]
fn binary_bodies_of_a_purged_object_are_erased() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("files");
    let object = match a.run(VcsRequest::SaveBinary {
        repo_id: repo.clone(),
        object_id: None,
        type_id: "image".into(),
        data_base64: B64.encode(b"photo"),
        title: Some("p.jpg".into()),
    }) {
        VcsResponse::Object { object } => object.object_id,
        other => panic!("{other:?}"),
    };
    let old = a.record(&repo, "photo");
    a.push(&remote, &repo);
    a.delete(&repo, &object);
    a.record(&repo, "remove photo");
    match a.purge(&repo, &[&object], &[]) {
        VcsResponse::Purged { blobs, .. } => assert_eq!(blobs, 1),
        other => panic!("{other:?}"),
    }
    a.push(&remote, &repo);
    let c = Device::new(200);
    c.pull(&remote);
    let view = c.object_at(&repo, &object, Some(&old));
    assert!(view.redacted);
    assert!(!view.blob.unwrap().present);
}

/// A remote that cannot rewrite its log: the default `erase`.
struct AppendOnly(MemoryRemote);

impl Remote for AppendOnly {
    fn head(&self) -> luvcs::remote::RemoteResult<u64> {
        self.0.head()
    }
    fn read(
        &self,
        after: u64,
        limit: u32,
    ) -> luvcs::remote::RemoteResult<Vec<luvcs_protocol::Entry>> {
        self.0.read(after, limit)
    }
    fn append(&self, entries: &[EntryBody]) -> luvcs::remote::RemoteResult<u64> {
        self.0.append(entries)
    }
    fn put_blob_chunk(
        &self,
        repo: &str,
        blob: &str,
        index: u32,
        bytes: &[u8],
    ) -> luvcs::remote::RemoteResult<()> {
        self.0.put_blob_chunk(repo, blob, index, bytes)
    }
    fn get_blob_chunk(
        &self,
        repo: &str,
        blob: &str,
        index: u32,
    ) -> luvcs::remote::RemoteResult<Option<Vec<u8>>> {
        self.0.get_blob_chunk(repo, blob, index)
    }
}

#[test]
fn a_remote_that_cannot_erase_is_reported_and_asked_again() {
    let remote = AppendOnly(MemoryRemote::new());
    let a = Device::new(0);
    let repo = a.create("crm");
    let customer = a.save_text(&repo, None, "name: Alice\n");
    a.record(&repo, "add");
    a.push(&remote, &repo);
    a.delete(&repo, &customer);
    a.record(&repo, "forget");
    a.purge(&repo, &[&customer], &[]);
    for _ in 0..2 {
        let error = push(&a.conn, &a.ctx(), &remote, "r", &repo).unwrap_err();
        assert!(
            matches!(&error, SyncError::Remote(RemoteError::Refused { code, .. }) if code == "unsupported"),
            "{error}"
        );
    }
    // The redaction itself was pushed once.
    let redactions = remote
        .read(0, 256)
        .unwrap()
        .iter()
        .flat_map(|e| match &e.body {
            EntryBody::Objects { objects, .. } => objects.clone(),
            _ => Vec::new(),
        })
        .filter(|o| o.kind == "redaction")
        .count();
    assert_eq!(redactions, 1);
}

#[test]
fn garbage_collection_removes_local_drafts_only() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "draft\n");
    a.save_text(&repo, Some(&object), "pushed\n");
    a.record(&repo, "first");
    a.push(&remote, &repo);
    // Rewritten after the push: the old version is on the remote, so kept.
    a.save_text(&repo, Some(&object), "pushed, amended\n");
    a.run(VcsRequest::Amend {
        repo_id: repo.clone(),
        description: "first".into(),
    });
    a.save_text(&repo, Some(&object), "unrecorded\n");

    // With the operation log kept, undo can still reach every draft.
    let (removed, _, operations) = a.gc(&repo, None);
    assert_eq!((removed, operations), (0, 0));
    assert!(a.holds(b"draft\n"));

    let (removed, _, operations) = a.gc(&repo, Some(0));
    assert!(removed > 0);
    assert!(operations > 0);
    assert!(!a.holds(b"draft\n"));
    for kept in [&b"pushed\n"[..], b"pushed, amended\n", b"unrecorded\n"] {
        assert!(a.holds(kept));
    }
    // Everything still reads, records and syncs.
    assert_eq!(
        a.object_at(&repo, &object, None).text.as_deref(),
        Some("unrecorded\n")
    );
    let last = a.record(&repo, "after gc");
    a.push(&remote, &repo);
    let b = Device::new(100);
    b.pull(&remote);
    assert_eq!(
        b.object_at(&repo, &object, Some(&last)).text.as_deref(),
        Some("unrecorded\n")
    );
    assert_eq!(a.gc(&repo, Some(0)).0, 0);
}

#[test]
fn undo_still_works_after_garbage_collection_that_keeps_operations() {
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "one\n");
    a.record(&repo, "one");
    a.save_text(&repo, Some(&object), "two\n");
    a.record(&repo, "two");
    a.gc(&repo, Some(2));
    a.run(VcsRequest::Undo {
        repo_id: repo.clone(),
    });
    a.run(VcsRequest::Undo {
        repo_id: repo.clone(),
    });
    assert_eq!(
        a.object_at(&repo, &object, None).text.as_deref(),
        Some("one\n")
    );
    // Older operations were forgotten.
    match a.try_run(VcsRequest::Undo {
        repo_id: repo.clone(),
    }) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }
}
