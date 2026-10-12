//! Resolving a conflict term by term: `VcsResolution::Terms`.

#![cfg(feature = "store")]

use std::cell::Cell;
use std::rc::Rc;

use luvcs::api::{
    HostContext, VcsConflictView, VcsErrorCode, VcsObjectView, VcsRequest, VcsResolution,
    VcsResponse, VcsTermChoice, VcsTermPick, execute,
};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::record::{FieldPolicy, RecordSchema};
use luvcs::remote::MemoryRemote;
use luvcs::store::SchemaRegistry;
use luvcs::sync::{pull, push};
use rusqlite::Connection;

type Random = Box<dyn Fn(&mut [u8]) -> Result<(), String>>;

fn schema() -> RecordSchema {
    RecordSchema::new("task.v1")
        .field("notes", FieldPolicy::Text)
        .field("tags", FieldPolicy::Set)
        .field("steps", FieldPolicy::List)
}

struct Device {
    conn: Connection,
    random: Random,
    clock: Cell<i64>,
    schemas: SchemaRegistry,
}

impl Device {
    fn new(seed: u8) -> Self {
        let counter = Rc::new(Cell::new(seed));
        Self {
            conn: Connection::open_in_memory().unwrap(),
            random: Box::new(move |out: &mut [u8]| {
                let n = counter.get().wrapping_add(1);
                counter.set(n);
                out.fill(n);
                Ok(())
            }),
            clock: Cell::new(1_700_000_000_000),
            schemas: SchemaRegistry::default().register(schema()),
        }
    }

    fn ctx(&self) -> HostContext<'_> {
        self.clock.set(self.clock.get() + 1);
        HostContext {
            digest: &RustDigest,
            random: &self.random,
            now_unix_ms: self.clock.get(),
            device_id: "dev".into(),
            actor_kind: ActorKind::User,
            schemas: self.schemas.clone(),
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

    fn save_record(&self, repo: &str, object: Option<&str>, json: &str, title: &str) -> String {
        match self.run(VcsRequest::SaveRecord {
            repo_id: repo.into(),
            object_id: object.map(Into::into),
            type_id: "task".into(),
            schema_id: "task.v1".into(),
            record_json: json.into(),
            title: Some(title.into()),
        }) {
            VcsResponse::Object { object } => object.object_id,
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

    fn record(&self, repo: &str) {
        self.run(VcsRequest::Record {
            repo_id: repo.into(),
            description: "edit".into(),
            objects: None,
        });
    }

    fn resolve(&self, repo: &str, object: &str, resolution: VcsResolution) -> VcsResponse {
        self.try_run(VcsRequest::Resolve {
            repo_id: repo.into(),
            object_id: object.into(),
            resolution,
        })
    }
}

/// `a` makes the repo with `base` (via `save`), both devices change it
/// (`left` on `a`, `right` on `b`), and `a` integrates `b`'s head. Returns
/// the repo and the conflicts.
fn diverge(
    save_base: impl Fn(&Device, &str) -> String,
    left: impl Fn(&Device, &str, &str),
    right: impl Fn(&Device, &str, &str),
) -> (Device, String, String, Vec<VcsConflictView>) {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let b = Device::new(100);
    let repo = match a.run(VcsRequest::CreateRepo {
        name: "tasks".into(),
    }) {
        VcsResponse::Repo { repo } => repo.repo_id,
        other => panic!("{other:?}"),
    };
    let object = save_base(&a, &repo);
    a.record(&repo);
    push(&a.conn, &a.ctx(), &remote, "r", &repo).unwrap();
    pull(&b.conn, &b.ctx(), &remote, "r").unwrap();
    left(&a, &repo, &object);
    a.record(&repo);
    right(&b, &repo, &object);
    b.record(&repo);
    push(&b.conn, &b.ctx(), &remote, "r", &repo).unwrap();
    pull(&a.conn, &a.ctx(), &remote, "r").unwrap();
    let status = match a.run(VcsRequest::Status {
        repo_id: repo.clone(),
    }) {
        VcsResponse::Status { status } => status,
        other => panic!("{other:?}"),
    };
    let theirs = status
        .heads
        .iter()
        .find(|h| Some(h.as_str()) != status.base_commit_id.as_deref())
        .unwrap()
        .clone();
    let conflicts = match a.run(VcsRequest::Integrate {
        repo_id: repo.clone(),
        commit_id: theirs,
    }) {
        VcsResponse::Integrated { conflicts, .. } => conflicts,
        other => panic!("{other:?}"),
    };
    (a, repo, object, conflicts)
}

fn choices(picks: Vec<VcsTermPick>) -> Vec<VcsTermChoice> {
    picks
        .into_iter()
        .enumerate()
        .map(|(i, pick)| VcsTermChoice {
            term: i as u32,
            pick,
        })
        .collect()
}

fn value(json: &str) -> VcsTermPick {
    VcsTermPick::Value {
        value_json: json.into(),
    }
}

fn object(r: VcsResponse) -> VcsObjectView {
    match r {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    }
}

fn record_json(view: &VcsObjectView) -> serde_json::Value {
    serde_json::from_str(view.record_json.as_deref().unwrap()).unwrap()
}

#[test]
fn record_fields_text_regions_and_the_title_are_answered_one_by_one() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| {
            d.save_record(
                repo,
                None,
                r#"{"name": "Trip", "budget": 100, "notes": "day 1\nday 2\nday 3\n", "owner": "kim"}"#,
                "trip.md",
            )
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                r#"{"name": "Hokkaido trip", "budget": 120, "notes": "day 1: Sapporo\nday 2\nday 3\n", "owner": "kim"}"#,
                "hokkaido.md",
            );
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                r#"{"name": "Family trip", "budget": 150, "notes": "day 1: Otaru\nday 2\nday 3\n", "owner": "lee"}"#,
                "family.md",
            );
        },
    );
    assert_eq!(conflicts.len(), 1);
    let c = &conflicts[0];
    let terms: Vec<(&str, &str)> = c
        .terms
        .iter()
        .map(|t| (t.path.as_str(), t.kind.as_str()))
        .collect();
    assert_eq!(
        terms,
        vec![
            ("budget", "value"),
            ("name", "value"),
            ("notes/#0", "text_region"),
            ("title", "metadata"),
        ]
    );
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![
                value("130"),
                VcsTermPick::Side { index: 1 },
                value(r#""day 1: Sapporo, then Otaru\n""#),
                value(r#""trip.md""#),
            ]),
        },
    ));
    assert_eq!(resolved.body_kind, "record");
    assert_eq!(resolved.title.as_deref(), Some("trip.md"));
    assert_eq!(
        record_json(&resolved),
        serde_json::json!({
            "name": "Family trip",
            "budget": 130,
            "notes": "day 1: Sapporo, then Otaru\nday 2\nday 3\n",
            // Merged cleanly: only the right side changed it.
            "owner": "lee",
        })
    );
    // The working tree now holds it, and it records.
    a.record(&repo);
}

#[test]
fn list_order_and_element_terms_are_answered_together() {
    let steps = |order: &[(&str, &str)]| {
        let elements: Vec<String> = order
            .iter()
            .map(|(id, v)| format!(r#"{{"id": "{id}", "value": "{v}"}}"#))
            .collect();
        format!(r#"{{"steps": {{"$elements": [{}]}}}}"#, elements.join(", "))
    };
    let (a, repo, id, conflicts) = diverge(
        |d, repo| {
            d.save_record(
                repo,
                None,
                &steps(&[("a", "book"), ("b", "pack"), ("c", "go")]),
                "t",
            )
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                &steps(&[("c", "go"), ("a", "book"), ("b", "pack bags")]),
                "t",
            );
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), &steps(&[("c", "go"), ("a", "book")]), "t");
        },
    );
    let c = &conflicts[0];
    let terms: Vec<(&str, &str)> = c
        .terms
        .iter()
        .map(|t| (t.path.as_str(), t.kind.as_str()))
        .collect();
    assert_eq!(terms, vec![("steps/b", "deleted_vs_edited")]);
    // Keep the edited element: left's version.
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Side { index: 0 }]),
        },
    ));
    assert_eq!(
        record_json(&resolved)["steps"]["$elements"],
        serde_json::json!([
            {"id": "c", "value": "go"},
            {"id": "a", "value": "book"},
            {"id": "b", "value": "pack bags"},
        ])
    );

    // Both sides reorder differently: an order term.
    let (a, repo, id, conflicts) = diverge(
        |d, repo| {
            d.save_record(
                repo,
                None,
                &steps(&[("a", "1"), ("b", "2"), ("c", "3")]),
                "t",
            )
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                &steps(&[("c", "3"), ("a", "1"), ("b", "2")]),
                "t",
            );
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                &steps(&[("b", "2"), ("a", "1"), ("c", "3!")]),
                "t",
            );
        },
    );
    let c = &conflicts[0];
    let terms: Vec<(&str, &str)> = c
        .terms
        .iter()
        .map(|t| (t.path.as_str(), t.kind.as_str()))
        .collect();
    assert_eq!(terms, vec![("steps", "order")]);
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Side { index: 0 }]),
        },
    ));
    // Left's order; right's edit of `c` merged on its own.
    assert_eq!(
        record_json(&resolved)["steps"]["$elements"],
        serde_json::json!([
            {"id": "c", "value": "3!"},
            {"id": "a", "value": "1"},
            {"id": "b", "value": "2"},
        ])
    );
}

#[test]
fn text_regions_take_a_side_both_or_a_value() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| d.save_text(repo, None, "one\nkeep\ntwo\nkeep\nthree\n"),
        |d, repo, id| {
            d.save_text(repo, Some(id), "ONE\nkeep\nTWO\nkeep\nTHREE\n");
        },
        |d, repo, id| {
            d.save_text(repo, Some(id), "uno\nkeep\ndos\nkeep\ntres\n");
        },
    );
    let c = &conflicts[0];
    assert_eq!(c.terms.len(), 3);
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![
                VcsTermPick::Side { index: 1 },
                VcsTermPick::Both,
                value(r#""3\n""#),
            ]),
        },
    ));
    assert_eq!(
        resolved.text.as_deref(),
        Some("uno\nkeep\nTWO\ndos\nkeep\n3\n")
    );
    assert_eq!(resolved.title.as_deref(), Some("a.md"));
}

#[test]
fn a_whole_object_term_can_delete_or_keep_the_object() {
    let setup = || {
        diverge(
            |d, repo| d.save_text(repo, None, "x\n"),
            |d, repo, id| {
                d.save_text(repo, Some(id), "x edited\n");
            },
            |d, repo, id| {
                d.run(VcsRequest::DeleteObject {
                    repo_id: repo.into(),
                    object_id: id.into(),
                });
            },
        )
    };
    let (a, repo, id, conflicts) = setup();
    let c = &conflicts[0];
    assert_eq!(c.terms[0].kind, "deleted_vs_edited");
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Absent]),
        },
    ));
    assert_eq!(resolved.body_kind, "deleted");

    let (a, repo, id, conflicts) = setup();
    let c = &conflicts[0];
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![value(r#""x, rewritten\n""#)]),
        },
    ));
    assert_eq!(resolved.text.as_deref(), Some("x, rewritten\n"));
}

#[test]
fn incomplete_misplaced_or_stale_answers_are_refused() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| d.save_text(repo, None, "one\nkeep\ntwo\n"),
        |d, repo, id| {
            d.save_text(repo, Some(id), "ONE\nkeep\nTWO\n");
        },
        |d, repo, id| {
            d.save_text(repo, Some(id), "uno\nkeep\ndos\n");
        },
    );
    let c = &conflicts[0];
    assert_eq!(c.terms.len(), 2);
    let refuse = |conflict_id: &str, picks: Vec<VcsTermChoice>, code: VcsErrorCode| match a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: conflict_id.into(),
            choices: picks,
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, code, "{}", error.message),
        other => panic!("{other:?}"),
    };
    let side = |term: u32, index: u32| VcsTermChoice {
        term,
        pick: VcsTermPick::Side { index },
    };
    refuse(&c.conflict_id, vec![side(0, 0)], VcsErrorCode::Invalid);
    refuse(
        &c.conflict_id,
        vec![side(0, 0), side(0, 1), side(1, 0)],
        VcsErrorCode::Invalid,
    );
    refuse(
        &c.conflict_id,
        vec![side(0, 0), side(1, 0), side(2, 0)],
        VcsErrorCode::Invalid,
    );
    refuse(
        &c.conflict_id,
        vec![side(0, 0), side(1, 2)],
        VcsErrorCode::Invalid,
    );
    refuse(
        &c.conflict_id,
        vec![
            side(0, 0),
            VcsTermChoice {
                term: 1,
                pick: value("42"),
            },
        ],
        VcsErrorCode::Invalid,
    );
    let other = format!("cfl_{}", "A".repeat(43));
    refuse(&other, vec![side(0, 0), side(1, 1)], VcsErrorCode::Stale);
    // Still in conflict after all of that.
    match a.run(VcsRequest::ReadObject {
        repo_id: repo.clone(),
        object_id: id.clone(),
        commit_id: None,
    }) {
        VcsResponse::Object { object } => assert_eq!(object.body_kind, "conflict"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn only_a_text_region_takes_both_sides() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| d.save_record(repo, None, r#"{"name": "a"}"#, "t"),
        |d, repo, id| {
            d.save_record(repo, Some(id), r#"{"name": "b"}"#, "t");
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), r#"{"name": "c"}"#, "t");
        },
    );
    match a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: conflicts[0].conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Both]),
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_schema_that_changed_since_the_merge_is_not_guessed_at() {
    let (mut a, repo, id, conflicts) = diverge(
        |d, repo| d.save_record(repo, None, r#"{"notes": "x\ny\n"}"#, "t"),
        |d, repo, id| {
            d.save_record(repo, Some(id), r#"{"notes": "X\ny\n"}"#, "t");
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), r#"{"notes": "x!\ny\n"}"#, "t");
        },
    );
    assert_eq!(conflicts[0].terms[0].kind, "text_region");
    // `notes` is opaque now: the same sides give a whole-value term.
    a.schemas = SchemaRegistry::default()
        .register(RecordSchema::new("task.v1").field("notes", FieldPolicy::Opaque));
    match a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: conflicts[0].conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Side { index: 0 }]),
        },
    ) {
        VcsResponse::Error { error } => {
            assert_eq!(error.code, VcsErrorCode::Invalid);
            assert!(error.message.contains("schema"), "{}", error.message);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_title_alone_in_conflict_keeps_the_merged_body() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| d.save_record(repo, None, r#"{"name": "a", "budget": 1}"#, "t.md"),
        |d, repo, id| {
            d.save_record(repo, Some(id), r#"{"name": "b", "budget": 1}"#, "left.md");
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), r#"{"name": "a", "budget": 2}"#, "right.md");
        },
    );
    let c = &conflicts[0];
    assert_eq!(c.terms.len(), 1);
    assert_eq!(c.terms[0].kind, "metadata");
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Side { index: 1 }]),
        },
    ));
    assert_eq!(resolved.title.as_deref(), Some("right.md"));
    assert_eq!(
        record_json(&resolved),
        serde_json::json!({"name": "b", "budget": 2})
    );
}
