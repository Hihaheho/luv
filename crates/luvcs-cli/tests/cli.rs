//! The `luv` binary end to end: two work trees exchanging history through a
//! shared directory, and through a command that serves the protocol.

use std::path::Path;
use std::process::Command;

const LUV: &str = env!("CARGO_BIN_EXE_luv");

/// Runs `luv` in `dir`; returns stdout, panics with both streams on failure.
fn luv(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(LUV)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("luv starts");
    assert!(
        out.status.success(),
        "luv {args:?} failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn luv_fails(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(LUV)
        .args(args)
        .current_dir(dir)
        .output()
        .expect("luv starts");
    assert!(!out.status.success(), "luv {args:?} should fail");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn two_work_trees_exchange_history_through_a_directory_and_a_command() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    let a = root.path().join("a");
    let b = root.path().join("b");
    for d in [&shared, &a, &b] {
        std::fs::create_dir_all(d).unwrap();
    }
    let shared_arg = shared.to_str().unwrap();

    // Device A: a repo with one commit, pushed through a shared directory.
    luv(&a, &["init", "--repo", "notes", "--include", "*.md"]);
    std::fs::write(a.join("a.md"), "hello\n").unwrap();
    luv(&a, &["commit", "-m", "first"]);
    luv(&a, &["remote", "set", "--path", shared_arg]);
    assert!(luv(&a, &["push"]).contains("pushed"));
    assert!(luv(&a, &["push"]).contains("pushed 0 objects"));

    // Device B starts from nothing: pull, attach, restore.
    assert!(luv_fails(&b, &["push"]).contains("Luv.eure"));
    luv(&b, &["remote", "set", "--path", shared_arg]);
    assert!(luv(&b, &["pull"]).contains("new repo"));
    assert!(luv(&b, &["pull"]).contains("up to date"));
    assert!(luv(&b, &["init", "--repo", "notes", "--include", "*.md"]).contains("Attached"));
    luv(&b, &["restore"]);
    assert_eq!(std::fs::read_to_string(b.join("a.md")).unwrap(), "hello\n");

    // B continues the history and pushes through a command remote that
    // serves the same directory over the protocol.
    std::fs::write(b.join("a.md"), "hello\nworld\n").unwrap();
    luv(&b, &["commit", "-m", "second"]);
    let command = format!("{LUV} serve --path {shared_arg}");
    luv(&b, &["remote", "set", "--command", &command]);
    assert!(luv(&b, &["push"]).contains("heads announced"));

    // A takes it in through the directory and sees both commits.
    assert!(luv(&a, &["pull"]).contains("took in"));
    let log = luv(&a, &["log"]);
    assert!(log.contains("second") && log.contains("first"), "{log}");
}

#[test]
fn a_remote_is_required_and_checked() {
    let dir = tempfile::tempdir().unwrap();
    luv(dir.path(), &["init", "--repo", "r", "--include", "*.md"]);
    assert!(luv_fails(dir.path(), &["push"]).contains("no remote"));
    // Neither or both targets is refused before anything is written.
    let both = luv_fails(
        dir.path(),
        &["remote", "set", "--path", "x", "--command", "y"],
    );
    assert!(!both.is_empty());
    assert!(luv(dir.path(), &["remote", "show"]).contains("no remote"));
}

#[test]
fn a_remote_command_that_cannot_start_is_an_error_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    luv(dir.path(), &["init", "--repo", "r", "--include", "*.md"]);
    luv(
        dir.path(),
        &["remote", "set", "--command", "/nonexistent/luv-remote"],
    );
    assert!(luv_fails(dir.path(), &["push"]).contains("cannot start"));
}
