//! Real-entry regression: a ChdirPass'd git command must keep the caller's
//! repo-relative subdirectory. The shim used to run `git -C <worktree root> …`,
//! so from `<worktree>/pkgs/identity`:
//! - `git rev-parse --show-prefix` printed an empty string (not `pkgs/identity/`),
//! - `git diff --quiet -- domain/x.txt` returned 0 for a MODIFIED file (false green),
//! - `git checkout -- domain/x.txt` / `git add domain/x.txt` failed on pathspecs.
//!
//! Controls pin the unchanged routing: worktree root, a foreign repo's subdir
//! (never mapped into the worktree) and a canonical subdir the worktree lacks.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const AGENT: &str = "agent-subdir";
const FILE: &str = "pkgs/identity/domain/x.txt";

fn real_git() -> PathBuf {
    for candidate in ["/usr/bin/git", "/opt/homebrew/bin/git", "/usr/local/bin/git"] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }
    panic!("no real git found");
}

fn tempdir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agentic-git-subdir-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path.canonicalize().unwrap()
}

fn run_real_git(real_git: &Path, cwd: &Path, args: &[&str]) -> Output {
    let output = Command::new(real_git)
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "real git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_binding(home: &Path, branch: &str, worktree: &Path, source: &Path) {
    let runtime = home.join("runtime").join(AGENT);
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::write(home.join(".config-integrity-key"), [7u8; 32]).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": AGENT,
        "task_id": format!("{AGENT}-task"),
        "branch": branch,
        "issued_at": "2026-01-01T00:00:00Z",
        "worktree": worktree.to_string_lossy(),
        "source_repo": source.to_string_lossy(),
    })
    .to_string();
    std::fs::write(runtime.join("binding.json"), &body).unwrap();
    let signature = agentic_git_core::integrity_core::sign(home, body.as_bytes());
    std::fs::write(runtime.join("binding.json.sig"), signature).unwrap();
}

fn run_shim(cwd: &Path, home: &Path, args: &[&str], real_git: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    command
        .arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", AGENT)
        .env("AGENTIC_GIT_REAL_GIT", real_git)
        .env("PATH", real_git.parent().unwrap())
        .env_remove("AGEND_HOME")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_BYPASS_AGENT")
        .env_remove("AGEND_GIT_BYPASS_AGENT")
        .env_remove("AGENTIC_GIT_BYPASS_UNTIL")
        .env_remove("AGEND_GIT_BYPASS_UNTIL")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH");
    command.output().unwrap()
}

struct Fixture {
    root: PathBuf,
    home: PathBuf,
    source: PathBuf,
    worktree: PathBuf,
    real_git: PathBuf,
}

/// canonical `source` + bound `worktree`, both carrying the tracked
/// `pkgs/identity/domain/x.txt`; the canonical also has an untracked
/// `only-in-canonical/` directory the worktree lacks.
fn fixture(tag: &str) -> Fixture {
    let root = tempdir(tag);
    let home = root.join("agend-home");
    let source = root.join("source");
    let worktree = root.join("bound-worktree");
    let real_git = real_git();

    std::fs::create_dir_all(source.join("pkgs/identity/domain")).unwrap();
    run_real_git(&real_git, &source, &["init", "-q", "-b", "main"]);
    run_real_git(&real_git, &source, &["config", "user.name", "test"]);
    run_real_git(&real_git, &source, &["config", "user.email", "test@example.com"]);
    std::fs::write(source.join(FILE), "original\n").unwrap();
    run_real_git(&real_git, &source, &["add", "."]);
    run_real_git(&real_git, &source, &["commit", "-q", "-m", "init"]);
    run_real_git(
        &real_git,
        &source,
        &["worktree", "add", "-q", "-b", "agent/subdir", worktree.to_str().unwrap()],
    );
    std::fs::create_dir_all(source.join("only-in-canonical")).unwrap();
    write_binding(&home, "agent/subdir", &worktree, &source);
    Fixture {
        root,
        home,
        source,
        worktree,
        real_git,
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn sub(fx: &Fixture) -> PathBuf {
    fx.worktree.join("pkgs/identity")
}

fn modify_in_worktree(fx: &Fixture) {
    std::fs::write(fx.worktree.join(FILE), "changed\n").unwrap();
}

#[test]
fn subdir_show_prefix_reports_the_prefix() {
    let fx = fixture("prefix");
    let out = run_shim(&sub(&fx), &fx.home, &["rev-parse", "--show-prefix"], &fx.real_git);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout(&out), "pkgs/identity/");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn subdir_diff_quiet_on_a_modified_file_is_not_a_false_green() {
    let fx = fixture("diffquiet");
    // control: unmodified → 0
    let clean = run_shim(&sub(&fx), &fx.home, &["diff", "--quiet", "--", "domain/x.txt"], &fx.real_git);
    assert_eq!(clean.status.code(), Some(0));
    modify_in_worktree(&fx);
    let dirty = run_shim(&sub(&fx), &fx.home, &["diff", "--quiet", "--", "domain/x.txt"], &fx.real_git);
    assert_eq!(
        dirty.status.code(),
        Some(1),
        "a modified file must make `git diff --quiet -- <relpath>` exit 1, not 0"
    );
    let status = run_shim(&sub(&fx), &fx.home, &["status", "--short"], &fx.real_git);
    assert_eq!(stdout(&status), "M domain/x.txt", "status paths are relative to the subdir");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn subdir_checkout_and_add_accept_relative_pathspecs() {
    let fx = fixture("pathspec");
    modify_in_worktree(&fx);
    let add = run_shim(&sub(&fx), &fx.home, &["add", "domain/x.txt"], &fx.real_git);
    assert!(add.status.success(), "add: {}", String::from_utf8_lossy(&add.stderr));
    assert_eq!(
        stdout(&run_real_git(&fx.real_git, &fx.worktree, &["diff", "--cached", "--name-only"])),
        FILE,
        "the relative pathspec must stage the file"
    );
    run_real_git(&fx.real_git, &fx.worktree, &["reset", "-q"]);
    let checkout = run_shim(&sub(&fx), &fx.home, &["checkout", "--", "domain/x.txt"], &fx.real_git);
    assert!(checkout.status.success(), "checkout: {}", String::from_utf8_lossy(&checkout.stderr));
    assert_eq!(std::fs::read_to_string(fx.worktree.join(FILE)).unwrap(), "original\n");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn canonical_subdir_cwd_maps_to_the_same_worktree_subdir() {
    let fx = fixture("canon-sub");
    let out = run_shim(
        &fx.source.join("pkgs/identity"),
        &fx.home,
        &["rev-parse", "--show-prefix"],
        &fx.real_git,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout(&out), "pkgs/identity/", "(b) mapped into the worktree's same subdir");
    let top = run_shim(
        &fx.source.join("pkgs/identity"),
        &fx.home,
        &["rev-parse", "--show-toplevel"],
        &fx.real_git,
    );
    assert_eq!(PathBuf::from(stdout(&top)).canonicalize().unwrap(), fx.worktree);
    std::fs::remove_dir_all(&fx.root).unwrap();
}

// ── controls: routing that must NOT change ──────────────────────────────────

#[test]
fn canonical_subdir_missing_from_the_worktree_falls_back_to_the_root() {
    let fx = fixture("canon-missing");
    let out = run_shim(
        &fx.source.join("only-in-canonical"),
        &fx.home,
        &["rev-parse", "--show-prefix"],
        &fx.real_git,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout(&out), "", "(b) no such subdir in the worktree → root, as before");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn worktree_root_cwd_keeps_an_empty_prefix() {
    let fx = fixture("root");
    let out = run_shim(&fx.worktree, &fx.home, &["rev-parse", "--show-prefix"], &fx.real_git);
    assert_eq!(stdout(&out), "");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_repo_subdir_is_never_mapped_into_the_worktree() {
    let fx = fixture("foreign-sub");
    let foreign = fx.root.join("foreign");
    std::fs::create_dir_all(foreign.join("pkgs/identity")).unwrap();
    run_real_git(&fx.real_git, &foreign, &["init", "-q", "-b", "main"]);
    let out = run_shim(
        &foreign.join("pkgs/identity"),
        &fx.home,
        &["rev-parse", "--show-prefix"],
        &fx.real_git,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout(&out), "pkgs/identity/", "reads in a foreign repo stay in that repo");
    std::fs::remove_dir_all(&fx.root).unwrap();
}
