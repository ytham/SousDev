# 🧑‍🍳 SousDev — Project Context

This document describes the current state of SousDev for agent context.

---

## What SousDev is

A Rust CLI with a built-in TUI dashboard that runs autonomous agentic workflows
on a cron schedule. It watches GitHub (and optionally Linear) repos for activity
— issues, pending PR reviews, reviewer comments — and handles them with AI agents
that edit code, run tests, open PRs, and post review comments.

**Binary:** `sousdev`
**Config:** `config.toml` (auto-discovered by walking up from CWD)
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

`ANTHROPIC_API_KEY` is only needed for harness-native techniques (react, reflexion, etc.).

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
│   ├── pr-review.md                 PR review (INLINE_COMMENT format)
│   ├── pr-comment-response.md       PR comment response
│   ├── reflect.md                   reflexion prompt for retry reasoning
│   └── ...
├── output/                          state files + logs (gitignored)
│   ├── runs.json                    run history
│   ├── handled-issues.json          handled issue records
│   ├── reviewed-prs.json            reviewed PR records
│   ├── pr-responses.json            PR response records
│   ├── failure-cooldowns.json       failure tracking with exponential backoff
│   └── logs/<workflow>/<label>.json  per-run structured logs (e.g. issue-10649.json)
└── src/
    ├── main.rs                      CLI entry point (clap)
    ├── lib.rs                       library root
    ├── types/
    │   ├── config.rs                HarnessConfig, WorkflowConfig, all sub-configs
    │   └── technique.rs             RunResult, TrajectoryStep, StepType
    ├── utils/
    │   ├── logger.rs                Logger (wraps tracing with prefix)
    │   ├── prompt_loader.rs         PromptLoader — file or inline, {{var}} substitution
    │   ├── config_loader.rs         Walk-up config.toml discovery
    │   └── truncate.rs              safe_truncate() — UTF-8 safe string truncation
    ├── providers/
    │   ├── provider.rs              LLMProvider trait, NoopProvider, Message, CompletionResult
    │   ├── anthropic.rs             Anthropic Messages API
    │   ├── openai.rs                OpenAI Chat Completions API
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
    │   ├── executor.rs              WorkflowExecutor — all 4 modes + refresh_info_only
    │   ├── cron_runner.rs           tokio-cron-scheduler + live rescheduling
    │   ├── workspace.rs             WorkspaceManager (clone, checkout, reset, teardown)
    │   ├── github_issues.rs         fetch issues (OR-logic labels), comment, close
    │   ├── github_prs.rs            fetch PRs (3 searches), reviews, inline comments
    │   ├── linear_issues.rs         Linear GraphQL API issue fetching
    │   ├── stores.rs                RunStore + HandledIssueStore + PrReviewStore + PrResponseStore + FailureCooldownStore
    │   ├── workflow_log.rs          Per-run structured log files
    │   └── stages/
    │       ├── trigger.rs           Shell command → stdout
    │       ├── parse.rs             stdout → ParsedTask | SkipWorkflowSignal
    │       ├── agent_loop.rs        Retry loop with reflexion-style reflection
    │       ├── external_agent_loop.rs  Spawn agent CLIs, real-time stream parser
    │       ├── reviewer.rs          Claude review + LLM-judge strategies
    │       ├── review_feedback_loop.rs  Reviewer → agent feedback cycle
    │       ├── pr_description.rs    Claude CLI generates title + body from diff
    │       ├── pull_request.rs      Commit, rebase CI changes, push, create PR
    │       ├── pr_checkout.rs       Set ctx.branch from PR
    │       ├── pr_review_poster.rs  Parse INLINE_COMMENT blocks → post to GitHub
    │       └── pr_comment_responder.rs  Reply to threads + summary + update PR description
    └── techniques/                  8 standalone reasoning algorithms
        ├── react/                   Think → Act → Observe
        ├── reflexion/               Self-reflection + episodic memory
        ├── tree_of_thoughts/        BFS/DFS scored reasoning tree
        ├── self_consistency/        N-sample majority vote
        ├── critique_loop/           Generate → Critique → Revise
        ├── plan_and_solve/          Plan first, then execute
        ├── skeleton_of_thought/     Outline → parallel expansion
        └── multi_agent_debate/      N agents debate, judge synthesises
```

---

## TUI Dashboard

Launched with bare `sousdev` (no subcommand). Three-column layout:

```
Sidebar (26) | Info pane (24) | Log pane (remaining)
```

### Panels

| Panel | Purpose | Key bindings |
|---|---|---|
| **Sidebar** | Workflow list + stage flowcharts | ↑↓ select, ←→ switch pane, i info expanded |
| **Info pane** | Compact item status per workflow | ↑↓ select, ⏎ show logs, g open URL, c clear, C clear all |
| **Log pane** | Real-time agent output (pretty mode) | f/b page, F/B begin/end, : menu |
| **Info Expanded** | Floating detail panel (full height, left side) | Same as Info pane + ESC close |
| **Command menu** | : triggered floating menu | ESC q e c p |
| **Status bar** | Bottom bar with workflow info + filter label | — |

### Pretty log mode (default)

- **Thinking blocks**: Subtle background + cyan left border, collapsed to 1 line, click to expand
- **Tool calls**: Purple `[tool]` prefix, result hidden, click to expand
- **Consolidated tools**: 3+ consecutive calls → last call + `[+] N more`, click to expand
- **System messages**: Stage transitions, executor messages
- **Click vs drag**: Click (< 3px movement) toggles expand; drag copies to clipboard

### Context-based key routing

```
Priority: CronEdit > Command > InfoExpanded > Info pane > Normal (Sidebar)
```

Each context handles its own keys exclusively. Universal: Ctrl+C quits.

### Session persistence

`.session.toml` stores enabled/disabled workflows. `config.toml` is updated when
cron schedules change via `:c`.

---

## The four workflow modes

### Mode 1: Issue autofix (`github_issues` or `linear_issues`)

```
Fetch issues (OR-logic labels, per-assignee queries)
  → skip handled (HandledIssueStore) + failure cooldown
  → for each unhandled:
      clone repo → create branch
      → AgentLoopStage (Claude fixes, reflexion-style reflection between retries)
      → ReviewFeedbackLoopStage (self-review)
      → PrDescriptionStage (Claude CLI generates title + body)
      → PullRequestStage (commit, rebase CI fixes, push, gh pr create --head --base)
      → "Closes <issue_url>" prepended to PR body
      → 🧑‍🍳 branding appended (configurable)
```

### Mode 2: PR reviewer (`github_prs`)

```
Fetch PRs (3 searches: user-review-requested, assignee, review-requested)
  → post-fetch filter: individually requested OR assigned
  → skip already-reviewed (unless new commits or @mention)
  → for each:
      checkout PR branch (FETCH_HEAD strategy with fallbacks)
      → AgentLoopStage (max_retries defaults to 0 for reviews)
      → PrReviewPosterStage (parse markers or fallback to trajectory)
      → duplicate check: skip if agent already posted via gh pr review
```

### Mode 3: PR comment responder (`github_pr_responses`)

```
Fetch authored PRs (author:@me)
  → fetch inline comments + timeline comments + PR review bodies
  → filter out bots ([bot] suffix)
  → review bodies filtered by timestamp (not ID — different numbering sequences)
  → for each PR with new comments:
      → AgentLoopStage (address comments)
      → ReviewFeedbackLoopStage
      → PullRequestStage (push to existing branch)
      → PrCommentResponderStage (reply + summary)
      → update PR description if significant changes (2+ files)
```

### Mode 4: Shell trigger

```
TriggerStage → ParseStage → AgentLoopStage → ReviewFeedbackLoopStage → PrDescriptionStage → PullRequestStage
```

---

## Agent execution

### Claude CLI streaming

`run_external_agent_loop` streams stdout line-by-line via `BufReader::lines()`.
Each line is parsed in real-time by `stream_parse_claude_line()` and emitted to
the TUI via `WorkflowLog`. Trajectory is built incrementally.

### Smart timeout

When the agent times out:
1. If a `git commit` was detected in streamed output, grace period drops to 60s
2. After timeout, check workspace for uncommitted changes or new commits
3. If changes found → treat as success, continue pipeline
4. If no changes → return error

### Reflexion-style retries

Between failed attempts, `generate_reflection()` calls Claude CLI with:
- The failed attempt's output (truncated)
- The error message
- `git diff --stat`

The reflection text replaces the raw error dump in the retry prompt.

Default retries: 0 for PR review, 1 for all others.

---

## Issue sources

### GitHub Issues
```toml
[workflows.github_issues]
assignees = ["@me"]
labels = ["bug", "SubTA/FaaS"]  # OR logic — matches any label
```

Multiple labels use OR logic (separate `gh issue list` per label, merged + deduped).

### Linear Issues
```toml
[workflows.linear_issues]
team = "ENG"
states = ["Todo"]
```

Requires `LINEAR_API_KEY`. Uses the top-level `target_repo` for the git repo.

---

## State and stores

| Store | File | Purpose |
|---|---|---|
| `RunStore` | `output/runs.json` | Append-only run history |
| `HandledIssueStore` | `output/handled-issues.json` | Issues with PRs opened |
| `PrReviewStore` | `output/reviewed-prs.json` | PRs reviewed (tracks head SHA) |
| `PrResponseStore` | `output/pr-responses.json` | PR comment cursors |
| `FailureCooldownStore` | `output/failure-cooldowns.json` | Exponential backoff (60min → 24h cap) |

All stores handle missing/empty files gracefully. The `output/` directory is
recreated on write if deleted.

---

## Cron runner

- Per-workflow overlap guard (`Arc<Mutex<bool>>`)
- Disabled workflows checked via shared `Arc<Mutex<HashSet<String>>>`
- **Lightweight Info pane refresh** runs BEFORE overlap guard (every tick updates the TUI even when agent is busy)
- **Live rescheduling** via `mpsc` channel — `:c` cron edits take effect immediately
- Schedule changes also persist to `config.toml`

---

## System prompt

Injected into every agent invocation:
- Claude: `--system-prompt` flag (native)
- Codex/Gemini: prepended as `<system>...</system>` block
- Default template: `prompts/system.md` with `{{blocked_commands}}` substitution
- `blocked_commands` config: advisory list (prompt-level, not enforced)

---

## Security notes

- All `gh api` calls use direct `Command::new("gh").arg(...)` — no shell injection
- `safe_truncate()` prevents UTF-8 boundary panics on multi-byte characters
- `--dangerously-skip-permissions` grants Claude full system access (by design)
- `blocked_commands` is prompt-level only — not a technical enforcement
- Issue bodies are a prompt injection vector — run in sandboxed environments for sensitive repos

---

## Key numbers

| Metric | Value |
|---|---|
| Test count | 531+ |
| Clippy warnings | 0 |
| Default agent timeout | 900s (15 min) |
| Default PR review timeout | 600s (10 min) |
| Default issue limit | 100 |
| Failure cooldown | 60min → 120min → 240min → ... cap 24h |
| Log files loaded on startup | Up to 5 per workflow |
| Background refresh | GitHub data fetched async after TUI renders |

---

## Key invariants

1. `cargo test` — 531+ tests, zero failures
2. `cargo clippy` — zero warnings
3. Every Stage returns `Ok(())` for business failures; only `Err` for unrecoverable errors
4. `HandledIssueStore.mark_handled()` only when `success && pr_url.is_some()`
5. PR workspaces (`-pr<N>`) are never torn down
6. Bug-fix workspaces torn down only after success; preserved on failure
7. The reviewer approval token is exactly `HARNESS_REVIEW_APPROVED`
8. The stream-json parser never crashes on malformed input
9. All string truncation uses `safe_truncate()` (UTF-8 safe)
10. `config.toml` must always be valid
11. `reviewer_login` is detected once per executor instance (lazy-cached)
12. Do not use `process::exit()` outside `main.rs`
13. Do not hardcode prompts — use `prompts/*.md`
14. All config fields must be `Option<T>` with documented defaults
15. Info pane shows ALL open items matching filters; items disappear only when closed
16. PR review comments (from `/pulls/{pr}/reviews` API) are filtered by timestamp, not ID
