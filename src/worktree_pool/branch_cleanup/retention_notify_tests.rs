//! 建立 branch-retention 單後必須通知 assignee：Codex reviewer 不巡 board，
//! 沒有 inbox 訊息就永遠不知道要用 `keep:` / `delete:` 回覆。

use super::record_retention_obligation;
use std::path::{Path, PathBuf};

fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-retention-notify-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).expect("home dir");
    dir
}

fn retention_tasks(home: &Path, repo: &str) -> Vec<crate::task_events::TaskRecord> {
    let project = crate::tasks::resolve_repository_project(home, Path::new(repo));
    crate::task_events::projected_state_at(&crate::task_events::board_root(home, &project))
        .map(|state| {
            state
                .tasks
                .into_values()
                .filter(|t| t.tags.iter().any(|tag| tag == super::RETENTION_TAG))
                .collect()
        })
        .unwrap_or_default()
}

const REPO: &str = "/nonexistent/neutral-repo";
const BRANCH: &str = "feat/neutral-branch";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn creating_an_obligation_notifies_the_assignee_with_task_id_and_answer_format() {
    let home = tmp_home("notify");
    record_retention_obligation(&home, REPO, BRANCH, HEAD, "", Some("agent-own"));

    let tasks = retention_tasks(&home, REPO);
    assert_eq!(tasks.len(), 1, "pre: exactly one obligation is created");
    let task = &tasks[0];
    assert_eq!(task.owner.as_ref().map(|o| o.0.as_str()), Some("agent-own"));

    let inbox = crate::inbox::storage::drain(&home, "agent-own");
    assert_eq!(
        inbox.len(),
        1,
        "the assignee must receive exactly one inbox message, got {inbox:?}"
    );
    let text = &inbox[0].text;
    for needle in [task.id.0.as_str(), REPO, BRANCH, "keep:", "delete:"] {
        assert!(text.contains(needle), "message must carry {needle}: {text}");
    }
    assert_eq!(inbox[0].task_id.as_deref(), Some(task.id.0.as_str()));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn a_deduplicated_raise_does_not_notify_again() {
    let home = tmp_home("dedup");
    record_retention_obligation(&home, REPO, BRANCH, HEAD, "", Some("agent-own"));
    record_retention_obligation(&home, REPO, BRANCH, HEAD, "", Some("agent-own"));

    assert_eq!(
        retention_tasks(&home, REPO).len(),
        1,
        "pre: the second raise is deduplicated by the board"
    );
    let inbox = crate::inbox::storage::drain(&home, "agent-own");
    assert_eq!(
        inbox.len(),
        1,
        "only the raise that actually wrote the obligation notifies: {inbox:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn an_orphan_obligation_sends_no_inbox_message() {
    let home = tmp_home("orphan");
    record_retention_obligation(&home, REPO, BRANCH, HEAD, "", None);

    let tasks = retention_tasks(&home, REPO);
    assert_eq!(tasks.len(), 1, "pre: the orphan obligation is still raised");
    assert!(tasks[0].owner.is_none(), "pre: nobody is accountable");
    assert!(
        !home.join("inbox").exists()
            || std::fs::read_dir(home.join("inbox"))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "no inbox may be written for an ownerless obligation"
    );
    std::fs::remove_dir_all(&home).ok();
}
