[繁體中文](FEATURE-teams.zh-TW.md)

# Teams

> **Status:** Current workflow guide
> **Audience:** Operators and agents
> **Authority:** fleet.yaml, team handlers, and live team schema
> **Last verified:** 2026-09-22 at `main@62b28f36`

Teams group agents into named units for structured collaboration. Each team has
members, an optional (but strongly recommended) orchestrator, and is stored as
part of `fleet.yaml` — not as a separate data source.

## Usage Scenarios

> **Target audience:** Both operators and agents.

**Team setup by operator.** An operator defines a new team in `fleet.yaml` or via the `team action=create` MCP tool — for example, creating a "fixup" team with a lead, dev, and reviewer, designating the lead as orchestrator. This structures how tasks are routed and who coordinates the group's work.

**Agent-to-team broadcast.** A lead agent needs to inform every member of its team about a status change. Instead of sending individual messages, it uses `send team=fixup` to broadcast to all members at once. The daemon resolves the team membership and delivers the message to each member's inbox.

**Orchestrator-based task routing.** When a task is assigned to a team name rather than a specific agent, the task board routes it to the team's orchestrator via `resolve_team_orchestrator`. If the orchestrator has been removed and the team is degraded, routing fails — prompting the operator to designate a new orchestrator.

## 1. Design Rationale

- A team is a named group of agents with a designated orchestrator.
- The orchestrator coordinates work within the team.
- Teams enable structured collaboration, division of labor, and targeted broadcasts.
- Team data lives in `fleet.yaml` under the `teams:` key.
- `teams.json` is a legacy bridge path; runtime CRUD reads and writes `fleet.yaml` directly.
- If team state looks wrong, check `fleet.yaml` first.

## 2. Files and Modules

- `src/teams.rs` — primary implementation (projection model).
- `src/fleet/mod.rs` — actual storage layer.
- `src/mcp/handlers/comms.rs` — routes the `team` selector to broadcast and checks team/orchestrator relationships.
- `src/mcp/handlers/dispatch.rs` — owns team CRUD dispatch, not message routing.
- `src/api/handlers/instance.rs` — reads team info for prompt injection.
- `Team` is the projection model; `TeamConfig` is the `fleet.yaml` write type.
- `stale_members` is populated only during list projection.
- `degraded` is a view-layer signal, not a persisted field.

Broadcast to a team with the `team` selector:

```json
{"team":"fixup","message":"Merge freeze starts now","request_kind":"update"}
```

**STOP:** Use `team` for a broadcast; do not treat it as a single recipient.

## 3. Data Model

| Field | Description |
|-------|-------------|
| `name` | Team name |
| `members` | List of member instance names |
| `orchestrator` | Optional; the team's coordinator (should be a member) |
| `description` | Optional description |
| `created_at` | Creation timestamp |
| `source_repo` | The team's source repository path |
| `project_id` | Optional explicit project-board id override |
| `repos` | Optional extra GitHub `owner/repo` slugs (one or a list) the team also owns beyond `source_repo`, e.g. a frontend next to the API checkout. Additive; only makes the team's instances eligible for that repo's pr-state notices (author / merge-ready / rebase). Worktree, dispatch and board identity still come from `source_repo`. Set by editing `fleet.yaml`; team rewrites preserve it. |
| `accept_from` | External agent names allowed to message the orchestrator directly; empty denies cross-team direct sends |
| `stale_members` | View-only; members not found in the live registry |

**Who gets the author notices.** The fleet usually shares one GitHub login, so the PR's `author.login` cannot name the agent. The daemon identifies the PR author from its own records, in order: the typed review assignment's `review_author`, the live binding on the branch, the assignee of a live (not done, cancelled or superseded) task carrying the branch. An assignment counts only for the PR number it was dispatched for, so a reused branch does not inherit the previous PR's author. A bound agent or task assignee must be eligible for the repo. Within a tier exactly one eligible non-reviewer must remain (several → nothing is sent), and an unreadable assignment record also sends nothing. Only when no record exists does `github_login` decide, and only while it maps to exactly one team; a stored login result is re-checked when the fleet changes. CI subscribers never identify the author; reviewers (`role_kind: reviewer`, assignment targets) never do either. A `pr_author` persisted by an older daemon (no provenance) is display-only and never routes a notice. A stored author only stands in for records that are gone, and only for the PR number it was resolved for: a live record from the same or a higher tier, or an unreadable assignment, overrides it, and a new PR on a reused branch is resolved afresh. A reviewer of an earlier PR on the branch is not excluded from authoring a later one, except for a retirement recorded by an older daemon: it has no PR number and excludes that agent on every PR of the branch. An unreadable assignment or reviewer-retirement record sends nothing. When nothing identifies the author the notice is not sent and one WARN per PR is logged.

## 4. `team action=create`

- `name` and `members` are required.
- `orchestrator` is optional but recommended; must be one of the members.
- `repository_path` is optional; omitting it triggers a warning about dispatch binding fallback.
- `project_id` optionally overrides project-board slug derivation without changing `repository_path`.
- `accept_from` sets the cross-team direct-send allowlist; the default empty list is fail-closed.
- Rejects creation if a team with the same name already exists.
- Enforces the **one-agent-one-team** constraint: rejects if any member belongs to another team.
- On success, writes the team to `fleet.yaml` and returns `status=created`.

## 5. `team action=list`

- Returns all teams from `fleet.yaml`.
- Cross-references members against the live agent registry.
- Members not found in the live registry appear in `stale_members` (sorted).
- Adds `degraded=true` if the orchestrator is missing.
- Pure read operation — does not modify teams.

## 6. `team action=update`

- Requires `name`.
- Supports `add` (new members), `remove` (existing members), `orchestrator`, `repository_path`, `project_id`, and `accept_from`.
- Cannot remove the current orchestrator without first designating a new one.
- The new orchestrator must be in the post-update member list.
- `add` enforces one-agent-one-team (cannot add a member who belongs to another team).
- `repository_path` is preserved if not explicitly changed.
- `project_id` and `accept_from` are also preserved unless explicitly supplied.
- `repos` (fleet.yaml-only) is always preserved.
- Writes back to `fleet.yaml` on success.

## 7. `team action=delete`

- Requires `name`.
- Cascades deletion to every member instance via `full_delete_instance`.
- Collects warnings if individual member deletions fail; continues cleaning.
- Removes the team from `fleet.yaml`.
- Returns `members_cleaned` and any `cascade_warnings`.

## 8. Member Removal and Auto-Degradation

- `remove_member_from_all` removes an instance from every team it belongs to.
- If the removed member was the orchestrator and other members remain, the team becomes **degraded** (orchestrator set to None).
- If the removed member was the last member, the team is deleted entirely.
- Degraded teams do not auto-elect a new orchestrator — this requires operator intervention.
- An urgent task is created for each newly degraded team.
- This function is part of instance teardown, not general collaboration.

## 9. Reference Queries

- `find_team_for(home, member)` — returns the team a member belongs to.
- `get_members(home, team_name)` — returns the member list.
- `resolve_team_orchestrator(home, name)` — resolves the orchestrator for routing; a degraded team returns an error.
- `is_orchestrator_of(home, caller, member)` — ACL check.
- These helpers expose the read model; they do not mutate team state.

## 10. Relationship with `send team=...`

- `send` supports a `team` parameter for broadcast delivery.
- `team=fixup` broadcasts to all members of the fixup team.
- Broadcast targets change whenever the member list changes.
- `stale_members` helps identify members who will not receive broadcasts.
- Team broadcast (message delivery) and orchestrator routing (task/ACL routing) are distinct operations that often appear together.

## 11. Relationship with the Task Board

- A task's `assignee` can be a team name.
- When assigned to a team, the task routes to the orchestrator via `resolve_team_orchestrator`.
- Degraded teams cannot route tasks.
- `team delete` may trigger task orphan cleanup and urgent task creation.

## 12. Relationship with Instance Lifecycle

- Deleting an instance calls `remove_member_from_all`.
- If the instance was an orchestrator, its team degrades.
- If the instance was the sole member, its team is deleted.
- `team list` with `stale_members` reveals discrepancies between the member roster and live registry.

## 13. Behavioral Constraints

- Team names should be stable.
- Member lists must not contain duplicates.
- The orchestrator must always be a member.
- `source_repo` should be set; without it, dispatch auto-bind falls back to a weaker path.
- Keep `project_id` aligned with the intended task board when repository-path slug derivation is ambiguous.
- Keep `accept_from` minimal; an empty list intentionally denies cross-team direct sends to the orchestrator.
- `update` should not leave a team in an un-routable state.
- `delete` cascade failures must be visible.
- `stale_members` output must be sorted.
- `degraded` status must be immediately obvious to operators.

## 14. Typical Workflow

1. Create a team with `team action=create` (specify orchestrator from the start).
2. Adjust members with `team action=update`.
3. To change the orchestrator, ensure the new one is still in the member list.
4. To disband a team, use `team action=delete`.
5. To check health, use `team action=list` and inspect `stale_members` / `degraded`.
6. To broadcast messages, use `send team=...`.
7. To find who coordinates whom, use `find_team_for`.

## 15. Implementation Checklist

- `fleet.yaml` is the sole write target.
- `list` is a projection — it must not become a write path.
- Missing `source_repo` should be surfaced as a warning.
- `degraded` must not be persisted.
- `stale_members` must not be written to `fleet.yaml`.
- `delete` cascade must preserve error aggregation.
- Orchestrator updates must validate against post-mutation members.
- One-agent-one-team is a critical invariant.
- Broadcast and routing must not be conflated.

## 16. Summary

Teams are the basic unit of collaborative grouping. Their source of truth is `fleet.yaml`. CRUD operations are `team create/delete/list/update`; broadcast is via `send team=...`. The orchestrator is the team's coordination point. A degraded team is not broken data — it is a state requiring operator attention. `stale_members` is an observability field. When team behavior is unexpected, check fleet first, then the live registry.
