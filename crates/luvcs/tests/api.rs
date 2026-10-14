//! The request API: what every host drives luv through.

#![cfg(feature = "store")]

use std::cell::Cell;
use std::rc::Rc;

use luvcs::api::{
    HostContext, VcsError, VcsErrorCode, VcsRequest, VcsResolution, VcsResponse, execute,
    record_from_eure,
};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::record::RecordValue;
use rusqlite::Connection;

const CARD_SCHEMA: &str = r#"
title = `text`
title.$merge = "text"
count = `integer`
count.$optional = true
price = `decimal`
price.$optional = true
tags {
  $variant: set
  item = `text`
}
tags.$optional = true
"#;

type RandomFn = dyn Fn(&mut [u8]) -> Result<(), String>;

struct Host {
    counter: Rc<Cell<u8>>,
    random: Box<RandomFn>,
}

impl Host {
    fn ctx(&self) -> HostContext<'_> {
        HostContext {
            digest: &RustDigest,
            random: &self.random,
            now_unix_ms: 1_700_000_000_000 + i64::from(self.counter.get()),
            device_id: "dev-a".into(),
            actor_kind: ActorKind::User,
        }
    }
}

fn host() -> Host {
    let counter = Rc::new(Cell::new(0u8));
    let c = counter.clone();
    Host {
        counter,
        random: Box::new(move |out: &mut [u8]| {
            let n = c.get().wrapping_add(1);
            c.set(n);
            out.fill(n);
            Ok(())
        }),
    }
}

fn run(conn: &Connection, host: &Host, req: VcsRequest) -> VcsResponse {
    let r = execute(conn, &host.ctx(), req);
    if let VcsResponse::Error { error } = &r {
        panic!("{}: {}", error.code, error.message);
    }
    r
}

fn create(conn: &Connection, host: &Host) -> String {
    match run(
        conn,
        host,
        VcsRequest::CreateRepo {
            name: "notes".into(),
        },
    ) {
        VcsResponse::Repo { repo } => repo.repo_id,
        other => panic!("{other:?}"),
    }
}

fn put_schema(conn: &Connection, host: &Host, repo: &str) -> String {
    match run(
        conn,
        host,
        VcsRequest::PutSchema {
            repo_id: repo.into(),
            schema_eure: CARD_SCHEMA.into(),
        },
    ) {
        VcsResponse::Schema { schema_id, .. } => schema_id,
        other => panic!("{other:?}"),
    }
}

#[test]
fn text_lifecycle_record_log_undo() {
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    assert!(matches!(
        run(&conn, &host, VcsRequest::ListRepos),
        VcsResponse::Repos { repos } if repos.len() == 1 && repos[0].name == "notes"
    ));

    let object_id = match run(
        &conn,
        &host,
        VcsRequest::SaveText {
            repo_id: repo.clone(),
            object_id: None,
            type_id: "note".into(),
            text: "hello\n".into(),
            title: Some("Hello".into()),
        },
    ) {
        VcsResponse::Object { object } => {
            assert_eq!(object.text.as_deref(), Some("hello\n"));
            assert_eq!(object.title.as_deref(), Some("Hello"));
            assert_eq!(object.body_kind, "text");
            object.object_id
        }
        other => panic!("{other:?}"),
    };
    assert!(object_id.starts_with("obj_"));

    match run(
        &conn,
        &host,
        VcsRequest::Status {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Status { status } => {
            assert_eq!(status.base_commit_id, None);
            assert_eq!(status.changes.len(), 1);
            assert_eq!(status.changes[0].kind, "added");
            assert_eq!(status.changes[0].object_id, object_id);
            assert!(status.heads.is_empty());
        }
        other => panic!("{other:?}"),
    }

    let c1 = match run(
        &conn,
        &host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: "first".into(),
            objects: None,
        },
    ) {
        VcsResponse::Commit { commit } => {
            assert_eq!(commit.changed_objects, vec![object_id.clone()]);
            assert!(commit.is_head);
            assert!(commit.parents.is_empty());
            commit.commit_id
        }
        other => panic!("{other:?}"),
    };

    // Edit the same object again, keeping its id.
    run(
        &conn,
        &host,
        VcsRequest::SaveText {
            repo_id: repo.clone(),
            object_id: Some(object_id.clone()),
            type_id: "note".into(),
            text: "hello world\n".into(),
            title: None,
        },
    );
    match run(
        &conn,
        &host,
        VcsRequest::Status {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Status { status } => {
            assert_eq!(status.base_commit_id.as_deref(), Some(c1.as_str()));
            assert_eq!(status.changes[0].kind, "changed");
            assert!(status.can_undo);
        }
        other => panic!("{other:?}"),
    }
    // The committed version is still readable by commit.
    match run(
        &conn,
        &host,
        VcsRequest::ReadObject {
            repo_id: repo.clone(),
            object_id: object_id.clone(),
            commit_id: Some(c1.clone()),
        },
    ) {
        VcsResponse::Object { object } => assert_eq!(object.text.as_deref(), Some("hello\n")),
        other => panic!("{other:?}"),
    }

    // Undo the edit: the working tree is back at the commit.
    run(
        &conn,
        &host,
        VcsRequest::Undo {
            repo_id: repo.clone(),
        },
    );
    match run(
        &conn,
        &host,
        VcsRequest::Status {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Status { status } => {
            assert!(status.changes.is_empty());
            assert!(status.can_redo);
        }
        other => panic!("{other:?}"),
    }
    run(
        &conn,
        &host,
        VcsRequest::Redo {
            repo_id: repo.clone(),
        },
    );
    let c2 = match run(
        &conn,
        &host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: "second".into(),
            objects: None,
        },
    ) {
        VcsResponse::Commit { commit } => commit.commit_id,
        other => panic!("{other:?}"),
    };
    match run(
        &conn,
        &host,
        VcsRequest::Log {
            repo_id: repo.clone(),
            from_commit_id: None,
            limit: 10,
        },
    ) {
        VcsResponse::Commits { commits } => {
            let ids: Vec<_> = commits.iter().map(|c| c.commit_id.clone()).collect();
            assert_eq!(ids, vec![c2.clone(), c1.clone()]);
            assert!(commits[0].is_head && !commits[1].is_head);
        }
        other => panic!("{other:?}"),
    }

    // An unknown id is a typed error, not a host failure.
    match execute(
        &conn,
        &host.ctx(),
        VcsRequest::Show {
            repo_id: repo.clone(),
            commit_id: "cmt_nope".into(),
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }
    match execute(
        &conn,
        &host.ctx(),
        VcsRequest::Status {
            repo_id: "repo_AAAAAAAAAAAAAAAAAAAAAA".into(),
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::NotFound),
        other => panic!("{other:?}"),
    }

    // Export / import round-trips the history into a new repo.
    let (archive, key) = match run(
        &conn,
        &host,
        VcsRequest::Export {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Archive {
            archive_base64,
            id_key_base64,
            repo_id,
            ..
        } => {
            assert_eq!(repo_id, repo);
            (archive_base64, id_key_base64)
        }
        other => panic!("{other:?}"),
    };
    let other = Connection::open_in_memory().unwrap();
    let copy = match run(
        &other,
        &host,
        VcsRequest::Import {
            name: "copy".into(),
            archive_base64: archive.clone(),
            id_key_base64: key.clone(),
        },
    ) {
        VcsResponse::Imported { repo: info, .. } => {
            assert_eq!(info.repo_id, repo);
            info.repo_id
        }
        other => panic!("{other:?}"),
    };
    // Importing the same repo twice on one device is refused.
    match execute(
        &other,
        &host.ctx(),
        VcsRequest::Import {
            name: "copy".into(),
            archive_base64: archive,
            id_key_base64: key,
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Conflict),
        other => panic!("{other:?}"),
    }
    match run(&other, &host, VcsRequest::Heads { repo_id: copy }) {
        VcsResponse::Heads { heads } => assert_eq!(heads, vec![c2]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn record_eure_round_trip_and_save() {
    let eure = "title = \"t\"\ntags = #{\"a\", \"b\"}\ncount = 3\nprice = 1.50\n";
    let record = record_from_eure(eure).unwrap();
    assert_eq!(
        record_from_eure("a = !").unwrap_err().code,
        VcsErrorCode::Invalid
    );

    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    let schema_id = put_schema(&conn, &host, &repo);
    assert!(schema_id.starts_with("cnt_"));
    // The same schema is the same ID, however it is written.
    assert_eq!(put_schema(&conn, &host, &repo), schema_id);
    match run(
        &conn,
        &host,
        VcsRequest::ReadSchema {
            repo_id: repo.clone(),
            schema_id: schema_id.clone(),
        },
    ) {
        VcsResponse::Schema { schema_eure, .. } => assert_eq!(
            record_from_eure(&schema_eure).unwrap(),
            record_from_eure(CARD_SCHEMA).unwrap()
        ),
        other => panic!("{other:?}"),
    }
    let bad_schema = execute(
        &conn,
        &host.ctx(),
        VcsRequest::PutSchema {
            repo_id: repo.clone(),
            schema_eure: "a = `text`\na.$merge = \"lines\"\n".into(),
        },
    );
    assert!(
        matches!(bad_schema, VcsResponse::Error { error } if error.code == VcsErrorCode::Invalid)
    );

    let save = |record_eure: &str| {
        execute(
            &conn,
            &host.ctx(),
            VcsRequest::SaveRecord {
                repo_id: repo.clone(),
                object_id: None,
                type_id: "card".into(),
                schema_id: schema_id.clone(),
                record_eure: record_eure.into(),
                title: None,
            },
        )
    };
    match save(eure) {
        VcsResponse::Object { object } => {
            assert_eq!(object.body_kind, "record");
            assert_eq!(object.schema_id.as_deref(), Some(schema_id.as_str()));
            let back = object.record_eure.as_deref().unwrap();
            assert_eq!(record_from_eure(back).unwrap(), record);
            // The decimal keeps the scale it was written with.
            assert!(back.contains("1.50"), "{back}");
        }
        other => panic!("{other:?}"),
    }
    // A record that does not match its schema is refused.
    match save("title = 1\n") {
        VcsResponse::Error { error } => {
            assert_eq!(error.code, VcsErrorCode::Invalid);
            assert!(error.message.contains("title"), "{}", error.message);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn integrate_conflict_and_resolve_by_side() {
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    let save = |text: &str, id: Option<String>| match run(
        &conn,
        &host,
        VcsRequest::SaveText {
            repo_id: repo.clone(),
            object_id: id,
            type_id: "note".into(),
            text: text.into(),
            title: None,
        },
    ) {
        VcsResponse::Object { object } => object.object_id,
        other => panic!("{other:?}"),
    };
    let record = |d: &str| match run(
        &conn,
        &host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: d.into(),
            objects: None,
        },
    ) {
        VcsResponse::Commit { commit } => commit.commit_id,
        other => panic!("{other:?}"),
    };
    let id = save("A\nB\nC\n", None);
    let base = record("base");
    save("A\nB-left\nC\n", Some(id.clone()));
    let left = record("left");
    // The other side: a second workspace at `base`, driven through the store
    // directly (another device would produce this through sync).
    let right = {
        use luvcs::id::{IdKey, ObjectId, OperationId, RepoId, WorkspaceId};
        use luvcs::model::Actor;
        use luvcs::store::{OperationContext, RecordSource, Store};
        let (repo_bytes, key): (Vec<u8>, Vec<u8>) = conn
            .query_row("SELECT repo_id, id_key FROM vcs_local_repos", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let mut store = Store::open(&conn, RustDigest).unwrap();
        let mut r = store
            .repo(
                RepoId::from_bytes(&repo_bytes).unwrap(),
                IdKey(key.try_into().unwrap()),
            )
            .unwrap();
        let ws2 = WorkspaceId([0xEE; 16]);
        r.create_workspace(ws2, Some(base.parse().unwrap()))
            .unwrap();
        let oid: ObjectId = id.parse().unwrap();
        let st = r
            .put_text_state(
                oid,
                "note",
                b"A\nB-right\nC\n",
                luvcs::cbor::Value::Map(vec![]),
            )
            .unwrap();
        let ctx = |n: u8| OperationContext {
            operation_id: OperationId([0xD0 + n; 16]),
            workspace_id: ws2,
            expected_token: None,
            actor: Actor {
                kind: ActorKind::User,
                device_id: "dev-b".into(),
                description: String::new(),
            },
            reason: String::new(),
            now_unix_ms: 5,
        };
        r.save_working(&ctx(1), &[(oid, Some(st))]).unwrap();
        r.record(&ctx(2), RecordSource::Working, "right")
            .unwrap()
            .to_string()
    };
    assert_ne!(left, right);
    let merge_commit = match run(
        &conn,
        &host,
        VcsRequest::Integrate {
            repo_id: repo.clone(),
            commit_id: right.clone(),
        },
    ) {
        VcsResponse::Integrated {
            conflicts,
            commit_id,
        } => {
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0].object_id, id);
            assert_eq!(conflicts[0].driver, "merge.text.v1");
            assert_eq!(conflicts[0].sides.len(), 2);
            commit_id
        }
        other => panic!("{other:?}"),
    };
    match run(
        &conn,
        &host,
        VcsRequest::Status {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Status { status } => {
            assert_eq!(status.conflicts.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    match run(
        &conn,
        &host,
        VcsRequest::Resolve {
            repo_id: repo.clone(),
            object_id: id.clone(),
            resolution: VcsResolution::Side { index: 1 },
        },
    ) {
        VcsResponse::Object { object } => {
            assert_eq!(object.body_kind, "text");
            assert_eq!(object.text.as_deref(), Some("A\nB-right\nC\n"));
        }
        other => panic!("{other:?}"),
    }
    let merged = record("merged");
    match run(
        &conn,
        &host,
        VcsRequest::Show {
            repo_id: repo.clone(),
            commit_id: merge_commit.clone(),
        },
    ) {
        VcsResponse::Commit { commit } => {
            assert_eq!(commit.parents.len(), 2);
            assert!(!commit.is_head);
        }
        other => panic!("{other:?}"),
    }
    match run(
        &conn,
        &host,
        VcsRequest::Show {
            repo_id: repo,
            commit_id: merged,
        },
    ) {
        VcsResponse::Commit { commit } => {
            assert_eq!(commit.parents, vec![merge_commit]);
            assert!(commit.is_head);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn binary_objects_carry_local_blobs_through_export_and_import() {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    let bytes: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 256) as u8).collect();

    let (object, blob) = match run(
        &conn,
        &host,
        VcsRequest::SaveBinary {
            repo_id: repo.clone(),
            object_id: None,
            type_id: "image".into(),
            data_base64: b64.encode(&bytes),
            title: Some("pic".into()),
        },
    ) {
        VcsResponse::Object { object } => {
            assert_eq!(object.body_kind, "binary");
            let blob = object
                .blob
                .clone()
                .expect("binary view carries its blob ref");
            assert_eq!(blob.byte_length, bytes.len() as u64);
            assert!(blob.present);
            assert!(blob.blob_id.starts_with("blob_"), "{}", blob.blob_id);
            (object.object_id, blob)
        }
        other => panic!("{other:?}"),
    };
    // The same bytes dedup to the same blob.
    match run(
        &conn,
        &host,
        VcsRequest::PutBlob {
            repo_id: repo.clone(),
            data_base64: b64.encode(&bytes),
        },
    ) {
        VcsResponse::Blob { blob: again } => assert_eq!(again.blob_id, blob.blob_id),
        other => panic!("{other:?}"),
    }
    match run(
        &conn,
        &host,
        VcsRequest::ListBlobs {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Blobs { blobs } => assert_eq!(blobs.len(), 1),
        other => panic!("{other:?}"),
    }
    // Range and whole reads.
    match run(
        &conn,
        &host,
        VcsRequest::ReadBlob {
            repo_id: repo.clone(),
            blob_id: blob.blob_id.clone(),
            offset: Some(10),
            length: Some(5),
        },
    ) {
        VcsResponse::BlobBytes {
            offset,
            data_base64,
            ..
        } => {
            assert_eq!(offset, 10);
            assert_eq!(b64.decode(data_base64).unwrap(), &bytes[10..15]);
        }
        other => panic!("{other:?}"),
    }
    match execute(
        &conn,
        &host.ctx(),
        VcsRequest::ReadBlob {
            repo_id: repo.clone(),
            blob_id: "blob_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            offset: None,
            length: None,
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::NotFound),
        other => panic!("{other:?}"),
    }

    run(
        &conn,
        &host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: "add pic".into(),
            objects: None,
        },
    );
    let (archive, key) = match run(
        &conn,
        &host,
        VcsRequest::Export {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Archive {
            archive_base64,
            id_key_base64,
            ..
        } => (archive_base64, id_key_base64),
        other => panic!("{other:?}"),
    };
    let other = Connection::open_in_memory().unwrap();
    run(
        &other,
        &host,
        VcsRequest::Import {
            name: "copy".into(),
            archive_base64: archive,
            id_key_base64: key,
        },
    );
    // The imported device has the bytes, and the object reads the same.
    match run(
        &other,
        &host,
        VcsRequest::ReadBlob {
            repo_id: repo.clone(),
            blob_id: blob.blob_id.clone(),
            offset: None,
            length: None,
        },
    ) {
        VcsResponse::BlobBytes { data_base64, .. } => {
            assert_eq!(b64.decode(data_base64).unwrap(), bytes);
        }
        other => panic!("{other:?}"),
    }
    match run(
        &other,
        &host,
        VcsRequest::ListObjects {
            repo_id: repo,
            commit_id: None,
        },
    ) {
        VcsResponse::Objects { objects } => {
            assert_eq!(objects.len(), 1);
            assert_eq!(objects[0].object_id, object);
            assert_eq!(objects[0].blob.as_ref().map(|b| b.present), Some(true));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn record_conflict_and_historical_snapshots_are_readable() {
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    let schema_id = put_schema(&conn, &host, &repo);
    let card = |title: &str| {
        RecordValue::map([
            ("title", RecordValue::text(title)),
            ("count", RecordValue::int(7)),
        ])
    };
    let save = |text: &str, id: Option<String>| match run(
        &conn,
        &host,
        VcsRequest::SaveRecord {
            repo_id: repo.clone(),
            object_id: id,
            type_id: "note".into(),
            schema_id: schema_id.clone(),
            record_eure: card(text).to_eure(),
            title: None,
        },
    ) {
        VcsResponse::Object { object } => object.object_id,
        other => panic!("{other:?}"),
    };
    let record = |d: &str| match run(
        &conn,
        &host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: d.into(),
            objects: None,
        },
    ) {
        VcsResponse::Commit { commit } => commit.commit_id,
        other => panic!("{other:?}"),
    };
    let id = save("A\nB\nC\n", None);
    let base = record("base");
    save("A\nB-left\nC\n", Some(id.clone()));
    let left = record("left");
    // The other side: a second workspace at `base`, driven through the store
    // directly (another device would produce this through sync).
    let right = {
        use luvcs::id::{IdKey, ObjectId, OperationId, RepoId, WorkspaceId};
        use luvcs::model::Actor;
        use luvcs::store::{OperationContext, RecordSource, Store};
        let (repo_bytes, key): (Vec<u8>, Vec<u8>) = conn
            .query_row("SELECT repo_id, id_key FROM vcs_local_repos", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let mut store = Store::open(&conn, RustDigest).unwrap();
        let mut r = store
            .repo(
                RepoId::from_bytes(&repo_bytes).unwrap(),
                IdKey(key.try_into().unwrap()),
            )
            .unwrap();
        let ws2 = WorkspaceId([0xEE; 16]);
        r.create_workspace(ws2, Some(base.parse().unwrap()))
            .unwrap();
        let oid: ObjectId = id.parse().unwrap();
        let st = r
            .put_record_state(
                oid,
                "note",
                schema_id.parse().unwrap(),
                &card("A\nB-right\nC\n"),
                luvcs::cbor::Value::Map(vec![]),
            )
            .unwrap();
        let ctx = |n: u8| OperationContext {
            operation_id: OperationId([0xD0 + n; 16]),
            workspace_id: ws2,
            expected_token: None,
            actor: Actor {
                kind: ActorKind::User,
                device_id: "dev-b".into(),
                description: String::new(),
            },
            reason: String::new(),
            now_unix_ms: 5,
        };
        r.save_working(&ctx(1), &[(oid, Some(st))]).unwrap();
        r.record(&ctx(2), RecordSource::Working, "right")
            .unwrap()
            .to_string()
    };
    assert_ne!(left, right);
    let merge = match run(
        &conn,
        &host,
        VcsRequest::Integrate {
            repo_id: repo.clone(),
            commit_id: right,
        },
    ) {
        VcsResponse::Integrated { commit_id, .. } => commit_id,
        other => panic!("{other:?}"),
    };
    let historical = match run(
        &conn,
        &host,
        VcsRequest::ReadObject {
            repo_id: repo.clone(),
            object_id: id.clone(),
            commit_id: Some(base.clone()),
        },
    ) {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    };
    let record = record_from_eure(historical.record_eure.as_deref().unwrap()).unwrap();
    assert_eq!(record, card("A\nB\nC\n"));
    let objects = match run(
        &conn,
        &host,
        VcsRequest::ListObjects {
            repo_id: repo.clone(),
            commit_id: Some(merge.clone()),
        },
    ) {
        VcsResponse::Objects { objects } => objects,
        other => panic!("{other:?}"),
    };
    let conflict = objects[0].conflict.as_ref().unwrap();
    assert_eq!(conflict.driver, "merge.record.v2");
    assert!(conflict.merged_text.is_none());
    let merged = record_from_eure(conflict.merged_record_eure.as_deref().unwrap()).unwrap();
    assert_eq!(merged, card("A\nB\nC\n"));
    for side in &conflict.sides {
        let record = record_from_eure(side.as_deref().unwrap()).unwrap();
        assert_eq!(record.get("count"), Some(&RecordValue::int(7)));
    }
    assert_eq!(conflict.terms.len(), 1);
    assert_eq!(conflict.terms[0].path, "title#0");
    let candidates: Vec<RecordValue> = conflict.terms[0]
        .candidates
        .iter()
        .map(|c| record_from_eure(c.as_deref().unwrap()).unwrap())
        .collect();
    assert_eq!(
        candidates,
        vec![
            RecordValue::text("B-left\n"),
            RecordValue::text("B-right\n")
        ]
    );
    match run(
        &conn,
        &host,
        VcsRequest::Log {
            repo_id: repo.clone(),
            from_commit_id: Some(merge.clone()),
            limit: 10,
        },
    ) {
        VcsResponse::Commits { commits } => {
            assert!(commits.iter().any(|c| c.commit_id == base));
            assert!(commits.iter().any(|c| c.commit_id == merge));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn error_codes_serialize_as_snake_case_strings() {
    let error = VcsError::new(VcsErrorCode::UnrecordedChanges, "m");
    let json = serde_json::to_value(&error).unwrap();
    assert_eq!(json["code"], "unrecorded_changes");
    assert_eq!(serde_json::from_value::<VcsError>(json).unwrap(), error);
    assert_eq!(VcsErrorCode::NotFound.to_string(), "not_found");
}

/// A text object saved as `base` / "n.md", then changed to `left` here and
/// to `right` on a second workspace (as another device would), and the right
/// side integrated. Returns (repo, object id, integrate conflicts).
fn diverge_and_integrate(
    conn: &Connection,
    host: &Host,
    left: (&str, &str),
    right: (&str, &str),
) -> (String, String, Vec<luvcs::api::VcsConflictView>) {
    let repo = create(conn, host);
    let save = |text: &str, title: &str, id: Option<String>| match run(
        conn,
        host,
        VcsRequest::SaveText {
            repo_id: repo.clone(),
            object_id: id,
            type_id: "note".into(),
            text: text.into(),
            title: Some(title.into()),
        },
    ) {
        VcsResponse::Object { object } => object.object_id,
        other => panic!("{other:?}"),
    };
    let record = |d: &str| match run(
        conn,
        host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: d.into(),
            objects: None,
        },
    ) {
        VcsResponse::Commit { commit } => commit.commit_id,
        other => panic!("{other:?}"),
    };
    let id = save("A\nB\nC\n", "n.md", None);
    let base = record("base");
    save(left.0, left.1, Some(id.clone()));
    record("left");
    let right = {
        use luvcs::cbor::Value;
        use luvcs::id::{IdKey, ObjectId, OperationId, RepoId, WorkspaceId};
        use luvcs::model::Actor;
        use luvcs::store::{OperationContext, RecordSource, Store};
        let (repo_bytes, key): (Vec<u8>, Vec<u8>) = conn
            .query_row("SELECT repo_id, id_key FROM vcs_local_repos", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        let mut store = Store::open(conn, RustDigest).unwrap();
        let mut r = store
            .repo(
                RepoId::from_bytes(&repo_bytes).unwrap(),
                IdKey(key.try_into().unwrap()),
            )
            .unwrap();
        let ws2 = WorkspaceId([0xEE; 16]);
        r.create_workspace(ws2, Some(base.parse().unwrap()))
            .unwrap();
        let oid: ObjectId = id.parse().unwrap();
        let st = r
            .put_text_state(
                oid,
                "note",
                right.0.as_bytes(),
                Value::map([("title", Value::text(right.1))]),
            )
            .unwrap();
        let ctx = |n: u8| OperationContext {
            operation_id: OperationId([0xD0 + n; 16]),
            workspace_id: ws2,
            expected_token: None,
            actor: Actor {
                kind: ActorKind::User,
                device_id: "dev-b".into(),
                description: String::new(),
            },
            reason: String::new(),
            now_unix_ms: 5,
        };
        r.save_working(&ctx(1), &[(oid, Some(st))]).unwrap();
        r.record(&ctx(2), RecordSource::Working, "right")
            .unwrap()
            .to_string()
    };
    let conflicts = match run(
        conn,
        host,
        VcsRequest::Integrate {
            repo_id: repo.clone(),
            commit_id: right,
        },
    ) {
        VcsResponse::Integrated { conflicts, .. } => conflicts,
        other => panic!("{other:?}"),
    };
    (repo, id, conflicts)
}

fn read_working(conn: &Connection, host: &Host, repo: &str, id: &str) -> luvcs::api::VcsObjectView {
    match run(
        conn,
        host,
        VcsRequest::ReadObject {
            repo_id: repo.into(),
            object_id: id.into(),
            commit_id: None,
        },
    ) {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    }
}

#[test]
fn concurrent_titles_conflict_and_resolve_with_a_title() {
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    // The text merges (different lines); the titles do not.
    let (repo, id, conflicts) =
        diverge_and_integrate(&conn, &host, ("A2\nB\nC\n", "a.md"), ("A\nB\nC2\n", "b.md"));
    assert_eq!(conflicts.len(), 1);
    let c = &conflicts[0];
    assert_eq!(c.driver, "merge.text.v1");
    assert_eq!(c.merged_text.as_deref(), Some("A2\nB\nC2\n"));
    assert_eq!(c.terms.len(), 1);
    assert_eq!(c.terms[0].kind, "metadata");
    assert_eq!(c.terms[0].path, "title");
    assert_eq!(
        c.terms[0].candidates,
        vec![Some("a.md".to_owned()), Some("b.md".to_owned())]
    );
    // Neither title is picked for the conflict state.
    assert_eq!(read_working(&conn, &host, &repo, &id).title, None);

    let resolve = |title: Option<&str>| {
        execute(
            &conn,
            &host.ctx(),
            VcsRequest::Resolve {
                repo_id: repo.clone(),
                object_id: id.clone(),
                resolution: VcsResolution::Text {
                    text: "A2\nB\nC2\n".into(),
                    title: title.map(str::to_owned),
                },
            },
        )
    };
    match resolve(None) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }
    match resolve(Some("c.md")) {
        VcsResponse::Object { object } => {
            assert_eq!(object.title.as_deref(), Some("c.md"));
            assert_eq!(object.text.as_deref(), Some("A2\nB\nC2\n"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rename_on_one_side_and_edit_on_the_other_merge_cleanly() {
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let (repo, id, conflicts) = diverge_and_integrate(
        &conn,
        &host,
        ("A\nB\nC\n", "moved.md"),
        ("A\nB\nC2\n", "n.md"),
    );
    assert!(conflicts.is_empty(), "{conflicts:?}");
    let object = read_working(&conn, &host, &repo, &id);
    assert_eq!(object.title.as_deref(), Some("moved.md"));
    assert_eq!(object.text.as_deref(), Some("A\nB\nC2\n"));
}

#[test]
fn set_title_keeps_the_body_and_refuses_conflicts() {
    use base64::Engine as _;
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    let saved = match run(
        &conn,
        &host,
        VcsRequest::SaveBinary {
            repo_id: repo.clone(),
            object_id: None,
            type_id: "image".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode([0u8, 1, 2]),
            title: Some("a.png".into()),
        },
    ) {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    };
    let set_title = |id: &str, title: Option<&str>| {
        execute(
            &conn,
            &host.ctx(),
            VcsRequest::SetTitle {
                repo_id: repo.clone(),
                object_id: id.into(),
                title: title.map(str::to_owned),
            },
        )
    };
    match set_title(&saved.object_id, Some("img/b.png")) {
        VcsResponse::Object { object } => {
            assert_eq!(object.object_id, saved.object_id);
            assert_eq!(object.title.as_deref(), Some("img/b.png"));
            assert_eq!(object.blob, saved.blob);
        }
        other => panic!("{other:?}"),
    }
    match set_title(&saved.object_id, None) {
        VcsResponse::Object { object } => assert_eq!(object.title, None),
        other => panic!("{other:?}"),
    }
    // A well-formed ID of no object (the test host's random bytes are small
    // counters, never 0xEE).
    let missing = luvcs::id::ObjectId([0xEE; 16]).to_string();
    match set_title(&missing, Some("x")) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
    match set_title("obj_!!", Some("x")) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }

    // An object in conflict keeps its title until it is resolved.
    let conn = Connection::open_in_memory().unwrap();
    let (repo, id, conflicts) =
        diverge_and_integrate(&conn, &host, ("A2\nB\n", "a.md"), ("A3\nB\n", "a.md"));
    assert_eq!(conflicts.len(), 1);
    match execute(
        &conn,
        &host.ctx(),
        VcsRequest::SetTitle {
            repo_id: repo,
            object_id: id,
            title: Some("b.md".into()),
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Conflict),
        other => panic!("{other:?}"),
    }
}

#[test]
fn resolving_a_content_conflict_keeps_the_agreed_title() {
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let (repo, id, conflicts) = diverge_and_integrate(
        &conn,
        &host,
        ("A\nB-l\nC\n", "n.md"),
        ("A\nB-r\nC\n", "n.md"),
    );
    assert_eq!(conflicts.len(), 1);
    assert!(conflicts[0].terms.iter().all(|t| t.kind == "text_region"));
    assert_eq!(
        read_working(&conn, &host, &repo, &id).title.as_deref(),
        Some("n.md")
    );
    match run(
        &conn,
        &host,
        VcsRequest::Resolve {
            repo_id: repo,
            object_id: id,
            resolution: VcsResolution::Text {
                text: "A\nB\nC\n".into(),
                title: None,
            },
        },
    ) {
        VcsResponse::Object { object } => assert_eq!(object.title.as_deref(), Some("n.md")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bookmarks_through_the_request_api() {
    use luvcs::api::{VcsAdvance, VcsBookmarkLife, VcsBookmarkLifeKind, VcsNewDefault};
    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    let save = |text: &str| {
        run(
            &conn,
            &host,
            VcsRequest::SaveText {
                repo_id: repo.clone(),
                object_id: None,
                type_id: "note".into(),
                text: text.into(),
                title: Some(format!("{text}.md")),
            },
        );
    };
    let list = || match run(
        &conn,
        &host,
        VcsRequest::ListBookmarks {
            repo_id: repo.clone(),
        },
    ) {
        VcsResponse::Bookmarks { bookmarks } => bookmarks,
        other => panic!("{other:?}"),
    };
    save("one");
    let c1 = match run(
        &conn,
        &host,
        VcsRequest::Record {
            repo_id: repo.clone(),
            description: "one".into(),
            objects: None,
        },
    ) {
        VcsResponse::Commit { commit } => commit.commit_id,
        other => panic!("{other:?}"),
    };
    let main = match run(
        &conn,
        &host,
        VcsRequest::CreateBookmark {
            repo_id: repo.clone(),
            name: "main".into(),
            commit_id: c1.clone(),
        },
    ) {
        VcsResponse::Bookmark { bookmark } => bookmark,
        other => panic!("{other:?}"),
    };
    assert_eq!(main.name.as_deref(), Some("main"));
    assert_eq!(main.target_commit_id.as_deref(), Some(c1.as_str()));
    assert_eq!(main.life.as_ref().unwrap().kind, VcsBookmarkLifeKind::Live);
    assert!(!main.is_selected && !main.is_default);

    run(
        &conn,
        &host,
        VcsRequest::SelectBookmark {
            repo_id: repo.clone(),
            bookmark_id: Some(main.bookmark_id.clone()),
        },
    );
    let record_advancing = |text: &str, token: &str| {
        save(text);
        match run(
            &conn,
            &host,
            VcsRequest::RecordAdvancing {
                repo_id: repo.clone(),
                description: text.into(),
                objects: None,
                token: token.into(),
            },
        ) {
            VcsResponse::Recorded { commit, advance } => (commit.commit_id, advance),
            other => panic!("{other:?}"),
        }
    };
    let (c2, advance) = record_advancing("two", &main.token);
    assert_eq!(advance, VcsAdvance::Advanced);
    let listed = list();
    assert_eq!(
        listed.selected_bookmark_id.as_ref(),
        Some(&main.bookmark_id)
    );
    let now = &listed.bookmarks[0];
    assert!(now.is_selected);
    assert_eq!(now.target_commit_id.as_deref(), Some(c2.as_str()));
    // The old token is stale now: the commit is recorded, the bookmark stays.
    let (_, advance) = record_advancing("three", &main.token);
    assert_eq!(advance, VcsAdvance::Stale);
    assert_eq!(
        list().bookmarks[0].target_commit_id.as_deref(),
        Some(c2.as_str())
    );

    let listed = run(
        &conn,
        &host,
        VcsRequest::SetDefaultBookmark {
            repo_id: repo.clone(),
            token: list().default_token,
            bookmark_id: Some(main.bookmark_id.clone()),
        },
    );
    let VcsResponse::Bookmarks { bookmarks: listed } = listed else {
        panic!("{listed:?}")
    };
    assert!(listed.bookmarks[0].is_default);
    assert_eq!(listed.default_bookmark_id.as_ref(), Some(&main.bookmark_id));

    let delete = |new_default: Option<VcsNewDefault>| {
        execute(
            &conn,
            &host.ctx(),
            VcsRequest::EditBookmark {
                repo_id: repo.clone(),
                bookmark_id: main.bookmark_id.clone(),
                token: list().bookmarks[0].token.clone(),
                name: None,
                commit_id: None,
                life: Some(VcsBookmarkLife {
                    kind: VcsBookmarkLifeKind::Deleted,
                    fused_into: None,
                }),
                new_default,
            },
        )
    };
    match delete(None) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }
    match delete(Some(VcsNewDefault::Unset)) {
        VcsResponse::Bookmark { bookmark } => {
            assert_eq!(bookmark.life.unwrap().kind, VcsBookmarkLifeKind::Deleted);
            assert!(!bookmark.is_default);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(list().default_bookmark_id, None);

    // Undo brings back both the bookmark and the default.
    run(
        &conn,
        &host,
        VcsRequest::Undo {
            repo_id: repo.clone(),
        },
    );
    let listed = list();
    assert_eq!(
        listed.bookmarks[0].life.as_ref().unwrap().kind,
        VcsBookmarkLifeKind::Live
    );
    assert!(listed.bookmarks[0].is_default);

    // A fused life needs the bookmark it fused into, and nothing else does.
    match execute(
        &conn,
        &host.ctx(),
        VcsRequest::EditBookmark {
            repo_id: repo.clone(),
            bookmark_id: main.bookmark_id.clone(),
            token: listed.bookmarks[0].token.clone(),
            name: None,
            commit_id: None,
            life: Some(VcsBookmarkLife {
                kind: VcsBookmarkLifeKind::Fused,
                fused_into: None,
            }),
            new_default: None,
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }
}
