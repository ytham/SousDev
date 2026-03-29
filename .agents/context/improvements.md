# Future Improvements

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

**New prompt template:** `prompts/documentation.md`
```markdown
The following code changes were made to address this task:

Task: {{task}}

Diff summary:
{{diff}}

List {{max_points}} key documentation points about these changes.
Each point should cover one distinct aspect (what changed, why,
how it affects the system, migration notes, etc.).

Format as a numbered list:
1. ...
2. ...
```

### Insertion point

After `PullRequestStage` in the stage pipeline in `run_single_issue`:
```rust
self.run_stage(&PullRequestStage, &mut ctx).await?;
// Only run if documentation is enabled and PR was created
if ctx.pr_url.is_some() && has_docs_config {
    self.run_stage(&DocumentationStage, &mut ctx).await?;
}
```

### Output

Posted as a PR comment via `gh pr comment <number> --repo <repo> --body-file <temp>`.

The comment contains assembled documentation:
```markdown
## Documentation — Automated by SousDev

### 1. Database schema changes
...

### 2. New API endpoint
...
```

### Why post as a PR comment, not a file commit

- Doesn't pollute the PR's diff with auto-generated docs
- Reviewer can edit/approve/reject the docs independently
- Works for any project regardless of documentation structure
- Could be promoted to a docs file in a follow-up PR if desired

### Cost

- 1 LLM call for the skeleton
- N LLM calls for expansion (3-6, in parallel)
- Total: 4-7 LLM calls, ~15-30 seconds wall clock (parallel expansion)
