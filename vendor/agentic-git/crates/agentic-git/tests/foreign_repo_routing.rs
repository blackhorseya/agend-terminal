//! Real-entry regression: a bound agent whose cwd is a FOREIGN git repo (a
//! separate object store, e.g. a throwaway fixture under /tmp) must have its
//! git commands run in THAT repo (or fail closed) — never be `ChdirPass`'d into
//! the bound worktree, which silently reads/WRITES the wrong repo. Because refs
//! live in the shared common dir, a ChdirPass'd `update-ref` mutates the
//! canonical repo's refs.
//!
//! Negative controls pin what must NOT change: the agent's stale workspace
//! clone (#2234), the worktree / canonical repo, and the leading `-C <foreign>`
//! form (x-monorepo fixtures) keep their routing.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const AGENT: &str = "agent-xrepo-read";

fn real_git() -> PathBuf {
    for candidate in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
        let path = PathBuf::from(candidate);
        if path.exists() {
            return path;
        }
    }
    panic!("no real git found");
}

fn tempdir(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agentic-git-foreign-read-{tag}-{}-{}",
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

/// `git init` + two commits (`a`, then `b`) so `HEAD~1..HEAD` touches only `b`.
fn init_repo_with_two_commits(real_git: &Path, dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    run_real_git(real_git, dir, &["init", "-q", "-b", "main"]);
    run_real_git(real_git, dir, &["config", "user.name", "test"]);
    run_real_git(real_git, dir, &["config", "user.email", "test@example.com"]);
    std::fs::write(dir.join("a"), "a\n").unwrap();
    run_real_git(real_git, dir, &["add", "a"]);
    run_real_git(real_git, dir, &["commit", "-q", "-m", "c1"]);
    std::fs::write(dir.join("b"), "b\n").unwrap();
    run_real_git(real_git, dir, &["add", "b"]);
    run_real_git(real_git, dir, &["commit", "-q", "-m", "c2"]);
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
    foreign: PathBuf,
    real_git: PathBuf,
}

/// canonical `source` (+ a bound `worktree`) and an unrelated `foreign` repo.
/// The worktree carries an extra commit/file (`wt-only`) so a read that leaks
/// into it is distinguishable from the foreign repo's own history.
fn fixture(tag: &str) -> Fixture {
    let root = tempdir(tag);
    let home = root.join("agend-home");
    let source = root.join("source");
    let worktree = root.join("bound-worktree");
    let foreign = root.join("foreign-fixture");
    let real_git = real_git();

    init_repo_with_two_commits(&real_git, &source);
    run_real_git(
        &real_git,
        &source,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "agent/foreign-read",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(worktree.join("wt-only"), "wt\n").unwrap();
    run_real_git(&real_git, &worktree, &["add", "wt-only"]);
    run_real_git(&real_git, &worktree, &["commit", "-q", "-m", "wt-only"]);
    write_binding(&home, "agent/foreign-read", &worktree, &source);

    init_repo_with_two_commits(&real_git, &foreign);
    Fixture {
        root,
        home,
        source,
        worktree,
        foreign,
        real_git,
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn foreign_cwd_rev_parse_reports_the_foreign_repo() {
    let fx = fixture("revparse");
    let out = run_shim(
        &fx.foreign,
        &fx.home,
        &["rev-parse", "--show-toplevel"],
        &fx.real_git,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        PathBuf::from(stdout(&out)).canonicalize().unwrap(),
        fx.foreign,
        "foreign-cwd rev-parse must report the foreign repo, not the bound worktree"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_diff_runs_on_the_foreign_repo() {
    let fx = fixture("diff");
    let out = run_shim(
        &fx.foreign,
        &fx.home,
        &["diff", "--name-only", "HEAD~1", "HEAD"],
        &fx.real_git,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        stdout(&out),
        "b",
        "foreign-cwd diff HEAD~1..HEAD must list the foreign repo's change"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_log_and_status_see_the_foreign_repo() {
    let fx = fixture("logstatus");
    std::fs::write(fx.foreign.join("untracked-in-foreign"), "x\n").unwrap();
    let log = run_shim(&fx.foreign, &fx.home, &["log", "--format=%s"], &fx.real_git);
    assert!(
        log.status.success(),
        "{}",
        String::from_utf8_lossy(&log.stderr)
    );
    assert_eq!(
        stdout(&log),
        "c2\nc1",
        "foreign log must not show the worktree's `wt-only` commit"
    );
    let status = run_shim(
        &fx.foreign,
        &fx.home,
        &["status", "--porcelain"],
        &fx.real_git,
    );
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(stdout(&status), "?? untracked-in-foreign");
    std::fs::remove_dir_all(&fx.root).unwrap();
}

/// Control: cwd inside the bound worktree keeps reading the worktree.
#[test]
fn worktree_cwd_read_stays_on_the_worktree() {
    let fx = fixture("wtcwd");
    let out = run_shim(
        &fx.worktree,
        &fx.home,
        &["rev-parse", "--show-toplevel"],
        &fx.real_git,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        PathBuf::from(stdout(&out)).canonicalize().unwrap(),
        fx.worktree
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

/// Control (#3498/#2950 canonical protection): cwd in the canonical source repo
/// shares the worktree's commondir → NOT foreign → still routed to the worktree.
#[test]
fn canonical_cwd_read_is_not_treated_as_foreign() {
    let fx = fixture("canonical");
    let out = run_shim(
        &fx.source,
        &fx.home,
        &["rev-parse", "--show-toplevel"],
        &fx.real_git,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        PathBuf::from(stdout(&out)).canonicalize().unwrap(),
        fx.worktree,
        "canonical cwd must keep ChdirPass to the bound worktree"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

/// Control (#2234): the agent's stale workspace clone (`<home>/workspace/<agent>`,
/// a SEPARATE object store) is the default cwd of every bound agent; its reads
/// must keep routing to the bound worktree, not to the empty/stale clone.
#[test]
fn workspace_clone_cwd_read_keeps_routing_to_the_worktree() {
    let fx = fixture("wsclone");
    let ws = fx.home.join("workspace").join(AGENT);
    init_repo_with_two_commits(&fx.real_git, &ws);
    let out = run_shim(
        &ws,
        &fx.home,
        &["rev-parse", "--show-toplevel"],
        &fx.real_git,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        PathBuf::from(stdout(&out)).canonicalize().unwrap(),
        fx.worktree,
        "workspace-clone cwd read must keep ChdirPass to the bound worktree (#2234)"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

// ── writes / plumbing: the update-ref incident ──────────────────────────────

fn git_out(real_git: &Path, repo: &Path, args: &[&str]) -> String {
    let out = run_real_git(real_git, repo, args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `(refs, HEAD)` snapshot of a repo — what a stray write would change.
fn ref_state(real_git: &Path, repo: &Path) -> (String, String) {
    (
        git_out(real_git, repo, &["for-each-ref"]),
        git_out(real_git, repo, &["symbolic-ref", "-q", "HEAD"]),
    )
}

fn assert_bound_repos_untouched(fx: &Fixture, before: &[(String, String); 2]) {
    assert_eq!(
        &ref_state(&fx.real_git, &fx.source),
        &before[0],
        "canonical refs/HEAD must be unchanged"
    );
    assert_eq!(
        &ref_state(&fx.real_git, &fx.worktree),
        &before[1],
        "bound worktree refs/HEAD must be unchanged"
    );
}

fn bound_snapshot(fx: &Fixture) -> [(String, String); 2] {
    [
        ref_state(&fx.real_git, &fx.source),
        ref_state(&fx.real_git, &fx.worktree),
    ]
}

#[test]
fn foreign_cwd_update_ref_writes_foreign_not_canonical() {
    let fx = fixture("updateref");
    let before = bound_snapshot(&fx);
    let out = run_shim(
        &fx.foreign,
        &fx.home,
        &["update-ref", "refs/heads/probe", "HEAD"],
        &fx.real_git,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_bound_repos_untouched(&fx, &before);
    assert!(
        git_out(&fx.real_git, &fx.foreign, &["for-each-ref", "refs/heads/probe"])
            .contains("refs/heads/probe"),
        "the foreign repo must receive the ref"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_symbolic_ref_writes_foreign_not_canonical() {
    let fx = fixture("symref");
    let before = bound_snapshot(&fx);
    let out = run_shim(
        &fx.foreign,
        &fx.home,
        &["symbolic-ref", "HEAD", "refs/heads/x"],
        &fx.real_git,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_bound_repos_untouched(&fx, &before);
    assert_eq!(
        git_out(&fx.real_git, &fx.foreign, &["symbolic-ref", "HEAD"]),
        "refs/heads/x"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_checkout_new_branch_leaves_bound_worktree_alone() {
    let fx = fixture("checkout");
    let before = bound_snapshot(&fx);
    let out = run_shim(&fx.foreign, &fx.home, &["checkout", "-q", "-b", "change"], &fx.real_git);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_bound_repos_untouched(&fx, &before);
    assert_eq!(
        git_out(&fx.real_git, &fx.foreign, &["symbolic-ref", "HEAD"]),
        "refs/heads/change"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_push_is_denied_not_run_against_the_worktree() {
    let fx = fixture("push");
    let out = run_shim(&fx.foreign, &fx.home, &["push"], &fx.real_git);
    assert!(!out.status.success(), "foreign-cwd push must fail closed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("foreign repository"),
        "deny must name the foreign-repo cause, got: {stderr}"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_unlisted_command_is_denied_loudly() {
    let fx = fixture("unlisted");
    let out = run_shim(&fx.foreign, &fx.home, &["frobnicate"], &fx.real_git);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("foreign repository"),
        "an unlisted command in a foreign repo must Deny with a foreign-repo message, got: {stderr}"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_git_dir_retarget_cannot_reach_canonical() {
    let fx = fixture("gitdir");
    let before = bound_snapshot(&fx);
    let git_dir = format!("--git-dir={}", fx.source.join(".git").display());
    let out = run_shim(
        &fx.foreign,
        &fx.home,
        &[&git_dir, "update-ref", "refs/heads/evil", "HEAD"],
        &fx.real_git,
    );
    assert!(!out.status.success(), "a --git-dir retarget from a foreign cwd must be denied");
    assert_bound_repos_untouched(&fx, &before);
    std::fs::remove_dir_all(&fx.root).unwrap();
}

#[test]
fn foreign_cwd_core_worktree_retarget_cannot_reach_canonical() {
    let fx = fixture("coreworktree");
    let before = bound_snapshot(&fx);
    let worktree_cfg = format!("core.worktree={}", fx.source.display());
    let out = run_shim(
        &fx.foreign,
        &fx.home,
        &["-c", &worktree_cfg, "checkout", "-q", "-b", "evil"],
        &fx.real_git,
    );
    assert!(!out.status.success(), "a core.worktree retarget from a foreign cwd must be denied");
    assert_bound_repos_untouched(&fx, &before);
    std::fs::remove_dir_all(&fx.root).unwrap();
}

// ── controls: behavior that must NOT change ─────────────────────────────────

/// x-monorepo fixture form: `git -C <foreign> <op>` already ran in the foreign repo.
#[test]
fn dash_c_foreign_forms_keep_working() {
    let fx = fixture("dashc");
    let before = bound_snapshot(&fx);
    let foreign = fx.foreign.to_str().unwrap();
    let diff = run_shim(
        &fx.worktree,
        &fx.home,
        &["-C", foreign, "diff", "--name-only", "HEAD~1", "HEAD"],
        &fx.real_git,
    );
    assert!(diff.status.success(), "{}", String::from_utf8_lossy(&diff.stderr));
    assert_eq!(stdout(&diff), "b");
    for args in [
        &["-C", foreign, "update-ref", "refs/remotes/origin/main", "main"][..],
        &["-C", foreign, "checkout", "-q", "-b", "change"],
    ] {
        let out = run_shim(&fx.worktree, &fx.home, args, &fx.real_git);
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    assert_bound_repos_untouched(&fx, &before);
    assert_eq!(
        git_out(&fx.real_git, &fx.foreign, &["symbolic-ref", "HEAD"]),
        "refs/heads/change"
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}

/// #2234 control: the stale workspace clone is a separate store but every bound
/// agent's default cwd — write plumbing and unlisted commands there keep today's
/// routing (ChdirPass into the worktree; never the new foreign deny).
#[test]
fn workspace_clone_cwd_keeps_legacy_routing_for_writes_and_unlisted() {
    let fx = fixture("wsclone-w");
    let ws = fx.home.join("workspace").join(AGENT);
    init_repo_with_two_commits(&fx.real_git, &ws);
    let ws_before = ref_state(&fx.real_git, &ws);
    let out = run_shim(&ws, &fx.home, &["update-ref", "refs/heads/probe", "HEAD"], &fx.real_git);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        git_out(&fx.real_git, &fx.source, &["for-each-ref", "refs/heads/probe"])
            .contains("refs/heads/probe"),
        "workspace-clone update-ref keeps routing to the bound worktree's repo (#2234)"
    );
    assert_eq!(ref_state(&fx.real_git, &ws), ws_before, "the stale clone is not touched");
    let unlisted = run_shim(&ws, &fx.home, &["frobnicate"], &fx.real_git);
    assert!(
        !String::from_utf8_lossy(&unlisted.stderr).contains("foreign repository"),
        "no foreign-repo deny from the workspace clone: {}",
        String::from_utf8_lossy(&unlisted.stderr)
    );
    std::fs::remove_dir_all(&fx.root).unwrap();
}
