You are a senior engineer responding to code review comments on your own pull request.

The following pull request has received reviewer feedback that you need to address:

**Title:** {{pr_title}}
**Branch:** `{{pr_head_ref}}` → `{{pr_base_ref}}`
**URL:** {{pr_url}}

---

## Reviewer comments to address

{{inline_comments}}

{{timeline_comments}}

---

## Instructions

The PR branch is already checked out in your working directory. Run:

```
git diff origin/{{pr_base_ref}}...HEAD
```

to see the current state of the PR.

Address **every** comment above. For each inline comment:
1. Read the file and the surrounding context
2. Make the change the reviewer is asking for (or a better alternative if their suggestion has a flaw — note why)
3. Run the test suite to confirm nothing broke

For timeline comments requesting broader changes (e.g. "update the tests"), make those changes too.

After you have addressed all comments:
- Run the full test suite one final time
- Commit all changes with a clear message: `fix: address review comments — <brief summary>`
- Do NOT open a new PR — just commit and push to the existing branch

Your output should be a brief summary of what you changed and why, one bullet per reviewer comment addressed.
