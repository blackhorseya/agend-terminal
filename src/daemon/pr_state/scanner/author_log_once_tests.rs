//! -18577-7 (c): `apply_gh_observations` runs on every gh poll (~60 s). While
//! the PR author stays unresolvable, `pr_author` stays empty, so each poll
//! re-ran the "first observation" block — one WARN + one INFO per poll
//! (PR #9 logged 281 lines). Both must appear once per PR.

#![allow(clippy::unwrap_used)]

use super::super::gh_poll::{AuthorIndex, GhPrMetadata, GhPrState};
use super::super::{new_for_branch, ReviewClass};
use super::apply_gh_observations;

fn exactly(lines: &[&str], needle: &str, want: usize) -> Result<(), String> {
    let got = lines.iter().filter(|l| l.contains(needle)).count();
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "`{needle}`: {got} lines, want {want}\n{}",
            lines.join("\n")
        ))
    }
}

fn meta(branch: &str) -> GhPrMetadata {
    GhPrMetadata {
        number: 9,
        author_login: "shared-bot".to_string(),
        head_ref: branch.to_string(),
        is_cross_repository: false,
        is_draft: false,
        state: GhPrState::Open,
        merged_at: None,
        head_ref_oid: Some("a".repeat(40)),
        base_ref_oid: Some("b".repeat(40)),
    }
}

fn home_with_fleet(yaml: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-18577-7-scan-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), yaml).unwrap();
    home
}

#[test]
#[tracing_test::traced_test]
fn unresolvable_author_logs_once_across_polls() {
    let home = home_with_fleet("instances: {}\n");
    let repo = format!("acme/scan-once-{}", uuid::Uuid::new_v4().simple());
    let idx = AuthorIndex::build(&home, &repo);
    let mut state = new_for_branch(&repo, "deps/bump", "sha-A", ReviewClass::Single);
    state.subscribers.clear();
    let prs = vec![meta("deps/bump")];
    for _ in 0..5 {
        apply_gh_observations(&mut state, &prs, "2026-10-25T00:00:00Z", Some(&idx), None);
    }
    assert!(state.pr_author.is_empty(), "nothing is eligible");
    assert_eq!(state.pr_number, 9);
    logs_assert(|lines| {
        exactly(lines, "no instance eligible for this repo", 1)?;
        exactly(lines, "first-observation populated", 0)
    });
    let _ = std::fs::remove_dir_all(&home);
}

/// Control: once the fleet gives the author an identity, the next poll
/// resolves and logs the populate line (the once-gate does not swallow it).
#[test]
#[tracing_test::traced_test]
fn author_resolving_later_is_still_logged() {
    let repo = format!("acme/scan-later-{}", uuid::Uuid::new_v4().simple());
    let empty = home_with_fleet("instances: {}\n");
    let named = home_with_fleet(&format!(
        "instances:\n  dev:\n    backend: claude\n    github_login: shared-bot\n    repo: {repo}\n"
    ));
    let mut state = new_for_branch(&repo, "feat/late", "sha-A", ReviewClass::Single);
    let prs = vec![meta("feat/late")];
    let idx = AuthorIndex::build(&empty, &repo);
    apply_gh_observations(&mut state, &prs, "t0", Some(&idx), None);
    apply_gh_observations(&mut state, &prs, "t1", Some(&idx), None);
    let idx = AuthorIndex::build(&named, &repo);
    apply_gh_observations(&mut state, &prs, "t2", Some(&idx), None);
    assert_eq!(state.pr_author, "dev");
    logs_assert(|lines| {
        exactly(lines, "first-observation populated", 1)?;
        exactly(lines, "pr_author=dev", 1)
    });
    let _ = std::fs::remove_dir_all(&empty);
    let _ = std::fs::remove_dir_all(&named);
}

/// r2 P1-1: a reviewer persisted as `pr_author` by the old tier-3 guess is
/// re-resolved on the next poll from the live gh login, not kept forever.
#[test]
fn persisted_reviewer_author_is_re_resolved_on_poll() {
    let repo = format!("acme/scan-rev-{}", uuid::Uuid::new_v4().simple());
    let home = home_with_fleet(&format!(
        "instances:\n  \
         lead:\n    backend: claude\n    github_login: shared-bot\n  \
         rev:\n    backend: codex\n    role_kind: reviewer\n\
         teams:\n  g:\n    orchestrator: lead\n    members: [lead, rev]\n    source_repo: {repo}\n"
    ));
    let idx = AuthorIndex::build(&home, &repo);
    let mut state = new_for_branch(&repo, "feat/rev", "sha-A", ReviewClass::Single);
    state.subscribers = vec!["rev".to_string()];
    state.pr_author = "rev".to_string();
    apply_gh_observations(&mut state, &[meta("feat/rev")], "t0", Some(&idx), None);
    assert_eq!(state.pr_author, "lead");
    let _ = std::fs::remove_dir_all(&home);
}
