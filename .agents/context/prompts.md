# Prompt Design Lessons

Lessons learned from reviewing SousDev's output quality against human-operated
tools (Conductor, Claude Code). Apply these principles to all new and existing
prompts.

## Scope completeness

- **Never say "minimal."** The word "minimal" causes agents to do partial work
  and rationalize the omission as "follow-up." Use "make the necessary changes"
  or "implement the fix" instead.
- **Require explicit scope verification.** Before finishing, the agent must
  re-read the issue and check every acceptance criterion, listed file, route,
  component, or TODO. If the issue says "23 routes," the agent must address
  all 23.
- **Do not allow the agent to defer work.** Unless the issue explicitly says
  to phase the implementation, the agent should complete all listed items in
  a single pass. "Follow-up PRs" are not acceptable unless requested.

## Issue interpretation

- **Issue reporters describe symptoms, not solutions.** Reporters may suggest
  a simplified fix ("just remove the filter") that doesn't account for the
  full code context. The agent must understand the underlying problem and
  devise the correct solution, even if it differs from the suggestion.
- **Not every issue is a bug.** Issues may be features, refactors, or TODO
  cleanups. The prompt should not assume "bug fix" framing for all issues.

## Downstream impact

- **Trace all data flow after every change.** When a filter, guard, or data
  transformation is modified, the agent must identify ALL downstream consumers
  (derived values, computed properties, sibling functions) and verify they
  still behave correctly.
- **Fix sibling code proactively.** If a reviewer points out a missing guard
  in one derived value, the agent should check whether other derived values
  have the same gap — don't wait for another review round.

## Test quality

- **Write negative tests.** When removing a guard or filter, write tests that
  confirm the guard's purpose is preserved by other means. Don't just test
  the happy path.
- **Write tests for all listed scenarios.** If the issue mentions specific
  edge cases, each one should have a dedicated test.

## Commit hygiene

- **Agents should commit with meaningful messages.** Instruct the agent to
  use conventional commit format: `fix:`, `feat:`, `refactor:`. The summary
  should describe the change, not the process ("apply agent changes" is bad).
- **The PullRequestStage derives commit messages** from: config title →
  LLM-generated PR title → first line of task → hardcoded fallback. The
  hardcoded fallback should never fire if the agent follows instructions.

## Internal review quality

- **Scope completeness is the #1 review criterion.** The internal reviewer
  must cross-reference the original task against the diff and verify every
  listed item was addressed. This is more important than code style.
- **Data flow tracing is required.** The reviewer must trace every changed
  filter/guard through all downstream consumers and flag missed ones.

## PR comment response quality

- **Look around after fixing.** After addressing a reviewer's specific
  comment, check whether the same class of issue exists in nearby code.
  Fix proactively — don't wait for another round.
- **Incorporate inline feedback into plans.** When using the plan-first
  workflow, inline review comments on the plan should be aggregated and
  woven into the plan before execution.

## Plan-first workflow

- **Plans should be thorough.** The plan quality determines the implementation
  quality. The plan should enumerate every file to change, every pattern to
  reuse, and every test scenario.
- **Human instructions should be clear.** The plan PR body should explain
  the review process, how to leave feedback, and how to approve — since not
  all teammates may know the workflow.
- **Use GitHub's Submit Review flow.** Instruct humans to use "Start a review"
  for inline comments so all feedback arrives at once, not as separate
  notifications.
