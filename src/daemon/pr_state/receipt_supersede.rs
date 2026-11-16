//! 同一位 reviewer 的新 receipt 取代舊 receipt 的規則（純搬移自 `pr_state/mod.rs`，
//! 為了讓 mod.rs 維持在 anti-monolith 行數上限內；行為不變）。
//!
//! 取代只依「派發序號」（`PrState::receipt_dispatch_seq`），不看抵達順序、不看
//! wall-clock；細節見 [`apply_receipt_to_state`]。

use super::*;

/// [`apply_receipt_to_state`] 的結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReceiptApplication {
    /// 已套用；`superseded` ＝ 被這張 receipt 取代的舊 receipt 的 assignment id。
    Applied { superseded: Vec<uuid::Uuid> },
    /// 同 reviewer instance 在同 subject 已有「派發較晚」的 receipt，這張是過時的：
    /// 沒有套用、也沒有取代任何東西。`newer_seq` ＝ 那張較新 receipt 的序號。
    Stale { newer_seq: u64 },
}

pub(super) fn apply_receipt_to_state(
    state: &mut PrState,
    receipt: crate::review_receipt::ReviewReceiptSummary,
) -> ReceiptApplication {
    let reviewer = receipt.reviewer_name.clone();
    let head = receipt.reviewed_head.clone();
    let verdict = receipt.verdict;
    let assignment_id = receipt.assignment_id;
    // 一個 assignment 只留一張 receipt（slot 只是路由資訊，不是唯一鍵）。
    //
    // 同一位 reviewer instance 在同一 subject（repo/PR/branch/class/head）重審時，誰
    // 取代誰只依「派發序號」（`receipt_dispatch_seq`，落地時由 assignment lock 內讀到的
    // record 蓋入）：
    //   - 兩邊都有序號、舊 < 新：新 receipt 取代舊的；
    //   - 兩邊都有序號、舊 > 新：這張是過時的。過時的 VERIFIED 不套用（回傳 Stale）；過時
    //     的非 VERIFIED（REJECTED／UNVERIFIED）一律保留、與較新的並存、繼續擋 merge；
    //   - 缺序號（任一邊或兩邊，例如升級前的舊資料）或序號相等：順序無法證明，兩張都保留、
    //     什麼都不取代（非 VERIFIED 仍擋 merge；VERIFIED 依 instance 去重，不會被算成
    //     兩位）。「依抵達順序取代」本身就是 fail-open（晚到的舊 receipt 會推翻較新的），
    //     所以不拿它當缺序號時的退路；代價是升級前就派出、仍在進行的重審需重新派審一次。
    // 不同 reviewer 的 receipt 互不影響，dual 的 N-of-M 計數不變。
    let new_seq = state
        .receipt_dispatch_seq
        .get(&receipt.assignment_id)
        .copied();
    let conflicts = |old: &crate::review_receipt::ReviewReceiptSummary| {
        old.assignment_id != receipt.assignment_id
            && old.reviewer_instance_id == receipt.reviewer_instance_id
            && old.repo == receipt.repo
            && old.pr_number == receipt.pr_number
            && old.branch == receipt.branch
            && old.review_class == receipt.review_class
            && old.reviewed_head == receipt.reviewed_head
    };
    let mut replace_ids = Vec::new();
    for old in state
        .validated_review_receipts
        .iter()
        .filter(|old| conflicts(old))
    {
        let old_seq = state.receipt_dispatch_seq.get(&old.assignment_id).copied();
        match (old_seq, new_seq) {
            (Some(old_seq), Some(new_seq)) if old_seq < new_seq => {
                replace_ids.push(old.assignment_id)
            }
            // 看起來過時：只有 VERIFIED 可以被丟掉（最壞只是多擋）。非 VERIFIED 永遠
            // 保留、與較新的 receipt 並存，繼續擋 merge——不論序號是因為什麼原因出錯，
            // 「判定過時就丟掉」都不能成為放行的路徑。
            (Some(old_seq), Some(new_seq))
                if old_seq > new_seq
                    && matches!(verdict, crate::review_receipt::ReviewVerdict::Verified) =>
            {
                return ReceiptApplication::Stale { newer_seq: old_seq }
            }
            _ => {}
        }
    }
    state.validated_review_receipts.retain(|old| {
        old.assignment_id != receipt.assignment_id && !replace_ids.contains(&old.assignment_id)
    });
    let superseded = replace_ids;
    state.receipt_dispatch_seq.retain(|id, _| {
        *id == assignment_id
            || state
                .validated_review_receipts
                .iter()
                .any(|r| r.assignment_id == *id)
    });
    // 任何 verdict 的 receipt 落地都釋放 reservation：該 assignment 本身，以及同一
    // reviewer 被這次重審取代的舊 assignment（authority 每個 target 只留一筆 record）。
    // REJECTED / UNVERIFIED 仍由 receipt 本身擋 merge（NonVerifiedReceipt），直到被
    // 同一位 reviewer 的新 receipt 取代。
    let reviewer_name = receipt.reviewer_name.clone();
    state.validated_review_receipts.push(receipt);
    state.reserved_assignments.retain(|reserved| {
        reserved.assignment_id != assignment_id && reserved.target != reviewer_name
    });
    let kind = match verdict {
        crate::review_receipt::ReviewVerdict::Verified => VerdictKind::Verified,
        crate::review_receipt::ReviewVerdict::Rejected => VerdictKind::Rejected { reason: None },
        crate::review_receipt::ReviewVerdict::Unverified => VerdictKind::Unverified,
    };
    // Legacy collapsed state remains display-compatible only; is_merge_ready and
    // assignment evidence consume validated_review_receipts instead.
    apply(
        state,
        Event::VerdictObserved {
            reviewer: &reviewer,
            reviewed_head: &head,
            kind,
        },
    );
    ReceiptApplication::Applied { superseded }
}
