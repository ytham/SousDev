# Gotchas and Pitfalls

Hard-won lessons from the initial build session. Read before making changes.

---

## UTF-8 String Safety

**NEVER** use `&s[..N]` to truncate strings. Multi-byte characters (emojis,
CJK, etc.) will panic if you slice at a byte boundary inside a character.

Always use `crate::utils::truncate::safe_truncate(s, max)`.

The crash that taught this lesson: `🔴` emoji in an agent's review output
caused a panic at `log_view.rs:244` when truncating for display.

---

## GitHub API ID Namespaces

GitHub uses **different ID sequences** for different comment types:

| API endpoint | ID range |
|---|---|
| `/issues/{N}/comments` (timeline) | ~4.1 billion |
| `/pulls/{N}/comments` (inline review) | ~2-3 billion |
| `/pulls/{N}/reviews` (review bodies) | ~4.0 billion |

**NEVER compare IDs across different APIs.** PR review body IDs (from
`/pulls/{N}/reviews`) cannot be filtered using a timeline comment cursor.
The `pr-responder` filters review bodies by **timestamp** (`responded_at`)
instead of ID.

---

## sed Commands Are Dangerous

Several bugs in this session were caused by overly broad `sed` commands that
matched unintended lines. Lessons:

- `sed` matching `status,` will match EVERY line ending in `status,` — not
  just the one in the struct you're targeting
- Always use `cargo build` immediately after `sed` to catch damage
- Prefer targeted `edit` tool operations over bulk `sed` for Rust code
- When `sed` adds fields to test structs, it may add them to the wrong struct
  type (e.g., adding `additions` to `PrResponseRecord` instead of only
  `PrReviewRecord`)

---

## Claude CLI Ignores Prompt Instructions

The Claude CLI agent frequently ignores prompt-level restrictions like
"do not run tests" or "do not post the review yourself." Technical
enforcement is required:

- The `PrReviewPosterStage` checks if the agent already posted a review
  (via `check_agent_already_posted`) before posting its own
- PR reviews are posted as timeline comments (`gh pr comment`) not formal
  reviews (`gh pr review`)
- The PR review prompt has `IMPORTANT CONSTRAINTS` at the very top (before
  the PR details) because Claude gives more weight to early instructions

---

## Rebase vs Real Code Changes

When a PR is rebased, the HEAD SHA changes even though the actual code diff
is identical. The `pr-reviewer` distinguishes rebases from real changes by
comparing `additions` and `deletions` counts (stored in `PrReviewRecord`).
Same counts + different SHA = rebase → skip.

---

## InProgress Status Preservation

The `refresh_info_only` function runs every cron tick (even when an agent is
busy) and emits a fresh `ItemsSummary` that replaces the entire item list.
Without special handling, this would overwrite `InProgress` status back to
`None`/`NoNewComments`.

The fix: the `ItemsSummary` handler preserves `InProgress` status from the
old list when replacing with new data.

---

## Shallow Clone Issues

PR review/response workspaces are created with `git clone --depth=50`. This
causes several problems:

- `gh pr checkout` fails with "cannot set up tracking information" because
  the remote branch isn't in the restricted refspec
- Fix: use `git checkout -B <branch> FETCH_HEAD` after explicit fetch
- For reused workspaces: reconfigure fetch refspec to `+refs/heads/*:refs/remotes/origin/*`
- Hard-reset (`git reset --hard` + `git clean -fd`) before checkout to clear
  leftover changes from previous runs

---

## Workspace Teardown Timing

Bug-fix workspaces must ONLY be torn down after successful PR creation.
A failed run should preserve the workspace for debugging. The teardown
call is inside the `Ok(_)` branch of the result match, not unconditionally
after the stage pipeline.

---

## Smart Timeout — Commit Detection

The agent streaming loop tracks whether `git commit` or `git add` appeared
in the streamed stdout. When detected, the timeout grace period drops from
the full duration to just 60 seconds. This prevents waiting 15 minutes for
an agent that finished its work but is stuck running tests.

After any timeout, the workspace is checked for changes (`git status --porcelain`
+ `git log origin/HEAD..HEAD`). If changes exist, the run is treated as success.

---

## Three Types of GitHub PR Comments

The `pr-responder` must check THREE separate APIs:

1. `/issues/{N}/comments` — timeline comments (top-level)
2. `/pulls/{N}/comments` — inline review comments (diff-level)
3. `/pulls/{N}/reviews` — PR review bodies (from "Submit review" button)

Missing any of these means comments go unaddressed.

---

## Label OR Logic

Multiple labels in `github_issues` config use OR logic: `labels = ["bug", "SubTA/FaaS"]`
means "issues with bug OR SubTA/FaaS." This is implemented as separate
`gh issue list` calls per label, merged and deduplicated.

---

## PR Reviewer Filtering

The PR reviewer uses a three-search strategy to find PRs, then a post-fetch
filter:

1. `user-review-requested:@me` (strict individual requests)
2. `assignee:@me` (assigned PRs)
3. `review-requested:@me` (broad, includes team requests)

Post-fetch filter: include if individually requested OR assigned. This prevents
the `eng` team auto-assignment from flooding the reviewer with every PR.

The `refresh_info_from_remote` startup function MUST apply the same filter
or the Info pane will show all team-requested PRs.

---

## Background Startup Refresh

`refresh_info_from_remote` was the startup bottleneck (5-9 seconds of GitHub
API calls). It now runs in a `tokio::spawn` background task, sending results
via `TuiEvent::ItemsSummary`. The TUI renders instantly with stale data from
local stores.

The function takes **owned** types (`HarnessConfig`, `PathBuf`, `TuiEventSender`)
because it runs in a spawned task — it cannot borrow from `App`.

---

## Test Gotchas

- `open_url_in_browser()` is guarded by `#[cfg(not(test))]` — tests that
  trigger `Enter` in the info panel won't actually open browser tabs
- `parse_claude_stream_trajectory()` is `#[cfg(test)]` only — production
  code uses `stream_parse_claude_line()` for real-time parsing
- Test `ItemSummary` and `GitHubPR` constructions need ALL fields including
  `comment_count`, `additions`, `deletions`, `requested_teams`, `assignees`
- The `FailureCooldownStore` tests verify behavior with missing/empty/corrupt
  files — all stores must handle these gracefully
