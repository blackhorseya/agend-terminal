//! -18577-7 (c): `apply_gh_observations` runs on every gh poll (~60 s). While
//! the PR author stays unresolvable, `pr_author` stays empty, so each poll
//! re-ran the "first observation" block — one WARN + one INFO per poll
//! (PR #9 logged 281 lines). Both must appear once per PR.

#![allow(clippy::unwrap_used)]

use super::super::gh_poll::{AuthorIndex, GhPrMetadata, GhPrState};
use super::super::{new_for_branch, ReviewClass};
use super::apply_gh_observations;

fn capture_logs<F: FnOnce()>(f: F) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(Buf(buf.clone()))
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(sub, f);
    let bytes = buf.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
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
fn unresolvable_author_logs_once_across_polls() {
    let home = home_with_fleet("instances: {}\n");
    let repo = format!("acme/scan-once-{}", uuid::Uuid::new_v4().simple());
    let idx = AuthorIndex::build(&home, &repo);
    let mut state = new_for_branch(&repo, "deps/bump", "sha-A", ReviewClass::Single);
    state.subscribers.clear();
    let prs = vec![meta("deps/bump")];
    let logs = capture_logs(|| {
        for _ in 0..5 {
            apply_gh_observations(&mut state, &prs, "2026-10-25T00:00:00Z", Some(&idx));
        }
    });
    assert!(state.pr_author.is_empty(), "nothing is eligible");
    assert_eq!(state.pr_number, 9);
    assert_eq!(
        logs.matches("no instance eligible for this repo").count(),
        1,
        "{logs}"
    );
    assert!(
        logs.matches("first-observation populated").count() <= 1,
        "{logs}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Control: once the fleet gives the author an identity, the next poll
/// resolves and logs the populate line (the once-gate does not swallow it).
#[test]
fn author_resolving_later_is_still_logged() {
    let repo = format!("acme/scan-later-{}", uuid::Uuid::new_v4().simple());
    let empty = home_with_fleet("instances: {}\n");
    let named = home_with_fleet(&format!(
        "instances:\n  dev:\n    backend: claude\n    github_login: shared-bot\n    repo: {repo}\n"
    ));
    let mut state = new_for_branch(&repo, "feat/late", "sha-A", ReviewClass::Single);
    let prs = vec![meta("feat/late")];
    let logs = capture_logs(|| {
        let idx = AuthorIndex::build(&empty, &repo);
        apply_gh_observations(&mut state, &prs, "t0", Some(&idx));
        apply_gh_observations(&mut state, &prs, "t1", Some(&idx));
        let idx = AuthorIndex::build(&named, &repo);
        apply_gh_observations(&mut state, &prs, "t2", Some(&idx));
    });
    assert_eq!(state.pr_author, "dev");
    assert!(
        logs.lines()
            .any(|l| l.contains("first-observation populated") && l.contains("pr_author=dev")),
        "{logs}"
    );
    let _ = std::fs::remove_dir_all(&empty);
    let _ = std::fs::remove_dir_all(&named);
}
