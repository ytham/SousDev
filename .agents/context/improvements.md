# Future Improvements

## Implemented (from initial session)

- ✅ Reflexion-style reflection between retries (in `agent_loop.rs`)
- ✅ TUI dashboard with three-column layout
- ✅ Pretty log mode with collapsible entries
- ✅ Info pane + Info Expanded floating panel
- ✅ Linear issue source integration
- ✅ System prompt with blocked commands
- ✅ Session persistence (.session.toml)
- ✅ Live cron rescheduling
- ✅ Failure cooldown with exponential backoff
- ✅ Smart timeout with commit detection
- ✅ Background startup refresh (fast TUI render)
- ✅ Security: shell injection fixes (no more `sh -c`)
- ✅ Security: UTF-8 safe truncation everywhere

---

## Skeleton-of-Thought Documentation Generation

**Status:** Planned
**Priority:** Medium
**Technique:** Skeleton-of-Thought (src/techniques/skeleton_of_thought/)

### Concept

A new post-PR-creation stage that generates documentation for code changes
using the Skeleton-of-Thought technique. After a successful PR is created,
the stage:

1. **Skeleton phase**: Ask the LLM to outline 3-6 key documentation points
   from the diff + task description
2. **Expansion phase**: Expand each point in parallel (`futures::join_all`)
   with full context from the diff
3. **Assembly**: Combine into a markdown document posted as a PR comment

### Why Skeleton-of-Thought fits

- Documentation is inherently structured (sections are independent)
- Parallel expansion leverages SoT's only unique advantage: `futures::join_all`
- The outline-then-expand pattern matches how docs are naturally written
- Each section can be expanded with the full diff context without
  interfering with other sections

### Design

**New stage:** `DocumentationStage` in `src/workflows/stages/documentation.rs`

**New config field on `WorkflowConfig`:**
```toml
[workflows.documentation]
enabled = true           # default false
max_points = 5           # SoT max outline points
```

### Insertion point

After `PullRequestStage` in `run_single_issue`. Posted as a PR comment.

### Cost

- 1 LLM call for the skeleton
- N LLM calls for expansion (3-6, in parallel)
- Total: 4-7 LLM calls, ~15-30 seconds wall clock

---

## Potential Future Features

### MCP Integration
SousDev does not currently use MCP (Model Context Protocol). Could be added
for richer tool integration with other AI tools/services.

### Container Isolation
The `blocked_commands` is prompt-level only. Technical enforcement via
container/VM isolation for agent workloads would improve security.

### Parallel Workflow Execution
Currently workflows within a tick run sequentially. Could use `tokio::JoinSet`
to process multiple issues/PRs in parallel.

### Diff-Based PR Description Caching
When the diff hasn't changed, the PR description generation could be skipped
to save Claude CLI invocations.

### Comment Threading
The pr-responder could thread replies to specific inline comments instead of
posting a single summary comment.

### Notification System
Desktop notifications (via `notify-rust` or similar) when a workflow completes,
an issue is fixed, or a review is posted.
