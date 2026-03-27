# SousDev — Complete Reimplementation Specification

This document contains every type, function, algorithm, command, regex, default value, and
data contract in SousDev. An agent reading only this file and the `prompts/` directory should
be able to recreate the entire project from scratch in any language.

---

## What SousDev is

A Rust CLI daemon that runs autonomous agentic workflows on a cron schedule. It watches
GitHub repos for activity — issues, pending PR reviews, reviewer comments — and handles them
with AI agents that edit code, run tests, open PRs, and post review comments.

**Binary name:** `sousdev`
**Config file:** `config.toml` (auto-discovered by walking up from CWD)
**Tagline:** "Prep, review, and plate your PRs automatically."

Two layers:
1. **Workflows** (primary) — four cron-scheduled workflow modes
2. **Techniques** (secondary) — eight standalone agentic reasoning algorithms

---

## External dependencies (runtime)

| Binary | Required by | Purpose |
|---|---|---|
| `git` | all workflow modes | clone, branch, commit, push, diff |
| `gh` (GitHub CLI) | all workflow modes | issue/PR list, create, comment, API calls |
| `claude` | `claude-loop` technique | AI agent CLI (`--print --dangerously-skip-permissions`) |
| `codex` | `codex-loop` technique | AI agent CLI (`--quiet`) |
| `gemini` | `gemini-loop` technique | AI agent CLI (`--yolo`) |

`gh auth login` must have been run. `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `GEMINI_API_KEY` set as env vars for the corresponding agent.

## Rust crate dependencies

`tokio` (full), `anyhow`, `thiserror`, `serde` + `serde_json`, `toml`, `clap` (derive),
`async-trait`, `reqwest` (json), `regex`, `chrono` (serde), `uuid` (v4), `tempfile`,
`tracing` + `tracing-subscriber` (env-filter, fmt), `tokio-cron-scheduler`, `futures`, `dirs`

Dev: `tokio-test`, `mockall`, `tempfile`, `wiremock`

---

## Project layout

```
sousdev/
├── Cargo.toml
├── config.toml                      reference config (TOML)
├── CLAUDE.md                        agent instructions
├── .gitignore                       target/, state files, .env
├── prompts/                         editable .md templates with {{variable}} placeholders
│   ├── bug-fix.md                   TDD agent task
│   ├── bug-fix-context.md           issue metadata context
│   ├── code-review.md               reviewer prompt (must emit HARNESS_REVIEW_APPROVED)
│   ├── review-feedback.md           critique fed back to agent on rejection
│   ├── pr-description.md            PR title + body generation
│   ├── pr-review.md                 PR review (INLINE_COMMENT format)
│   ├── pr-comment-response.md       PR comment response
│   ├── react-system.md              ReAct system prompt
│   ├── reflexion-system.md          Reflexion system prompt
│   └── reflexion-reflect.md         Reflexion self-reflection prompt
└── src/
    ├── main.rs                      CLI entry point (clap). Only place process::exit() allowed.
    ├── lib.rs                       Re-exports: types, utils, providers, tools, workflows, techniques
    ├── types/
    │   ├── config.rs                HarnessConfig, WorkflowConfig, all sub-configs
    │   └── technique.rs             RunResult, TrajectoryStep, StepType
    ├── utils/
    │   ├── logger.rs                Logger (wraps tracing with prefix)
    │   ├── prompt_loader.rs         PromptLoader — file or inline, {{var}} substitution
    │   └── config_loader.rs         Walk-up config.toml discovery
    ├── providers/
    │   ├── provider.rs              LLMProvider trait, Message, CompleteOptions, CompletionResult
    │   ├── anthropic.rs             Anthropic Messages API
    │   ├── openai.rs                OpenAI Chat Completions API
    │   └── ollama.rs                Ollama local API
    ├── tools/
    │   ├── registry.rs              ToolRegistry, Tool, ToolExecutor trait
    │   └── built_ins.rs             readFile, writeFile, shell
    ├── workflows/
    │   ├── workflow.rs              ParsedTask, make_skipped_result
    │   ├── stage.rs                 Stage trait, StageContext, ResolvedPrompts
    │   ├── executor.rs              WorkflowExecutor — all 4 modes
    │   ├── cron_runner.rs           tokio-cron-scheduler daemon
    │   ├── workspace.rs             WorkspaceManager (clone, checkout, reset, teardown)
    │   ├── github_issues.rs         fetch_github_issues, comment, close, detect_repo
    │   ├── github_prs.rs            fetch PRs, inline comments, replies, login detection
    │   ├── stores.rs                RunStore + HandledIssueStore + PrReviewStore + PrResponseStore
    │   └── stages/
    │       ├── trigger.rs           Shell command → stdout
    │       ├── parse.rs             stdout → ParsedTask | SkipWorkflowSignal
    │       ├── agent_loop.rs        Retry loop with resume context
    │       ├── external_agent_loop.rs  Spawn agent CLIs, stream-json parser
    │       ├── reviewer.rs          Claude review + LLM-judge strategies
    │       ├── review_feedback_loop.rs  Reviewer → agent feedback cycle
    │       ├── pr_description.rs    git diff → LLM → TITLE: + BODY:
    │       ├── pull_request.rs      git add/commit/push + gh pr create
    │       ├── pr_checkout.rs       Set ctx.branch from PR
    │       ├── pr_review_poster.rs  Parse INLINE_COMMENT blocks → post to GitHub
    │       └── pr_comment_responder.rs  Reply to threads + summary comment
    └── techniques/
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

## The four workflow modes

### Mode routing (executor)

```
if config.github_pr_responses  → run_pr_response_mode()
if config.github_prs           → run_prs_mode()
if config.github_issues        → run_issues_mode()
else                           → run_standard_mode()
```

First match wins.

---

### Mode 1: Bug autofix (`github_issues`)

**Stage sequence per issue:**
```
fetchGitHubIssues(assignees, labels, limit)
  → filter via HandledIssueStore (skip already-handled)
  → for each unhandled issue (sequential):

WorkspaceManager.setup(run_id, Some(issue_number))
  dir = <workspaces_dir>/<repo_slug>-issue<N>
  if .git exists → reset (fetch, checkout base, hard reset, recreate branch)
  else           → git clone --depth=50 --filter=blob:none --single-branch --branch <base> <url> .
  git checkout -b <branch>

AgentLoopStage
  → exponential backoff retry (base 2s, doubles, max 60s, up to max_retries)
  → resume context on retries (git diff --stat + prior answer)

ReviewFeedbackLoopStage
  → ReviewerStage: one pass → approval or reviewFeedback
  → if rejected: load review-feedback.md, re-run AgentLoopStage
  → up to max_review_rounds

PrDescriptionStage
  → git diff HEAD → LLM → parse TITLE: and BODY:

PullRequestStage
  → git add -A, commit, push
  → gh pr list --head <branch> (skip if PR exists)
  → gh pr create --title ... --body-file <tmpfile>

HandledIssueStore.mark_handled(workflow, issue_number, record)
  — ONLY when success && pr_url.is_some()
```

**Branch naming:**
- Issues: `{branch_prefix}issue-{N}` (default prefix: `sousdev/`)
- Standard: `{branch_prefix}{run_id[:12]}` (default prefix: `sousdev/auto-`)

---

### Mode 2: PR reviewer (`github_prs`)

**Constraint:** `technique` must be `"claude-loop"`. Enforced at runtime inside try/catch (error appears as `result.error`).

**Stage sequence per PR:**
```
fetchGitHubPRs(search="review-requested:@me is:open [extra]", limit)
detectGitHubLogin() → cached as reviewer_login

for each PR:
  PrReviewStore.get_record(workflow, pr_number)
    no record       → review
    different SHA    → review (new commits)
    same SHA         → fetch timeline comments after lastCommentId
                       if any body.trim() == "@{reviewer_login}" → review
                       else → skip

WorkspaceManager.setup_for_pr_review(pr, run_id)
  dir = <workspaces_dir>/<repo_slug>-pr<N>
  if .git → git fetch + gh pr checkout <N>
  else    → clone + gh pr checkout <N>

PrCheckoutStage → sets ctx.branch = pr.head_ref_name
AgentLoopStage  → claude reads diff, produces structured review
PrReviewPosterStage
  → parse INLINE_COMMENT <path>:<line> blocks
  → parse SUMMARY block
  → post inline comments via gh api
  → post summary via gh pr comment

PrReviewStore.mark_reviewed(workflow, record)
  lastCommentId = max(id) across all timeline comments
```

**PR workspaces are NEVER torn down** — preserved for reuse across ticks.

---

### Mode 3: PR comment responder (`github_pr_responses`)

**Constraint:** `technique` must be `"claude-loop"` (same enforcement as Mode 2).

**Stage sequence per PR:**
```
fetchGitHubPRs(search="author:@me [extra]", limit)
detectGitHubLogin() → cached

for each PR:
  PrResponseStore.get_record(workflow, pr_number)
  fetchInlineReviewComments(repo, pr, afterId=record.lastInlineCommentId)
    → only root comments (in_reply_to_id is None)
  fetchPRComments(repo, pr, afterId=record.lastTimelineCommentId)
  filter out comments where login == reviewer_login
  if both empty → skip

WorkspaceManager.setup_for_pr_review(pr, run_id)

AgentLoopStage             → address comments
ReviewFeedbackLoopStage    → self-review before pushing
PullRequestStage           → push to existing branch (skips gh pr create)
PrCommentResponderStage
  → git rev-parse HEAD → short SHA
  → for each inline comment: reply_to_inline_comment(repo, comment_id, body)
  → post_summary_comment(repo, pr_number, summary)

PrResponseStore.mark_responded(workflow, record)
  lastInlineCommentId = max(id) across all inline root comments
  lastTimelineCommentId = max(id) across all timeline comments
```

---

### Mode 4: Shell trigger

```
TriggerStage  → sh -c <command>, store stdout in metadata
ParseStage    → call parser(stdout) → ParsedTask | None (→ SkipWorkflowSignal)
WorkspaceManager.setup(run_id, None)
AgentLoopStage → ReviewFeedbackLoopStage → PrDescriptionStage → PullRequestStage
```

---

## Stage interface

```rust
#[async_trait]
pub trait Stage: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, ctx: &mut StageContext) -> Result<()>;
}
```

Stages **mutate `&mut StageContext` directly**. Return `Ok(())` for business-logic failures.
Only `Err` for unrecoverable errors.

### StageContext — all fields

```
config: Arc<WorkflowConfig>
provider: Arc<dyn LLMProvider>
registry: Arc<ToolRegistry>
workspace_dir: PathBuf
branch: String
parsed_task: ParsedTask             // { task, context?, metadata? }
harness_root: PathBuf
prompts: ResolvedPrompts            // 8 string fields
target_repo: Option<String>
logger: Logger
run_id: String
retry_count: usize
review_rounds: usize
aborted: Arc<AtomicBool>            // check via ctx.is_aborted()

// Stage outputs (None until set):
agent_result: Option<RunResult>
review_result: Option<CritiqueLoopResult>
review_feedback: Option<String>     // critique text; triggers agent re-run
pr_url: Option<String>
pr_title: Option<String>
pr_generated_body: Option<String>

// PR review mode:
reviewing_pr: Option<GitHubPR>
pr_review_result: Option<PrReviewResult>

// PR response mode:
responding_pr: Option<GitHubPR>
unaddressed_comments: Option<UnaddressedComments>   // { inline, timeline }
pr_response_result: Option<PrResponseResult>

// Shared:
reviewer_login: Option<String>
```

### ResolvedPrompts — 8 fields

```
code_review, review_feedback, pr_description, pr_review,
pr_comment_response, react_system, reflexion_system, reflexion_reflect
```

Resolution precedence (low → high):
```
{harness_root}/prompts/{name}.md  →  HarnessConfig.prompts.*  →  WorkflowConfig.prompts.*
```

---

## All types

### RunResult
```
technique: String, answer: String, trajectory: Vec<TrajectoryStep>,
llm_calls: usize, duration_ms: u64, success: bool, error: Option<String>
```
Constructors: `RunResult::success(technique, answer, trajectory, llm_calls, duration_ms)`,
`RunResult::failure(technique, error, trajectory, duration_ms)`

### TrajectoryStep
```
index: usize, step_type: StepType, content: String, timestamp: String,
metadata: HashMap<String, Value>
```
`StepType` enum: `Thought`, `Action`, `Observation`, `Reflection`

### ParsedTask
```
task: String, context: Option<String>, metadata: Option<HashMap<String, Value>>
```
`full_text()` → task + "\n\nAdditional context:\n{context}" if present

### WorkflowResult
```
workflow_name, run_id, started_at, completed_at: String
success, skipped: bool
pr_url, pr_title, error: Option<String>
pr_number, issue_number: Option<u64>
retry_count, review_rounds: usize
trajectory: Vec<TrajectoryStep>
agent_result: Option<RunResult>           // #[serde(skip)]
review_result: Option<CritiqueLoopResult> // #[serde(skip)]
pr_review_result: Option<PrReviewResult>
pr_response_result: Option<PrResponseResult>
```

### PrReviewResult
`inline_comment_count: usize`, `summary_posted: bool`, `head_sha: String`, `errors: Vec<String>`

### PrResponseResult
`inline_replies_posted: usize`, `summary_posted: bool`, `new_head_sha: String`, `errors: Vec<String>`

### GitHubIssue
```
number: u64, title: String, body: Option<String>, url: String,
labels: Vec<{name: String}>, assignees: Vec<{login: String}>,
created_at: String, updated_at: String, state: String, repo: String
```

### GitHubPR
```
number: u64, title: String, body: Option<String>, url: String,
head_ref_name: String, head_ref_oid: String, base_ref_name: String,
author: {login: String}, labels: Vec<{name: String}>,
review_decision: String, created_at: String, updated_at: String, repo: String
```

### PRComment
`id: u64`, `login: String`, `body: String`, `created_at: String`

### InlineReviewComment
```
id: u64, login: String, body: String, path: String, line: Option<u64>,
diff_hunk: Option<String>, created_at: String, in_reply_to_id: Option<u64>
```

### HandledIssueRecord
```
pr_number: Option<u64>, issue_url: String, issue_title: String,
issue_repo: String, pr_url: Option<String>, pr_open: bool,
handled_at: String, updated_at: String
```

### PrReviewRecord
```
pr_number: u64, pr_url: String, pr_title: String, pr_repo: String,
head_sha: String, last_comment_id: u64, reviewed_at: String
```

### PrResponseRecord
```
pr_number: u64, pr_url: String, pr_repo: String, head_sha: String,
last_inline_comment_id: u64, last_timeline_comment_id: u64, responded_at: String
```

---

## LLMProvider trait

```rust
#[async_trait]
pub trait LLMProvider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    async fn complete(&self, messages: &[Message], options: Option<&CompleteOptions>) -> Result<CompletionResult>;
}
```

`Message { role: MessageRole, content: String }` — roles: System, User, Assistant
`CompleteOptions { temperature: Option<f64>, max_tokens: Option<u32> }`
`CompletionResult { content: String, done: bool }`

### Provider implementations

**Anthropic** — `POST https://api.anthropic.com/v1/messages`
Headers: `x-api-key: {ANTHROPIC_API_KEY}`, `anthropic-version: 2023-06-01`
System prompt extracted from messages → top-level `system` field. Default max_tokens: 8192.

**OpenAI** — `POST https://api.openai.com/v1/chat/completions`
Header: `Authorization: Bearer {OPENAI_API_KEY}`. All 3 roles pass through directly.

**Ollama** — `POST {base_url}/api/chat` (default: `http://localhost:11434`)
`stream: false`. Env var: `OLLAMA_BASE_URL`.

**`resolve_provider(config)`** — factory matching `"anthropic"` | `"openai"` | `"ollama"`.

---

## Tool system

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, args: &Value) -> Result<String>;
}

pub struct Tool { name, description, parameters: Value, executor: Arc<dyn ToolExecutor> }
pub struct ToolRegistry { tools: HashMap<String, Tool> }
```

**Built-ins:**
- `readFile` — reads `path` or `file_path`. Returns content.
- `writeFile` — writes `content` to `path`. Creates parent dirs. Returns "Written N bytes to path".
- `shell` — runs `sh -c {command}`. Success → stdout. Failure → "STDERR: ...\nSTDOUT: ..."

---

## External agent loop

### ExternalAgentAdapter
```
name: String, binary: String,
prompt_delivery: PromptDelivery (Stdin | Argument),
build_args: Box<dyn Fn(&ExternalAgentRunOptions) -> Vec<String> + Send + Sync>
```

### Built-in adapters

| Adapter | Binary | Delivery | Args |
|---|---|---|---|
| `claude_adapter` | `claude` | Stdin | `--print --dangerously-skip-permissions --output-format stream-json --verbose [--model M] [extras] -` |
| `codex_adapter` | `codex` | Argument | `--quiet [--model M] [extras]` |
| `gemini_adapter` | `gemini` | Argument | `--yolo [--model M] [extras]` |

Model env vars: `ANTHROPIC_MODEL`, `OPENAI_MODEL`, `GEMINI_MODEL`

### `run_external_agent_loop(prompt, ctx, adapter, options) -> Result<RunResult>`
1. Spawn process: stdin/stdout/stderr piped
2. Write prompt to stdin (if Stdin delivery), close stdin
3. Wait with timeout (default 600s)
4. For claude: `extract_claude_final_answer(stdout)` — scan lines for `{"type":"result","result":"..."}`
5. Build 2-step trajectory (prompt + output)
6. Exit code 0 = success, non-0 = failure

### Claude stream-json events
Line-delimited JSON. Key event: `{"type":"result","result":"<final answer>","num_turns":N,"total_cost_usd":0.005}`

---

## AgentLoopStage — retry + resume

Defaults: `max_retries=1`, `backoff_ms=2000`, `MAX_BACKOFF_MS=60000`

```
loop attempt 0..=max_retries:
  if attempt > 0: task = build_resume_task_text(base_task, prior_result, workspace_dir)
  run agent (claude/codex/gemini via adapter)
  if success: set ctx.agent_result, return Ok
  if last attempt: set ctx.agent_result, return Err
  sleep min(backoff_ms * 2^attempt, 60000) ms
```

**`build_resume_task_text`:** Appends to base task:
```
---
Context from previous attempt (do not undo this work unless it is incorrect):
Previous attempt output:
<answer[:1000]>

Files changed by previous attempt (git diff --stat):
<stat[:30 lines]>
```

---

## ReviewerStage — strategy pattern

**ClaudeReviewStrategy** (when technique == "claude-loop"):
1. Load `code_review` prompt with `{{task}}` and `{{round_note}}`
2. Run claude via `run_external_agent_loop`
3. If output contains `HARNESS_REVIEW_APPROVED` → score 10, no feedback (approved)
4. Else → score 4, feedback = cleaned output (rejected)

**LlmJudgeStrategy** (harness-native techniques):
1. Build review task: "Review the following output...\n\nOriginal task:\n{task}\n\nAgent output:\n{answer}"
2. Run `run_critique_loop` with max_rounds=1
3. If score >= 7.0 → approved. Else → feedback = critique text.

**Constants:**
- `APPROVAL_TOKEN = "HARNESS_REVIEW_APPROVED"`
- `REVIEW_APPROVAL_SCORE = 7.0`
- `REVIEW_CRITERIA = ["correctness", "completeness", "safety", "code quality"]`

---

## ReviewFeedbackLoopStage

Default: `max_review_rounds = 2`

```
for round in 0..max_review_rounds:
  ReviewerStage.run(ctx)
  ctx.review_rounds = round + 1
  if ctx.review_feedback is None: break  // approved
  if not last round:
    load review-feedback.md with {original_task, review_comments, test_command="pnpm test"}
    ctx.parsed_task.task = rendered
    ctx.review_feedback = None
    AgentLoopStage.run(ctx)
  else:
    log "Max review rounds reached"
```

---

## PrDescriptionStage

1. `git diff HEAD` in workspace (truncated to 40000 bytes)
2. Load `pr_description` prompt with `{{task}}`, `{{diff}}`, `{{branch}}`
3. Call provider.complete()
4. Parse response for `TITLE: <text>` and `BODY:\n<text>` markers
5. Set ctx.pr_title, ctx.pr_generated_body
6. Config title/body override LLM-generated values

---

## PullRequestStage

```
1. git add -A
2. git status --porcelain → if dirty: git commit -m <msg>
3. git rev-list --count <base>..HEAD → if 0: return (no commits)
4. git push -u origin <branch>
5. gh pr list --head <branch> --state open --json url --limit 1
   → if URL found: set ctx.pr_url, return (PR exists)
6. title = ctx.pr_title || "sousdev: {first line of task}"
7. body = ctx.pr_generated_body || "Automated change produced by SousDev.\n\nTask:\n{task}"
8. write body to NamedTempFile
9. gh pr create --title <title> --body-file <tmp> [--draft] [--label <l>]* [--repo <repo>]
10. extract URL via regex: r"https://github\.com/[^\s]+"
11. set ctx.pr_url
```

Default commit message: `"chore: apply sousdev agent changes"`

---

## PrReviewPosterStage

**Parsing agent output:**

Inline comment regex: `r"(?i)INLINE_COMMENT\s+(.+?):(\d+)\s*$"` (multiline)
Blocks: text between `INLINE_COMMENT <path>:<line>` and `END_INLINE_COMMENT`
Summary: text between `SUMMARY` and `END_SUMMARY` (case-insensitive)

Fallback: if no structured blocks found, entire answer becomes summary.

**Posting:**
- Each inline comment: `gh api --method POST /repos/{repo}/pulls/{N}/comments -f commit_id=... -f path=... -F line=... -f side=RIGHT -f body=...`
- Summary: `gh pr comment {N} --repo {repo} --body "## PR Review\n\n{count} inline comments.\n\n{summary}"`
- Per-comment errors caught and recorded in `errors[]`

---

## PrCommentResponderStage

1. `git rev-parse HEAD` → full SHA and short SHA (first 7 chars)
2. For each inline comment: `reply_to_inline_comment(repo, comment_id, reply_body)`
   - Reply body: search agent output for lines mentioning file path (up to 3 lines)
   - Fallback: `"I've addressed the comment on \`{path}\`:{line}. Please review the latest commit."`
3. Post summary: `"## Review comment response (commit \`{sha7}\`)\n\nI've addressed {n} inline comment(s)...\n\n**Agent summary:**\n{truncated_output[:2000]}"`

---

## WorkspaceManager

### `setup(run_id, issue_number)` — bug-fix and shell-trigger

```
repo_url = config.repo_url || target_repo → expand to full URL
repo_slug = last path component of URL, strip .git
base_branch = config.base_branch || "main"
workspaces_dir = config.workspaces_dir || ~/sousdev/workspaces

dir = <workspaces_dir>/<repo_slug>-issue<N>   (if issue_number)
      <workspaces_dir>/<repo_slug>-<run_id[:12]>

branch = {prefix}issue-{N} or {prefix}{run_id[:12]}

if .git exists:
  git fetch --depth=50 origin <base> (best-effort)
  git checkout <base>
  git reset --hard origin/<base> (best-effort)
  git branch -D <branch> (ignore error)
  git checkout -b <branch>

if non-empty, no .git: rm -rf, mkdir

git clone --depth=50 --filter=blob:none --single-branch --branch <base> <url> .
git checkout -b <branch>
```

### `setup_for_pr_review(pr, run_id)` — PR review and response

```
dir = <workspaces_dir>/<repo_slug>-pr<N>  (stable across ticks)

if .git: git fetch --depth=50 origin (best-effort)
else: clone

gh pr checkout <N> --repo <repo>
```

**PR workspaces are never torn down.** Bug-fix workspaces torn down only after success.

### URL expansion

```
"owner/repo" + ssh   → git@github.com:owner/repo.git
"owner/repo" + https → https://github.com/owner/repo.git
full URL             → as-is
```

### `repo_to_gh_identifier(target_repo)` → "owner/repo"

Regex: `r"^[\w.\-]+/[\w.\-]+$"` (simple), `r"github\.com[:/]([\w.\-]+/[\w.\-]+?)(?:\.git)?$"` (URL)

---

## State files

All JSON, gitignored. Written with `serde_json::to_string_pretty`. Atomic read-full → modify → write-full.

### `.sousdev-runs.json`
JSON array: `[WorkflowResult, ...]`

### `.sousdev-handled-issues.json`
```json
{ "<workflow_name>": { "<issue_number>": HandledIssueRecord } }
```

### `.sousdev-reviewed-prs.json`
```json
{ "<workflow_name>": { "<pr_number>": PrReviewRecord } }
```

### `.sousdev-pr-responses.json`
```json
{ "<workflow_name>": { "<pr_number>": PrResponseRecord } }
```

---

## Complete gh CLI command inventory

| Function | Command |
|---|---|
| `fetch_github_issues` | `gh issue list --repo {repo} --state open --limit {limit} --json number,title,body,url,labels,assignees,createdAt,updatedAt,state [--assignee {a}] [--label {l}]*` |
| `comment_on_issue` | `gh issue comment {number} --repo {repo} --body {body}` |
| `close_issue` | `gh issue close {number} --repo {repo}` |
| `detect_repo` | `gh repo view --json nameWithOwner --jq .nameWithOwner` |
| `fetch_github_prs` | `gh pr list --repo {repo} --search {query} --limit {limit} --json number,title,body,url,headRefName,headRefOid,baseRefName,author,labels,reviewDecision,createdAt,updatedAt` |
| `fetch_pr_comments` | `gh api /repos/{repo}/issues/{N}/comments --jq '[.[] \| {id,login:.user.login,body,createdAt:.created_at}]'` |
| `fetch_inline_review_comments` | `gh api /repos/{repo}/pulls/{N}/comments --jq '[.[] \| {id,login:.user.login,body,path,line,diffHunk:.diff_hunk,createdAt:.created_at,inReplyToId:.in_reply_to_id}]'` |
| `post_inline_comment` | `gh api --method POST /repos/{repo}/pulls/{N}/comments -f commit_id={sha} -f path={path} -F line={line} -f side=RIGHT -f body={body}` |
| `post_summary_comment` | `gh pr comment {N} --repo {repo} --body {body}` |
| `reply_to_inline_comment` | `gh api --method POST /repos/{repo}/pulls/comments/{id}/replies -f body={body}` |
| `detect_github_login` | `gh api user --jq .login` |
| `setup_for_pr_review` | `gh pr checkout {N} --repo {repo}` |
| `check_existing_pr` | `gh pr list --head {branch} --state open --json url --limit 1 [--repo {repo}]` |
| `create_pr` | `gh pr create --title {title} --body-file {tmp} [--draft] [--label {l}]* [--repo {repo}]` |

---

## Complete regex inventory

| Location | Pattern | Purpose |
|---|---|---|
| `github_issues.rs` | `r"^[\w.\-]+/[\w.\-]+$"` | Simple owner/repo |
| `github_issues.rs` | `r"github\.com[:/]([\w.\-]+/[\w.\-]+?)(?:\.git)?$"` | Extract owner/repo from URL |
| `react/mod.rs` | `r"(?s)\x60\x60\x60(?:json)?\s*(\{.*?\})\s*\x60\x60\x60"` | Fenced JSON block |
| `react/mod.rs` | `r"(?s)<tool>\s*([^<]+?)\s*</tool>"` | XML tool name |
| `react/mod.rs` | `r"(?s)<args>\s*(\{.*?\})\s*</args>"` | XML tool args |
| `pull_request.rs` | `r"https://github\.com/[^\s]+"` | PR URL extraction |
| `pr_review_poster.rs` | `r"(?i)INLINE_COMMENT\s+(.+?):(\d+)\s*$"` | Inline comment header |
| `plan_and_solve/mod.rs` | `r"^\d+[.)]\s+"` | Numbered step |
| `plan_and_solve/mod.rs` | `r"^(?i)step\s+\d+[.):]\s*"` | "Step N:" heading |
| `skeleton_of_thought/mod.rs` | `r"^\d+[.)]\s+(.+)$"` | Outline point |

---

## Complete default values inventory

| Setting | Default |
|---|---|
| `git_method` | `"https"` |
| `base_branch` | `"main"` |
| `branch_prefix` (issues) | `"sousdev/"` |
| `branch_prefix` (auto) | `"sousdev/auto-"` |
| `workspaces_dir` | `~/sousdev/workspaces` |
| `limit` (issues/PRs) | `10` |
| `timeout_ms` (trigger) | `60_000` |
| `timeout_secs` (external agent) | `600` |
| `max_retries` | `1` |
| `backoff_ms` | `2_000` |
| `MAX_BACKOFF_MS` | `60_000` |
| `max_review_rounds` | `2` |
| `max_tokens` (Anthropic) | `8192` |
| `MAX_DIFF_BYTES` | `40_000` |
| `DEFAULT_COMMIT_MSG` | `"chore: apply sousdev agent changes"` |
| `APPROVAL_TOKEN` | `"HARNESS_REVIEW_APPROVED"` |
| `REVIEW_APPROVAL_SCORE` | `7.0` |
| `react max_iterations` | `10` |
| `reflexion max_trials` | `3` |
| `reflexion memory_window` | `5` |
| `reflexion max_inner_iterations` | `8` |
| `tot branching` | `3` |
| `tot max_depth` | `3` |
| `tot score_threshold` | `4.0` |
| `self_consistency samples` | `5` |
| `self_consistency temperature` | `0.7` |
| `skeleton max_points` | `6` |
| `debate num_agents` | `3` |
| `debate rounds` | `2` |
| `critique satisfaction_threshold` | `7.0` |

---

## The eight techniques

### ReAct (react)

Options: `task`, `provider`, `registry?`, `system_prompt?`, `max_iterations?`, `harness_root?`

```
messages = [system_prompt, user(task)]
loop up to max_iterations:
  response = provider.complete(messages, max_tokens=2048)
  record Thought
  if parse_tool_call(response) → (name, args):
    record Action
    result = registry.execute(name, args)
    record Observation
    append assistant(response) + user(result) to messages
  else:
    return response as final answer (success)
```

Tool call parsing: JSON `{"tool":"name","args":{}}` in fenced blocks or bare `{}`, OR XML `<tool>name</tool><args>{}</args>`.

### Reflexion

Options: `task`, `provider`, `registry?`, `system_prompt?`, `reflect_prompt?`, `max_trials?`, `memory_window?`, `max_inner_iterations?`, `harness_root?`

```
reflections = []
for trial in 0..max_trials:
  context = task + last memory_window reflections
  run inner ReAct loop (max_inner_iterations)
  if success: return
  if not last trial:
    ask LLM to reflect (max_tokens=512)
    push reflection
return best answer or failure
```

### Tree of Thoughts

Options: `task`, `provider`, `branching?`, `strategy?`, `max_depth?`, `score_threshold?`

BFS: expand all frontier → score → prune below threshold → keep top branching → repeat
DFS: expand best-first, recurse, backtrack on low scores

Expand prompt: "Generate {branching} distinct next reasoning steps, separated by ---THOUGHT---"
Score prompt: "Rate this thought 0-10... Respond with only: SCORE: <integer>"

`parse_score`: look for `SCORE: N`, fallback to lone number, default 5.0 (ToT) or 0.0 (critique)

### Self-Consistency

Options: `task`, `provider`, `samples?`, `temperature?`

Sample N chains at temperature. Majority vote (threshold: len/2 + 1). If no majority: LLM consensus ("pick best or synthesize").

### Critique Loop

Options: `task`, `provider`, `max_rounds`, `criteria: Vec<String>`, `satisfaction_threshold`

Returns: `CritiqueLoopResult { answer, rounds: Vec<CritiqueRound { response, critique, score }> }`

```
for round in 0..max_rounds:
  response = generate (first round: task; later: task + previous critique)
  critique = LLM rates against criteria, emits SCORE: N and CRITIQUE: text
  if score >= threshold: break
```

### Plan and Solve (PS+)

Options: `task`, `provider`, `registry?`, `detailed_plan?`, `max_steps?`

```
plan = LLM generates numbered plan (PS+ adds sub-steps + pitfalls)
for each step: execute via LLM (with tool calls if registry present)
synthesis = LLM produces final answer
```

### Skeleton of Thought

Options: `task`, `provider`, `max_points?`, `parallel_expansion?`

```
skeleton = LLM generates numbered outline (max max_points items)
expansions = for each point: LLM expands (parallel or sequential)
answer = join as "**i. point**\nexpansion"
```

### Multi-Agent Debate

Options: `task`, `provider`, `num_agents?`, `rounds?`, `aggregation?`

```
each agent: initial position (max_tokens=512)
for round in 0..rounds:
  each agent sees all other positions, revises (max_tokens=512)
aggregation:
  Judge: LLM synthesizes from all positions (max_tokens=1024)
  Majority: majority_vote on final positions
```

---

## Prompt templates and their variables

| File | Used by | Variables |
|---|---|---|
| `bug-fix.md` | user's buildTask | `issue_number`, `issue_title`, `issue_body`, `test_command` |
| `bug-fix-context.md` | user's buildTask | `issue_url`, `issue_created_at`, `issue_labels`, `test_command` |
| `code-review.md` | ClaudeReviewStrategy | `task`, `round_note` |
| `review-feedback.md` | ReviewFeedbackLoopStage | `original_task`, `review_comments`, `test_command` |
| `pr-description.md` | PrDescriptionStage | `task`, `diff`, `branch` |
| `pr-review.md` | PR review executor | `pr_title`, `pr_author`, `pr_head_ref`, `pr_base_ref`, `pr_body` |
| `pr-comment-response.md` | PR response executor | `pr_title`, `pr_author`, `pr_head_ref`, `pr_base_ref`, `pr_url`, `inline_comments`, `timeline_comments` |
| `react-system.md` | ReAct technique | — |
| `reflexion-system.md` | Reflexion technique | — |
| `reflexion-reflect.md` | Reflexion reflect step | `task`, `last_attempt` |

---

## HarnessConfig — full type

```
provider: String                  // "anthropic" | "openai" | "ollama"
model: String
target_repo: Option<String>
git_method: Option<String>        // "ssh" | "https" (default "https")
logging: Option<LoggingConfig>    // { level?, pretty? }
prompts: Option<PromptConfig>     // 8 optional string fields
techniques: Option<TechniquesConfig>  // 8 optional technique config blocks
workflows: Vec<WorkflowConfig>    // #[serde(skip)] — set programmatically
```

## WorkflowConfig — full type

```
name: String
schedule: String                  // cron expression
github_pr_responses: Option<GitHubPRResponseWorkflowConfig>
github_prs: Option<GitHubPRsWorkflowConfig>
github_issues: Option<GitHubIssuesWorkflowConfig>
trigger: Option<TriggerConfig>
agent_loop: AgentLoopConfig       // { technique, external_agent?, max_retries?, backoff_ms?, max_review_rounds?, max_iterations? }
workspace: Option<WorkspaceConfig>  // { repo_url?, base_branch?, branch_prefix?, workspaces_dir? }
pull_request: Option<PullRequestConfig>  // { title?, body?, draft?, labels? }
retry: Option<RetryConfig>        // { max_attempts?, backoff_ms? }
prompts: Option<WorkflowPromptConfig>  // { code_review?, review_feedback?, pr_description? }
```

---

## CLI commands

```
sousdev list                              list configured workflows
sousdev workflow <name> [--no-workspace]  run workflow immediately
sousdev start [--no-workspace]            start cron daemon
sousdev status [<name>] [--limit N]       show run history
sousdev logs <name> <run-id-prefix>       full trajectory for a run
sousdev run <technique> --task "..."      run technique directly
sousdev techniques                        list all techniques
sousdev technique <name>                  details + paper citation
```

Global: `--config <path>`, `--help`, `--version`

---

## Cron runner

```
for each workflow:
  schedule cron job via tokio_cron_scheduler
  on tick:
    if overlap_guard locked: skip
    else: lock, resolve provider, create executor, run, unlock
graceful shutdown: tokio::signal::ctrl_c → sched.shutdown()
```

Overlap guard: `Arc<Mutex<bool>>` per workflow name.

---

## Key invariants

1. `cargo test` — 268+ tests, zero failures.
2. `cargo clippy` — zero warnings.
3. Every Stage returns `Ok(())` for business failures. Only `Err` for unrecoverable errors.
4. `HandledIssueStore.mark_handled()` only called when `success && pr_url.is_some()`.
5. `PrReviewStore.mark_reviewed()` / `PrResponseStore.mark_responded()` only after `success && !skipped`.
6. The reviewer approval token is exactly `HARNESS_REVIEW_APPROVED`.
7. The stream-json parser never crashes regardless of malformed input.
8. `config.toml` must always be valid and well-commented.
9. PR workspaces (`-pr<N>`) are never torn down.
10. Bug-fix workspaces are torn down only after success; otherwise preserved.
11. `github_prs` and `github_pr_responses` modes fail clearly if technique != "claude-loop".
12. `reviewer_login` is detected once per executor instance (lazy-cached).
13. Do not use `process::exit()` outside `main.rs`.
14. Do not hardcode prompts in source. Use `prompts/*.md`.
15. Do not use `println!` for debugging. Use `ctx.logger` or `tracing`.
16. All config fields must be `Option<T>` with documented defaults.
