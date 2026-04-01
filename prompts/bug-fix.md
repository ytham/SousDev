Fix the following bug reported in GitHub issue #{{issue_number}}.

## Issue: {{issue_title}}

{{issue_body}}

## Your instructions

Follow this exact sequence — do NOT skip steps:

### Step 1 — Understand the bug

Read the relevant source files to understand what the code is supposed to do
and why it is currently broken. Use the issue description as your guide.

IMPORTANT: Issue descriptions often contain simplified suggestions from
reporters who may not know the full code context. Do NOT blindly follow
their suggested fix. The description tells you WHAT is broken, not
necessarily HOW to fix it. Understand the underlying problem and devise
the correct solution, even if it differs from what was suggested.

If there is not enough information in the description to act upon,
stop here. Write a comment on the issue explaining what additional information
is needed, then exit.

### Step 2 — Write failing tests

Add tests that:
- Specifically target the behaviour described in the bug report
- Currently FAIL (because the bug exists)
- Will PASS once the bug is fixed

Also write **negative tests** that verify existing behaviour which must NOT
change. If the bug report suggests removing a guard or filter, write a test
confirming the guard's intended purpose is still preserved for other cases.

Run the tests now and confirm the positive tests fail and the negative tests pass:

```
{{test_command}}
```

If the positive tests don't fail, revisit Step 1 — they don't capture the bug yet.

### Step 3 — Fix the bug

Make the minimal code change needed to fix the bug without breaking anything else.
Do not refactor unrelated code.

### Step 4 — Trace downstream impact

After making the fix, identify ALL other code paths that depend on or derive
from the data/logic you changed. Ask yourself:

- Are there other derived values, computed properties, or functions that read
  from the same source data?
- Does removing or changing a guard/filter here affect other consumers that
  assumed that guard was in place?
- Could the fix cause a sibling code path to regress?

If you find dependent code, verify each one still behaves correctly and update
it as needed. Add tests for any new edge cases you discover.

### Step 5 — Confirm all tests pass

Run the full test suite:

```
{{test_command}}
```

All tests must pass (including the new ones from Step 2).
If any tests fail, go back to Step 3.

### Step 6 — Done

When all tests pass, stop. Do not add unrelated changes.
