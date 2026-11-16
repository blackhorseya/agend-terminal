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

/// 派一張新的 assignment；同一 target 的舊 record 會被 `persist` 取代。
fn assign(
    home: &Path,
    target: &str,
    reviewer: crate::types::InstanceId,
    head: &str,
    task: &str,
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
        "2026-11-16T00:00:00Z",
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
