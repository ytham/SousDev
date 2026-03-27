Write a GitHub pull request title and description for the following change.

## Original task / issue

{{task}}

## What the agent did

{{agent_answer}}

## Git diff

```diff
{{diff}}
```

---

Respond in exactly this format:

TITLE: <one-line PR title, max 72 chars, no backticks>

BODY:
<markdown PR description with these sections:

## Summary
<2–4 bullet points describing what changed>

## Why
<1–2 sentences explaining the motivation>

## Notes for reviewer
<bullet points for code areas that the reviewer should take extra care when reviewing and why>

## Testing

### Test cases created
<list of all test cases created>

### Verification
<how to verify the fix works>
>
