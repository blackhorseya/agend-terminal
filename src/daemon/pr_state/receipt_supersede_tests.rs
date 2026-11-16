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
    // 裸 state 沒有 record：每張 receipt 明確給派發序號（reviewer one 的第二張較晚派）。
    let mut next_seq = 0u64;
    let mut apply_with_seq = |state: &mut PrState, receipt: ReviewReceiptSummary| {
        next_seq += 1;
        state
            .receipt_dispatch_seq
            .insert(receipt.assignment_id, next_seq);
        apply_receipt_to_state(state, receipt);
    };
    apply_with_seq(
        &mut state,
        make("one", reviewer_one, ReviewVerdict::Rejected),
    );
    apply_with_seq(
        &mut state,
        make("two", reviewer_two, ReviewVerdict::Verified),
    );
    assert!(matches!(
        merge_readiness(&state),
        Err(MergeDeficit::NonVerifiedReceipt { .. })
    ));
    apply_with_seq(
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

/// 同一批緩衝裡同一 instance 有兩張衝突的 receipt：由「派發順序」決定誰贏（後派的
/// A2 贏），與緩衝目錄的讀取順序無關。跑多輪讓讀取順序有機會不同；任何一輪都
/// 不得因讀取順序而讓較舊的 receipt 贏。
#[test]
fn buffered_replay_later_dispatch_wins_regardless_of_read_order() {
    for round in 0..24 {
        let (first, second) = if round % 2 == 0 {
            (ReviewVerdict::Rejected, ReviewVerdict::Verified)
        } else {
            (ReviewVerdict::Verified, ReviewVerdict::Rejected)
        };
        let home = buffered_rename_replay("buffered-mixed", first, second);
        match second {
            ReviewVerdict::Verified => assert_eq!(
                readiness(&home),
                Ok(()),
                "round {round}: 後派的 VERIFIED 應取代較舊的 REJECTED，實際 {:?}",
                readiness(&home)
            ),
            _ => assert!(
                matches!(
                    readiness(&home),
                    Err(MergeDeficit::NonVerifiedReceipt { .. })
                ),
                "round {round}: 後派的 REJECTED 不得被較舊的 VERIFIED 推翻，實際 {:?}",
                readiness(&home)
            ),
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}

// ───────── 緩衝的舊 receipt 重放時，不得取代 state 裡已有的較新 receipt（PR #35 r1 P1）─────────
//
// 緩衝只會發生在 PR state 不存在的期間，因此 state 裡已經存在的同 instance
// receipt 一定比任何緩衝中的 receipt 晚抵達。重放不得讓舊的取代新的，也不得因此
// 退役較新的 record。

fn create_state_only(home: &Path, head: &str) {
    with_pr_state_or_create(
        home,
        REPO,
        BRANCH,
        || {
            let mut state = new_for_branch(REPO, BRANCH, head, ReviewClass::Single);
            state.pr_number = PR;
            state.draft_state = DraftState::Ready;
            state
        },
        |_| {},
    )
    .unwrap();
}

fn ci_tick_green(home: &Path, head: &str) {
    record_ci_result(
        home,
        REPO,
        BRANCH,
        head,
        CiConclusion::Green,
        vec!["lead".into()],
        ReviewClass::Single,
    );
    store::redrive_reserved(home, REPO, BRANCH);
}

/// A1（舊名）VERIFIED 在 state 不存在時被緩衝；state 建立後 A2（新名，同 instance）
/// REJECTED 直接落地。CI tick 重放 A1：舊的不得取代新的，A2 的 REJECTED 與 record
/// 都要保留，merge 仍須被擋。
#[test]
fn buffered_older_verified_does_not_replace_newer_rejected() {
    let home = home("buffered-late-old-verified");
    let head = "5".repeat(40);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &a1, ReviewVerdict::Verified); // 緩衝
    create_state_only(&home, &head);
    submit(&home, &a2, ReviewVerdict::Rejected); // 直接落地
    ci_tick_green(&home, &head);

    let state = load(&home, REPO, BRANCH).unwrap();
    assert_eq!(state.validated_review_receipts.len(), 1);
    assert_eq!(
        state.validated_review_receipts[0].assignment_id, a2.assignment_id,
        "較新的 REJECTED 不得被緩衝的舊 VERIFIED 取代"
    );
    assert!(
        record_exists(&home, "new-reviewer-name"),
        "較新的 record 不得被退役"
    );
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "新的 REJECTED 必須擋住 merge，實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 緩衝的舊 REJECTED 重放時，state 已有較新的 VERIFIED：非 VERIFIED 永遠不丟掉（過時的
/// 非 VERIFIED 保留、繼續擋 merge；最壞只是多擋、需要重新派審），不放行。
#[test]
fn buffered_older_rejected_is_kept_and_blocks_next_to_newer_verified() {
    let home = home("buffered-late-old-rejected");
    let head = "6".repeat(40);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &a1, ReviewVerdict::Rejected); // 緩衝
    create_state_only(&home, &head);
    submit(&home, &a2, ReviewVerdict::Verified); // 直接落地
    ci_tick_green(&home, &head);

    let state = load(&home, REPO, BRANCH).unwrap();
    assert_eq!(
        state.validated_review_receipts.len(),
        2,
        "過時的非 VERIFIED 必須保留（不取代也不被取代）"
    );
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "保留的 REJECTED 必須擋住，實際 {:?}",
        readiness(&home)
    );
    // 之後同 reviewer 重新審查（序號更大）一次 VERIFIED 就能放行，不需要 force。
    let a3 = assign(&home, "third-reviewer-name", reviewer, &head, "t-review-3");
    submit(&home, &a3, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert_eq!(
        readiness(&home),
        Ok(()),
        "重新審查後仍被擋: {:?}",
        reserved_targets(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Guard：被略過的舊 receipt 之後，同 reviewer 再以第三個名字 VERIFIED，必須能
/// 不靠 force 放行（沒有任何舊名 record 殘留成 reservation）。
#[test]
fn buffered_older_skipped_then_re_review_opens_gate() {
    let home = home("buffered-late-old-then-re-review");
    let head = "7".repeat(40);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &a1, ReviewVerdict::Verified); // 緩衝
    create_state_only(&home, &head);
    submit(&home, &a2, ReviewVerdict::Rejected);
    ci_tick_green(&home, &head);
    assert!(is_blocked(&home));

    let a3 = assign(&home, "third-reviewer-name", reviewer, &head, "t-review-3");
    submit(&home, &a3, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert_eq!(
        readiness(&home),
        Ok(()),
        "重新審查後仍有殘留 reservation: {:?}",
        reserved_targets(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

// ───────── 派發順序（dispatch_seq）才是取代／退役的依據（PR #35 r1 P1）─────────

/// 直接修改磁碟上的 record JSON（以 serde_json::Value，不依賴 struct 欄位），
/// 模擬舊資料（缺 seq）或相等 seq。
fn set_dispatch_seq(home: &Path, target: &str, seq: Option<u64>) {
    let path = store::record_path_for_test(home, REPO, BRANCH, target);
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let object = value.as_object_mut().unwrap();
    match seq {
        Some(seq) => {
            object.insert("dispatch_seq".into(), serde_json::json!(seq));
        }
        None => {
            object.remove("dispatch_seq");
        }
    }
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn dispatch_seq_of(home: &Path, target: &str) -> Option<u64> {
    let path = store::record_path_for_test(home, REPO, BRANCH, target);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value.get("dispatch_seq").and_then(|v| v.as_u64())
}

/// reviewer 的 late_old_receipt probe（直接落地版）：A2（新名，後派）REJECTED
/// 先落地，A1（舊名，先派）的 VERIFIED 晚到。舊的不得取代新的，也不得退役 A2；
/// 被判定過時的 A1 本身的 record 要退役（它已被較新的 receipt 回應過）。
#[test]
fn direct_late_old_verified_does_not_replace_newer_rejected() {
    let home = home("direct-late-old");
    let head = "8".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &a2, ReviewVerdict::Rejected);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(is_blocked(&home));

    // A1 的 VERIFIED 晚到；是否被接受不重要，結果不得放行。
    let source_id = format!("m-{}", uuid::Uuid::new_v4());
    let late = crate::review_receipt::ValidatedCodeReviewReceipt::for_test(ReviewReceiptSummary {
        receipt_id: format!("review-receipt:{source_id}"),
        source_id,
        evidence_digest: "a".repeat(64),
        assignment_id: a1.assignment_id,
        reviewer_instance_id: reviewer,
        reviewer_name: a1.target.clone(),
        repo: REPO.into(),
        pr_number: PR,
        branch: BRANCH.into(),
        task_id: a1.task_id.clone(),
        reviewed_head: head.clone(),
        review_class: ReviewClass::Single,
        slot: ReviewSlot::Primary,
        verdict: ReviewVerdict::Verified,
    });
    let _ = record_validated_receipt(&home, &late, None);
    store::redrive_reserved(&home, REPO, BRANCH);

    assert!(
        record_exists(&home, "new-reviewer-name"),
        "較新的 record 不得被晚到的舊 receipt 退役"
    );
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "晚到的舊 VERIFIED 不得推翻較新的 REJECTED，實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 這個問題原本在 main 上就存在：A2 的 record 已被正常的 task terminal cleanup
/// 移除（不是被本 PR 退役），晚到的 A1 VERIFIED 仍不得取代 A2 的 REJECTED。
#[test]
fn late_old_verified_after_newer_record_cleanup_does_not_open_gate() {
    let home = home("late-old-after-cleanup");
    let head = "9".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    submit(&home, &a2, ReviewVerdict::Rejected);
    // A2 的 task terminal cleanup：record 被移除。
    let _ = std::fs::remove_file(store::record_path_for_test(
        &home,
        REPO,
        BRANCH,
        "new-reviewer-name",
    ));

    let source_id = format!("m-{}", uuid::Uuid::new_v4());
    let late = crate::review_receipt::ValidatedCodeReviewReceipt::for_test(ReviewReceiptSummary {
        receipt_id: format!("review-receipt:{source_id}"),
        source_id,
        evidence_digest: "a".repeat(64),
        assignment_id: a1.assignment_id,
        reviewer_instance_id: reviewer,
        reviewer_name: a1.target.clone(),
        repo: REPO.into(),
        pr_number: PR,
        branch: BRANCH.into(),
        task_id: a1.task_id.clone(),
        reviewed_head: head.clone(),
        review_class: ReviewClass::Single,
        slot: ReviewSlot::Primary,
        verdict: ReviewVerdict::Verified,
    });
    let _ = record_validated_receipt(&home, &late, None);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "A2 的 record 已被 cleanup 後，晚到的舊 VERIFIED 仍不得放行，實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 舊資料（缺 seq）：順序無法證明，兩張 receipt 都保留、什麼都不退役；
/// 非 VERIFIED 仍擋 merge，VERIFIED 不會因為同 instance 兩張而被算成兩位。
#[test]
fn missing_dispatch_seq_keeps_both_receipts_and_blocks() {
    let home = home("missing-seq");
    let head = "a".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    set_dispatch_seq(&home, "old-reviewer-name", None);
    submit(&home, &a1, ReviewVerdict::Rejected);
    submit(&home, &a2, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);

    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .validated_review_receipts
            .len(),
        2,
        "順序無法證明時不得取代"
    );
    assert!(record_exists(&home, "old-reviewer-name"));
    assert!(record_exists(&home, "new-reviewer-name"));
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 相等的 seq 同樣無法證明順序：保留兩張、不退役、保持擋住。
#[test]
fn equal_dispatch_seq_keeps_both_receipts_and_blocks() {
    let home = home("equal-seq");
    let head = "b".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    let seq = dispatch_seq_of(&home, "old-reviewer-name").expect("新 record 應有 dispatch_seq");
    set_dispatch_seq(&home, "new-reviewer-name", Some(seq));
    submit(&home, &a1, ReviewVerdict::Rejected);
    submit(&home, &a2, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);

    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .validated_review_receipts
            .len(),
        2
    );
    assert!(record_exists(&home, "old-reviewer-name"));
    assert!(record_exists(&home, "new-reviewer-name"));
    assert!(is_blocked(&home));
    let _ = std::fs::remove_dir_all(&home);
}

/// 計數器不得倒退：record 全被移除（cleanup）之後新派的 assignment，seq 仍須大於
/// 先前的；而且計數器檔案不得讓 branch 的 authority 被判成 Unreadable。
#[test]
fn dispatch_seq_is_monotonic_across_record_removal() {
    let home = home("seq-monotonic");
    let head = "c".repeat(40);
    let reviewer = crate::types::InstanceId::new();
    let _a1 = assign(&home, "reviewer-a", reviewer, &head, "t-review-1");
    let _a2 = assign(&home, "reviewer-b", reviewer, &head, "t-review-2");
    let s1 = dispatch_seq_of(&home, "reviewer-a").expect("新 record 應有 dispatch_seq");
    let s2 = dispatch_seq_of(&home, "reviewer-b").unwrap();
    assert!(s2 > s1, "同一 branch 的 seq 必須遞增");
    for target in ["reviewer-a", "reviewer-b"] {
        let _ = std::fs::remove_file(store::record_path_for_test(&home, REPO, BRANCH, target));
    }
    assert!(!matches!(
        store::probe_branch_authority(&home, REPO, BRANCH),
        store::BranchAuthority::Unreadable
    ));
    let _a3 = assign(&home, "reviewer-c", reviewer, &head, "t-review-3");
    let s3 = dispatch_seq_of(&home, "reviewer-c").unwrap();
    assert!(s3 > s2, "record 移除後 seq 不得倒退或重用");
    // 同 id 重新 persist 不得重新配發。
    let a3 = store::lookup_by_assignment_id_strict(&home, _a3.assignment_id).unwrap();
    store::persist(&home, &a3).unwrap();
    assert_eq!(dispatch_seq_of(&home, "reviewer-c"), Some(s3));
    assert!(!matches!(
        store::probe_branch_authority(&home, REPO, BRANCH),
        store::BranchAuthority::Unreadable
    ));
    let _ = std::fs::remove_dir_all(&home);
}

// ───────── 兩邊都缺 dispatch_seq（升級前的舊資料）：fail closed，不依抵達順序取代 ─────────
//
// 在 main 上實測，「依抵達順序取代」本身就是 fail-open（晚到的舊 VERIFIED 會推翻較新的
// REJECTED）；缺序號的舊資料正好走這條路，所以兩邊都缺序號時也要視為順序無法證明。

/// 晚到的舊 VERIFIED：A1、A2 都是舊資料（缺 seq）。A2 REJECTED 先落地，A1 VERIFIED
/// 晚到：兩張都保留、什麼都不退役，gate 保持關閉。
#[test]
fn legacy_both_missing_seq_late_old_verified_keeps_gate_closed() {
    let home = home("legacy-late-old");
    let head = "d".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let a1 = assign(&home, "old-reviewer-name", reviewer, &head, "t-review-1");
    let a2 = assign(&home, "new-reviewer-name", reviewer, &head, "t-review-2");
    set_dispatch_seq(&home, "old-reviewer-name", None);
    set_dispatch_seq(&home, "new-reviewer-name", None);
    submit(&home, &a2, ReviewVerdict::Rejected);
    submit(&home, &a1, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);

    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .validated_review_receipts
            .len(),
        2,
        "順序無法證明時不得取代"
    );
    assert!(record_exists(&home, "old-reviewer-name"));
    assert!(record_exists(&home, "new-reviewer-name"));
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 同名重審 REJECTED → VERIFIED，兩邊都缺 seq：保持擋住（升級前就派出的重審需要重新
/// 派審一次，不需要 force）。
#[test]
fn legacy_both_missing_seq_same_name_re_review_stays_blocked() {
    let home = home("legacy-same-name");
    let head = "e".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let r1 = assign(&home, "reviewer", reviewer, &head, "t-review-1");
    set_dispatch_seq(&home, "reviewer", None);
    submit(&home, &r1, ReviewVerdict::Rejected);
    let r2 = assign(&home, "reviewer", reviewer, &head, "t-review-2");
    set_dispatch_seq(&home, "reviewer", None);
    submit(&home, &r2, ReviewVerdict::Verified);
    store::redrive_reserved(&home, REPO, BRANCH);

    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .validated_review_receipts
            .len(),
        2
    );
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

// ───────── 計數器遺失／回退：序號不得小於 PR state 已記錄的序號（PR #35 r2 P1）─────────
//
// PR state 裡的 `receipt_dispatch_seq` 是比較的另一半。就算 active record 都被 cleanup
// 移除、計數器檔也不見了，新配發的序號仍必須大於 PR state 已記錄的任何序號；否則較新派發
// 的 REJECTED 會被當成「過時」而忽略，gate 因此打開。

fn rejected_receipt_for(
    assignment: &ActiveAssignment,
    reviewer: crate::types::InstanceId,
    head: &str,
) -> crate::review_receipt::ValidatedCodeReviewReceipt {
    let source_id = format!("m-{}", uuid::Uuid::new_v4());
    crate::review_receipt::ValidatedCodeReviewReceipt::for_test(ReviewReceiptSummary {
        receipt_id: format!("review-receipt:{source_id}"),
        source_id,
        evidence_digest: "a".repeat(64),
        assignment_id: assignment.assignment_id,
        reviewer_instance_id: reviewer,
        reviewer_name: assignment.target.clone(),
        repo: REPO.into(),
        pr_number: PR,
        branch: BRANCH.into(),
        task_id: assignment.task_id.clone(),
        reviewed_head: head.to_string(),
        review_class: ReviewClass::Single,
        slot: ReviewSlot::Primary,
        verdict: ReviewVerdict::Rejected,
    })
}

/// reviewer 的 missing_counter probe：舊 VERIFIED 已在 state（序號 2），record 全被 cleanup
/// 移除，計數器檔遺失，之後新派的 assignment 序號必須大於 2，它的 REJECTED 必須生效。
#[test]
fn missing_counter_after_record_cleanup_does_not_ignore_newer_rejection() {
    let home = home("missing-counter");
    let head = "f".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let _dummy = assign(
        &home,
        "dummy",
        crate::types::InstanceId::new(),
        &head,
        "t-dummy",
    );
    let old = assign(&home, "old-reviewer-name", reviewer, &head, "t-old");
    let old_seq = dispatch_seq_of(&home, "old-reviewer-name").unwrap();
    submit(&home, &old, ReviewVerdict::Verified);
    for target in ["dummy", "old-reviewer-name"] {
        std::fs::remove_file(store::record_path_for_test(&home, REPO, BRANCH, target)).unwrap();
    }
    std::fs::remove_file(store::branch_dir(&home, REPO, BRANCH).join("dispatch-seq.counter"))
        .unwrap();

    let newer = assign(&home, "new-reviewer-name", reviewer, &head, "t-new");
    let new_seq = dispatch_seq_of(&home, "new-reviewer-name").unwrap();
    assert!(
        new_seq > old_seq,
        "計數器遺失後序號倒退: old={old_seq} new={new_seq}"
    );
    let _ = record_validated_receipt(&home, &rejected_receipt_for(&newer, reviewer, &head), None);
    store::redrive_reserved(&home, REPO, BRANCH);
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "較新派發的 REJECTED 必須擋住 merge，實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 計數器被改小（回退）同樣不得讓序號倒退。
#[test]
fn lowered_counter_does_not_regress_dispatch_seq() {
    let home = home("lowered-counter");
    let head = "f".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let old = assign(&home, "old-reviewer-name", reviewer, &head, "t-old");
    let old_seq = dispatch_seq_of(&home, "old-reviewer-name").unwrap();
    submit(&home, &old, ReviewVerdict::Verified);
    let _ = std::fs::remove_file(store::record_path_for_test(
        &home,
        REPO,
        BRANCH,
        "old-reviewer-name",
    ));
    std::fs::write(
        store::branch_dir(&home, REPO, BRANCH).join("dispatch-seq.counter"),
        "0",
    )
    .unwrap();

    let _newer = assign(&home, "new-reviewer-name", reviewer, &head, "t-new");
    let new_seq = dispatch_seq_of(&home, "new-reviewer-name").unwrap();
    assert!(
        new_seq > old_seq,
        "計數器回退後序號倒退: old={old_seq} new={new_seq}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// PR state 檔存在但讀不出來時，無法確認序號下限：persist 必須失敗（fail closed），
/// 不得在不知道下限的情況下配發序號。
#[test]
fn unreadable_pr_state_makes_persist_fail_closed() {
    let home = home("unreadable-state");
    let head = "f".repeat(40);
    seed_state(&home, &head);
    let state_path = pr_state_dir(&home).join(pr_state_filename(REPO, BRANCH));
    std::fs::write(&state_path, "{ not json").unwrap();
    let record = ActiveAssignment::new_pending_typed(
        REPO,
        BRANCH,
        "reviewer",
        crate::types::InstanceId::new(),
        PR,
        &head,
        ReviewSlot::Primary,
        "lead",
        "t-review",
        ReviewClass::Single,
        ReviewAuthor::External("octocat".into()),
        "review",
        None,
        None,
        "2026-11-16T00:00:00Z",
    );
    assert!(
        store::persist(&home, &record).is_err(),
        "PR state 讀不出來時不得配發序號"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// 不對稱規則：不論 seq 為什麼出錯（計數器遺失、被改、舊資料……），「過時」的非 VERIFIED
/// receipt 都不能被丟掉，也不能因此放行。這裡直接把新派發 A2 的序號改成比舊 VERIFIED
/// 還小，模擬序號錯誤：A2 的 REJECTED 必須保留並擋住 merge。
#[test]
fn stale_non_verified_receipt_is_never_dropped_even_with_broken_seq() {
    let home = home("broken-seq-rejected");
    let head = "f".repeat(40);
    seed_state(&home, &head);
    let reviewer = crate::types::InstanceId::new();
    let old = assign(&home, "old-reviewer-name", reviewer, &head, "t-old");
    set_dispatch_seq(&home, "old-reviewer-name", Some(100));
    submit(&home, &old, ReviewVerdict::Verified);
    let newer = assign(&home, "new-reviewer-name", reviewer, &head, "t-new");
    set_dispatch_seq(&home, "new-reviewer-name", Some(1));
    let _ = record_validated_receipt(&home, &rejected_receipt_for(&newer, reviewer, &head), None);
    store::redrive_reserved(&home, REPO, BRANCH);

    assert_eq!(
        load(&home, REPO, BRANCH)
            .unwrap()
            .validated_review_receipts
            .len(),
        2,
        "序號看起來過時的 REJECTED 也必須保留"
    );
    assert!(
        record_exists(&home, "new-reviewer-name"),
        "不得因此退役它自己的 record"
    );
    assert!(
        matches!(
            readiness(&home),
            Err(MergeDeficit::NonVerifiedReceipt { .. })
        ),
        "實際 {:?}",
        readiness(&home)
    );
    let _ = std::fs::remove_dir_all(&home);
}
