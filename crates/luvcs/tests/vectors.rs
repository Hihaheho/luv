//! Golden vectors for the contract. Every host must reproduce these bytes and IDs from the same inputs;
//! a change here is a format change and needs a new capability.
//!
//! Regenerate with `LUVCS_WRITE_VECTORS=1 cargo test -p luvcs --test vectors`
//! and review the diff.

use std::collections::BTreeMap;

use luvcs::cbor::{self, Value};
use luvcs::digest::RustDigest;
use luvcs::id::{
    ChangeId, CommitId, ContentId, IdKey, ObjectId, ObjectStateId, RepoId, TreeId, content_id,
};
use luvcs::model::{
    Actor, ActorKind, COMMIT_FORMAT_V1, CONFLICT_FORMAT_V1, Commit, Conflict, ConflictTerm,
    ObjectBody, ObjectState, TermKind,
};
use luvcs::record::RecordValue;
use luvcs::tree;

const VECTORS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/vectors/v1.json");

fn key() -> IdKey {
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = i as u8;
    }
    IdKey(k)
}

fn compute() -> serde_json::Value {
    let d = RustDigest;
    let key = key();
    let mut out = serde_json::Map::new();

    // content
    let text = b"hello, luv\n";
    let cid = content_id(&d, &key, text).unwrap();
    out.insert("content_id.hello".into(), cid.to_string().into());
    out.insert(
        "content_id.empty".into(),
        content_id(&d, &key, b"").unwrap().to_string().into(),
    );

    // record
    let record = RecordValue::map([
        ("title", RecordValue::text("A note")),
        (
            "body",
            RecordValue::code("line 1\nline 2\n", Some("markdown")),
        ),
        ("score", RecordValue::decimal("-1.250".parse().unwrap())),
        ("ratio", RecordValue::f64(0.5)),
        ("n", RecordValue::int(-3)),
        (
            "big",
            RecordValue::int(
                "-123456789012345678901234567890"
                    .parse::<num_bigint::BigInt>()
                    .unwrap(),
            ),
        ),
        ("flag", RecordValue::bool(true)),
        ("nothing", RecordValue::null()),
        ("blob", RecordValue::bytes(vec![0, 1, 2, 0xff])),
        (
            "pair",
            RecordValue::tuple([RecordValue::int(1), RecordValue::text("b")]),
        ),
        ("tags", RecordValue::set(["x", "y"])),
        (
            "by_number",
            RecordValue::map([(luvcs::record::ObjectKey::from(1), RecordValue::text("one"))]),
        ),
        (
            "links",
            RecordValue::array([
                RecordValue::map([("id", RecordValue::text("t1"))]),
                RecordValue::map([("id", RecordValue::text("t2"))]),
            ]),
        ),
        (
            "status",
            RecordValue::map([("reason", RecordValue::text("r"))])
                .with_ext("variant", RecordValue::text("closed")),
        ),
    ]);
    let record_bytes = record.encode().unwrap();
    out.insert("record.v2.hex".into(), hex::encode(&record_bytes).into());
    let rcid = content_id(&d, &key, &record_bytes).unwrap();
    out.insert("content_id.record".into(), rcid.to_string().into());
    // A schema is a record too; its content ID is the schema ID.
    let schema =
        RecordValue::from_eure("title = `text`\nbody = `text.markdown`\nbody.$merge = \"text\"\n")
            .unwrap();
    let schema_bytes = schema.encode().unwrap();
    out.insert("schema.hex".into(), hex::encode(&schema_bytes).into());
    let scid = content_id(&d, &key, &schema_bytes).unwrap();
    out.insert("content_id.schema".into(), scid.to_string().into());

    // states
    let obj_a = ObjectId([0xa0; 16]);
    let obj_b = ObjectId([0x0b; 16]);
    let state_a = ObjectState::new(obj_a, "note", ObjectBody::Text { content: cid })
        .with_metadata(Value::map([("title", Value::text("hello"))]));
    let state_b = ObjectState::new(
        obj_b,
        "note",
        ObjectBody::Record {
            content: rcid,
            schema: scid,
        },
    );
    let sa = state_a.id(&d, &key).unwrap();
    let sb = state_b.id(&d, &key).unwrap();
    out.insert(
        "state.a.hex".into(),
        hex::encode(state_a.encode().unwrap()).into(),
    );
    out.insert("state_id.a".into(), sa.to_string().into());
    out.insert("state_id.b".into(), sb.to_string().into());
    let deleted = ObjectState::new(obj_a, "note", ObjectBody::Deleted);
    out.insert(
        "state_id.deleted".into(),
        deleted.id(&d, &key).unwrap().to_string().into(),
    );

    // tree
    let empty = tree::empty_root(&d, &key).unwrap();
    out.insert("tree_id.empty".into(), empty.to_string().into());
    let mut entries = BTreeMap::new();
    entries.insert(obj_a, sa);
    entries.insert(obj_b, sb);
    for i in 0..6u8 {
        let mut k = [0u8; 16];
        k[0] = 0xa0;
        k[15] = i;
        entries.insert(ObjectId(k), ObjectStateId([i; 32]));
    }
    let built = tree::build(&d, &key, &entries).unwrap();
    out.insert("tree_id.sample".into(), built.root.to_string().into());
    out.insert("tree.sample.node_count".into(), built.nodes.len().into());
    let root_node = &built.nodes[&built.root];
    out.insert(
        "tree.sample.root_node.hex".into(),
        hex::encode(root_node.encode()).into(),
    );

    // commit
    let commit = Commit {
        format: COMMIT_FORMAT_V1,
        repo_id: RepoId([0x11; 16]),
        change_id: ChangeId([0x22; 16]),
        parents: vec![CommitId([0x33; 32])],
        root: built.root,
        author: Actor {
            kind: ActorKind::User,
            device_id: "device-1".into(),
            description: String::new(),
        },
        created_at_unix_ms: 1_760_000_000_000,
        description: "初版".into(),
        required_capabilities: vec!["core.v1".into()],
    };
    out.insert(
        "commit.hex".into(),
        hex::encode(commit.encode().unwrap()).into(),
    );
    out.insert(
        "commit_id".into(),
        commit.id(&d, &key).unwrap().to_string().into(),
    );

    // conflict
    let conflict = Conflict {
        format: CONFLICT_FORMAT_V1,
        object_id: obj_a,
        driver: "merge.text.v1".into(),
        base: vec![sa],
        sides: vec![sb, sa],
        merged: Some(cid),
        terms: vec![ConflictTerm {
            path: "#0".into(),
            kind: TermKind::TextRegion,
            candidates: vec![Some(cid), None],
        }],
    };
    out.insert(
        "conflict.hex".into(),
        hex::encode(conflict.encode().unwrap()).into(),
    );
    out.insert(
        "conflict_id".into(),
        conflict.id(&d, &key).unwrap().to_string().into(),
    );

    // canonical cbor sample
    let sample = Value::map([
        ("z", Value::Null),
        (
            "a",
            Value::Array(vec![Value::int(-1), Value::Uint(256), Value::Bool(false)]),
        ),
        ("bb", Value::bytes(vec![0xff, 0x00])),
    ]);
    out.insert(
        "cbor.sample.hex".into(),
        hex::encode(cbor::encode(&sample)).into(),
    );

    // id display
    out.insert(
        "tree_id.display_roundtrip".into(),
        TreeId::parse(&built.root.to_string())
            .unwrap()
            .to_string()
            .into(),
    );
    out.insert(
        "content_id.display_len".into(),
        ContentId([0; 32]).to_string().len().into(),
    );

    serde_json::Value::Object(out)
}

#[test]
fn golden_vectors_v1() {
    let computed = compute();
    if std::env::var_os("LUVCS_WRITE_VECTORS").is_some() {
        std::fs::write(
            VECTORS,
            serde_json::to_string_pretty(&computed).unwrap() + "\n",
        )
        .unwrap();
        return;
    }
    let stored: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(VECTORS).expect("tests/vectors/v1.json exists"),
    )
    .unwrap();
    let (c, s) = (computed.as_object().unwrap(), stored.as_object().unwrap());
    let mut mismatches = Vec::new();
    for (k, v) in c {
        if s.get(k) != Some(v) {
            mismatches.push(format!("{k}: computed {v}, stored {:?}", s.get(k)));
        }
    }
    for k in s.keys() {
        if !c.contains_key(k) {
            mismatches.push(format!("{k}: stored but no longer computed"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "golden vector mismatch:\n{}",
        mismatches.join("\n")
    );
}
