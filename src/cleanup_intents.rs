use std::path::{Path, PathBuf};

mod owner_attestation;
#[cfg(test)]
use owner_attestation::KEEP_TTL_DAYS;
use owner_attestation::{retention_obligations, retention_project, settle_by_owner_attestation};

fn intents_dir(home: &Path) -> PathBuf {
    home.join("cleanup-intents")
}

fn intent_key(repo: &str, branch: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    branch.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Candidate ids of every intent that is still on disk, or `None` when the
/// intent store cannot be read completely (unreadable dir/entry/file, or a file
/// that does not parse). A missing directory is a fact — no intents — but an
/// unreadable one is not, and `None` tells the caller it may NOT conclude that
/// an intent is gone (-160 c: absence is only proven by a complete listing).
pub(crate) fn live_intent_ids(home: &Path) -> Option<std::collections::HashSet<String>> {
    let entries = match std::fs::read_dir(intents_dir(home)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(std::collections::HashSet::new())
        }
        Err(_) => return None,
    };
    let mut ids = std::collections::HashSet::new();
    for entry in entries {
        let path = entry.ok()?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // An unreadable/unparseable file makes the listing incomplete, so the
        // whole answer becomes "unknown" (`None`) — deliberately NOT `continue`.
        let Ok(body) = std::fs::read_to_string(&path) else {
            return None;
        };
        let intent: CleanupIntent = serde_json::from_str(&body).ok()?;
        ids.insert(intent_key(&intent.repo, &intent.branch));
    }
    Some(ids)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CleanupIntent {
    pub repo: String,
    pub branch: String,
    pub expected_head: String,
    pub task_id: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scm_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
}

pub(crate) fn aged_review_intents(
    home: &Path,
    now: chrono::DateTime<chrono::Utc>,
    min_age: chrono::Duration,
) -> Vec<CleanupIntent> {
    let entries = match std::fs::read_dir(intents_dir(home)) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|body| serde_json::from_str::<CleanupIntent>(&body).ok())
        .filter(|intent| intent.branch.starts_with("review/"))
        .filter(|intent| {
            chrono::DateTime::parse_from_rfc3339(&intent.created_at)
                .map(|created| now.signed_duration_since(created.with_timezone(&chrono::Utc)))
                .is_ok_and(|age| age >= min_age)
        })
        .collect()
}

/// Retry the event-driven merged-branch task close from durable review intents.
/// A merge observation can be missed, and older code also skipped every task
/// when several exact structured links shared one branch. Review intents retain
/// enough repo + task identity to heal that state on the periodic sweep.
pub(crate) fn reconcile_merged_subject_tasks(home: &Path) {
    let entries = match std::fs::read_dir(intents_dir(home)) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut checked = std::collections::HashSet::new();
    for intent in entries
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|body| serde_json::from_str::<CleanupIntent>(&body).ok())
        .filter(|intent| intent.branch.starts_with("review/"))
    {
        let Ok(task) = crate::tasks::load_routed(home, &intent.task_id) else {
            continue;
        };
        if review_task_terminal(&task.record().status) {
            continue;
        }
        let Some(subject_branch) = task.record().branch.as_deref() else {
            continue;
        };
        let repo = Path::new(&intent.repo);
        if !repo.is_dir() || !checked.insert((intent.repo.clone(), subject_branch.to_string())) {
            continue;
        }
        let default = crate::git_helpers::default_branch(repo);
        if matches!(
            crate::branch_sweep::pr_merge_status(repo, &default, subject_branch),
            crate::branch_sweep::PrMergeStatus::Merged
        ) {
            crate::status_summary::auto_close_merged_tasks(home, subject_branch);
        }
    }
}

pub(crate) fn persist_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    expected_head: &str,
    task_id: &str,
    scm_slug: Option<&str>,
    pr_number: Option<u64>,
) -> Result<(), String> {
    let dir = intents_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create cleanup-intents dir: {e}"))?;
    let intent = CleanupIntent {
        repo: repo.to_string(),
        branch: branch.to_string(),
        expected_head: expected_head.to_string(),
        task_id: task_id.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        scm_slug: scm_slug.map(String::from),
        pr_number,
    };
    let key = intent_key(repo, branch);
    let path = dir.join(format!("{key}.json"));
    let body = serde_json::to_string_pretty(&intent)
        .map_err(|e| format!("serialize cleanup intent: {e}"))?;
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("write cleanup intent: {e}"))
}

#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum SettleOutcome {
    Deleted,
    Pending(String),
    HeadDrift(String),
    BranchAbsent,
    GitError(String),
    DeleteFailed(String),
}

/// Settle a cleanup intent: delete the branch with expected-head CAS.
///
/// `merged` is the authoritative terminal signal — only `true` authorizes
/// deletion. `pr_number` provides generation identity — when present on
/// both the event and the intent, they must match to prevent a stale
/// merged event from settling a newer intent.
///
/// Intent file is removed ONLY after successful delete or confirmed branch
/// absence. Git/I/O errors preserve the intent for retry.
pub(crate) fn settle_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    merged: bool,
    event_pr_number: Option<u64>,
) -> Option<SettleOutcome> {
    let key = intent_key(repo, branch);
    let path = intents_dir(home).join(format!("{key}.json"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return None;
    };
    let Ok(intent) = serde_json::from_str::<CleanupIntent>(&content) else {
        return None;
    };

    if intent.repo != repo || intent.branch != branch {
        return None;
    }

    if !merged {
        return Some(SettleOutcome::Pending(format!(
            "cleanup intent for '{branch}': not yet merged — intent preserved"
        )));
    }

    // Generation identity: intent with pr_number requires a matching event
    // pr_number. Missing event generation when intent has one → fail-closed.
    match (intent.pr_number, event_pr_number) {
        (Some(intent_pr), Some(event_pr)) if intent_pr != event_pr => {
            return Some(SettleOutcome::Pending(format!(
                "cleanup intent for '{branch}': PR generation mismatch \
                 (intent=#{intent_pr}, event=#{event_pr}) — intent preserved"
            )));
        }
        (Some(intent_pr), None) => {
            return Some(SettleOutcome::Pending(format!(
                "cleanup intent for '{branch}': intent carries PR #{intent_pr} \
                 but terminal event has no generation — fail-closed"
            )));
        }
        _ => {}
    }

    // #3503: merge is confirmed and (when both sides carry generation) the
    // generation matches — retire any OPEN branch-retention obligation on
    // this lane now, independent of the local branch-delete outcome below.
    // This is the chokepoint both the CI-watch event path (`settle_by_scm_slug`)
    // and the periodic sweep (`sweep_settle_merged`) funnel through once
    // `merged` is authoritative, so retirement fires from either source.
    owner_attestation::retire_merged_lane(home, repo, branch);

    let source_repo = Path::new(&intent.repo);
    if !source_repo.is_dir() {
        return Some(SettleOutcome::GitError(format!(
            "cleanup intent for '{branch}': source repo '{}' not a directory — intent preserved",
            intent.repo
        )));
    }
    // Typed branch-existence check: show-ref --verify --quiet exit codes:
    // 0 = present, 1 = absent (confirmed), any other = git error (preserve).
    let full_ref = format!("refs/heads/{branch}");
    match crate::git_helpers::git_bypass(
        source_repo,
        &["show-ref", "--verify", "--quiet", &full_ref],
    ) {
        Err(e) => {
            return Some(SettleOutcome::GitError(format!(
                "cleanup intent for '{branch}': git spawn error ({e}) — intent preserved for retry"
            )));
        }
        Ok(o) => match o.status.code() {
            Some(0) => {}
            Some(1) => {
                let _ = std::fs::remove_file(&path);
                return Some(SettleOutcome::BranchAbsent);
            }
            other => {
                return Some(SettleOutcome::GitError(format!(
                    "cleanup intent for '{branch}': show-ref exit {} — intent preserved for retry",
                    other.map_or("signal".to_string(), |c| c.to_string())
                )));
            }
        },
    }
    // Branch exists — get tip SHA for CAS.
    let tip = match crate::git_helpers::git_cmd(source_repo, &["rev-parse", branch]) {
        Ok(t) => t,
        Err(e) => {
            return Some(SettleOutcome::GitError(format!(
                "cleanup intent for '{branch}': rev-parse error ({e}) — intent preserved for retry"
            )));
        }
    };
    if tip.trim() != intent.expected_head {
        return Some(SettleOutcome::HeadDrift(format!(
            "cleanup intent for '{branch}': expected head {} but actual tip is {} \
             — preserved (fail-closed)",
            intent.expected_head,
            tip.trim()
        )));
    }
    let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
    match del {
        Ok(o) if o.status.success() => {
            let _ = std::fs::remove_file(&path);
            Some(SettleOutcome::Deleted)
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Some(SettleOutcome::DeleteFailed(format!(
                "git branch -D failed: {stderr}"
            )))
        }
        Err(e) => Some(SettleOutcome::DeleteFailed(format!(
            "git branch -D failed: {e}"
        ))),
    }
}

/// Settle intents matching an SCM slug. The CI poller knows the slug but not
/// the local canonical path; this scans all intents for a slug match.
pub(crate) fn settle_by_scm_slug(
    home: &Path,
    scm_slug: &str,
    branch: &str,
    merged: bool,
    event_pr_number: Option<u64>,
) -> Option<SettleOutcome> {
    let dir = intents_dir(home);
    let entries = std::fs::read_dir(&dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if intent.branch == branch && intent.scm_slug.as_deref().is_some_and(|s| s == scm_slug) {
            return settle_intent(home, &intent.repo, branch, merged, event_pr_number);
        }
    }
    None
}

/// Durable retry consumer: scan all intents, check merge status via
/// `pr_merge_status` for each, and settle those confirmed merged. Called
/// from the periodic sweep to ensure intents are settled even if the
/// original CI watch was removed before settlement succeeded.
pub(crate) fn sweep_settle_merged(home: &Path) {
    let dir = intents_dir(home);
    let now = chrono::Utc::now();
    let mut obligations_by_project =
        std::collections::HashMap::<String, Vec<crate::task_events::TaskRecord>>::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let repo_path = Path::new(&intent.repo);
        if !repo_path.is_dir() {
            continue;
        }
        // #3517 follow-up: re-evaluate the residue BEFORE asking the merge
        // ledger anything. A lane the merge already retired can have moved on
        // since, and that question is answerable from the branch and the board
        // alone — it must not depend on the merge still being observable.
        owner_attestation::reraise_drifted_merged_lane(home, &intent);
        let default = crate::git_helpers::default_branch(repo_path);
        // Independently observe the merged PR number — never echo the
        // intent's own pr_number as the event generation.
        let observed_pr =
            crate::branch_sweep::merged_pr_number(repo_path, &default, &intent.branch);
        let Some(pr_num) = observed_pr else {
            // No merge evidence exists for this branch — the seam where the
            // merge ledger gives up. Ask the owner's recorded answer instead.
            let project = retention_project(home, &intent.repo);
            let obligations = obligations_by_project
                .entry(project.clone())
                .or_insert_with(|| retention_obligations(home, &project));
            settle_by_owner_attestation(home, &intent, now, obligations);
            continue;
        };
        match settle_intent(home, &intent.repo, &intent.branch, true, Some(pr_num)) {
            Some(SettleOutcome::Deleted) => {
                tracing::info!(
                    branch = %intent.branch,
                    "cleanup intent settled by sweep retry"
                );
            }
            Some(SettleOutcome::DeleteFailed(reason)) | Some(SettleOutcome::GitError(reason)) => {
                tracing::warn!(
                    branch = %intent.branch, %reason,
                    "cleanup intent sweep retry failed — will retry next tick"
                );
            }
            _ => {}
        }
    }
}

pub(crate) fn intent_repos(home: &Path) -> Vec<String> {
    let dir = intents_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut repos = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(intent) = serde_json::from_str::<CleanupIntent>(&content) {
                repos.insert(intent.repo);
            }
        }
    }
    repos.into_iter().collect()
}

#[allow(dead_code)]
pub(crate) fn has_intent(home: &Path, repo: &str, branch: &str) -> bool {
    let key = intent_key(repo, branch);
    intents_dir(home).join(format!("{key}.json")).exists()
}

/// Record the cleanup intent for a branch that survived a worktree release, so
/// [`reconcile_terminal_review_intents`] can settle it later. Returns the error
/// message when persistence fails, `None` on success (or when the branch tip is
/// unreadable, which leaves nothing to record).
///
/// Recording is not a deletion decision: the reconcile sweep re-verifies task
/// terminality, holders, checkout state and the head CAS on its own.
pub(crate) fn persist_release_intent(
    home: &Path,
    repo: &str,
    branch: &str,
    task_id: &str,
) -> Option<String> {
    let tip = crate::git_helpers::git_cmd(Path::new(repo), &["rev-parse", branch])
        .ok()
        .map(|s| s.trim().to_string())?;
    let scm_slug = crate::git_helpers::git_cmd(Path::new(repo), &["remote", "get-url", "origin"])
        .ok()
        .and_then(|url| crate::branch_sweep::extract_github_repo_for_intent(&url));
    // Derive PR number from task metadata for generation identity.
    let pr_number = if task_id.is_empty() {
        None
    } else {
        crate::tasks::load_routed(home, task_id)
            .ok()
            .and_then(|rt| rt.task.metadata.get("pr_number").and_then(|v| v.as_u64()))
    };
    match persist_intent(
        home,
        repo,
        branch,
        &tip,
        task_id,
        scm_slug.as_deref(),
        pr_number,
    ) {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!(
                %branch, error = %e,
                "cleanup intent persistence failed — branch may leak"
            );
            Some(e)
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ReconcileCandidate {
    pub candidate_id: String,
    pub branch: String,
    pub expected_head: String,
    pub task_id: String,
    pub outcome: String,
}

#[derive(Debug)]
pub(crate) struct ReconcileResult {
    pub candidates: Vec<ReconcileCandidate>,
    pub settled: usize,
    pub preserved: usize,
}

fn review_task_terminal(status: &crate::task_events::TaskStatus) -> bool {
    status.is_terminal() || *status == crate::task_events::TaskStatus::Verified
}

fn is_branch_checked_out(repo: &Path, branch: &str) -> bool {
    let output = match crate::git_helpers::git_bypass(repo, &["worktree", "list", "--porcelain"]) {
        Ok(o) if o.status.success() => o,
        _ => return true, // unknown → preserve (fail-closed)
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(b) = line.strip_prefix("branch refs/heads/") {
            if b == branch {
                return true;
            }
        }
    }
    false
}

pub(crate) fn reconcile_terminal_review_intents(home: &Path, dry_run: bool) -> ReconcileResult {
    let dir = intents_dir(home);
    let mut result = ReconcileResult {
        candidates: Vec::new(),
        settled: 0,
        preserved: 0,
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return result,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let intent: CleanupIntent = match serde_json::from_str(&content) {
            Ok(i) => i,
            Err(_) => continue,
        };
        if !intent.branch.starts_with("review/") {
            continue;
        }
        let cid = intent_key(&intent.repo, &intent.branch);

        if intent.task_id.is_empty() {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:no_task_id".into(),
            });
            continue;
        }

        let task_terminal = match crate::tasks::load_routed(home, &intent.task_id) {
            Ok(rt) => review_task_terminal(&rt.record().status),
            Err(_) => false,
        };
        if !task_terminal {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:task_not_terminal".into(),
            });
            continue;
        }

        let repo_path = Path::new(&intent.repo);
        // `is_dir()` 會把任何 I/O 錯誤吞成 false，無法區分「確定不存在」與
        // 「讀不到」。只有 NotFound 才是可證明的解除；其餘一律 fail-closed。
        let repo_gate = match std::fs::metadata(repo_path) {
            Ok(m) if m.is_dir() => None,
            Ok(_) => Some((false, "preserved:repo_unreadable:not_dir".into())),
            // repo 路徑確定不存在：移除 intent，outcome 不再是 preserved:*，
            // hygiene 收單器據此收掉 episode。intent 必須真的移除（或本來就不在）
            // 才算解除，否則收單會回報沒發生的事。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let unlink = if dry_run {
                    Ok(())
                } else {
                    std::fs::remove_file(&p)
                };
                match unlink {
                    // 已被別人刪掉（NotFound）也算 intent 不在了。
                    Err(u) if u.kind() != std::io::ErrorKind::NotFound => Some((
                        false,
                        format!("preserved:intent_unlink_failed:{:?}", u.kind()),
                    )),
                    _ => Some((
                        true,
                        format!(
                            "settled:repo_missing: repository path no longer exists: {}",
                            intent.repo
                        ),
                    )),
                }
            }
            Err(e) => Some((false, format!("preserved:repo_unreadable:{:?}", e.kind()))),
        };
        if let Some((settled, outcome)) = repo_gate {
            if settled {
                result.settled += 1;
            } else {
                result.preserved += 1;
            }
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome,
            });
            continue;
        }

        if crate::worktree_cleanup::branch_has_other_active_binding(
            home,
            repo_path,
            &intent.branch,
            None,
        )
        .unwrap_or(true)
        {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:active_binding".into(),
            });
            continue;
        }

        // Typed branch-existence check (show-ref exit 0/1/other).
        let full_ref = format!("refs/heads/{}", intent.branch);
        match crate::git_helpers::git_bypass(
            repo_path,
            &["show-ref", "--verify", "--quiet", &full_ref],
        ) {
            Err(e) => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: format!("preserved:git_spawn_error:{e}"),
                });
                continue;
            }
            Ok(o) => match o.status.code() {
                Some(0) => {} // branch exists
                Some(1) => {
                    // branch confirmed absent — remove intent
                    if !dry_run {
                        let _ = std::fs::remove_file(&p);
                    }
                    result.settled += 1;
                    result.candidates.push(ReconcileCandidate {
                        candidate_id: cid,
                        branch: intent.branch.clone(),
                        expected_head: intent.expected_head.clone(),
                        task_id: intent.task_id.clone(),
                        outcome: "settled:branch_absent".into(),
                    });
                    continue;
                }
                other => {
                    result.preserved += 1;
                    result.candidates.push(ReconcileCandidate {
                        candidate_id: cid,
                        branch: intent.branch.clone(),
                        expected_head: intent.expected_head.clone(),
                        task_id: intent.task_id.clone(),
                        outcome: format!(
                            "preserved:git_show_ref_exit:{}",
                            other.map_or("signal".into(), |c| c.to_string())
                        ),
                    });
                    continue;
                }
            },
        }

        // Acquire branch lease lock — shared classifier for both dry-run and apply.
        let _branch_lock =
            match crate::binding::acquire_branch_lease_lock(home, &intent.repo, &intent.branch) {
                Ok(lock) => lock,
                Err(e) => {
                    result.preserved += 1;
                    result.candidates.push(ReconcileCandidate {
                        candidate_id: cid,
                        branch: intent.branch.clone(),
                        expected_head: intent.expected_head.clone(),
                        task_id: intent.task_id.clone(),
                        outcome: format!("preserved:lock_failed:{e}"),
                    });
                    continue;
                }
            };

        // Under lock: re-verify all gates.
        // Re-read intent from disk (concurrent modification).
        let re_content = match std::fs::read_to_string(&p) {
            Ok(c) => c,
            Err(_) => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:intent_gone_under_lock".into(),
                });
                continue;
            }
        };
        let re_intent: CleanupIntent = match serde_json::from_str::<CleanupIntent>(&re_content) {
            Ok(i) if i.branch == intent.branch && i.expected_head == intent.expected_head => i,
            _ => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:intent_changed_under_lock".into(),
                });
                continue;
            }
        };

        // Re-verify task terminal under lock.
        let still_terminal = crate::tasks::load_routed(home, &re_intent.task_id)
            .map(|rt| review_task_terminal(&rt.record().status))
            .unwrap_or(false);
        if !still_terminal {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:task_no_longer_terminal".into(),
            });
            continue;
        }

        // Re-verify no binding under lock.
        if crate::worktree_cleanup::branch_has_other_active_binding(
            home,
            repo_path,
            &intent.branch,
            None,
        )
        .unwrap_or(true)
        {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:binding_appeared_under_lock".into(),
            });
            continue;
        }

        // Re-verify exact head under lock via typed show-ref + rev-parse.
        let tip = match crate::git_helpers::git_cmd(repo_path, &["rev-parse", &intent.branch]) {
            Ok(t) if !t.is_empty() => t.trim().to_string(),
            _ => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:head_unresolvable_under_lock".into(),
                });
                continue;
            }
        };
        if tip != intent.expected_head {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: format!("preserved:head_drift:{tip}"),
            });
            continue;
        }

        // Worktree occupancy gate: refuse if the branch is checked out.
        if is_branch_checked_out(repo_path, &intent.branch) {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:checked_out".into(),
            });
            continue;
        }

        let default = crate::git_helpers::default_branch(repo_path);
        let open_pr_status =
            crate::branch_sweep::open_pr_status(repo_path, &default, &intent.branch);
        if !matches!(open_pr_status, crate::branch_sweep::OpenPrStatus::NotOpen) {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: format!("preserved:open_pr_status:{open_pr_status:?}"),
            });
            continue;
        }

        // All gates passed under lock. Dry-run stops here.
        if dry_run {
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "qualified:dry_run".into(),
            });
            continue;
        }

        // Create collision-safe recovery ref before delete.
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let short_head = if tip.len() >= 8 { &tip[..8] } else { &tip };
        let recovery_ref = format!(
            "refs/agend/recovery/{}/{}-{}",
            intent.branch, short_head, epoch
        );
        // Check for existing recovery at this exact ref (don't overwrite).
        if crate::git_helpers::git_ok(
            repo_path,
            &["show-ref", "--verify", "--quiet", &recovery_ref],
        ) {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:recovery_ref_collision".into(),
            });
            continue;
        }
        if crate::git_helpers::git_cmd(repo_path, &["update-ref", &recovery_ref, &tip]).is_err() {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:recovery_ref_create_failed".into(),
            });
            continue;
        }
        // Verify recovery ref points to exact tip.
        let ref_verified = crate::git_helpers::git_cmd(repo_path, &["rev-parse", &recovery_ref])
            .map(|s| s.trim().to_string())
            .is_ok_and(|s| s == tip);
        if !ref_verified {
            result.preserved += 1;
            result.candidates.push(ReconcileCandidate {
                candidate_id: cid,
                branch: intent.branch.clone(),
                expected_head: intent.expected_head.clone(),
                task_id: intent.task_id.clone(),
                outcome: "preserved:recovery_ref_verify_failed".into(),
            });
            continue;
        }

        // Atomic CAS delete: update-ref -d with expected old SHA.
        let full_ref = format!("refs/heads/{}", intent.branch);
        match crate::git_helpers::git_bypass(
            repo_path,
            &["update-ref", "-d", &full_ref, &intent.expected_head],
        ) {
            Ok(o) if o.status.success() => {
                let _ = std::fs::remove_file(&p);
                result.settled += 1;
                crate::event_log::log(
                    home,
                    "review_branch_reconciled",
                    &intent.task_id,
                    &format!(
                        "candidate_id={} branch={} head={} recovery_ref={}",
                        cid, intent.branch, tip, recovery_ref
                    ),
                );
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: format!("settled:deleted:recovery={recovery_ref}"),
                });
            }
            _ => {
                result.preserved += 1;
                result.candidates.push(ReconcileCandidate {
                    candidate_id: cid,
                    branch: intent.branch.clone(),
                    expected_head: intent.expected_head.clone(),
                    task_id: intent.task_id.clone(),
                    outcome: "preserved:delete_failed".into(),
                });
            }
        }
    }
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
