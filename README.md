# SousDev

**Prep, review, and plate your PRs automatically.**

SousDev is a Rust toolkit that runs autonomous agentic workflows on a cron schedule. It watches your GitHub repos for activity — new issues, pending PR reviews, reviewer comments — and handles them with AI agents that edit code, run tests, open PRs, and post review comments.

---

## Workflows

| Mode | Config field | What it does |
|---|---|---|
| **Bug autofix** | `github_issues` | Fetches assigned issues, fixes them autonomously, opens PRs |
| **PR reviewer** | `github_prs` | Reviews PRs where your review is requested, posts inline comments |
| **PR responder** | `github_pr_responses` | Addresses reviewer comments on your open PRs, pushes fixes |
| **Shell trigger** | `trigger` + `parser` | Runs any shell command and acts on its output |

### Bug autofix flow

```
cron tick
  → fetch GitHub issues (assignees, labels, limit)
  → filter out already-handled issues
  → for each unhandled issue:
      clone repo → create branch
      → AgentLoopStage (claude fixes the bug, runs tests)
      → ReviewFeedbackLoopStage (self-review, critique → re-run)
      → PRDescriptionStage (LLM writes title + body from diff)
      → PullRequestStage (commit, push, gh pr create)
```

### PR reviewer flow

```
cron tick
  → fetch PRs where review requested (review-requested:@me)
  → skip already-reviewed (unless new commits or @me ping)
  → for each unreviewed PR:
      checkout PR branch
      → AgentLoopStage (claude reads diff, produces review)
      → PRReviewPosterStage (post inline comments + summary)
```

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
      → PRCommentResponderStage (reply "Addressed in <sha>" + summary)
```

---

## Quickstart

### Prerequisites

- Rust 1.74+ (stable)
- `git` and [`gh` CLI](https://cli.github.com/) authenticated (`gh auth login`)
- `claude` CLI installed (`npm install -g @anthropic-ai/claude-code`)
- `ANTHROPIC_API_KEY` set in environment

### Install and run

```bash
cd sousdev
cargo build --release

# Set your API key
export ANTHROPIC_API_KEY=sk-ant-...

# Edit the config
cp config.toml my-project/config.toml
# Edit target_repo, git_method, etc.

# List configured pipelines
cargo run --release -- list

# Run a pipeline once (no cron)
cargo run --release -- workflow github-bug-autofix

# Start the cron daemon
cargo run --release -- start
```

---

## Configuration

SousDev looks for `config.toml` by walking up from the current directory.

```toml
provider = "anthropic"
model = "claude-sonnet-4-20250514"
target_repo = "your-org/your-repo"
git_method = "ssh"

[logging]
level = "info"
pretty = false

[techniques.react]
max_iterations = 10

[techniques.reflexion]
max_trials = 3
```

Pipelines are configured programmatically in Rust because they use closures (`buildTask`, `filter`, `parser`) that can't be serialised to TOML:

```rust
use sousdev::types::config::*;
use sousdev::pipelines::pipeline::ParsedTask;

let pipeline = PipelineConfig {
    name: "github-bug-autofix".into(),
    schedule: "0 * * * *".into(), // every hour

    github_issues: Some(GitHubIssuesPipelineConfig {
        assignees: Some(vec!["@me".into()]),
        labels: Some(vec!["bug".into()]),
        limit: Some(3),
        ..Default::default()
    }),

    agent_loop: AgentLoopConfig {
        technique: "claude-loop".into(),
        external_agent: Some(ExternalAgentConfig {
            timeout_secs: Some(300),
            ..Default::default()
        }),
        max_retries: Some(1),
        max_review_rounds: Some(1),
        ..Default::default()
    },

    workspace: Some(WorkspaceConfig {
        base_branch: Some("main".into()),
        ..Default::default()
    }),

    pull_request: Some(PullRequestConfig {
        draft: Some(false),
        labels: Some(vec!["sousdev".into(), "bug-fix".into()]),
        ..Default::default()
    }),

    ..Default::default()
};
```

---

## CLI commands

```bash
sousdev list                              # list configured pipelines
sousdev workflow <name>                   # run immediately (ignores cron)
sousdev workflow <name> --no-workspace    # run in CWD, skip git clone
sousdev start                             # start cron daemon
sousdev status [<name>] [--limit N]       # show run history
sousdev logs <name> <run-id-prefix>       # full trajectory for a run

sousdev run <technique> --task "..."      # run a technique directly
sousdev techniques                        # list all techniques
sousdev technique <name>                  # details + paper citation
```

---

## Standalone techniques

Eight agentic reasoning algorithms, usable independently or inside pipelines:

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

## LLM providers

| Provider | Config value | Required env var |
|---|---|---|
| Anthropic Claude | `"anthropic"` | `ANTHROPIC_API_KEY` |
| OpenAI | `"openai"` | `OPENAI_API_KEY` |
| Ollama (local) | `"ollama"` | `OLLAMA_BASE_URL` (default: `http://localhost:11434`) |

---

## Output directory

All state and logs live under `output/` (gitignored). SousDev manages this automatically:

```
output/
├── runs.json                    Run history for all pipelines
├── handled-issues.json          Issues processed by bug-autofix pipelines
├── reviewed-prs.json            PRs reviewed by reviewer pipelines
├── pr-responses.json            PR comment cursors for responder pipelines
└── logs/
    ├── github-bug-autofix/      Per-run structured log files
    │   ├── <run-id>.json
    │   └── ...
    ├── github-pr-reviewer/
    └── github-pr-responder/
```

Each run log is a JSON file with a header (pipeline name, run ID, status) and a timestamped
`entries` array — designed for a TUI with scrollable per-run views and status tabs.

---

## Project structure

```
sousdev/
├── Cargo.toml
├── config.toml                  ← reference config (edit for your project)
├── prompts/                     ← editable .md prompt templates
│   ├── bug-fix.md
│   ├── code-review.md
│   ├── pr-review.md
│   ├── pr-comment-response.md
│   └── ...
└── src/
    ├── main.rs                  ← CLI entry point (clap)
    ├── lib.rs                   ← library entry point
    ├── types/                   ← config, RunResult, TrajectoryStep
    ├── utils/                   ← logger, prompt loader, config loader
    ├── providers/               ← LLMProvider trait + Anthropic/OpenAI/Ollama
    ├── tools/                   ← ToolRegistry + read_file/write_file/shell
    ├── pipelines/
    │   ├── executor.rs          ← PipelineExecutor (all 4 modes)
    │   ├── github_issues.rs     ← fetch issues, comment, close
    │   ├── github_prs.rs        ← fetch PRs, post comments, reply
    │   ├── stores.rs            ← RunStore + 3 deduplication stores (in output/)
    │   ├── workspace.rs         ← WorkspaceManager (clone, checkout, reset)
    │   ├── workflow_log.rs      ← per-run structured logs (output/logs/<pipeline>/<run>.json)
    │   ├── cron_runner.rs       ← tokio-cron-scheduler daemon
    │   └── stages/              ← all 11 pipeline stages
    └── techniques/              ← 8 standalone reasoning algorithms
```

---

## Development

```bash
cargo test            # 275 tests, all mocked (no API keys needed)
cargo build           # debug build
cargo build --release # optimized build
cargo clippy          # lint
```

---

## License

MIT
