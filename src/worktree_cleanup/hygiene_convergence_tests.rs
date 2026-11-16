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

// ── cleanup-intent-preserved:<candidate_id> ─────────────────────────

const INTENT_KEY_PREFIX: &str = "cleanup-intent-preserved:";

/// 寫入一個超過 24h 的 review intent（aged → sweep 會為 preserved 的 intent 開單）。
fn persist_aged_intent(home: &Path, repo: &Path, branch: &str, task_id: &str) {
    crate::cleanup_intents::persist_intent(
        home,
        &repo.display().to_string(),
        branch,
        "0000000000000000000000000000000000000000",
        task_id,
        None,
        None,
    )
    .expect("persist intent");
    let path = intent_files(home).into_iter().next().expect("intent file");
    let mut intent: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    intent["created_at"] =
        serde_json::Value::String((chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339());
    std::fs::write(&path, serde_json::to_vec_pretty(&intent).unwrap()).unwrap();
}

fn intent_files(home: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(home.join("cleanup-intents"))
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn intent_episode_ids(home: &Path) -> Vec<(crate::task_events::TaskId, String)> {
    crate::daemon::hygiene_task::active_episodes(home)
        .unwrap()
        .into_iter()
        .filter(|(_, k)| k.starts_with(INTENT_KEY_PREFIX))
        .collect()
}

/// 讓 review task 變成 terminal（Created + Done）。
fn seed_done_task(home: &Path, task_id: &str) {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let tid = TaskId(task_id.to_string());
    let board = crate::task_events::board_root(home, crate::task_events::DEFAULT_PROJECT);
    std::fs::create_dir_all(&board).unwrap();
    let emitter = InstanceName::from("test");
    crate::task_events::append(
        &board,
        &emitter,
        TaskEvent::Created {
            task_id: tid.clone(),
            title: "review subject".into(),
            description: String::new(),
            priority: "normal".into(),
            tags: Vec::new(),
            owner: None,
            depends_on: Vec::new(),
            parent_id: None,
            governing_decision_id: None,
            review_class: None,
            branch: None,
            due_at: None,
            routed_to: None,
            bind: None,
            eta_secs: None,
        },
    )
    .unwrap();
    crate::task_events::append(
        &board,
        &emitter,
        TaskEvent::Done {
            task_id: tid,
            by: emitter.clone(),
            source: crate::task_events::DoneSource::ReportAutoClose {
                report_summary: "test done".into(),
                closed_at: chrono::Utc::now().to_rfc3339(),
            },
        },
    )
    .unwrap();
}

/// RED：intent 檔已不存在（已處理／已刪除）→ 對應 episode 必須收掉。
#[test]
fn intent_gone_closes_preserved_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-gone");
    let repo = setup_test_repo("hyg-intent-gone");
    persist_aged_intent(&home, &repo, "review/gone-1", "t-intent-gone");

    run_sweep(&home); // intent 仍 preserved → 開單
    let episodes = intent_episode_ids(&home);
    assert_eq!(
        episodes.len(),
        1,
        "前置：preserved 的 aged intent 要有一張 episode"
    );

    for f in intent_files(&home) {
        std::fs::remove_file(f).unwrap();
    }
    run_sweep(&home);

    assert!(
        !episode_is_open(&home, &episodes[0].0),
        "intent 已不存在，episode 必須收掉"
    );
    let line = cancel_event_line(&home, &episodes[0].0);
    assert!(
        line.contains("cleanup intent") && line.contains("no longer"),
        "close reason 必須寫明 intent 已解除: {line:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// RED：reconcile 結果不再是 `preserved:*`（task 變 terminal、branch 不存在 →
/// settled）→ episode 必須在同一個 tick 收掉。
#[test]
fn intent_settled_by_reconcile_closes_preserved_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-settled");
    let repo = setup_test_repo("hyg-intent-settled");
    persist_aged_intent(&home, &repo, "review/settled-1", "t-intent-settled");

    run_sweep(&home); // task 不存在 → preserved:task_not_terminal → 開單
    let episodes = intent_episode_ids(&home);
    assert_eq!(episodes.len(), 1, "前置：應有一張 episode");

    seed_done_task(&home, "t-intent-settled"); // 解除條件成立：task terminal，branch 本就不存在
    run_sweep(&home);

    assert!(
        !episode_is_open(&home, &episodes[0].0),
        "reconcile 已 settled，episode 必須收掉"
    );
    let line = cancel_event_line(&home, &episodes[0].0);
    assert!(
        line.contains("settled"),
        "close reason 必須帶上 reconcile 結果: {line:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照：intent 仍然 preserved → episode 保持 open，且沿用同一張單。
#[test]
fn still_preserved_intent_keeps_episode_open() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-still");
    let repo = setup_test_repo("hyg-intent-still");
    persist_aged_intent(&home, &repo, "review/still-1", "t-intent-still");

    run_sweep(&home);
    let first = intent_episode_ids(&home);
    run_sweep(&home);
    let second = intent_episode_ids(&home);

    assert_eq!(first.len(), 1, "前置：應有一張 episode");
    assert_eq!(first, second, "仍 preserved 時不得收單，也不得換單");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// RED：repo 路徑確定不存在（NotFound）→ intent 解除、episode 收單，
/// close reason 寫明 repo 不存在並帶路徑。
#[test]
fn intent_repo_not_found_closes_preserved_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-repo-gone");
    let repo = setup_test_repo("hyg-intent-repo-gone");
    persist_aged_intent(&home, &repo, "review/repo-gone-1", "t-repo-gone");

    run_sweep(&home); // task 未 terminal → preserved → 開單
    let episodes = intent_episode_ids(&home);
    assert_eq!(episodes.len(), 1, "前置：應有一張 episode");

    seed_done_task(&home, "t-repo-gone");
    std::fs::remove_dir_all(&repo).unwrap(); // 拋棄式 clone 被刪
    run_sweep(&home);

    assert!(
        !episode_is_open(&home, &episodes[0].0),
        "repo 路徑確定不存在，episode 必須收掉"
    );
    assert!(
        intent_files(&home).is_empty(),
        "repo 不存在即可證明解除，intent 必須移除"
    );
    let line = cancel_event_line(&home, &episodes[0].0);
    assert!(
        line.contains("repository path no longer exists")
            && line.contains(&repo.display().to_string()),
        "close reason 必須寫明 repo 不存在並含路徑: {line:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照：repo 路徑存在但不是目錄（檔案）→ 不是「不存在」，維持 preserved，
/// outcome 與 repo_missing 可區分，episode 不收。
#[test]
fn intent_repo_path_is_file_keeps_episode_open() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-repo-file");
    let repo = setup_test_repo("hyg-intent-repo-file");
    persist_aged_intent(&home, &repo, "review/repo-file-1", "t-repo-file");
    run_sweep(&home);
    let episodes = intent_episode_ids(&home);
    assert_eq!(episodes.len(), 1, "前置：應有一張 episode");

    seed_done_task(&home, "t-repo-file");
    std::fs::remove_dir_all(&repo).unwrap();
    std::fs::write(&repo, b"not a directory").unwrap();
    let result = crate::cleanup_intents::reconcile_terminal_review_intents(&home, true);
    let outcome = result
        .candidates
        .iter()
        .map(|c| c.outcome.as_str())
        .next()
        .expect("candidate");
    assert!(
        outcome.starts_with("preserved:repo_unreadable:") && !outcome.contains("repo_missing"),
        "非目錄必須是可區分的 preserved reason: {outcome:?}"
    );

    run_sweep(&home);
    assert!(
        episode_is_open(&home, &episodes[0].0),
        "路徑存在但不是目錄，不得收單"
    );
    assert_eq!(intent_files(&home).len(), 1, "intent 必須保留");
    std::fs::remove_file(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照（unix）：父目錄權限 000 → 讀不到（EACCES）≠ 不存在，維持 preserved。
#[cfg(unix)]
#[test]
fn intent_repo_unreadable_parent_keeps_episode_open() {
    use std::os::unix::fs::PermissionsExt;
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-repo-eacces");
    let parent = tmp_home("hyg-intent-repo-eacces-parent");
    let repo = parent.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    persist_aged_intent(&home, &repo, "review/repo-eacces-1", "t-repo-eacces");
    run_sweep(&home);
    let episodes = intent_episode_ids(&home);
    assert_eq!(episodes.len(), 1, "前置：應有一張 episode");
    seed_done_task(&home, "t-repo-eacces");

    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
    // root 不受權限限制，此時無法製造 EACCES，直接略過。
    if std::fs::metadata(&repo).is_ok() {
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let result = crate::cleanup_intents::reconcile_terminal_review_intents(&home, true);
    let outcome = result.candidates[0].outcome.clone();
    run_sweep(&home);
    let still_open = episode_is_open(&home, &episodes[0].0);
    let intents = intent_files(&home).len();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(
        outcome.starts_with("preserved:repo_unreadable:") && !outcome.contains("repo_missing"),
        "權限錯誤必須是可區分的 preserved reason: {outcome:?}"
    );
    assert!(still_open, "讀不到 repo 不得收單");
    assert_eq!(intents, 1, "intent 必須保留");
    std::fs::remove_dir_all(&parent).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照（判斷不了就不動）：intent 目錄裡有一個讀不懂的檔案，無法證明
/// 「某個 intent 已經不在」，所以 episode 不得被收。
#[test]
fn unreadable_intent_state_keeps_episode_open() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-ambiguous");
    std::fs::create_dir_all(home.join("cleanup-intents")).unwrap();
    std::fs::write(home.join("cleanup-intents/corrupt.json"), b"{ not json").unwrap();
    let task = seed_episode(&home, &format!("{INTENT_KEY_PREFIX}0123456789abcdef"));

    run_sweep(&home);

    assert!(
        episode_is_open(&home, &task),
        "intent 狀態無法判讀時不得收單（寧可漏收不可誤收）"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// 沿用既有行為：收單之後同樣的 intent 再次卡住 → 重新開一張新單。
#[test]
fn intent_blocked_again_after_close_reopens_new_episode() {
    let _lock = ENV_LOCK.lock();
    let home = tmp_home("hyg-intent-reopen");
    let repo = setup_test_repo("hyg-intent-reopen");
    persist_aged_intent(&home, &repo, "review/reopen-1", "t-intent-reopen");
    run_sweep(&home);
    let first = intent_episode_ids(&home);
    assert_eq!(first.len(), 1, "前置：應有一張 episode");
    for f in intent_files(&home) {
        std::fs::remove_file(f).unwrap();
    }
    run_sweep(&home);
    assert!(
        intent_episode_ids(&home).is_empty(),
        "前置：episode 應已收掉"
    );

    persist_aged_intent(&home, &repo, "review/reopen-1", "t-intent-reopen");
    run_sweep(&home);

    let again = intent_episode_ids(&home);
    assert_eq!(again.len(), 1, "再次卡住必須重新開單: {again:?}");
    assert_ne!(again[0].0, first[0].0, "必須是新單，不是復活舊單");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

// ── (d) upsert log 降噪 ─────────────────────────────────────────────

/// 取出某個 key 的 "hygiene task upserted" 在 INFO 等級的行數。
fn info_upsert_lines(lines: &[&str], key: &str) -> usize {
    lines
        .iter()
        .filter(|l| l.contains("hygiene task upserted") && l.contains(key) && l.contains("INFO"))
        .count()
}

fn upsert(home: &Path, key: &str, reason: &str) {
    super::upsert_hygiene(
        home,
        key.to_string(),
        "[hygiene] fixture".to_string(),
        serde_json::json!({"reason": reason}),
    );
}

/// RED：新建一張單要記 info（有人要知道開了新單）。
#[tracing_test::traced_test]
#[test]
fn upsert_logs_info_when_episode_is_created() {
    let home = tmp_home("hyg-log-create");
    upsert(&home, "residue-fetch-degraded:/log-create", "r1");
    logs_assert(|lines| {
        let n = info_upsert_lines(lines, "/log-create");
        (n == 1)
            .then_some(())
            .ok_or_else(|| format!("新建應記 1 行 info，實際 {n}: {lines:?}"))
    });
    std::fs::remove_dir_all(&home).ok();
}

/// RED：evidence 沒有變更的重複觀察不得記 info（一天兩萬多行的來源），
/// 降到 debug。
#[tracing_test::traced_test]
#[test]
fn upsert_with_unchanged_evidence_does_not_log_info() {
    let home = tmp_home("hyg-log-unchanged");
    let key = "residue-fetch-degraded:/log-unchanged";
    upsert(&home, key, "same"); // 新建 → info
    for _ in 0..5 {
        upsert(&home, key, "same"); // 同樣 evidence → 不得 info
    }
    logs_assert(|lines| {
        let n = info_upsert_lines(lines, "/log-unchanged");
        (n == 1)
            .then_some(())
            .ok_or_else(|| format!("只有新建那一次可以是 info，實際 {n} 行"))
    });
    std::fs::remove_dir_all(&home).ok();
}

/// 負向對照：evidence 真的變更時仍要記 info。
#[tracing_test::traced_test]
#[test]
fn upsert_logs_info_when_evidence_changes() {
    let home = tmp_home("hyg-log-changed");
    let key = "residue-fetch-degraded:/log-changed";
    upsert(&home, key, "before"); // 新建 → info
    upsert(&home, key, "before"); // 不變 → 不 info
    upsert(&home, key, "after"); // 變更 → info
    logs_assert(|lines| {
        let n = info_upsert_lines(lines, "/log-changed");
        (n == 2)
            .then_some(())
            .ok_or_else(|| format!("新建 + 變更各 1 行 info，實際 {n}"))
    });
    std::fs::remove_dir_all(&home).ok();
}
