Execute the following approved implementation plan for issue #{{issue_number}}.

## Issue: {{issue_title}}

{{issue_body}}

## Approved plan

{{plan}}

## Your instructions

The plan above was reviewed and approved by a human engineer. Follow it
precisely — do NOT skip any steps.

### Step 1 — Verify your starting point

Run `git log --oneline -3` and `git diff --stat` to confirm the workspace
is clean and you're on the correct branch.

### Step 2 — Implement every step in the plan

Work through each numbered item in the plan's **Approach** section:
- Modify every file listed in **Files to modify**
- Reuse the patterns identified in **Key reuse points**
- Do not refactor unrelated code

### Step 3 — Write tests

Follow the **Testing strategy** from the plan:
- Write tests that verify the new behaviour
- Write negative tests confirming existing behaviour is preserved
- If the plan mentions specific scenarios, test every one

### Step 4 — Trace downstream impact

After making changes, ask yourself:
- Are there other derived values or functions that depend on what I changed?
- Does removing or changing a guard/filter affect other consumers?
- Could my changes cause a sibling code path to regress?

Fix any issues proactively.

### Step 5 — Run the test suite

```
{{test_command}}
```

All tests must pass. If any fail, fix them before proceeding.

### Step 6 — Verify scope completeness

Re-read the issue and the plan one final time. Confirm you addressed:
- Every item in the plan's Approach section
- Every file in the plan's Files to modify section
- Every acceptance criterion from the issue

If anything is missing, go back to Step 2.

### Step 7 — Commit

Commit your changes with a clear conventional commit message:

```
git add -A
git commit -m "<type>(<scope>): <summary>"
```

Use `fix:` for bug fixes, `feat:` for new functionality, `refactor:` for
restructuring. The summary should describe the change, not the issue number.

Do NOT commit the plan file deletion — that will be handled separately.
