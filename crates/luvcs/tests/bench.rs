//! Timings of everyday operations against repos of growing size. Not run
//! by default:
//!
//! ```text
//! cargo test --release -p luvcs --test bench -- --ignored --nocapture
//! ```
//!
//! Each repo holds `n` record objects in a file-backed store. The repo is
//! filled through the store directly, in batches, so that building it does
//! not dominate; every measured step goes through the request API the way
//! a host calls it. Times are medians of several runs where an operation
//! can be repeated.

#![cfg(feature = "store")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use luvcs::api::{HostContext, VcsRequest, VcsResponse, execute};
use luvcs::cbor::Value;
use luvcs::digest::RustDigest;
use luvcs::id::{IdKey, ObjectId, OperationId, RepoId, WorkspaceId};
use luvcs::model::{Actor, ActorKind};
use luvcs::record::RecordValue;
use luvcs::remote::{MemoryRemote, Remote};
use luvcs::store::{OperationContext, Store};
use luvcs::sync::{pull, push};
use luvcs_protocol::EntryBody;
use rusqlite::Connection;

type Random = Box<dyn Fn(&mut [u8]) -> Result<(), String>>;

struct Device {
    conn: Connection,
    random: Random,
    clock: Cell<i64>,
    /// The task schema's ID, once stored.
    schema: RefCell<Option<String>>,
    _dir: tempfile::TempDir,
}

impl Device {
    fn new(seed: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("store.sqlite")).unwrap();
        // A host's usual setting for a local store.
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .unwrap();
        let counter = Rc::new(Cell::new(seed));
        Self {
            conn,
            random: Box::new(move |out: &mut [u8]| {
                let n = counter.get().wrapping_add(1);
                counter.set(n);
                for (i, chunk) in out.chunks_mut(8).enumerate() {
                    let v = (n ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes();
                    chunk.copy_from_slice(&v[..chunk.len()]);
                }
                Ok(())
            }),
            clock: Cell::new(1_700_000_000_000),
            schema: RefCell::new(None),
            _dir: dir,
        }
    }

    fn ctx(&self) -> HostContext<'_> {
        self.clock.set(self.clock.get() + 1);
        HostContext {
            digest: &RustDigest,
            random: &self.random,
            now_unix_ms: self.clock.get(),
            device_id: "bench".into(),
            actor_kind: ActorKind::User,
        }
    }

    fn run(&self, request: VcsRequest) -> VcsResponse {
        let r = execute(&self.conn, &self.ctx(), request);
        if let VcsResponse::Error { error } = &r {
            panic!("{}: {}", error.code, error.message);
        }
        r
    }

    fn schema_id(&self, repo: &str) -> String {
        if let Some(id) = self.schema.borrow().clone() {
            return id;
        }
        let id = match self.run(VcsRequest::PutSchema {
            repo_id: repo.into(),
            schema_eure: SCHEMA.into(),
        }) {
            VcsResponse::Schema { schema_id, .. } => schema_id,
            other => panic!("{other:?}"),
        };
        *self.schema.borrow_mut() = Some(id.clone());
        id
    }

    fn save(&self, repo: &str, object: Option<&str>, i: usize, rev: usize) -> String {
        let schema_id = self.schema_id(repo);
        match self.run(VcsRequest::SaveRecord {
            repo_id: repo.into(),
            object_id: object.map(Into::into),
            type_id: "task".into(),
            schema_id,
            record_eure: record_eure(i, rev),
            title: Some(format!("tasks/{i}.json")),
        }) {
            VcsResponse::Object { object } => object.object_id,
            other => panic!("{other:?}"),
        }
    }

    fn record(&self, repo: &str) -> String {
        match self.run(VcsRequest::Record {
            repo_id: repo.into(),
            description: "bench".into(),
            objects: None,
        }) {
            VcsResponse::Commit { commit } => commit.commit_id,
            other => panic!("{other:?}"),
        }
    }

    fn bytes(&self) -> u64 {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let pages: i64 = self
            .conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        let size: i64 = self
            .conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        (pages * size) as u64
    }
}

const SCHEMA: &str = r#"
name = `text`
rev = `integer`
done = `boolean`
notes = `text`
notes.$merge = "text"
tags {
  $variant: set
  item = `text`
}
"#;

fn record_eure(i: usize, rev: usize) -> String {
    format!(
        "name = \"task {i}\"\nrev = {rev}\ndone = false\nnotes = \"line one of task {i}\\nline two\\n\"\ntags = #{{\"t{}\"}}\n",
        i % 7
    )
}

fn record_value(i: usize) -> RecordValue {
    RecordValue::from_eure(&record_eure(i, 0)).unwrap()
}

/// A repo of `n` records on a new device, recorded as one commit; the
/// object IDs come back in creation order.
fn filled(n: usize, seed: u64) -> (Device, String, Vec<String>) {
    let d = Device::new(seed);
    let repo = match d.run(VcsRequest::CreateRepo {
        name: "bench".into(),
    }) {
        VcsResponse::Repo { repo } => repo.repo_id,
        other => panic!("{other:?}"),
    };
    d.run(VcsRequest::Status {
        repo_id: repo.clone(),
    });
    let (repo_bytes, key, ws): (Vec<u8>, Vec<u8>, Vec<u8>) = d
        .conn
        .query_row(
            "SELECT repo_id, id_key, workspace_id FROM vcs_local_repos",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let mut ids = Vec::with_capacity(n);
    {
        let mut store = Store::open(&d.conn, RustDigest).unwrap();
        let mut r = store
            .repo(
                RepoId::from_bytes(&repo_bytes).unwrap(),
                IdKey(key.try_into().unwrap()),
            )
            .unwrap();
        let ws = WorkspaceId::from_bytes(&ws).unwrap();
        let schema = r
            .put_schema(&RecordValue::from_eure(SCHEMA).unwrap())
            .unwrap();
        *d.schema.borrow_mut() = Some(schema.to_string());
        for (batch, start) in (0..n).step_by(5_000).enumerate() {
            let mut changes = Vec::new();
            for i in start..(start + 5_000).min(n) {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&(i as u64 + 1).to_be_bytes());
                id[8..].copy_from_slice(&seed.to_le_bytes());
                let oid = ObjectId(id);
                let st = r
                    .put_record_state(
                        oid,
                        "task",
                        schema,
                        &record_value(i),
                        Value::map([("title", Value::text(format!("tasks/{i}.json")))]),
                    )
                    .unwrap();
                changes.push((oid, Some(st)));
                ids.push(oid.to_string());
            }
            r.save_working(
                &OperationContext {
                    operation_id: OperationId([batch as u8 + 1; 16]),
                    workspace_id: ws,
                    expected_token: None,
                    actor: Actor {
                        kind: ActorKind::Import,
                        device_id: "bench".into(),
                        description: String::new(),
                    },
                    reason: "fill".into(),
                    now_unix_ms: 1,
                },
                &changes,
            )
            .unwrap();
        }
    }
    d.record(&repo);
    (d, repo, ids)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time<T>(f: impl FnOnce() -> T) -> (Duration, T) {
    let start = Instant::now();
    let out = f();
    (start.elapsed(), out)
}

fn ms(d: Duration) -> String {
    format!("{:.2} ms", d.as_secs_f64() * 1000.0)
}

/// Object payload bytes in the remote's log.
fn log_bytes(remote: &MemoryRemote) -> usize {
    let mut total = 0;
    let mut after = 0;
    loop {
        let entries = remote.read(after, 256).unwrap();
        if entries.is_empty() {
            return total;
        }
        for e in entries {
            after = e.index;
            if let EntryBody::Objects { objects, .. } = &e.body {
                total += objects
                    .iter()
                    .map(|o| o.bytes_base64.len() * 3 / 4)
                    .sum::<usize>();
            }
        }
    }
}

fn report(n: usize) {
    const RUNS: usize = 9;
    println!("\n## {n} objects");
    let (fill, (a, repo, ids)) = time(|| filled(n, 1));
    println!("fill (store, batched)          {}", ms(fill));
    println!("store size                     {} KiB", a.bytes() / 1024);

    // One edit, the way a host saves a changed object.
    let mut saves = Vec::new();
    for k in 0..RUNS {
        let target = &ids[(k * 7919) % n];
        saves.push(time(|| a.save(&repo, Some(target), (k * 7919) % n, k + 1)).0);
    }
    println!("SaveRecord (one object)        {}", ms(median(saves)));
    let mut adds = Vec::new();
    for k in 0..RUNS {
        adds.push(time(|| a.save(&repo, None, n + k, 0)).0);
    }
    println!("SaveRecord (new object)        {}", ms(median(adds)));
    let mut statuses = Vec::new();
    for _ in 0..RUNS {
        statuses.push(
            time(|| {
                a.run(VcsRequest::Status {
                    repo_id: repo.clone(),
                })
            })
            .0,
        );
    }
    println!("Status (a few changes)         {}", ms(median(statuses)));
    let (record, _) = time(|| a.record(&repo));
    println!("Record (a few changes)         {}", ms(record));
    let mut reads = Vec::new();
    for k in 0..RUNS {
        let target = ids[(k * 104_729) % n].clone();
        reads.push(
            time(|| {
                a.run(VcsRequest::ReadObject {
                    repo_id: repo.clone(),
                    object_id: target,
                    commit_id: None,
                })
            })
            .0,
        );
    }
    println!("ReadObject                     {}", ms(median(reads)));
    let (log, _) = time(|| {
        a.run(VcsRequest::Log {
            repo_id: repo.clone(),
            from_commit_id: None,
            limit: 20,
        })
    });
    println!("Log (20)                       {}", ms(log));

    // Sync.
    let remote = MemoryRemote::new();
    let (first, pushed) = time(|| push(&a.conn, &a.ctx(), &remote, "r", &repo).unwrap());
    println!(
        "push, first                    {} ({} objects, {} KiB)",
        ms(first),
        pushed.objects,
        log_bytes(&remote) / 1024
    );
    let b = Device::new(1_000_000);
    let (pulled, _) = time(|| pull(&b.conn, &b.ctx(), &remote, "r").unwrap());
    println!("pull, first                    {}", ms(pulled));
    b.run(VcsRequest::Status {
        repo_id: repo.clone(),
    });
    a.save(&repo, Some(&ids[1]), 1, 100);
    a.record(&repo);
    let (again, pushed) = time(|| push(&a.conn, &a.ctx(), &remote, "r", &repo).unwrap());
    println!(
        "push, one change               {} ({} objects)",
        ms(again),
        pushed.objects
    );
    let (pulled, _) = time(|| pull(&b.conn, &b.ctx(), &remote, "r").unwrap());
    println!("pull, one change               {}", ms(pulled));

    // Two devices edit different objects; one integrates the other. b is
    // still where its workspace began, so its commit forks from there.
    a.save(&repo, Some(&ids[2]), 2, 200);
    a.record(&repo);
    b.save(&repo, Some(&ids[3]), 3, 300);
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
    let (integrate, _) = time(|| {
        a.run(VcsRequest::Integrate {
            repo_id: repo.clone(),
            commit_id: theirs,
        })
    });
    println!("Integrate (one edit each side) {}", ms(integrate));

    let (gc, _) = time(|| {
        a.run(VcsRequest::Gc {
            repo_id: repo.clone(),
            keep_operations: Some(0),
        })
    });
    println!("Gc                             {}", ms(gc));
    println!("store size after               {} KiB", a.bytes() / 1024);
}

/// Repo sizes to measure: `LUV_BENCH_SIZES=1000,10000` or the default.
fn sizes() -> Vec<usize> {
    std::env::var("LUV_BENCH_SIZES")
        .ok()
        .map(|s| s.split(',').map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1_000, 10_000, 100_000])
}

#[test]
#[ignore]
fn operations_by_repo_size() {
    for n in sizes() {
        report(n);
    }
}

/// Import sizes: `LUV_BENCH_IMPORT=1000,10000` or the default.
fn import_sizes() -> Vec<usize> {
    std::env::var("LUV_BENCH_IMPORT")
        .ok()
        .map(|s| s.split(',').map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1_000, 10_000, 100_000])
}

/// A host importing `n` objects one request at a time.
#[test]
#[ignore]
fn importing_through_the_api() {
    println!();
    for n in import_sizes() {
        let d = Device::new(7);
        let repo = match d.run(VcsRequest::CreateRepo {
            name: "import".into(),
        }) {
            VcsResponse::Repo { repo } => repo.repo_id,
            other => panic!("{other:?}"),
        };
        let (total, _) = time(|| {
            for i in 0..n {
                d.save(&repo, None, i, 0);
            }
        });
        let (record, _) = time(|| d.record(&repo));
        println!(
            "import {n:>6} by SaveRecord     {} total, {:.3} ms each, record {}",
            ms(total),
            total.as_secs_f64() * 1000.0 / n as f64,
            ms(record)
        );
    }
}

/// Where one save's time goes: the parts every operation on the working
/// tree repeats.
#[test]
#[ignore]
fn where_the_time_goes() {
    for n in [10_000, 100_000] {
        println!("\n## {n} objects");
        let (d, repo, ids) = filled(n, 1);
        let (repo_bytes, key, ws): (Vec<u8>, Vec<u8>, Vec<u8>) = d
            .conn
            .query_row(
                "SELECT repo_id, id_key, workspace_id FROM vcs_local_repos",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let key = IdKey(key.try_into().unwrap());
        let ops_before: i64 = d
            .conn
            .query_row("SELECT count(*) FROM vcs_operation_objects", [], |r| {
                r.get(0)
            })
            .unwrap();
        let bytes_before = d.bytes();
        d.save(&repo, Some(&ids[0]), 0, 1);
        let ops_after: i64 = d
            .conn
            .query_row("SELECT count(*) FROM vcs_operation_objects", [], |r| {
                r.get(0)
            })
            .unwrap();
        println!(
            "operation log rows per save    {} (store +{} KiB)",
            ops_after - ops_before,
            (d.bytes() - bytes_before) / 1024
        );
        let new_rows: i64 = d
            .conn
            .query_row(
                "SELECT count(*) FROM vcs_objects WHERE kind = 'tree'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        println!("tree nodes stored              {new_rows}");

        let mut store = Store::open(&d.conn, RustDigest).unwrap();
        let r = store
            .repo(RepoId::from_bytes(&repo_bytes).unwrap(), key.clone())
            .unwrap();
        let working = r
            .workspace(WorkspaceId::from_bytes(&ws).unwrap())
            .unwrap()
            .working;
        let (walk, entries) = time(|| r.tree_entries(working).unwrap());
        println!("tree walk (verified)           {}", ms(walk));
        let (build, built) = time(|| luvcs::tree::build(&RustDigest, &key, &entries).unwrap());
        println!(
            "tree build                     {} ({} nodes)",
            ms(build),
            built.nodes.len()
        );
        let (nodes, _) = time(|| r.tree_nodes(working).unwrap());
        println!("tree node list (verified)      {}", ms(nodes));
    }
}
