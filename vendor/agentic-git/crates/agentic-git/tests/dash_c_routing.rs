//! Behavioral coverage for explicit `git -C <foreign-repo>` mutation routing.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn real_git_path() -> PathBuf {
    for candidate in [
        "/usr/bin/git",
        "/opt/homebrew/bin/git",
        "/usr/local/bin/git",
    ] {
        if Path::new(candidate).exists() {
            return PathBuf::from(candidate);
        }
    }
    panic!("no real git found");
}

fn real_git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(real_git_path())
        .args(args)
        .current_dir(dir)
        .env("AGENTIC_GIT_BYPASS", "1")
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .expect("real git runs")
}

fn setup_git(dir: &Path, args: &[&str]) {
    let output = real_git(dir, args);
    assert!(
        output.status.success(),
        "setup git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture_home() -> PathBuf {
    // pid + clock alone collide when tests of this binary run in parallel (the
    // macOS clock ticks in microseconds), so add a per-process counter.
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let home = PathBuf::from(std::env::var("HOME").expect("HOME set")).join(format!(
        ".agend-dash-c-routing-{}-{}-{}",
        std::process::id(),
        seq,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".config-integrity-key"), [7u8; 32]).unwrap();
    home
}

fn write_signed_binding(home: &Path, worktree: &Path, source_repo: &Path) {
    let dir = home.join("runtime/agent-a");
    std::fs::create_dir_all(&dir).unwrap();
    let body = serde_json::json!({
        "version": 1,
        "agent": "agent-a",
        "task_id": "t-dash-c",
        "branch": "feat/a",
        "issued_at": "2026-09-03T00:00:00Z",
        "worktree": worktree.to_str().unwrap(),
        "source_repo": source_repo.to_str().unwrap(),
    })
    .to_string();
    std::fs::write(dir.join("binding.json"), &body).unwrap();
    let signature = agentic_git_core::integrity_core::sign(home, body.as_bytes());
    std::fs::write(dir.join("binding.json.sig"), signature).unwrap();
}

fn bound_agent_fixture(home: &Path) -> PathBuf {
    let source = home.join("source");
    std::fs::create_dir_all(&source).unwrap();
    setup_git(&source, &["init", "-b", "main"]);
    setup_git(
        &source,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let worktree = home.join("worktree");
    setup_git(
        &source,
        &[
            "worktree",
            "add",
            worktree.to_str().unwrap(),
            "-b",
            "feat/a",
        ],
    );
    std::fs::write(
        worktree.join(".agend-managed"),
        format!(
            "agent=agent-a\nbranch=feat/a\nsource_repo={}\nleased_at=2026-09-03T00:00:00+00:00\n",
            source.display()
        ),
    )
    .unwrap();
    write_signed_binding(home, &worktree, &source);
    worktree
}

fn foreign_repo(home: &Path) -> PathBuf {
    let repo = home.join("foreign");
    std::fs::create_dir_all(&repo).unwrap();
    setup_git(&repo, &["init", "-b", "main"]);
    setup_git(
        &repo,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    repo
}

fn run_shim(cwd: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentic-git"));
    command
        .arg0("git")
        .args(args)
        .current_dir(cwd)
        .env("AGENTIC_GIT_HOME", home)
        .env("AGENTIC_GIT_AGENT", "agent-a")
        .env("AGENTIC_GIT_REAL_GIT", real_git_path())
        .env_remove("AGEND_HOME")
        .env_remove("AGEND_INSTANCE_NAME")
        .env_remove("AGEND_REAL_GIT")
        .env_remove("AGENTIC_GIT_BYPASS")
        .env_remove("AGEND_GIT_BYPASS")
        .env_remove("AGENTIC_GIT_SHIM_DEPTH")
        .env_remove("AGEND_GIT_SHIM_DEPTH");
    command.output().expect("shim runs")
}

fn commit_count(repo: &Path) -> String {
    let output = real_git(repo, &["rev-list", "--count", "HEAD"]);
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn explicit_dash_c_add_and_commit_mutate_foreign_repo_only() {
    let home = fixture_home();
    let worktree = bound_agent_fixture(&home);
    let foreign = foreign_repo(&home);
    let file = foreign.join("foreign-only.txt");
    std::fs::write(&file, "foreign change\n").unwrap();
    let bound_status_before = real_git(&worktree, &["status", "--porcelain"]);
    assert!(bound_status_before.status.success());

    let add = run_shim(
        &worktree,
        &home,
        &["-C", foreign.to_str().unwrap(), "add", file.to_str().unwrap()],
    );
    assert!(
        add.status.success(),
        "explicit -C add must target the foreign repo: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let commit = run_shim(
        &worktree,
        &home,
        &[
            "-C",
            foreign.to_str().unwrap(),
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "foreign commit",
        ],
    );
    assert!(
        commit.status.success(),
        "explicit -C commit must target the foreign repo: {}",
        String::from_utf8_lossy(&commit.stderr)
    );

    assert_eq!(commit_count(&foreign), "2", "foreign repo gets the commit");
    assert_eq!(commit_count(&worktree), "1", "bound worktree stays untouched");
    let status = real_git(&worktree, &["status", "--porcelain"]);
    assert!(status.status.success());
    assert_eq!(
        status.stdout,
        bound_status_before.stdout,
        "bound worktree state must remain unchanged"
    );

    std::fs::remove_dir_all(home).unwrap();
}

/// Give the bound worktree a commit the canonical source does not have, so a
/// read through the shim proves WHICH repo it ran in.
fn commit_in_worktree(worktree: &Path) {
    setup_git(
        worktree,
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "worktree-only",
        ],
    );
}

fn stdout_trimmed(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// P2 (PR #47 review): a bound agent's RELATIVE leading `-C` must resolve against
/// the caller's cwd — the base the shim used to pick `effective_target` — not
/// against the worktree the shim itself `-C`s into before exec. Before the fix
/// git applied `-C <worktree> -C <rel>` in order and failed with exit 128
/// ("cannot change to …"); the same target as an absolute path worked.
#[test]
fn bound_relative_dash_c_reads_the_repo_it_names() {
    let home = fixture_home();
    let worktree = bound_agent_fixture(&home);
    commit_in_worktree(&worktree);
    let source = home.join("source");

    // cwd = the parent of both repos; `worktree` / `source` are relative.
    for (target, want_subject) in [("worktree", "worktree-only"), ("source", "initial")] {
        let abs = home.join(target);
        let abs_log = run_shim(
            &home,
            &home,
            &["-C", abs.to_str().unwrap(), "log", "-1", "--format=%s"],
        );
        assert!(
            abs_log.status.success(),
            "absolute -C {target} baseline must work"
        );
        assert_eq!(stdout_trimmed(&abs_log), want_subject);

        let cat = run_shim(
            &home,
            &home,
            &["-C", target, "cat-file", "-e", "HEAD^{commit}"],
        );
        assert!(
            cat.status.success(),
            "relative -C {target} cat-file must exit 0, got {:?}: {}",
            cat.status.code(),
            String::from_utf8_lossy(&cat.stderr)
        );
        let log = run_shim(&home, &home, &["-C", target, "log", "-1", "--format=%s"]);
        assert!(
            log.status.success(),
            "relative -C {target} log must exit 0 (pre-existing ChdirPass bug), got {:?}: {}",
            log.status.code(),
            String::from_utf8_lossy(&log.stderr)
        );
        assert_eq!(
            stdout_trimmed(&log),
            want_subject,
            "relative -C {target} must read the repo it names"
        );
    }

    // Relative to a non-root cwd, and cumulative `-C` (each relative to the last).
    let up = run_shim(
        &source,
        &home,
        &["-C", "../worktree", "log", "-1", "--format=%s"],
    );
    assert!(
        up.status.success(),
        "`-C ../worktree` from source/ must work"
    );
    assert_eq!(stdout_trimmed(&up), "worktree-only");
    let cumulative = run_shim(
        &home,
        &home,
        &[
            "-C",
            "source",
            "-C",
            "../worktree",
            "log",
            "-1",
            "--format=%s",
        ],
    );
    assert!(
        cumulative.status.success(),
        "cumulative relative -C must work"
    );
    assert_eq!(stdout_trimmed(&cumulative), "worktree-only");

    std::fs::remove_dir_all(home).unwrap();
}

/// Unbound-style Passthrough and absolute `-C` keep their behaviour: an absolute
/// `-C` is untouched by the rewrite.
#[test]
fn bound_absolute_dash_c_is_unchanged() {
    let home = fixture_home();
    let worktree = bound_agent_fixture(&home);
    commit_in_worktree(&worktree);
    let log = run_shim(
        &home,
        &home,
        &[
            "-C",
            worktree.to_str().unwrap(),
            "cat-file",
            "-e",
            "HEAD^{commit}",
        ],
    );
    assert!(
        log.status.success(),
        "absolute -C cat-file must keep working"
    );
    std::fs::remove_dir_all(home).unwrap();
}

/// r2 review P2: git defines an EMPTY `-C ""` as "do not change directory". The
/// path rewrite must not turn it into a non-empty absolute path (`dir.join("")`),
/// which, applied after the shim's own `-C <worktree>`, moved a bound agent's
/// read from its worktree to the cwd's repo (canonical).
#[test]
fn bound_empty_dash_c_is_a_no_op() {
    let home = fixture_home();
    let worktree = bound_agent_fixture(&home);
    commit_in_worktree(&worktree);
    let source = home.join("source");

    // cwd = canonical source: a bound agent's plain `log` is routed to its worktree.
    let plain = run_shim(&source, &home, &["log", "-1", "--format=%s"]);
    assert!(plain.status.success());
    assert_eq!(stdout_trimmed(&plain), "worktree-only");
    for args in [
        &["-C", "", "log", "-1", "--format=%s"][..],
        &["-C", "", "-C", "", "log", "-1", "--format=%s"][..],
    ] {
        let out = run_shim(&source, &home, args);
        assert!(out.status.success(), "{args:?} must exit 0");
        assert_eq!(
            stdout_trimmed(&out),
            "worktree-only",
            "{args:?} must read like the plain form"
        );
    }

    std::fs::remove_dir_all(home).unwrap();
}
