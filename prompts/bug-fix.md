Fix the following bug reported in GitHub issue #{{issue_number}}.

## Issue: {{issue_title}}

{{issue_body}}

## Your instructions

Follow this exact sequence — do NOT skip steps:

### Step 1 — Understand the bug

Read the relevant source files to understand what the code is supposed to do
and why it is currently broken. Use the issue description as your guide.

IMPORTANT: If there is not enough information in the description to act upon,
stop here. Write a comment on the issue explaining what additional information
is needed, then exit.

### Step 2 — Write a failing test

Add one or more tests that:
- Specifically target the behaviour described in the bug report
- Currently FAIL (because the bug exists)
- Will PASS once the bug is fixed

Run the tests now and confirm they fail:

```
{{test_command}}
```

If they don't fail, revisit Step 1 — the tests don't capture the bug yet.

### Step 3 — Fix the bug

Make the minimal code change needed to fix the bug without breaking anything else.
Do not refactor unrelated code.

### Step 4 — Confirm all tests pass

Run the full test suite:

```
{{test_command}}
```

All tests must pass (including the new ones from Step 2).
If any tests fail, go back to Step 3.

### Step 5 — Done

When all tests pass, stop. Do not add unrelated changes.
