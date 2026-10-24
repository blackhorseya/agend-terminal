//! A worktree bound WHILE a sweep round is running must not be reclaimed by
//! that round. `sweep_from_registry` reads its occupancy view (`active_dirs`,
//! which includes every live `binding.json`) once, up front, then walks the
//! managed repos one by one — each preceded by a network `fetch --prune` — so
//! a single round can span minutes. Live incident (2026-10-25): a checkout
//! bound at 06:09:41, the round that had snapshotted before it reached that
//! repo at 06:09:52, and the clean zero-commit branch was removed and
//! `branch -D`'d as "merged" eleven seconds after it was created.
//!
//! The race is driven deterministically through the one step that runs per
//! repo between the snapshot and worktree enumeration: the repo's own
//! `fetch --prune`. The test repo's remote is a local bare clone whose
//! `remote.origin.uploadpack` is a script that lands the binding (exactly
//! when a concurrent checkout would) and then hands off to the real
//! `git-upload-pack`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::tests::{
    git_commit_dated, git_in, setup_test_repo_with_default, tmp_home, write_managed_marker,
    write_source_repo_binding, ENV_LOCK,
};
use super::*;
use std::os::unix::fs::PermissionsExt;

const AGENT: &str = "late-binder";
const BRANCH: &str = "fix/fresh-from-main";

struct BindRace {
    repo: PathBuf,
    remote: PathBuf,
    home: PathBuf,
    wt: PathBuf,
    fired: PathBuf,
}

/// Incident shape: the default branch's tip is old (so the
/// `SQUASH_GC_MIN_TIP_AGE` floor does not protect anything), and the
/// worktree's branch is created from it with zero commits of its own — clean,
/// and trivially an ancestor of the default branch. The fetch hook always
/// touches `fired`; with `land_binding` it also writes this worktree's
/// `binding.json` before the fetch completes.
fn setup(tag: &str, land_binding: bool) -> BindRace {
    let repo = setup_test_repo_with_default(tag, "main");
    std::fs::write(repo.join("README.md"), "old tip").unwrap();
    git_in(&repo, &["add", "."]);
    git_commit_dated(&repo, "old default tip", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["branch", BRANCH]);
    let wt = repo.join("wt-fresh");
    git_in(&repo, &["worktree", "add", wt.to_str().unwrap(), BRANCH]);
    write_managed_marker(&wt, AGENT, BRANCH);

    let remote = repo.with_extension("remote.git");
    git_in(
        repo.parent().unwrap(),
        &[
            "clone",
            "--bare",
            "-q",
            repo.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git_in(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );

    let home = tmp_home(tag);
    // Repo discovery only — this binding names no worktree, so the round's
    // up-front occupancy snapshot does not cover `wt`.
    write_source_repo_binding(&home, "other-agent", &repo);

    let binding_dir = crate::paths::runtime_dir(&home).join(AGENT);
    let staged = home.join("staged-binding.json");
    std::fs::write(
        &staged,
        serde_json::to_string(&serde_json::json!({
            "branch": BRANCH,
            "source_repo": repo.display().to_string(),
            "worktree": wt.display().to_string(),
        }))
        .unwrap(),
    )
    .unwrap();
    let fired = home.join("fetch-hook-fired");
    let land = if land_binding {
        format!(
            "mkdir -p '{dir}' && cp '{staged}' '{dir}/binding.json'\n",
            dir = binding_dir.display(),
            staged = staged.display(),
        )
    } else {
        String::new()
    };
    let hook = home.join("upload-pack-hook.sh");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n{land}touch '{fired}'\nexec git-upload-pack \"$@\"\n",
            fired = fired.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    git_in(
        &repo,
        &["config", "remote.origin.uploadpack", hook.to_str().unwrap()],
    );

    BindRace {
        repo,
        remote,
        home,
        wt,
        fired,
    }
}

fn run_sweep(race: &BindRace) -> Vec<(String, String, &'static str)> {
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let configs: HashMap<String, Option<PathBuf>> = HashMap::new();
    let removed = sweep_from_registry(&race.home, &configs, &[]);
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    removed
}

fn teardown(race: BindRace) {
    std::fs::remove_dir_all(&race.repo).ok();
    std::fs::remove_dir_all(&race.remote).ok();
    std::fs::remove_dir_all(&race.home).ok();
}

#[test]
fn sweep_keeps_worktree_bound_after_the_rounds_occupancy_snapshot() {
    let _lock = ENV_LOCK.lock();
    let race = setup("bind-race", true);

    let removed = run_sweep(&race);

    assert!(
        race.fired.exists(),
        "the fetch hook never ran, so the binding never landed mid-round — \
         this test proved nothing"
    );
    assert!(
        removed.is_empty(),
        "a worktree whose binding landed after the round's occupancy snapshot \
         but before its repo was swept must not be reclaimed: {removed:?}"
    );
    assert!(race.wt.exists(), "the freshly bound worktree must survive");
    teardown(race);
}

/// Negative control: the identical setup with a hook that lands NO binding is
/// reclaimed. Without this, the test above could pass because something else
/// in the setup made the worktree ineligible.
#[test]
fn sweep_still_reclaims_the_same_worktree_when_no_binding_lands() {
    let _lock = ENV_LOCK.lock();
    let race = setup("bind-race-control", false);

    let removed = run_sweep(&race);

    assert!(race.fired.exists(), "the fetch hook never ran");
    assert!(
        removed.iter().any(|(branch, _, _)| branch == BRANCH),
        "with no binding, the clean zero-commit worktree on an old default tip \
         is eligible and must be reclaimed: {removed:?}"
    );
    teardown(race);
}
