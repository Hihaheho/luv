//! Push and pull between two devices through a remote.

#![cfg(feature = "store")]

use std::cell::Cell;
use std::rc::Rc;

use base64::Engine as _;
use luvcs::api::{HostContext, VcsRequest, VcsResponse, execute};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::remote::{DirRemote, MemoryRemote, Remote};
use luvcs::store::SchemaRegistry;
use luvcs::sync::{SyncError, pull, push};
use luvcs_protocol::{EntryBody, WireObject};
use rusqlite::Connection;

type Random = Box<dyn Fn(&mut [u8]) -> Result<(), String>>;

/// One device: its own store and its own source of randomness and time.
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
            schemas: SchemaRegistry::default(),
        }
    }

    fn run(&self, request: VcsRequest) -> VcsResponse {
        let r = execute(&self.conn, &self.ctx(), request);
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

    fn heads(&self, repo: &str) -> Vec<String> {
        match self.run(VcsRequest::Heads {
            repo_id: repo.into(),
        }) {
            VcsResponse::Heads { mut heads } => {
                heads.sort();
                heads
            }
            other => panic!("{other:?}"),
        }
    }

    fn text_at(&self, repo: &str, object: &str, commit: &str) -> String {
        match self.run(VcsRequest::ReadObject {
            repo_id: repo.into(),
            object_id: object.into(),
            commit_id: Some(commit.into()),
        }) {
            VcsResponse::Object { object } => object.text.expect("a text object"),
            other => panic!("{other:?}"),
        }
    }

    fn push(&self, remote: &dyn Remote, repo: &str) -> luvcs::sync::PushReport {
        push(&self.conn, &self.ctx(), remote, "r", repo).unwrap()
    }

    fn pull(&self, remote: &dyn Remote) -> luvcs::sync::PullReport {
        pull(&self.conn, &self.ctx(), remote, "r").unwrap()
    }
}

#[test]
fn a_second_device_receives_history_through_the_remote() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "hello\n");
    let commit = a.record(&repo, "first");

    let pushed = a.push(&remote, &repo);
    assert!(pushed.objects > 0);
    assert!(pushed.heads_announced);

    let b = Device::new(100);
    let pulled = b.pull(&remote);
    assert_eq!(pulled.repos, vec![repo.clone()]);
    assert_eq!(pulled.head, pushed.head);
    assert_eq!(b.heads(&repo), vec![commit.clone()]);
    assert_eq!(b.text_at(&repo, &object, &commit), "hello\n");

    // The new device can build on what it received.
    b.save_text(&repo, Some(&object), "hello\nworld\n");
    let second = b.record(&repo, "second");
    b.push(&remote, &repo);
    a.pull(&remote);
    assert_eq!(a.heads(&repo), vec![second.clone()]);
    assert_eq!(a.text_at(&repo, &object, &second), "hello\nworld\n");
}

#[test]
fn pushing_and_pulling_again_changes_nothing() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    a.save_text(&repo, None, "x\n");
    a.record(&repo, "one");
    let first = a.push(&remote, &repo);
    let again = a.push(&remote, &repo);
    assert_eq!(
        (again.objects, again.blobs, again.heads_announced),
        (0, 0, false)
    );
    assert_eq!(again.head, first.head);
    // A device pulling its own entries takes in no new repo and no new head.
    let own = a.pull(&remote);
    assert!(own.repos.is_empty());
    assert_eq!(a.heads(&repo).len(), 1);
    assert_eq!(a.pull(&remote).entries, 0);
}

#[test]
fn concurrent_commits_become_two_heads_everywhere() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "base\n");
    let base = a.record(&repo, "base");
    a.push(&remote, &repo);
    let b = Device::new(100);
    b.pull(&remote);

    // Both devices commit on top of `base` before either hears of the other.
    a.save_text(&repo, Some(&object), "base\nfrom a\n");
    let ca = a.record(&repo, "a");
    b.save_text(&repo, Some(&object), "base\nfrom b\n");
    let cb = b.record(&repo, "b");
    a.push(&remote, &repo);
    b.push(&remote, &repo);
    a.pull(&remote);
    b.pull(&remote);

    let mut expected = vec![ca, cb];
    expected.sort();
    assert_eq!(a.heads(&repo), expected);
    assert_eq!(b.heads(&repo), expected);
    assert!(!expected.contains(&base));
}

#[test]
fn binary_objects_travel_with_their_bytes() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("pics");
    // Larger than one chunk, so the upload is split.
    let bytes: Vec<u8> = (0..(5 * 1024 * 1024u32)).map(|i| (i % 251) as u8).collect();
    let object = match a.run(VcsRequest::SaveBinary {
        repo_id: repo.clone(),
        object_id: None,
        type_id: "image".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        title: Some("p.bin".into()),
    }) {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    };
    let blob_id = object.blob.as_ref().unwrap().blob_id.clone();
    a.record(&repo, "pic");
    assert_eq!(a.push(&remote, &repo).blobs, 1);

    let b = Device::new(100);
    assert_eq!(b.pull(&remote).blobs, 1);
    match b.run(VcsRequest::ReadBlob {
        repo_id: repo,
        blob_id,
        offset: None,
        length: None,
    }) {
        VcsResponse::BlobBytes { data_base64, .. } => {
            let got = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .unwrap();
            assert_eq!(got, bytes);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn objects_that_do_not_hash_to_their_ids_are_refused() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    a.save_text(&repo, None, "x\n");
    a.record(&repo, "one");
    a.push(&remote, &repo);

    // A hostile remote rewrites the bytes of an object in the log.
    let mut entries = remote.read(0, 100).unwrap();
    let tampered = remote_with(&mut entries, |objects| {
        let o = &mut objects[0];
        o.bytes_base64 = base64::engine::general_purpose::STANDARD.encode(b"forged");
    });
    let b = Device::new(100);
    let err = pull(&b.conn, &b.ctx(), &tampered, "r").unwrap_err();
    assert!(matches!(err, SyncError::Store(_)), "{err}");
    // Nothing of it was taken in, and the cursor did not pass the bad entry.
    let n: i64 = b
        .conn
        .query_row("SELECT count(*) FROM vcs_objects", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    let cursor: i64 = b
        .conn
        .query_row("SELECT last_index FROM vcs_sync_cursor", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cursor, 1, "only the repo creation was applied");
}

/// A copy of the log in which the first objects entry is changed by `edit`.
fn remote_with(
    entries: &mut [luvcs_protocol::Entry],
    edit: impl FnOnce(&mut Vec<WireObject>),
) -> MemoryRemote {
    let mut edit = Some(edit);
    for e in entries.iter_mut() {
        if let EntryBody::Objects { objects, .. } = &mut e.body
            && let Some(edit) = edit.take()
        {
            edit(objects);
        }
    }
    let copy = MemoryRemote::new();
    let bodies: Vec<EntryBody> = entries.iter().map(|e| e.body.clone()).collect();
    copy.append(&bodies).unwrap();
    copy
}

#[test]
fn a_repo_with_a_different_key_is_not_merged_into() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let repo = a.create("notes");
    a.push(&remote, &repo);
    // The same repo ID with another key in the log.
    let mut bodies: Vec<EntryBody> = remote
        .read(0, 10)
        .unwrap()
        .into_iter()
        .map(|e| e.body)
        .collect();
    if let EntryBody::RepoCreated { id_key_base64, .. } = &mut bodies[0] {
        *id_key_base64 = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    }
    let forged = MemoryRemote::new();
    forged.append(&bodies).unwrap();
    let err = pull(&a.conn, &a.ctx(), &forged, "forged").unwrap_err();
    assert!(matches!(err, SyncError::Log(_)), "{err}");
}

#[test]
fn a_directory_remote_syncs_two_devices() {
    let dir = tempfile::tempdir().unwrap();
    let remote_a = DirRemote::open(dir.path()).unwrap();
    let remote_b = DirRemote::open(dir.path()).unwrap();
    let a = Device::new(0);
    let repo = a.create("notes");
    let object = a.save_text(&repo, None, "via dir\n");
    let commit = a.record(&repo, "first");
    a.push(&remote_a, &repo);

    let b = Device::new(100);
    b.pull(&remote_b);
    assert_eq!(b.text_at(&repo, &object, &commit), "via dir\n");
}

/// A remote whose blob store can be switched off, to model a dropped
/// connection in the middle of a pull.
struct FlakyBlobs {
    inner: MemoryRemote,
    down: Cell<bool>,
}

impl Remote for FlakyBlobs {
    fn head(&self) -> luvcs::remote::RemoteResult<u64> {
        self.inner.head()
    }
    fn read(
        &self,
        after: u64,
        limit: u32,
    ) -> luvcs::remote::RemoteResult<Vec<luvcs_protocol::Entry>> {
        self.inner.read(after, limit)
    }
    fn append(&self, entries: &[EntryBody]) -> luvcs::remote::RemoteResult<u64> {
        self.inner.append(entries)
    }
    fn put_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
        bytes: &[u8],
    ) -> luvcs::remote::RemoteResult<()> {
        self.inner.put_blob_chunk(repo_id, blob_id, index, bytes)
    }
    fn get_blob_chunk(
        &self,
        repo_id: &str,
        blob_id: &str,
        index: u32,
    ) -> luvcs::remote::RemoteResult<Option<Vec<u8>>> {
        if self.down.get() {
            return Err(luvcs::remote::RemoteError::Io("connection dropped".into()));
        }
        self.inner.get_blob_chunk(repo_id, blob_id, index)
    }
}

#[test]
fn a_blob_that_failed_to_download_is_fetched_by_the_next_pull() {
    let remote = FlakyBlobs {
        inner: MemoryRemote::new(),
        down: Cell::new(false),
    };
    let a = Device::new(0);
    let repo = a.create("pics");
    let bytes = vec![7u8; 1000];
    let object = match a.run(VcsRequest::SaveBinary {
        repo_id: repo.clone(),
        object_id: None,
        type_id: "image".into(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        title: Some("p.bin".into()),
    }) {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    };
    let blob_id = object.blob.unwrap().blob_id;
    a.record(&repo, "pic");
    a.push(&remote, &repo);

    let b = Device::new(100);
    remote.down.set(true);
    assert!(pull(&b.conn, &b.ctx(), &remote, "r").is_err());

    // The log was taken in, so the next pull has no entries to read; it
    // must still fetch the blob it was told about.
    remote.down.set(false);
    let report = b.pull(&remote);
    assert_eq!((report.entries, report.blobs), (0, 1));
    match b.run(VcsRequest::ReadBlob {
        repo_id: repo,
        blob_id,
        offset: None,
        length: None,
    }) {
        VcsResponse::BlobBytes { data_base64, .. } => {
            let got = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .unwrap();
            assert_eq!(got, bytes);
        }
        other => panic!("{other:?}"),
    }
    // And once fetched it is not fetched again.
    assert_eq!(b.pull(&remote).blobs, 0);
}
