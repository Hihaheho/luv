//! The request API: what every host drives luv through.

#![cfg(feature = "store")]

use std::cell::Cell;
use std::rc::Rc;

use luvcs::api::{
    HostContext, VcsError, VcsErrorCode, VcsRequest, VcsResolution, VcsResponse, execute,
    record_from_json, record_to_json,
};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::record::{FieldPolicy, RecordSchema};
use luvcs::store::SchemaRegistry;
use rusqlite::Connection;

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
            schemas: SchemaRegistry::default().register(
                RecordSchema::new("card.v1")
                    .field("title", FieldPolicy::Text)
                    .field("tags", FieldPolicy::Set),
            ),
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
fn record_json_round_trip_and_save() {
    let json = r#"{"title":"t","tags":{"$elements":[{"id":"a","value":null},{"id":"b","value":null}]},"n":3,"price":{"$dec":"1.5"}}"#;
    let record = record_from_json(json).unwrap();
    let back = record_to_json(&record);
    assert_eq!(
        back,
        serde_json::from_str::<serde_json::Value>(json).unwrap()
    );
    assert_eq!(
        record_from_json("[1,2]").unwrap_err().code,
        VcsErrorCode::Invalid
    );

    let conn = Connection::open_in_memory().unwrap();
    let host = host();
    let repo = create(&conn, &host);
    match run(
        &conn,
        &host,
        VcsRequest::SaveRecord {
            repo_id: repo,
            object_id: None,
            type_id: "card".into(),
            schema_id: "card.v1".into(),
            record_json: json.into(),
            title: None,
        },
    ) {
        VcsResponse::Object { object } => {
            assert_eq!(object.body_kind, "record");
            assert_eq!(object.schema_id.as_deref(), Some("card.v1"));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(object.record_json.as_deref().unwrap())
                    .unwrap(),
                back
            );
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
    let save = |text: &str, id: Option<String>| match run(
        &conn,
        &host,
        VcsRequest::SaveRecord {
            repo_id: repo.clone(),
            object_id: id,
            type_id: "note".into(),
            schema_id: "card.v1".into(),
            record_json: serde_json::json!({"title": text, "count": 7}).to_string(),
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
        let mut store = Store::open(&conn, RustDigest)
            .unwrap()
            .with_schemas(host.ctx().schemas);
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
                "card.v1",
                &record_from_json(r#"{"title":"A\nB-right\nC\n", "count":7}"#).unwrap(),
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
    let record: serde_json::Value =
        serde_json::from_str(historical.record_json.as_deref().unwrap()).unwrap();
    assert_eq!(record["count"], 7);
    assert_eq!(record["title"], "A\nB\nC\n");
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
    assert_eq!(conflict.driver, "merge.record.v1");
    assert!(conflict.merged_text.is_none());
    let merged: serde_json::Value =
        serde_json::from_str(conflict.merged_record_json.as_deref().unwrap()).unwrap();
    assert_eq!(merged["count"], 7);
    for side in &conflict.sides {
        let record: serde_json::Value = serde_json::from_str(side.as_deref().unwrap()).unwrap();
        assert_eq!(record["count"], 7);
    }
    assert!(!conflict.terms.is_empty());
    let candidates: Vec<serde_json::Value> = conflict.terms[0]
        .candidates
        .iter()
        .map(|c| serde_json::from_str(c.as_deref().unwrap()).unwrap())
        .collect();
    assert_eq!(
        candidates,
        vec![
            serde_json::json!("B-left\n"),
            serde_json::json!("B-right\n")
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
