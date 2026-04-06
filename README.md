# 🧑‍🍳 SousDev

**Prep, review, and plate your PRs automatically.**

SousDev (like a sous-chef, not a sous-vide) is a Rust CLI with a built-in TUI that runs autonomous agentic workflows on a cron schedule. It watches your GitHub (or Linear) repos for activity — new issues, pending PR reviews, reviewer comments — and handles them with AI agents that edit code, run tests, open PRs, and post review comments.

Run bare `sousdev` to launch the interactive TUI dashboard. All workflows, logs, and status are visible in real-time.

---

## Quickstart

### Prerequisites

- Rust 1.74+ (stable)
- `git` and [`gh` CLI](https://cli.github.com/) authenticated (`gh auth login`)
- `claude` CLI installed (`npm install -g @anthropic-ai/claude-code`)
- Optional: `codex` CLI (for `codex-loop` technique, requires `OPENAI_API_KEY`)
- Optional: `gemini` CLI (for `gemini-loop` technique, requires `GEMINI_API_KEY`)

### Setup

```bash
cd sousdev
cargo build --release

# Authenticate the Claude CLI (one-time — no API key needed)
claude

# Edit the config
cp config.toml my-project/config.toml
# Edit target_repo, git_method, schedules, etc.

# Optional: set up .env for harness-native techniques or Linear
cp .env.example .env
# Edit .env if needed (most users won't need this)
```

### Run

```bash
# Launch the TUI dashboard (recommended)
./target/release/sousdev

# Or use CLI commands:
sousdev list                              # list configured workflows
sousdev workflow issue-autofix            # run a workflow once
sousdev start                             # start headless cron daemon
```

---

## Workflows

| Mode | Config field | What it does |
|---|---|---|
| **Issue autofix** | `github_issues` or `linear_issues` | Fetches assigned issues, fixes them autonomously, opens PRs |
| **Plan-first autofix** | `github_issues` + `plan_first = true` | Creates a plan PR for human review before writing code (default) |
| **PR reviewer** | `github_prs` | Reviews PRs where your review is requested, posts inline comments |
| **PR responder** | `github_pr_responses` | Addresses reviewer comments on your open PRs, pushes fixes |
| **Shell trigger** | `trigger` + `parser` | Runs any shell command and acts on its output |

### Issue autofix flow

```
cron tick
  → fetch issues (GitHub or Linear, filtered by assignee/labels)
  → skip handled issues + failure cooldown
  → for each unhandled issue:
      clone repo → create branch
      → AgentLoopStage (Claude fixes the bug, runs tests)
        → Reflexion-style reflection between retries
      → ReviewFeedbackLoopStage (self-review, critique → re-run)
      → PrDescriptionStage (Claude writes title + body from diff)
      → PullRequestStage (commit, push, rebase CI fixes, gh pr create)
```

### Plan-first autofix flow (default: `plan_first = true`)

```
cron tick
  → fetch issues → skip handled
  → for each unhandled issue:
      State: (none) → plan_posted
        → Agent generates implementation plan
        → Opens plan PR for human review
        → [Pl] PlanPending badge in TUI
      State: plan_posted → plan_approved (60s background polling)
        → Human approves plan PR on GitHub
      State: plan_approved → code_complete
        → Agent executes the approved plan
        → Opens implementation PR (or pushes to plan branch)
```

### PR reviewer flow

```
cron tick
  → fetch PRs where review requested + verify reviewer match
  → skip already-reviewed (unless new commits or new human comment)
  → for each unreviewed PR:
      fetch PR branch → checkout
      → AgentLoopStage / ApiReviewLoopStage (reads diff, produces review)
      → PrReviewPosterStage (post inline comments + summary with verdict)
```

**Multi-model review:** When 2+ models are configured in `[[models]]`, runs parallel reviews from each model and consolidates results. Each model outputs a verdict (`✅ Approved` or `🔴 Not Approved`), and the consolidated review includes a per-model verdict table.

**API-based review:** When API keys are available (`ANTHROPIC_API_KEY` or `OPENAI_API_KEY`), uses native tool-use APIs instead of CLI binaries. Read-only tool set with allowlisted shell commands. Falls back to CLI when no API key.

### PR responder flow

```
cron tick
  → fetch your open PRs (author:@me)
  → check for new inline + timeline comments since last response
  → for each PR with new comments:
      checkout PR branch
      → AgentLoopStage (address every comment)
      → ReviewFeedbackLoopStage (self-review before push)
      → PullRequestStage (push to existing branch)
      → PrCommentResponderStage (reply "Addressed in <sha>" + summary)
```

---

## Authentication

The default `claude-loop` technique uses the **Claude CLI**, which authenticates via its own OAuth flow — **no API key needed**. Just run `claude` once to authenticate.

For optional features, SousDev loads a `.env` file automatically on startup:

```bash
cp .env.example .env
```

| Variable | When needed | Used by |
|---|---|---|
| `ANTHROPIC_API_KEY` | Harness-native techniques + API-based PR review + multi-model review | Direct Anthropic API calls |
| `OPENAI_API_KEY` | OpenAI models + API-based PR review + multi-model review | OpenAI provider |
| `LINEAR_API_KEY` | Only for Linear issue source | `linear_issues` workflow trigger |
| `GITHUB_TOKEN` | Usually not needed (`gh` CLI handles auth) | Override for `gh` auth |

Most users running the default `claude-loop` workflow **don't need any API keys** — only `gh auth login` and `claude` CLI auth. Setting API keys unlocks API-based review (native tool-use) and multi-model review (parallel reviews from 2+ models).

---

## TUI Dashboard

Launch with bare `sousdev` (no subcommand). The TUI shows:

### Layout

```
┌──────────────┬────────────────────────────────────────────┐
│  Workflows   │  Log pane (real-time agent output)         │
│              │                                            │
│  > issue-... │  │ Let me read the file first.             │
│    every hour│  │ I see the problem on line 42.           │
│    [+] agent │                                            │
│    [ ] review│  [tool] Read("src/main.rs", limit=100)     │
│    [ ] pr-...|  [+] 3 more tool calls — click to expand  │
│              │                                            │
│  pr-reviewer │  │ All tests pass. The fix is complete.    │
│    every 5min│                                            │
│              │──────────────────────────────────────────  │
│  ↑↓ select   │  issue-autofix  owner/repo  running │ #42 │
│  i  info     │  : menu  f/b page  F/B end/begin         │
└──────────────┴────────────────────────────────────────────┘
```

### Key bindings

**Normal mode:**

| Key | Action |
|-----|--------|
| `↑↓` | Select workflow |
| `f`/`b` | Page down/up in logs |
| `F`/`B` | Jump to end/beginning of logs |
| `i` | Toggle info panel (issue/PR status) |
| `:` | Open command menu |
| `Ctrl+C` | Quit |

**Info panel (`i`):**

| Key | Action |
|-----|--------|
| `↑↓` | Navigate items (first item is "All logs") |
| `Enter` | Filter logs to selected item |
| `g` | Open item URL in browser |
| `c` | Clear error status (retry on next tick) |
| `C` | Clear all errors |
| `Esc` | Close panel |

**Command menu (`:`):**

| Key | Action |
|-----|--------|
| `q` | Quit |
| `e` | Enable/disable selected workflow |
| `c` | Edit cron schedule (accepts `5m`, `2hr`, or cron notation) |
| `p` | Pause/resume |

### Pretty log mode

Enabled by default (`pretty = true` in `[logging]`). Features:
- **Thinking blocks**: Cyan left border, first 4 lines shown, click to expand
- **Tool calls**: Result hidden by default, click to show
- **Consolidated tool calls**: 3+ consecutive calls collapsed, click to expand
- **Click vs drag**: Click toggles expand, drag copies to clipboard

---

## Configuration

SousDev looks for `config.toml` by walking up from the current directory.

```toml
# 🧑‍🍳 SousDev configuration
target_repo = "your-org/your-repo"
git_method = "ssh"

# Model configuration — first entry is primary, additional entries enable multi-model review
[[models]]
provider = "anthropic"
model = "claude-opus-4-6"

[[models]]
provider = "openai"
model = "gpt-5.4"

# System prompt injected into every agent invocation
# system_prompt = "prompts/system.md"   # default
blocked_commands = []                    # commands the agent must never run

[logging]
level = "info"
pretty = true       # structured log rendering in TUI (default true)

[[workflows]]
name = "issue-autofix"
schedule = "0 0 * * * *"    # every hour
# plan_first = true          # default: create plan PR for human review before coding

[workflows.github_issues]
assignees = ["@me"]
labels = ["bug"]
limit = 3

# Or use Linear instead:
# [workflows.linear_issues]
# team = "ENG"
# states = ["Todo"]
# limit = 3

[workflows.agent_loop]
technique = "claude-loop"
max_retries = 1
max_review_rounds = 1

[workflows.agent_loop.external_agent]
timeout_secs = 300

[workflows.workspace]
base_branch = "main"
branch_prefix = "sousdev/issue-"

[workflows.pull_request]
draft = false
labels = []
# show_branding = true   # append "🧑‍🍳 Automated by SousDev" to PR body

[workflows.retry]
max_attempts = 2
backoff_ms = 10000
```

Schedule changes made in the TUI (via `:c`) take effect immediately and persist to `config.toml`.

---

## Issue Sources

### GitHub Issues

```toml
[workflows.github_issues]
assignees = ["@me"]
labels = ["bug"]
limit = 3
```

### Linear Issues

```toml
[workflows.linear_issues]
team = "ENG"
states = ["Todo"]
labels = ["bug"]
assignee = "Alice"
limit = 3
```

Requires `LINEAR_API_KEY` in `.env`. The git repository is taken from `target_repo`.

---

## CLI Commands

```bash
sousdev                                   # launch TUI dashboard
sousdev list                              # list configured workflows
sousdev workflow <name>                   # run workflow once (no cron)
sousdev workflow <name> --no-workspace    # run in CWD, skip git clone
sousdev start                             # start headless cron daemon
sousdev status [<name>] [--limit N]       # show run history
sousdev logs <name> <run-id-prefix>       # full trajectory for a run

sousdev run <technique> --task "..."      # run a technique directly
sousdev techniques                        # list all techniques
sousdev technique <name>                  # details + paper citation
```

---

## Standalone Techniques

Eight agentic reasoning algorithms, usable independently or inside workflows:

| Technique | What it does | Paper |
|---|---|---|
| `react` | Think → Act → Observe loop | Yao et al., 2022 |
| `reflexion` | Self-reflection + episodic memory | Shinn et al., 2023 |
| `tree-of-thoughts` | BFS/DFS scored reasoning tree | Yao et al., 2023 |
| `self-consistency` | N-sample majority vote | Wang et al., 2022 |
| `critique-loop` | Generate → Critique → Revise | Bai et al., 2022 |
| `plan-and-solve` | Plan first, then execute (PS+) | Wang et al., 2023 |
| `skeleton-of-thought` | Outline → parallel expansion | Ning et al., 2023 |
| `multi-agent-debate` | N agents debate, judge synthesises | Du et al., 2023 |

```bash
sousdev run react              --task "Fix the auth bug in src/auth.rs"
sousdev run reflexion          --task "Write a sorting algorithm" --max-trials 5
sousdev run tree-of-thoughts   --task "Use 4 7 8 14 to reach 24" --strategy bfs
sousdev run self-consistency   --task "What is 17 * 23?" --samples 7
sousdev run critique-loop      --task "Write a binary search" --max-rounds 3
sousdev run plan-and-solve     --task "Migrate auth.rs to use JWT"
sousdev run skeleton-of-thought --task "Explain REST vs GraphQL" --max-points 6
sousdev run multi-agent-debate --task "Is Pluto a planet?" --num-agents 3
```

---

## LLM Providers

Configured via `[[models]]` array in `config.toml`. First entry is the primary model; additional entries enable multi-model PR review.

| Provider | `[[models]]` provider value | Required env var | Tool-use support |
|---|---|---|---|
| Anthropic Claude | `"anthropic"` | `ANTHROPIC_API_KEY` | Yes (native) |
| OpenAI | `"openai"` | `OPENAI_API_KEY` | Yes (function calling) |
| Ollama (local) | `"ollama"` | `OLLAMA_BASE_URL` (default: `http://localhost:11434`) | No |

---

## State Files

All state lives under `output/` (gitignored):

```
output/
├── runs.json                    Run history for all workflows
├── handled-issues.json          Issues processed by issue-autofix
├── reviewed-prs.json            PRs reviewed by pr-reviewer
├── pr-responses.json            PR comment cursors for pr-responder
├── failure-cooldowns.json       Failure tracking with exponential backoff
└── logs/
    └── <workflow-name>/
        └── <run-id>.json        Per-run structured log
```

Session state (`.session.toml`) tracks enabled/disabled workflows across restarts.

---

## Project Structure

```
sousdev/
├── Cargo.toml
├── config.toml              ← reference config
├── .env.example             ← environment variable template
├── prompts/                 ← editable .md prompt templates
│   ├── system.md            ← system prompt (injected into every agent call)
│   ├── bug-fix.md
│   ├── code-review.md
│   ├── pr-description.md
│   ├── pr-review.md         ← PR review (CRITICAL CONSTRAINTS, verdict requirement)
│   ├── pr-comment-response.md
│   ├── reflect.md           ← reflexion prompt for retry reasoning
│   ├── plan-generation.md   ← plan-first: generate implementation plan
│   ├── plan-pr-body.md      ← plan-first: PR body for plan review
│   ├── plan-execution.md    ← plan-first: execute approved plan
│   ├── review-consolidation.md ← multi-model: consolidate parallel reviews
│   └── ...
└── src/
    ├── main.rs              ← CLI entry point (clap)
    ├── lib.rs               ← library root
    ├── types/               ← config, RunResult, TrajectoryStep
    ├── utils/               ← logger, prompt loader, config loader
    ├── providers/           ← LLMProvider trait + Anthropic/OpenAI/Ollama
    ├── tools/               ← ToolRegistry + read_file/write_file/shell
    ├── tui/                 ← ratatui TUI dashboard
    │   ├── app.rs           ← App state, event handling, key routing
    │   ├── events.rs        ← TuiEvent channel types
    │   ├── session.rs       ← .session.toml persistence
    │   ├── ui.rs            ← layout + toast rendering
    │   └── widgets/         ← sidebar, log_view, info_panel, command_menu
    ├── workflows/
    │   ├── executor.rs      ← WorkflowExecutor (all 5 modes)
    │   ├── github_issues.rs ← gh issue list/comment/close
    │   ├── github_prs.rs    ← gh pr list/comment/reply
    │   ├── linear_issues.rs ← Linear GraphQL API
    │   ├── stores.rs        ← RunStore + dedup + failure cooldown + plan_state
    │   ├── multi_review.rs  ← multi-model PR review: detection, consolidation
    │   ├── workspace.rs     ← clone, checkout, reset, teardown
    │   ├── workflow_log.rs  ← per-run structured logs
    │   ├── cron_runner.rs   ← tokio-cron-scheduler + live rescheduling
    │   └── stages/          ← 12 workflow stages (including api_review_loop)
    └── techniques/          ← 8 standalone reasoning algorithms
```

---

## Development

```bash
cargo test            # 608+ tests, all mocked (no API keys needed)
cargo build           # debug build
cargo build --release # optimized build
cargo clippy          # lint (must pass with zero warnings)
```

---

## License

MIT
