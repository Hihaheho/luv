//! Resolving a conflict term by term: `VcsResolution::Terms`.

#![cfg(feature = "store")]

use std::cell::Cell;
use std::rc::Rc;

use luvcs::api::{
    HostContext, VcsConflictView, VcsErrorCode, VcsObjectView, VcsRequest, VcsResolution,
    VcsResponse, VcsTermChoice, VcsTermPick, execute, record_from_eure,
};
use luvcs::digest::RustDigest;
use luvcs::model::ActorKind;
use luvcs::record::RecordValue;
use luvcs::remote::MemoryRemote;
use luvcs::sync::{pull, push};
use rusqlite::Connection;

type Random = Box<dyn Fn(&mut [u8]) -> Result<(), String>>;

const TASK_SCHEMA: &str = r#"
name = `text`
name.$optional = true
budget = `integer`
budget.$optional = true
notes = `text`
notes.$optional = true
notes.$merge = "text"
owner = `text`
owner.$optional = true
tags {
  $variant: set
  item = `text`
}
tags.$optional = true
steps {
  $variant: array
  item {
    id = `text`
    value = `text`
  }
  $merge-key = "id"
  max-length = 3
}
steps.$optional = true
"#;

struct Device {
    conn: Connection,
    random: Random,
    clock: Cell<i64>,
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

    /// Stores the task schema (the same ID every time) and returns its ID.
    fn put_schema(&self, repo: &str) -> String {
        match self.run(VcsRequest::PutSchema {
            repo_id: repo.into(),
            schema_eure: TASK_SCHEMA.into(),
        }) {
            VcsResponse::Schema { schema_id, .. } => schema_id,
            other => panic!("{other:?}"),
        }
    }

    fn save_record(&self, repo: &str, object: Option<&str>, eure: &str, title: &str) -> String {
        let schema_id = self.put_schema(repo);
        match self.run(VcsRequest::SaveRecord {
            repo_id: repo.into(),
            object_id: object.map(Into::into),
            type_id: "task".into(),
            schema_id,
            record_eure: eure.into(),
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

fn value(value: &str) -> VcsTermPick {
    VcsTermPick::Value {
        value: value.into(),
    }
}

fn object(r: VcsResponse) -> VcsObjectView {
    match r {
        VcsResponse::Object { object } => object,
        other => panic!("{other:?}"),
    }
}

fn record(view: &VcsObjectView) -> RecordValue {
    record_from_eure(view.record_eure.as_deref().unwrap()).unwrap()
}

fn rec(eure: &str) -> RecordValue {
    record_from_eure(eure).unwrap()
}

#[test]
fn record_fields_text_regions_and_the_title_are_answered_one_by_one() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| {
            d.save_record(
                repo,
                None,
                "name = \"Trip\"\nbudget = 100\nnotes = \"day 1\\nday 2\\nday 3\\n\"\nowner = \"kim\"\n",
                "trip.md",
            )
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                "name = \"Hokkaido trip\"\nbudget = 120\nnotes = \"day 1: Sapporo\\nday 2\\nday 3\\n\"\nowner = \"kim\"\n",
                "hokkaido.md",
            );
        },
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                "name = \"Family trip\"\nbudget = 150\nnotes = \"day 1: Otaru\\nday 2\\nday 3\\n\"\nowner = \"lee\"\n",
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
            ("notes#0", "text_region"),
            ("title", "metadata"),
        ]
    );
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![
                value("= 130"),
                VcsTermPick::Side { index: 1 },
                value("day 1: Sapporo, then Otaru\n"),
                value("trip.md"),
            ]),
        },
    ));
    assert_eq!(resolved.body_kind, "record");
    assert_eq!(resolved.title.as_deref(), Some("trip.md"));
    assert_eq!(
        record(&resolved),
        RecordValue::map([
            ("name", RecordValue::text("Family trip")),
            ("budget", RecordValue::int(130)),
            (
                "notes",
                RecordValue::text("day 1: Sapporo, then Otaru\nday 2\nday 3\n")
            ),
            // Merged cleanly: only the right side changed it.
            ("owner", RecordValue::text("lee")),
        ])
    );
    // The working tree now holds it, and it records.
    a.record(&repo);
}

#[test]
fn list_order_and_element_terms_are_answered_together() {
    let steps = |order: &[(&str, &str)]| -> String {
        order
            .iter()
            .map(|(id, v)| format!("steps[] {{\n  id = \"{id}\"\n  value = \"{v}\"\n}}\n"))
            .collect()
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
    assert_eq!(terms, vec![("steps[id=\"b\"]", "deleted_vs_edited")]);
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
        record(&resolved),
        rec(&steps(&[("c", "go"), ("a", "book"), ("b", "pack bags")]))
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
        record(&resolved),
        rec(&steps(&[("c", "3!"), ("a", "1"), ("b", "2")]))
    );

    // Each side adds within the limit of three; together they break it.
    let (a, repo, id, conflicts) = diverge(
        |d, repo| d.save_record(repo, None, &steps(&[("a", "1")]), "t"),
        |d, repo, id| {
            d.save_record(
                repo,
                Some(id),
                &steps(&[("a", "1"), ("b", "2"), ("c", "3")]),
                "t",
            );
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), &steps(&[("a", "1"), ("d", "4")]), "t");
        },
    );
    let c = &conflicts[0];
    let terms: Vec<(&str, &str)> = c
        .terms
        .iter()
        .map(|t| (t.path.as_str(), t.kind.as_str()))
        .collect();
    assert_eq!(terms, vec![("steps", "invalid")]);
    // The provisional record keeps the base value there.
    assert_eq!(
        rec(c.merged_record_eure.as_deref().unwrap()),
        rec(&steps(&[("a", "1")]))
    );
    let resolved = object(a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: c.conflict_id.clone(),
            choices: choices(vec![VcsTermPick::Side { index: 1 }]),
        },
    ));
    assert_eq!(record(&resolved), rec(&steps(&[("a", "1"), ("d", "4")])));
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
                value("3\n"),
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
            choices: choices(vec![value("x, rewritten\n")]),
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
        |d, repo| d.save_record(repo, None, "name = \"a\"\n", "t"),
        |d, repo, id| {
            d.save_record(repo, Some(id), "name = \"b\"\n", "t");
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), "name = \"c\"\n", "t");
        },
    );
    let refuse = |pick: VcsTermPick| match a.resolve(
        &repo,
        &id,
        VcsResolution::Terms {
            conflict_id: conflicts[0].conflict_id.clone(),
            choices: choices(vec![pick]),
        },
    ) {
        VcsResponse::Error { error } => assert_eq!(error.code, VcsErrorCode::Invalid),
        other => panic!("{other:?}"),
    };
    refuse(VcsTermPick::Both);
    // Not Eure, and Eure the schema refuses.
    refuse(value("= !"));
    refuse(value("= 42"));
}

#[test]
fn the_schema_travels_with_the_records_that_name_it() {
    let remote = MemoryRemote::new();
    let a = Device::new(0);
    let b = Device::new(100);
    let repo = match a.run(VcsRequest::CreateRepo {
        name: "tasks".into(),
    }) {
        VcsResponse::Repo { repo } => repo.repo_id,
        other => panic!("{other:?}"),
    };
    let id = a.save_record(&repo, None, "name = \"a\"\n", "t");
    a.record(&repo);
    push(&a.conn, &a.ctx(), &remote, "r", &repo).unwrap();
    pull(&b.conn, &b.ctx(), &remote, "r").unwrap();
    let schema_id = match b.run(VcsRequest::ReadObject {
        repo_id: repo.clone(),
        object_id: id.clone(),
        commit_id: Some(
            match b.run(VcsRequest::Heads {
                repo_id: repo.clone(),
            }) {
                VcsResponse::Heads { heads } => heads[0].clone(),
                other => panic!("{other:?}"),
            },
        ),
    }) {
        VcsResponse::Object { object } => object.schema_id.unwrap(),
        other => panic!("{other:?}"),
    };
    // `b` never stored the schema itself, yet has it.
    match b.run(VcsRequest::ReadSchema {
        repo_id: repo.clone(),
        schema_id,
    }) {
        VcsResponse::Schema { schema_eure, .. } => {
            assert_eq!(rec(&schema_eure), rec(TASK_SCHEMA))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_title_alone_in_conflict_keeps_the_merged_body() {
    let (a, repo, id, conflicts) = diverge(
        |d, repo| d.save_record(repo, None, "name = \"a\"\nbudget = 1\n", "t.md"),
        |d, repo, id| {
            d.save_record(repo, Some(id), "name = \"b\"\nbudget = 1\n", "left.md");
        },
        |d, repo, id| {
            d.save_record(repo, Some(id), "name = \"a\"\nbudget = 2\n", "right.md");
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
    assert_eq!(record(&resolved), rec("name = \"b\"\nbudget = 2\n"));
}
