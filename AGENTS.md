# CLAUDE.md

## What this is

SousDev is a Rust CLI daemon that runs autonomous agentic workflows on a cron schedule. It watches GitHub repos and handles issues, PR reviews, and reviewer comments using AI agents that edit code, run tests, and post results.

Four workflow modes: bug autofix (`github_issues`), PR reviewer (`github_prs`), PR comment responder (`github_pr_responses`), shell trigger (`trigger`).

Eight standalone techniques: ReAct, Reflexion, Tree of Thoughts, Self-Consistency, Critique Loop, Plan-and-Solve, Skeleton-of-Thought, Multi-Agent Debate.

## Agent scratchpad

Use `.agents/sandbox/` freely for your own context retention and planning. Write notes,
task breakdowns, partial results, investigation logs, or anything else that helps you
maintain state across turns. This directory is gitignored — treat it as your working memory.

## Build, test, lint

```bash
cargo test          # 268 tests — run before every commit
cargo clippy        # must pass with zero warnings
cargo build         # debug build
```

All tests must pass and clippy must be clean before committing.

Tests are fully mocked — no API keys, no real git, no network calls.

## Project layout

```
src/
  main.rs                  CLI (clap). Only place process::exit() is allowed.
  lib.rs                   Library root — re-exports all modules.
  types/config.rs          HarnessConfig, WorkflowConfig, all sub-configs.
  types/technique.rs       RunResult, TrajectoryStep — fixed return shape.
  utils/                   Logger, PromptLoader ({{var}} substitution), config_loader.
  providers/               LLMProvider trait + Anthropic, OpenAI, Ollama.
  tools/                   ToolRegistry + built-ins (read_file, write_file, shell).
  workflows/
    executor.rs            WorkflowExecutor — routes all 4 modes. Most critical file.
    stage.rs               Stage trait + StageContext (shared mutable context).
    stores.rs              RunStore, HandledIssueStore, PrReviewStore, PrResponseStore.
    workspace.rs           WorkspaceManager — clone, checkout, reset, teardown.
    github_issues.rs       gh issue list/comment/close wrappers.
    github_prs.rs          gh pr list, inline comments, replies, login detection.
    cron_runner.rs         tokio-cron-scheduler daemon with overlap guard.
    stages/                11 workflow stages (trigger → parse → agent → review → PR).
  techniques/              8 standalone reasoning algorithms, each in its own module.
prompts/                   Editable .md templates with {{variable}} placeholders.
config.toml                Reference config. Must stay valid and well-commented.
```

## Key rules

- **Stages mutate `&mut StageContext` directly.** Do not return partial updates.
- **Stages return `Ok(())` for business failures** (reviewer rejected, agent empty). Only `Err` for unrecoverable errors (process crash, CLI not found).
- **Check `ctx.is_aborted()`** at the top of every stage.
- **Do not hardcode prompts.** Use `prompts/*.md` files loaded via `PromptLoader`.
- **Do not use `println!`/`eprintln!` for debugging.** Use `ctx.logger.debug()` or `tracing::debug!()`.
- **Do not call `process::exit()`** outside `main.rs`.
- **All config fields must be `Option<T>`** with documented defaults.
- **Error handling:** `anyhow::Result` everywhere. `thiserror` for typed errors.
- **Async:** tokio with `full` features. All I/O is async. `async_trait` for async trait methods.
- **Trait objects:** `Arc<dyn LLMProvider>`, `Arc<ToolRegistry>`, `Arc<WorkflowConfig>`.

## Conventions

- `serde` for all serialization. Config = TOML. State files = JSON.
- Doc comments (`///`) on every public item.
- Tests are in-module `#[cfg(test)] mod tests` blocks, not separate files.
- Mock providers implement `LLMProvider` directly as test-local structs.
- `tempfile::TempDir` for filesystem tests.
- Commit style: `<type>: <summary>` — types: feat, fix, refactor, test, docs, chore.

## Invariants

1. `cargo test` — 275+ tests, zero failures.
2. `cargo clippy` — zero warnings.
3. `HandledIssueStore.mark_handled()` only called when `success && pr_url.is_some()`.
4. `PrReviewStore.mark_reviewed()` and `PrResponseStore.mark_responded()` only after `success && !skipped`.
5. The reviewer approval token is exactly `HARNESS_REVIEW_APPROVED`. Do not change without updating `reviewer.rs`.
6. The claude stream-json parser must never crash regardless of malformed input.
7. `config.toml` must always be a valid, runnable example.
8. PR workspaces (`-pr<N>` dirs) are never torn down — always preserved for reuse.
9. Bug-fix workspaces are torn down only after success; otherwise preserved.
10. `github_prs` and `github_pr_responses` modes fail with a clear error if technique is not `claude-loop`.

## Do not

- Add runtime dependencies without clear justification.
- Commit the `output/` directory (gitignored — contains state files and per-run logs).
- Mutate `StageContext` outside the `Stage::run()` method.
- Skip `ctx.is_aborted()` checks in stages.
- Use blocking I/O in async contexts.
