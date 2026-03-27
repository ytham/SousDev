---
name: cargo-fix-cycle
description: Run the SousDev build-test-clippy cycle and systematically fix all errors until everything passes clean.
---

## When to use

Use this after making code changes to ensure the project compiles, all tests pass, and
clippy is clean. This is required before every commit.

## Steps

1. **Build first** — catch type errors and missing imports before running tests:

```bash
cargo build 2>&1 | grep "^error" | head -20
```

If there are errors, fix them one at a time starting from the first. Rebuild after each fix.
Do not move to step 2 until `cargo build` produces zero errors.

2. **Run all tests**:

```bash
cargo test --lib 2>&1 | tail -10
```

The last line must show `0 failed`. If tests fail:
- Read the failure output carefully — it shows expected vs actual values
- Fix the root cause, not the test (unless the test expectation is wrong)
- Re-run only the failing test to iterate quickly: `cargo test <test_name> -- --nocapture`
- After fixing, run the full suite again

3. **Run clippy**:

```bash
cargo clippy 2>&1 | grep "^warning\|^error" | head -20
```

Fix any warnings. Common ones in this project:
- Unused imports → remove them
- Unnecessary `clone()` → use references
- `&String` in function args → use `&str`
- Missing docs on public items → add `///` doc comments

4. **Verify final state**:

```bash
cargo test 2>&1 | grep "test result"
```

Must show: `test result: ok. N passed; 0 failed; 0 ignored`

## Key numbers

- Minimum test count: 275 (current count — never go below this)
- Clippy warnings allowed: 0
- Build errors allowed: 0

## Common pitfalls

- **Adding a field to `StageContext`**: You must also add it to `make_base_ctx()` in
  executor.rs AND to every test file that constructs a `StageContext` manually
  (trigger.rs, parse.rs, pr_checkout.rs).
- **Adding a field to `PipelineResult`**: Mark it `#[serde(skip_serializing_if = "Option::is_none")]`
  if optional, or add a `Default` value.
- **Adding a field to `ResolvedPrompts`**: Update `build_resolved_prompts()` in executor.rs
  AND `DEFAULT_RESOLVED_PROMPTS` equivalent in test helpers.
