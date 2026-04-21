# 🧑‍🍳 SousDev — Project Context

This document describes the current state of SousDev for agent context.
Last updated after focus directives, review scoring, plan revision, REST API fallback, and inline comments via reqwest.

---

## What SousDev is

A Rust CLI with a built-in TUI dashboard that runs autonomous agentic workflows
on a cron schedule. It watches GitHub (and optionally Linear) repos for activity
— issues, pending PR reviews, reviewer comments — and handles them with AI agents
that edit code, run tests, open PRs, and post review comments.

**Binary:** `sousdev`
**Config:** `config.toml` (auto-discovered by walking up from CWD). Uses `[[models]]` array (first entry is primary; additional entries enable multi-model review). Legacy `provider`/`model` fields replaced.
**Env vars:** `.env` file loaded automatically via `dotenvy`
**Session state:** `.session.toml` (gitignored, persists enabled/disabled workflows)
**Branding:** 🧑‍🍳 SousDev — "Prep, review, and plate your PRs automatically."

---

## Runtime dependencies

| Binary | Required by | Auth method |
|---|---|---|
| `git` | all workflows | — |
| `gh` (GitHub CLI) | all workflows | `gh auth login` (OAuth) |
| `claude` | `claude-loop` technique | Claude CLI OAuth (no API key needed) |
| `codex` | `codex-loop` technique | `OPENAI_API_KEY` env var |
| `gemini` | `gemini-loop` technique | `GEMINI_API_KEY` env var |

The default `claude-loop` workflow requires NO API keys — only `gh auth login` and `claude` CLI auth.

When `ANTHROPIC_API_KEY` or `OPENAI_API_KEY` is set, the corresponding provider enables **API-based PR review** (native tool-use) and **multi-model review** (parallel reviews from 2+ models).

---

## Project layout

```
sousdev/
├── Cargo.toml
├── config.toml                      reference config
├── .env.example                     environment variable template
├── .session.toml                    session state (gitignored)
├── prompts/                         editable .md templates with {{variable}} placeholders
│   ├── system.md                    system prompt (injected into every agent call)
│   ├── bug-fix.md                   TDD agent task
│   ├── code-review.md               reviewer prompt
│   ├── review-feedback.md           critique fed back to agent on rejection
│   ├── pr-description.md            PR title + body generation
│   ├── pr-review.md                 PR review (CRITICAL CONSTRAINTS at top, explicit verdict requirement)
│   ├── pr-comment-response.md       PR comment response ("look around" for sibling code)
│   ├── reflect.md                   reflexion prompt for retry reasoning
│   ├── plan-generation.md           Plan-first: generate implementation plan from issue
│   ├── plan-pr-body.md              Plan-first: PR body for plan review
│   ├── plan-execution.md            Plan-first: execute approved plan
│   ├── review-consolidation.md      Multi-model: consolidate parallel reviews
│   └── ...
├── output/                          state files + logs (gitignored)
│   ├── runs.json
│   ├── handled-issues.json
│   ├── reviewed-prs.json
│   ├── pr-responses.json
│   ├── failure-cooldowns.json       failure tracking with exponential backoff
│   └── logs/<workflow>/<label>.json  per-run logs (e.g. issue-10649.json)
└── src/
    ├── main.rs                      CLI entry point (clap)
    ├── lib.rs                       library root
    ├── types/
    │   ├── config.rs                HarnessConfig, WorkflowConfig, ModelConfig, all sub-configs
    │   └── technique.rs             RunResult, TrajectoryStep, StepType (PartialEq, Eq)
    ├── utils/
    │   ├── logger.rs                Logger (wraps tracing with prefix)
    │   ├── prompt_loader.rs         PromptLoader — file or inline, {{var}} substitution
    │   ├── config_loader.rs         Walk-up config.toml discovery
    │   └── truncate.rs              safe_truncate() — UTF-8 safe string truncation
    ├── providers/
    │   ├── provider.rs              LLMProvider trait, NoopProvider, Message, CompletionResult,
    │   │                            ContentBlock, ToolDefinition, ToolChoice, StopReason, TokenUsage
    │   ├── anthropic.rs             Anthropic Messages API (full tool-use support)
    │   ├── openai.rs                OpenAI Chat Completions API (full tool-use / function calling)
    │   └── ollama.rs                Ollama local API
    ├── tools/
    │   ├── registry.rs              ToolRegistry, Tool, ToolExecutor trait
    │   └── built_ins.rs             readFile, writeFile, shell
    ├── tui/
    │   ├── mod.rs                   TUI entry point
    │   ├── app.rs                   App state, context-based key routing, event handling
    │   ├── events.rs                TuiEvent channel types, ItemSummary, ItemStatus
    │   ├── session.rs               .session.toml persistence
    │   ├── ui.rs                    Layout + theme constants + toast rendering
    │   └── widgets/
    │       ├── sidebar.rs           Workflow list + stage flowcharts
    │       ├── info.rs              Compact item status pane (between sidebar and logs)
    │       ├── info_expanded.rs     Floating expanded item detail panel
    │       ├── log_view.rs          Pretty log mode + flat mode + status bar
    │       └── command_menu.rs      : menu + cron edit overlay
    ├── workflows/
    │   ├── workflow.rs              ParsedTask, make_skipped_result
    │   ├── stage.rs                 Stage trait, StageContext, ResolvedPrompts
    │   ├── executor.rs              WorkflowExecutor — 5 modes + refresh_info_only
    │   ├── cron_runner.rs           tokio-cron-scheduler + live rescheduling + info refresh
    │   ├── workspace.rs             WorkspaceManager (clone, checkout, reset, teardown)
    │   ├── github_issues.rs         fetch issues (OR-logic labels), comment, close
    │   ├── github_prs.rs            fetch PRs (4 searches), reviews, inline comments via reqwest
    │   ├── linear_issues.rs         Linear GraphQL API issue fetching
    │   ├── stores.rs                RunStore + HandledIssueStore + PrReviewStore +
    │   │                            PrResponseStore + FailureCooldownStore.
    │   │                            HandledIssueRecord gains `state`, `branch`, and `last_plan_comment_id` fields (plan_state module).
    │   ├── multi_review.rs          Multi-model PR review: ReviewerModel, detection, consolidation, scoring (0-100), focus directives
    │   ├── workflow_log.rs          Per-run structured log files
    │   └── stages/
    │       ├── trigger.rs           Shell command → stdout
    │       ├── parse.rs             stdout → ParsedTask | SkipWorkflowSignal
    │       ├── agent_loop.rs        Retry loop with reflexion-style reflection
    │       ├── external_agent_loop.rs  Spawn agent CLIs, real-time stream parser
    │       ├── reviewer.rs          Claude review + LLM-judge strategies
    │       ├── review_feedback_loop.rs  Reviewer → agent feedback cycle
    │       ├── pr_description.rs    Claude CLI generates title + body from diff
    │       ├── pull_request.rs      Commit, rebase CI, push, create PR + agent fallback
    │       ├── pr_checkout.rs       Set ctx.branch from PR
    │       ├── api_review_loop.rs   API-based review agent loop with read-only tools
    │       ├── pr_review_poster.rs  Posts timeline comment (NOT formal review) + dedup check
    │       └── pr_comment_responder.rs  Reply + summary + update PR description
    └── techniques/                  8 standalone reasoning algorithms
```

---

## TUI Dashboard

Three-column layout: `Sidebar (26) | Info pane (34) | Log pane (remaining)`

### Rendering performance

- **`needs_redraw` flag**: Render only when state changes (idle CPU ~10% → near-zero)
- **Adaptive poll timeout**: 500ms when idle, 100ms when active
- **Mouse scroll batching**: Drain ALL pending terminal events before rendering (0ms poll)
- **Terminal resize handling**: `Event::Resize` triggers immediate redraw

### Key routing (context stack, highest priority first)

```
CronEdit > Command menu > Info Expanded > Info pane > Normal (Sidebar)
```

Each context owns its keys exclusively. Universal: `Ctrl+C` quits.
`Esc` pops the topmost context. `:` opens command menu from any context.

### Panels

| Panel | Purpose |
|---|---|
| **Sidebar** | Workflow list + stage flowcharts. Blue thick border when active. |
| **Info pane** | Compact item status (34 chars wide). Workflow name header, selected item detail footer (status + issue/PR name). Clicking an item sets the log filter. Active/inactive title dimming. |
| **Log pane** | Real-time agent output in pretty mode. Click expands entries. |
| **Info Expanded** | Floating left-side panel, full height, 10px left margin. |
| **Command menu** | Floating bottom bar (`BG_MENU`), flush to bottom, shows version. |
| **Status bar** | Bottom 2 rows: workflow info + filter label + item title. |
| **Toast** | Full-width bar, no y-padding, centered message in lighter green, dark green background. Auto-expires. |

### Pretty log mode

- **Thinking**: Subtle background (`BG_THOUGHT`), thick cyan border (`▎`), collapsed to 1 line by default. Click expands. Newlines flattened to `  ` when collapsed, split to actual lines when expanded.
- **Tool calls**: Purple `[tool]` prefix, result hidden. Click expands.
- **Consolidated tools**: 3+ consecutive → last call + `[+] N more`. Click expands.
- **Color-coded log lines**: 10-color palette per item. Colored borders in pretty mode, colored dots in flat mode.
- **Expandable error entries**: Click to see full multi-line API error details.
- **Click vs drag**: < 3px movement = click (toggle expand), >= 3px = drag (copy).
- **Copy**: Always includes full expanded content of selected entries.

### Item statuses

| Badge | Status | Used by |
|---|---|---|
| `[  ]` | None / unchecked | All |
| `[>>]` | InProgress | All (preserved across refreshes) |
| `[PR]` | Success (PR opened) | issue-autofix |
| `[!!]` | Error / Cooldown | All |
| `[A✓]` | Agent approved, needs manual approval | pr-reviewer |
| `[A✗]` | Agent found concerns, needs review | pr-reviewer |
| `[✓✓]` | PR has GitHub reviewDecision == APPROVED | pr-reviewer |
| `[Pl]` | PlanPending — plan PR waiting for human review | plan-first issues |
| `[--]` | No new comments (0 count) | pr-responder |
| `[ N]` | Comment count (gray=no new, cyan=new) | pr-responder |
| `[**]` | New comments (no count available) | pr-responder |

---

## The five workflow modes

### Mode 1: Issue autofix (`github_issues` or `linear_issues`)

- Labels use **OR logic** (separate query per label, merged + deduped)
- Default limit: 100 (effectively unlimited)
- `Closes <issue_url>` prepended to PR body
- 🧑‍🍳 branding appended (configurable via `show_branding`)
- Branch name: `{branch_prefix}{issue_number}` (no redundant "issue-" in template)
- Workspaces torn down only on success; preserved on failure
- Smart timeout: if agent committed, 60s grace period; if changes detected after timeout, treat as success
- Reflexion-style reflection between retries
- Agent-assisted PR creation fallback when automated flow fails

### Mode 1b: Plan-first autofix (`github_issues` + `plan_first = true`)

- Default mode (`plan_first = true` by default)
- State machine: `plan_posted` → `plan_approved` → `code_complete`
- Agent creates a plan PR for human review before writing code
- 60-second background polling for approval
- **Plan revision on feedback**: when reviewer leaves non-approval comments on plan PR, agent re-runs to revise the plan (not just append). `last_plan_comment_id` tracks processed comments to prevent re-processing.
- `HandledIssueRecord` tracks `state`, `branch`, and `last_plan_comment_id` fields
- TUI shows `[Pl]` PlanPending badge while awaiting approval
- New prompts: `plan-generation.md`, `plan-pr-body.md`, `plan-execution.md`

### Mode 2: PR reviewer (`github_prs`)

- **Four GitHub searches** merged: `user-review-requested:@me`, `assignee:@me`, `review-requested:@me`, `reviewed-by:@me`
- Post-fetch filter: individually requested OR assigned to user OR has review record
- Re-review trigger: any new human comment OR `@sousdev focus:` OR `@sousdev review` OR `@<user> review` (even from self for focus directives)
- `max_retries` defaults to 0 for PR review (no retries)
- Reviews posted as **timeline comments only** (NOT formal GitHub reviews — no approval/rejection)
- **"Review in progress" placeholder**: posted when review starts (lists models + focus areas), deleted when full review is posted
- Duplicate detection: checks if agent already posted via `gh pr review` (10-min window)
- Rebase detection: SHA changed but additions/deletions same → skip (not a real code change)
- PR review prompt has **IMPORTANT CONSTRAINTS** at top: no build/test/install, no `gh pr review`
- `has_concerns` tracked in `PrReviewRecord` for `[A✓]` vs `[A✗]` status
- **Focus directives**: `@sousdev focus: <text>` in PR comments and `## Review focus` in PR descriptions inject focus areas into review prompts. Displayed in consolidated review under "### Focus directives" with attribution.
- **Review scoring**: Each model scores PR 0-100. Summary table includes Score column. `Avg Score:` line. Parsed via `parse_score()`.
- **Verdict calibration**: Only reject for real harm (bugs, security, data loss). Approve with comments for everything else.
- **Multi-model review**: When 2+ model CLIs/API keys available (from `[[models]]` config), runs parallel reviews and consolidates. Auto-detects available models.
- **API-based review**: Uses native tool-use APIs (Anthropic, OpenAI) instead of CLI binaries. Falls back to CLI when no API key. Read-only tool set with allowlisted shell commands (`api_review_loop.rs`).
- **Review verdicts**: Each model outputs `Verdict: ✅ Approved` or `Verdict: 🔴 Not Approved`. Consolidated reviews include per-model verdict table. `PrReviewResult` includes `score` and `verdict` fields.
- **REST API fallback**: When GraphQL rate limit exceeded, falls back to REST API `/repos/{repo}/pulls` with separate rate limit.
- **Inline comments via reqwest**: Posted directly via GitHub API (not `gh` CLI). API version `2022-11-28` with `line` + `side` parameters. Only failed inline observations shown in timeline; successfully-posted ones removed. If all posted, inline section removed entirely.
- **Inline comment path matching**: Short filenames from models matched against full paths via suffix matching.
- **Markdown inline comment parser**: Fallback when models don't use structured `INLINE_COMMENT` markers. Parses `**path:line**`, `- \`path:line\``, etc.
- **Verdict parsing**: `strip_leading_emojis()` handles emoji/branding prefixes like `🧑‍🍳 Verdict:` and `📊 Avg Score:`.
- **`--disallowedTools`** for Claude CLI is the primary enforcement layer (deterministic). Blocks `gh pr review`, `gh pr comment`, etc.
- **`--permission-mode auto`** for PR review (instead of `--dangerously-skip-permissions`) — read-only review doesn't need bypass.
- `github_prs` mode accepts `claude-loop`, `codex-loop`, or `gemini-loop` techniques.
- `[✓✓]` Approved badge shows when PR has GitHub reviewDecision == APPROVED

### Mode 3: PR comment responder (`github_pr_responses`)

- Fetches **three comment types**: timeline, inline review, PR review bodies
- PR review bodies filtered by **timestamp** (not ID — different numbering sequences)
- Bot comments filtered (`[bot]` suffix, `github-actions`)
- Author's own comments ARE included (can direct the agent)
- Updates PR description if significant changes (2+ files)
- Comment count shown in Info pane badge

### Mode 4: Shell trigger

Standard pipeline: Trigger → Parse → Agent → Review → PR Description → Pull Request

---

## Agent execution

### Claude CLI streaming

Real-time line-by-line parsing via `BufReader::lines()`. Each stream-json line
parsed by `stream_parse_claude_line()` and emitted to TUI immediately. Trajectory
built incrementally during streaming.

### Smart timeout with commit detection

1. If `git commit` detected in streamed output → grace period drops to 60s
2. After timeout → check workspace for changes
3. Changes found → treat as success, continue pipeline
4. No changes → error

### Reflexion-style retries

Between failures, `generate_reflection()` calls Claude CLI with error context.
Reflection replaces raw error dump in retry prompt. Default retries: 0 for PR
review, 1 for all others.

---

## Cron runner

- Per-workflow overlap guard
- **`refresh_info_only()`** runs BEFORE overlap guard every tick → Info pane stays current even when agent is busy
- `InProgress` status preserved across `ItemsSummary` refreshes
- Live rescheduling via `mpsc` channel → schedule changes take effect immediately
- Background startup refresh → TUI renders instantly, GitHub data arrives async

---

## State and stores

| Store | File | Key fields |
|---|---|---|
| `RunStore` | `output/runs.json` | Append-only history |
| `HandledIssueStore` | `output/handled-issues.json` | issue_number → record (state, branch, last_plan_comment_id) |
| `PrReviewStore` | `output/reviewed-prs.json` | pr_number → record (has_concerns, additions, deletions, head_sha) |
| `PrResponseStore` | `output/pr-responses.json` | pr_number → record (responded_at for timestamp filtering) |
| `FailureCooldownStore` | `output/failure-cooldowns.json` | Exponential backoff: 60min → 24h cap |

All stores handle missing/empty/corrupt files gracefully. Directory recreated on write if deleted.

---

## Theme (all colors in `src/tui/ui.rs`)

```
BG_LOGS            Rgb(16, 16, 22)     Log pane (darkest)
BG_INFO            Rgb(20, 20, 28)     Info pane
BG_SIDEBAR         Rgb(24, 24, 32)     Sidebar
BG_INFO_EXPANDED   Rgb(28, 28, 38)     Floating panels + command menu
BG_STATUS_BAR      Rgb(30, 30, 42)     Status bar
BG_THOUGHT         Rgb(24, 26, 34)     Thinking block background
BG_MENU            Rgb(35, 35, 45)     Command menu background
BG_ROW_FOCUS       Rgb(40, 40, 54)     Keyboard cursor highlight
BG_TEXT_SELECTION   Rgb(50, 50, 70)     Mouse drag selection
ACCENT_BORDER      Rgb(80, 110, 200)   Active panel border (thick ▎)
ACCENT_THOUGHT     Rgb(80, 160, 200)   Thinking block left border
ACCENT_TOOL        Rgb(140, 120, 200)  [tool] tag
ACCENT_INFO_LEVEL  Rgb(70, 110, 190)   Info log level
ACCENT_TOAST       Rgb(40, 130, 70)    Toast notifications (dark green background, lighter green text)
```

---

## Security notes

- All `gh api` calls use direct `Command::new("gh").arg(...)` — no shell injection
- Inline comments posted via `reqwest` with GitHub token from `gh auth token` — API version `2022-11-28`
- `safe_truncate()` in `src/utils/truncate.rs` prevents UTF-8 boundary panics
- `--dangerously-skip-permissions` grants Claude full system access (by design, for code-writing modes)
- `--permission-mode auto` used for PR review (read-only, no bypass needed)
- `--disallowedTools` for Claude CLI is the primary enforcement layer (deterministic) — blocks `gh pr review`, `gh pr comment`, etc.
- `blocked_commands` is prompt-level only — not technically enforced
- Issue bodies are a prompt injection vector
- PR reviews are posted as timeline comments, never formal GitHub reviews

---

## Key numbers

| Metric | Value |
|---|---|
| Test count | 642+ |
| Clippy warnings | 0 |
| Default agent timeout | 900s (15 min) |
| Default PR review timeout | 600s (10 min) |
| Default issue/PR limit | 100 (effectively unlimited) |
| Failure cooldown | 60min → 120min → 240min → ... cap 24h |
| Log files loaded on startup | Up to 5 per workflow |
| Prompts | 10+ template files |
| Stages | 12 (including api_review_loop) |
| Info panel width | 34 chars |
| Log cap | 10K entries/workflow |
| RunStore max | 500 entries (pruned) |
| Startup time | ~50-100ms (GitHub data fetched in background) |

---

## Key invariants

1. `cargo test` — 642+ tests, zero failures
2. `cargo clippy` — zero warnings
3. Every Stage returns `Ok(())` for business failures; only `Err` for unrecoverable errors
4. `HandledIssueStore.mark_handled()` only when `success && pr_url.is_some()`
5. PR workspaces (`-pr<N>`) are never torn down; issue workspaces torn down only on success
6. The reviewer approval token is exactly `HARNESS_REVIEW_APPROVED`
7. The stream-json parser never crashes on malformed input
8. All string truncation uses `safe_truncate()` (UTF-8 safe — prevents emoji panics)
9. `config.toml` must always be valid
10. `reviewer_login` is detected once per executor instance (lazy-cached)
11. Do not use `process::exit()` outside `main.rs`
12. Do not hardcode prompts — use `prompts/*.md`
13. All config fields must be `Option<T>` with documented defaults
14. Info pane shows ALL open items matching filters; items disappear only when closed
15. PR review comments (from `/pulls/{pr}/reviews` API) filtered by timestamp, not ID
16. `InProgress` status is preserved when `ItemsSummary` refreshes the item list
17. Reviews are NEVER posted as formal GitHub reviews — only as timeline comments
18. Multiple labels in issue config use OR logic (separate query per label)
19. `open_url_in_browser()` is a no-op during `#[cfg(test)]`
20. Background `refresh_info_from_remote` uses owned types (runs in `tokio::spawn`)
21. Initial PR list matches cron tick filter (individually requested, assigned, or has review record)
