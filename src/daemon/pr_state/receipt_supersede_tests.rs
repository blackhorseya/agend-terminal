//! 同一位 reviewer 重審同一個 PR：新 assignment 的 receipt 必須取代舊 receipt，
//! 且任何 verdict 落地後都不得留下 reservation 擋住 merge。
//!
//! 情境 A～D 都走真實路徑：assignment store → `record_validated_receipt` →
//! pr-state → `merge_readiness`，並在每個終點再跑一次 reconcile 的
//! `redrive_reserved`，確認 reservation 不會被重新推導出來。

use super::*;
use crate::daemon::assignment_authority::{self as store, ActiveAssignment};
use crate::mcp::handlers::comms_gates::ReviewAuthor;
use crate::review_receipt::{ReviewReceiptSummary, ReviewSlot, ReviewVerdict};

const REPO: &str = "owner/repo";
const BRANCH: &str = "feat/test";
const PR: u64 = 100;

fn home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-receipt-supersede-{tag}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(home.join("inbox")).unwrap();
    std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
    home
}

fn seed_state(home: &Path, head: &str) {
    let mut state = tests::new_state(head, ReviewClass::Single);
    state.ci_state = CiState::Green {
        sha: head.to_string(),
        observed_at: chrono::Utc::now().to_rfc3339(),
    };
    state.draft_state = DraftState::Ready;
    save(home, &state).unwrap();
}

/// 每張 assignment 的建立時間嚴格遞增（跨測試共用單調計數），模擬「後派的比較新」。
fn next_created_at() -> String {
    use std::sync::atomic::{AtomicI64, Ordering};
    static TICK: AtomicI64 = AtomicI64::new(0);
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    chrono::DateTime::from_timestamp(1_800_000_000 + tick, 0)
        .unwrap()
        .to_rfc3339()
}

/// 派一張新的 assignment；同一 target 的舊 record 會被 `persist` 取代。
fn assign(
    home: &Path,
    target: &str,
    reviewer: crate::types::InstanceId,
    head: &str,
    task: &str,
) -> ActiveAssignment {
    assign_at(home, target, reviewer, head, task, &next_created_at())
}

/// 同 [`assign`]，但可注入 `created_at`（例如模擬系統時鐘倒退）。
fn assign_at(
    home: &Path,
    target: &str,
    reviewer: crate::types::InstanceId,
    head: &str,
    task: &str,
    created_at: &str,
) -> ActiveAssignment {
    let assignment = ActiveAssignment::new_pending_typed(
        REPO,
        BRANCH,
        target,
        reviewer,
        PR,
        head,
        ReviewSlot::Primary,
        "lead",
        task,
        ReviewClass::Single,
        ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        created_at,
    );
    store::persist(home, &assignment).unwrap();
    assignment
}

fn submit(home: &Path, assignment: &ActiveAssignment, verdict: ReviewVerdict) {
    let source_id = format!("m-{}", uuid::Uuid::new_v4());
    let receipt =
        crate::review_receipt::ValidatedCodeReviewReceipt::for_test(ReviewReceiptSummary {
            receipt_id: format!("review-receipt:{source_id}"),
            source_id,
            evidence_digest: "a".repeat(64),
            assignment_id: assignment.assignment_id,
            reviewer_instance_id: assignment.target_instance_id.unwrap(),
            reviewer_name: assignment.target.clone(),
            repo: REPO.into(),
            pr_number: PR,
            branch: BRANCH.into(),
            task_id: assignment.task_id.clone(),
            reviewed_head: assignment.reviewed_head.clone().unwrap(),
            review_class: ReviewClass::Single,
            slot: ReviewSlot::Primary,
            verdict,
        });
    assert!(record_validated_receipt(home, &receipt, None));
}

fn readiness(home: &Path) -> Result<(), MergeDeficit> {
    merge_readiness(&load(home, REPO, BRANCH).unwrap())
}

/// 終點檢查：落地後立即、以及 reconcile 重新推導之後，都必須可 merge。
fn assert_ready_now_and_after_reconcile(home: &Path) {
    assert_eq!(readiness(home), Ok(()), "receipt 落地後應可 merge");
    store::redrive_reserved(home, REPO, BRANCH);
    assert_eq!(
        readiness(home),
        Ok(()),
        "reconcile 重新推導後 reservation 不得被加回來"
    );
}

fn is_blocked(home: &Path) -> bool {
    readiness(home).is_err()
}

fn same_head_re_review(tag: &str, first: ReviewVerdict) {
    let home = home(tag);
    let head = "a".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let r1 = assign(&home, "reviewer", reviewer, &head, "t-review-1");
    submit(&home, &r1, first);
    assert!(is_blocked(&home), "非 VERIFIED 必須擋 merge（fail-closed）");

    let r2 = assign(&home, "reviewer", reviewer, &head, "t-review-2");
    submit(&home, &r2, ReviewVerdict::Verified);
    assert_ready_now_and_after_reconcile(&home);

    let state = load(&home, REPO, BRANCH).unwrap();
    assert_eq!(
        state.validated_review_receipts.len(),
        1,
        "同 reviewer、同 head 只留最後一張 receipt"
    );
    assert_eq!(
        state.validated_review_receipts[0].assignment_id,
        r2.assignment_id
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 情境 A：同 head，r1 REJECTED，r2 VERIFIED。
#[test]
fn case_a_same_head_rejected_then_verified() {
    same_head_re_review("a", ReviewVerdict::Rejected);
}

/// 情境 B：同 head，r1 REJECTED 的 reservation 已進 pr-state，r2 VERIFIED 後
/// 不得殘留 `reserved_assignments`。
#[test]
fn case_b_stale_reservation_released_after_reverify() {
    let home = home("b");
    let head = "b".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let r1 = assign(&home, "reviewer", reviewer, &head, "t-review-1");
    // 一次 reconcile tick：reservation 由 authority 推導進 pr-state。
    store::redrive_reserved(&home, REPO, BRANCH);
    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .reserved_assignments
            .len(),
        1
    );
    submit(&home, &r1, ReviewVerdict::Rejected);
    // REJECTED 本身由 receipt 擋 merge，reservation 不再是擋門的來源。
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .reserved_assignments
            .is_empty(),
        "receipt 落地後 reservation 必須釋放，reconcile 也不得加回來"
    );
    assert!(
        is_blocked(&home),
        "釋放 reservation 後仍須由 REJECTED receipt 擋住"
    );

    let r2 = assign(&home, "reviewer", reviewer, &head, "t-review-2");
    submit(&home, &r2, ReviewVerdict::Verified);
    assert_ready_now_and_after_reconcile(&home);
    let _ = std::fs::remove_dir_all(&home);
}

/// 情境 C：同 head，r1 UNVERIFIED，r2 VERIFIED。
#[test]
fn case_c_same_head_unverified_then_verified() {
    same_head_re_review("c", ReviewVerdict::Unverified);
}

/// 情境 D：r1 REJECTED 後作者 push，head 前進；r2 在新 head VERIFIED。
/// 舊 head 的 assignment 不得以 reservation 殘留而擋住新 head。
#[test]
fn case_d_head_advance_then_verified_on_new_head() {
    let home = home("d");
    let h1 = "c".repeat(40);
    let h2 = "d".repeat(40);
    seed_state(&home, &h1);
    let reviewer = crate::types::InstanceId::new();

    let r1 = assign(&home, "reviewer", reviewer, &h1, "t-review-1");
    store::redrive_reserved(&home, REPO, BRANCH);
    submit(&home, &r1, ReviewVerdict::Rejected);
    assert!(is_blocked(&home));

    record_ci_result(
        &home,
        REPO,
        BRANCH,
        &h2,
        CiConclusion::Green,
        vec!["dev".into()],
        ReviewClass::Single,
    );
    // 作者 push 後的 CI 觀測會把舊 head 的 r1 重新推導成 reservation。
    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .reserved_assignments
            .len(),
        1
    );
    // r2 在新 head 派工並立刻 VERIFIED，期間沒有 reconcile tick。
    let r2 = assign(&home, "reviewer", reviewer, &h2, "t-review-2");
    submit(&home, &r2, ReviewVerdict::Verified);
    assert_ready_now_and_after_reconcile(&home);
    let _ = std::fs::remove_dir_all(&home);
}

/// 負向對照：同 head 上另一位 reviewer 的 REJECTED 仍須擋住，
/// 即使有別位 reviewer 的 VERIFIED。
#[test]
fn negative_other_reviewers_rejected_still_blocks() {
    let home = home("neg");
    let head = "e".repeat(40);
    seed_state(&home, &head);

    let rejector = assign(
        &home,
        "reviewer-one",
        crate::types::InstanceId::new(),
        &head,
        "t-review-1",
    );
    let approver = assign(
        &home,
        "reviewer-two",
        crate::types::InstanceId::new(),
        &head,
        "t-review-2",
    );
    submit(&home, &rejector, ReviewVerdict::Rejected);
    submit(&home, &approver, ReviewVerdict::Verified);

    assert!(is_blocked(&home), "另一位 reviewer 的 REJECTED 必須擋住");
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(is_blocked(&home), "reconcile 後仍須擋住");
    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .validated_review_receipts
            .len(),
        2
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// reviewer 改名：同一個 instance（穩定的 InstanceId）先以舊名 REJECTED，再以新名
/// VERIFIED。authority record 以 target 名稱為鍵，所以舊名的 record 仍在。
/// 「同 reviewer」必須以 instance id 判斷：舊 receipt 被取代後，舊名 record 不得在
/// 下一次 reconcile 被當成 Unengaged 而重新 reservation。
#[test]
fn renamed_instance_old_record_is_not_re_reserved() {
    let home = home("renamed");
    let head = "f".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let r1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-old");
    store::redrive_reserved(&home, REPO, BRANCH);
    submit(&home, &r1, ReviewVerdict::Rejected);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "REJECTED 必須擋: {:?}",
        readiness(&home)
    );

    let r2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-new");
    submit(&home, &r2, ReviewVerdict::Verified);
    let state = load(&home, REPO, BRANCH).unwrap();
    assert_eq!(
        state.validated_review_receipts.len(),
        1,
        "同 instance 的新 receipt 取代舊的"
    );
    assert_ready_now_and_after_reconcile(&home);
    let _ = std::fs::remove_dir_all(&home);
}

/// 負向對照：另一個 instance 的 assignment 尚無 receipt（Unengaged）時，
/// 即使有別位 reviewer VERIFIED，reservation 仍須擋住——沒有 receipt、也沒有被
/// 同 instance 取代的 assignment 仍算 reservation。
#[test]
fn negative_unengaged_assignment_of_other_instance_still_reserved() {
    let home = home("neg-unengaged");
    let head = "9".repeat(40);
    seed_state(&home, &head);

    let _pending = assign(
        &home,
        "reviewer-one",
        crate::types::InstanceId::new(),
        &head,
        "t-review-1",
    );
    let approver = assign(
        &home,
        "reviewer-two",
        crate::types::InstanceId::new(),
        &head,
        "t-review-2",
    );
    submit(&home, &approver, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::ReservedAssignments { count: 1 })
        ),
        "尚未回覆的另一位 reviewer 仍須擋住: {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 較新的 assignment 不得被較舊 assignment 的 receipt 當成「已被取代」：同一個
/// reviewer 對同一個 head 先 VERIFIED（A1），之後 lead 再派 A2（例如 PR 描述改了）
/// 但尚未交件。A1 的舊 receipt 不能頂掉之後才派的 A2；A2 仍須保留 reservation 擋住
/// merge，否則會在新審查完成前放行。
#[test]
fn newer_pending_assignment_same_name_is_not_bypassed_by_older_verified() {
    let home = home("newer-same-name");
    let head = "1".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let a1 = assign(&home, "reviewer", reviewer, &head, "t-review-1");
    submit(&home, &a1, ReviewVerdict::Verified);
    assert_eq!(readiness(&home), Ok(()), "A1 VERIFIED 後可 merge");

    let _a2 = assign(&home, "reviewer", reviewer, &head, "t-review-2");
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::ReservedAssignments { count: 1 })
        ),
        "尚未交件的較新 assignment 必須擋 merge: {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 同上，但較新的 assignment 用另一個 target 名稱（reviewer 改名）：舊名的 receipt
/// 同樣不能頂掉新名、尚未交件的 assignment。
#[test]
fn newer_pending_assignment_renamed_is_not_bypassed_by_older_verified() {
    let home = home("newer-renamed");
    let head = "2".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-old");
    submit(&home, &a1, ReviewVerdict::Verified);
    assert_eq!(readiness(&home), Ok(()), "A1 VERIFIED 後可 merge");

    let _a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-new");
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::ReservedAssignments { count: 1 })
        ),
        "改名後尚未交件的較新 assignment 必須擋 merge: {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 時鐘倒退 guard：同一個 instance 改名後，A1（舊名）以較晚的時鐘值 VERIFIED，
/// 邏輯上較新的 A2（新名、尚未交件）卻帶著較早的 `created_at`（兩次派工之間系統時鐘
/// 往回調）。兩筆 record 因 target 名稱不同而同時 active。A1 的舊 receipt 不得因為
/// 時間戳排序而頂掉 A2：A2 必須仍保留 reservation 擋住 merge。
#[test]
fn clock_rollback_does_not_let_older_verified_bypass_newer_pending() {
    let home = home("clock-rollback");
    let head = "3".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let a1 = assign_at(
        &home,
        "old-reviewer-name",
        reviewer,
        &head,
        "t-review-1",
        "2030-01-01T00:00:00Z",
    );
    submit(&home, &a1, ReviewVerdict::Verified);
    assert_eq!(readiness(&home), Ok(()), "A1 VERIFIED 後可 merge");

    let a2 = assign_at(
        &home,
        "new-reviewer-name",
        reviewer,
        &head,
        "t-review-2",
        "2029-01-01T00:00:00Z",
    );
    assert!(
        a2.created_at < a1.created_at,
        "fixture 必須模擬時鐘倒退（邏輯上 A2 較新、時間戳卻較早）"
    );
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::ReservedAssignments { count: 1 })
        ),
        "時鐘倒退時尚未交件的較新 assignment 仍須擋 merge: {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

// ───────── reviewer 改名後的舊名 record（-248）─────────
//
// authority record 的 key 是 target 名稱，同一個 stable InstanceId 改名後，
// 舊名 record 不會被新名 `persist` 取代。以下測試涵蓋：舊名 record 在新 receipt
// 取代舊 receipt 後必須一併退役（不得被 reconcile 重新推導成 reservation），
// 以及所有「不可放寬」的 guard。任何能打開 merge gate 的判斷都不得依賴
// wall-clock `created_at`。

fn reserved_targets(home: &Path) -> Vec<String> {
    let mut targets: Vec<String> = load(home, REPO, BRANCH)
        .unwrap()
        .reserved_assignments
        .iter()
        .map(|r| r.target.clone())
        .collect();
    targets.sort();
    targets
}

fn record_exists(home: &Path, target: &str) -> bool {
    store::record_path_for_test(home, REPO, BRANCH, target).exists()
}

/// 新名 record（A2）之後照常退役（task terminal 清理）時，舊名 A1 不得復活成
/// reservation。r3 P2：退役新 record 後舊 record 被重新 reservation。
#[test]
fn renamed_instance_new_record_retirement_does_not_restore_old_reservation() {
    let home = home("renamed-a2-retired");
    let head = "c".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();

    let r1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    store::redrive_reserved(&home, REPO, BRANCH);
    submit(&home, &r1, ReviewVerdict::Rejected);
    let r2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &r2, ReviewVerdict::Verified);
    assert_eq!(readiness(&home), Ok(()));

    // 模擬 A2 的 terminal-task 清理；A1 的清理先前失敗或尚未完成。
    let _ = std::fs::remove_file(store::record_path_for_test(
        &home,
        REPO,
        BRANCH,
        "new-reviewer-name",
    ));
    store::redrive_reserved(&home, REPO, BRANCH);
    assert_eq!(
        readiness(&home),
        Ok(()),
        "退役 A2 後舊名 record 復活成 reservation: {:?}",
        reserved_targets(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Guard：只退役「被這張 receipt 取代」的同 InstanceId record。另一位 reviewer
/// 仍 pending 的 record 不受影響，仍須 reserved。
#[test]
fn other_reviewers_pending_record_is_not_retired() {
    let home = home("other-reviewer-pending");
    let head = "1".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let other = crate::types::InstanceId::new();

    let r1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    submit(&home, &r1, ReviewVerdict::Rejected);
    let _pending = assign(&home, "other-reviewer", other, &head, "t-review-other");
    let r2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &r2, ReviewVerdict::Verified);

    store::redrive_reserved(&home, REPO, BRANCH);
    assert_eq!(reserved_targets(&home), vec!["other-reviewer".to_string()]);
    assert!(record_exists(&home, "other-reviewer"));
    assert!(matches!(
        readiness(&home),
        Err(MergeDeficit::ReservedAssignments { count: 1 })
    ));
    let _ = std::fs::remove_dir_all(&home);
}

/// Guard：dual 的 N-of-M 計數與 fail-closed。同一位 reviewer 的新 receipt 取代
/// 舊 receipt；另一位 reviewer 的 receipt 缺漏時仍不足額。
#[test]
fn dual_supersede_and_missing_receipt_fail_closed() {
    let head = "f".repeat(40);
    let mut state = tests::new_state(&head, ReviewClass::Dual);
    state.ci_state = CiState::Green {
        sha: head.clone(),
        observed_at: chrono::Utc::now().to_rfc3339(),
    };
    assert!(matches!(
        merge_readiness(&state),
        Err(MergeDeficit::InsufficientVerified { have: 0, need: 2 })
    ));

    let reviewer_one = crate::types::InstanceId::new();
    let reviewer_two = crate::types::InstanceId::new();
    let make = |name: &str, instance: crate::types::InstanceId, verdict: ReviewVerdict| {
        let source_id = format!("m-{}", uuid::Uuid::new_v4());
        ReviewReceiptSummary {
            receipt_id: format!("review-receipt:{source_id}"),
            source_id,
            evidence_digest: "a".repeat(64),
            assignment_id: uuid::Uuid::new_v4(),
            reviewer_instance_id: instance,
            reviewer_name: name.into(),
            repo: REPO.into(),
            pr_number: PR,
            branch: BRANCH.into(),
            task_id: format!("t-{name}"),
            reviewed_head: head.clone(),
            review_class: ReviewClass::Dual,
            slot: ReviewSlot::Primary,
            verdict,
        }
    };
    apply_receipt_to_state(
        &mut state,
        make("one", reviewer_one, ReviewVerdict::Rejected),
    );
    apply_receipt_to_state(
        &mut state,
        make("two", reviewer_two, ReviewVerdict::Verified),
    );
    assert!(matches!(
        merge_readiness(&state),
        Err(MergeDeficit::NonVerifiedReceipt { .. })
    ));
    apply_receipt_to_state(
        &mut state,
        make("one", reviewer_one, ReviewVerdict::Verified),
    );
    assert_eq!(state.validated_review_receipts.len(), 2);
    assert_eq!(merge_readiness(&state), Ok(()));
    state
        .validated_review_receipts
        .retain(|r| r.reviewer_instance_id == reviewer_one);
    assert!(matches!(
        merge_readiness(&state),
        Err(MergeDeficit::InsufficientVerified { have: 1, need: 2 })
    ));
}

/// verdict_buffer 重放路徑：PR state 尚不存在時，A1（舊名）與 A2（新名，同 InstanceId）
/// 的 receipt 都先被緩衝；state 建立後由 `record_ci_result`（持有 assignment lock）
/// 重放，同 instance 的後一張取代前一張。回傳重放並 redrive 之後的 home。
/// 緩衝目錄的讀取順序不是抵達順序，所以呼叫端的斷言不得依賴誰贏。
fn buffered_rename_replay(
    tag: &str,
    first: ReviewVerdict,
    second: ReviewVerdict,
) -> std::path::PathBuf {
    let home = home(tag);
    let head = "2".repeat(40);
    let reviewer = crate::types::InstanceId::new();

    let r1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let r2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    // subject 不存在：兩張 receipt 都進 typed buffer。
    submit(&home, &r1, first);
    submit(&home, &r2, second);
    assert!(load(&home, REPO, BRANCH).is_none(), "state 尚未建立");

    with_pr_state_or_create(
        &home,
        REPO,
        BRANCH,
        || {
            let mut state = new_for_branch(REPO, BRANCH, &head, ReviewClass::Single);
            state.pr_number = PR;
            state.draft_state = DraftState::Ready;
            state
        },
        |_| {},
    )
    .unwrap();
    record_ci_result(
        &home,
        REPO,
        BRANCH,
        &head,
        CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    store::redrive_reserved(&home, REPO, BRANCH);
    home
}

/// 兩張都是 VERIFIED：無論哪一張被取代，被取代那張的舊 record 必須一併退役，
/// reconcile 後不得被重新推導成 reservation。
#[test]
fn buffered_replay_retires_superseded_old_name_record() {
    let home = buffered_rename_replay(
        "buffered-replay",
        ReviewVerdict::Verified,
        ReviewVerdict::Verified,
    );
    let state = load(&home, REPO, BRANCH).unwrap();
    assert_eq!(
        state.validated_review_receipts.len(),
        1,
        "重放後同 instance 只留一張 receipt"
    );
    assert_eq!(
        readiness(&home),
        Ok(()),
        "重放取代後被取代的 record 被重新推導成 reservation: {:?}",
        reserved_targets(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 同一批緩衝裡有非 VERIFIED（順序無法證明）：結果必須固定為 fail-closed，
/// 不得因目錄讀取順序而偶爾放行。多跑幾輪，讓讀取順序有機會不同。
#[test]
fn buffered_replay_mixed_verdicts_always_fail_closed() {
    for round in 0..24 {
        let (first, second) = if round % 2 == 0 {
            (ReviewVerdict::Rejected, ReviewVerdict::Verified)
        } else {
            (ReviewVerdict::Verified, ReviewVerdict::Rejected)
        };
        let home = buffered_rename_replay("buffered-mixed", first, second);
        assert!(
            matches!(
                readiness(&home),
                Err(MergeDeficit::NonVerifiedReceipt { .. })
            ),
            "round {round}: 順序不明時必須擋住，實際 {:?}",
            readiness(&home)
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
