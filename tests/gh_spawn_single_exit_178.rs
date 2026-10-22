#![allow(clippy::unwrap_used, clippy::expect_used)]

//! #178 — single `gh` exit invariant. The daemon must pick the gh identity
//! (`GH_CONFIG_DIR`) per repo, which is only enforceable if every `gh` CLI
//! spawn goes through ONE place that can apply it: `GitHubScmProvider::run`
//! in `src/scm/mod.rs`. A raw `Command::new("gh")` anywhere else silently
//! runs as the daemon's global gh account — the exact defect #178 fixes
//! (KicksCrew private repos → `Could not resolve to a Repository`).
//!
//! ## What it checks
//!
//! Every non-comment line under `src/` containing `Command::new("gh")` must
//! live in an allowed file:
//! - `src/scm/mod.rs` — the ScmProvider exit itself.
//! - `src/github_token.rs` — REST token discovery (`gh auth token`), which
//!   applies the identity on its own spawn.

use std::path::{Path, PathBuf};

const SRC_DIR: &str = "src";
const PATTERN: &str = "Command::new(\"gh\")";
const ALLOWED: &[&str] = &["src/scm/mod.rs", "src/github_token.rs"];

fn rs_files_under(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

fn is_allowed(root: &Path, file: &Path) -> bool {
    let rel = file
        .strip_prefix(root.parent().unwrap_or(root))
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    ALLOWED.iter().any(|a| rel == *a)
}

/// `(file, line, snippet)` for every raw `gh` spawn outside [`ALLOWED`].
fn scan_raw_gh_spawns(src_root: &Path) -> Vec<(PathBuf, usize, String)> {
    let mut out = Vec::new();
    for file in rs_files_under(src_root) {
        if is_allowed(src_root, &file) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            if line.contains(PATTERN) {
                out.push((file.clone(), idx + 1, line.trim().to_string()));
            }
        }
    }
    out
}

#[test]
fn gh_cli_is_spawned_only_through_scm_exit_178() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SRC_DIR);
    let violations = scan_raw_gh_spawns(&root);
    assert!(
        violations.is_empty(),
        "#178: raw `Command::new(\"gh\")` outside the ScmProvider exit — these spawns \
         bypass per-repo gh identity selection and run as the global account. Route \
         them through `crate::scm::make_scm_provider(..)`:\n{}",
        violations
            .iter()
            .map(|(f, l, s)| format!("  {}:{l}: {s}", f.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn scanner_flags_raw_gh_and_ignores_comments_178() {
    let base = std::env::temp_dir().join(format!("gh_exit_178_{}", std::process::id()));
    let src = base.join("src");
    std::fs::create_dir_all(src.join("scm")).unwrap();
    std::fs::write(
        src.join("offender.rs"),
        "// Command::new(\"gh\") in prose is fine\nfn f() { let _ = std::process::Command::new(\"gh\"); }\n",
    )
    .unwrap();
    std::fs::write(
        src.join("scm").join("mod.rs"),
        "fn run() { let _ = Command::new(\"gh\"); }\n",
    )
    .unwrap();
    let v = scan_raw_gh_spawns(&src);
    std::fs::remove_dir_all(&base).ok();
    assert_eq!(
        v.len(),
        1,
        "only the non-allowed, non-comment spawn is flagged: {v:?}"
    );
    assert!(v[0].0.ends_with("offender.rs") && v[0].1 == 2);
}
