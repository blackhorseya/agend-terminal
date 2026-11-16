//! -160 (c)：hygiene episode「已解除就收單」。
//!
//! 每個收單路徑都配一個負向對照：只有「能證明已解除」才收，判斷不了就不動
//! （比照 `vanished_target_reason` 的不對稱原則）。

use super::tests::{
    episode_is_open, git_in, hygiene_tasks, run_sweep, seed_episode, setup_test_repo, tmp_home,
    write_source_repo_binding, ENV_LOCK,
};
use std::path::Path;

/// 把 `repo` 的 origin 指向一個可正常 fetch 的本地 bare repo。
fn attach_healthy_origin(repo: &Path, tag: &str) -> std::path::PathBuf {
    let bare = tmp_home(&format!("{tag}-bare"));
    std::fs::remove_dir_all(&bare).ok();
    git_in(
        repo,
        &[
            "clone",
            "--bare",
            &repo.display().to_string(),
            &bare.display().to_string(),
        ],
    );
    git_in(
        repo,
        &["remote", "add", "origin", &bare.display().to_string()],
    );
    bare
}

fn episode_key(repo: &Path) -> String {
    format!("residue-fetch-degraded:{}", repo.display())
}

fn open_episode_ids(home: &Path, key: &str) -> Vec<crate::task_events::TaskId> {
    crate::daemon::hygiene_task::active_episodes(home)
        .unwrap()
        .into_iter()
        .filter(|(_, k)| k == key)
        .map(|(id, _)| id)
        .collect()
}

/// 該任務 `Cancelled` event 的 reason。projection 不保留 reason，所以透過
/// task_events 的正規讀取 API（`stream_envelopes`）取事件，不直接碰 log 檔。
fn cancel_event_line(home: &Path, id: &crate::task_events::TaskId) -> String {
    crate::task_events::stream_envelopes(home)
        .unwrap()
        .into_iter()
        .filter_map(|env| match env.event {
            crate::task_events::TaskEvent::Cancelled {
                task_id, reason, ..
            } if task_id == *id => Some(reason),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// RED：fetch 先壞、後來恢復，下一次成功的 `fetch --prune` 必須收掉 episode，
/// 並把理由寫進 close reason。
#[test]
fn fetch_recovery_closes_fetch_degraded_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-fetch-recover");
    let repo = setup_test_repo("hyg-fetch-recover");
    let bare = attach_healthy_origin(&repo, "hyg-fetch-recover");
    write_source_repo_binding(&home, "other-agent", &repo);
    let task = seed_episode(&home, &episode_key(&repo));

    run_sweep(&home);

    assert!(
        !episode_is_open(&home, &task),
        "fetch --prune 成功後，fetch-degraded episode 必須被收掉"
    );
    let line = cancel_event_line(&home, &task);
    assert!(
        line.contains("fetch --prune succeeded"),
        "close reason 必須寫明收單理由（fetch 已恢復）: {line:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&bare).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照：fetch 仍然失敗 → episode 不動（只更新同一張單）。
#[test]
fn still_failing_fetch_keeps_episode_open() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-fetch-still-bad");
    let repo = setup_test_repo("hyg-fetch-still-bad");
    git_in(
        &repo,
        &["remote", "add", "origin", "/nonexistent/agend-160-fixture"],
    );
    write_source_repo_binding(&home, "other-agent", &repo);
    let task = seed_episode(&home, &episode_key(&repo));

    run_sweep(&home);

    assert!(
        episode_is_open(&home, &task),
        "fetch 仍失敗時不得收單（判斷不了／仍在發生就不動）"
    );
    assert_eq!(
        open_episode_ids(&home, &episode_key(&repo)),
        vec![task],
        "仍失敗時沿用同一張單，不開新單"
    );
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照：收單只針對「該 repo」的 episode，別的 repo 的 episode 不受影響。
#[test]
fn fetch_recovery_only_closes_its_own_repo_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-fetch-scope");
    let repo = setup_test_repo("hyg-fetch-scope");
    let bare = attach_healthy_origin(&repo, "hyg-fetch-scope");
    write_source_repo_binding(&home, "other-agent", &repo);
    // 另一個「路徑仍存在」的 repo：既有的 vanished-target 收單不會碰它，
    // 能收掉它的只可能是這次新增的 fetch 恢復路徑（不得越界）。
    let other_repo = tmp_home("hyg-fetch-scope-other");
    std::fs::create_dir_all(&other_repo).unwrap();
    let other = seed_episode(&home, &episode_key(&other_repo));
    let mine = seed_episode(&home, &episode_key(&repo));

    run_sweep(&home);

    assert!(!episode_is_open(&home, &mine), "自己的 episode 要收");
    assert!(
        episode_is_open(&home, &other),
        "其他 repo 的 episode 不得被順手收掉"
    );
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&other_repo).ok();
    std::fs::remove_dir_all(&bare).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 沿用既有行為：收單後同一問題再次發生 → 重新開一張新單。
#[test]
fn fetch_failure_after_recovery_reopens_new_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-fetch-reopen");
    let repo = setup_test_repo("hyg-fetch-reopen");
    let bare = attach_healthy_origin(&repo, "hyg-fetch-reopen");
    write_source_repo_binding(&home, "other-agent", &repo);
    let first = seed_episode(&home, &episode_key(&repo));

    run_sweep(&home); // 恢復 → 收單
    assert!(
        !episode_is_open(&home, &first),
        "前置：第一次 sweep 應已收單"
    );

    git_in(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "/nonexistent/agend-160-fixture",
        ],
    );
    run_sweep(&home); // 再次失敗 → 重新開單

    let ids = open_episode_ids(&home, &episode_key(&repo));
    assert_eq!(
        ids.len(),
        1,
        "再次失敗必須重新開出一張 active 單: {:?}",
        hygiene_tasks(&home)
    );
    assert_ne!(ids[0], first, "必須是新單，不是復活舊單");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&bare).ok();
    std::fs::remove_dir_all(&home).ok();
}
