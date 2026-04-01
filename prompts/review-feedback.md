You previously worked on this task:

<original_task>
{{original_task}}
</original_task>

A code reviewer has left the following comments on your changes.
Address ALL of them:

<review_comments>
{{review_comments}}
</review_comments>

Make the necessary changes so that the reviewer's concerns are fully resolved.

After addressing each concern, check whether the same class of issue exists
in sibling code — other derived values, computed properties, or functions
that read from the same data source. Fix proactively rather than waiting for
another review round.

If the reviewer flagged incomplete scope (missing files, routes, or
acceptance criteria), go back to the original task, enumerate everything
that was requested, and complete ALL remaining items.

Run the test suite after making changes to confirm nothing is broken:

```
{{test_command}}
```

Commit your changes with a clear conventional commit message:

```
git add -A && git commit -m "<type>(<scope>): <summary>"
```
