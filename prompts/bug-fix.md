Fix the following issue reported in GitHub issue #{{issue_number}}.

## Issue: {{issue_title}}

{{issue_body}}

## Your instructions

Follow this exact sequence — do NOT skip steps:

### Step 1 — Understand the issue

Read the relevant source files to understand what the code is supposed to do
and what needs to change. Use the issue description as your guide.

IMPORTANT: Issue descriptions often contain simplified suggestions from
reporters who may not know the full code context. Do NOT blindly follow
their suggested fix. The description tells you WHAT is broken or needed,
not necessarily HOW to implement it. Understand the underlying problem and
devise the correct solution, even if it differs from what was suggested.

If there is not enough information in the description to act upon,
stop here. Write a comment on the issue explaining what additional information
is needed, then exit.

### Step 2 — Assess the full scope

Before writing any code, re-read the issue carefully and list every item
that needs to be addressed:

- If the issue has acceptance criteria, enumerate each one
- If it lists specific files, routes, components, or TODOs, count them all
- If it references multiple affected locations, identify every one

You must address ALL listed items, not just a subset. Do not defer items
to "follow-up work" unless the issue explicitly says to phase the work.

### Step 3 — Write failing tests

Add tests that:
- Specifically target the behaviour described in the issue
- Currently FAIL (because the issue exists)
- Will PASS once the issue is fixed

Also write **negative tests** that verify existing behaviour which must NOT
change. If the issue suggests removing a guard or filter, write a test
confirming the guard's intended purpose is still preserved for other cases.

Run the tests now and confirm the positive tests fail and the negative tests pass:

```
{{test_command}}
```

If the positive tests don't fail, revisit Step 1 — they don't capture the issue yet.

Note: if the issue is a pure refactor or route rename with no behavioural
change, this step may not apply. Use your judgment.

### Step 4 — Implement the fix

Make the code changes needed to resolve the issue. Apply changes to ALL
affected files and locations identified in Step 2. Do not refactor
unrelated code.

### Step 5 — Trace downstream impact

After making the fix, identify ALL other code paths that depend on or derive
from the data/logic you changed. Ask yourself:

- Are there other derived values, computed properties, or functions that read
  from the same source data?
- Does removing or changing a guard/filter here affect other consumers that
  assumed that guard was in place?
- Could the fix cause a sibling code path to regress?

If you find dependent code, verify each one still behaves correctly and update
it as needed. Add tests for any new edge cases you discover.

### Step 6 — Confirm all tests pass

Run the full test suite:

```
{{test_command}}
```

All tests must pass (including the new ones from Step 3).
If any tests fail, go back to Step 4.

### Step 7 — Verify scope completeness

Re-read the issue one final time. Check every acceptance criterion,
every listed file/route/component, every TODO mentioned. Confirm you
addressed ALL of them, not just a subset. If anything is missing, go back
to Step 4.

### Step 8 — Commit and finish

Commit your changes with a clear conventional commit message that describes
what you did:

```
git add -A && git commit -m "<type>(<scope>): <summary>"
```

Use `fix:` for bug fixes, `feat:` for new functionality, `refactor:` for
restructuring. The summary should describe the change, not the issue number.

When done, stop. Do not add unrelated changes.
