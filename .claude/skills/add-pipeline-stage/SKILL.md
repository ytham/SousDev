---
name: add-pipeline-stage
description: Add a new pipeline stage to SousDev following the established Stage trait pattern, mod.rs registration, and StageContext wiring.
---

## When to use

Use this when adding a new stage to the pipeline (e.g. a new post-processing step, a new
GitHub interaction, a new validation check). Every stage follows the same pattern.

## Steps

1. **Create the stage file** at `src/pipelines/stages/<name>.rs`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use crate::pipelines::stage::{Stage, StageContext};

pub struct MyNewStage;

#[async_trait]
impl Stage for MyNewStage {
    fn name(&self) -> &str { "<name>" }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        if ctx.is_aborted() {
            return Err(anyhow::anyhow!("MyNewStage aborted."));
        }

        // Read from ctx, do work, write results back to ctx.
        // Return Ok(()) for business-logic failures (record outcome in ctx).
        // Only Err for unrecoverable errors.

        Ok(())
    }
}
```

2. **Register in `src/pipelines/stages/mod.rs`** — add `pub mod <name>;`

3. **Add any new output fields to `StageContext`** in `src/pipelines/stage.rs`:
   - New fields must be `Option<T>` and default to `None`
   - Add them in the appropriate section (stage outputs, PR review, PR response)
   - If the field is serialized in `PipelineResult`, also add it to `stores.rs`

4. **Wire into the executor** in `src/pipelines/executor.rs`:
   - Import the stage: `use crate::pipelines::stages::<name>::MyNewStage;`
   - Call it in the correct position within the stage sequence of the relevant mode
   - Pass `&mut ctx` and handle the result

5. **Add to `StageContext` construction** — if you added fields to `StageContext`, update
   `make_base_ctx()` in executor.rs and every test file that constructs a `StageContext`
   (trigger.rs, parse.rs, pr_checkout.rs tests).

6. **Write tests** in the stage file using `#[cfg(test)] mod tests`:
   - Test the happy path
   - Test the aborted case
   - Test error/edge cases
   - Use a mock `StageContext` with the minimum fields needed

7. **Run `cargo test` and `cargo clippy`** — both must pass clean.

## Checklist

- [ ] Stage file created with `Stage` trait impl
- [ ] Registered in `stages/mod.rs`
- [ ] New `StageContext` fields added (if any)
- [ ] Executor updated to call the stage
- [ ] All `StageContext` construction sites updated
- [ ] Tests written (happy path, abort, edge cases)
- [ ] `cargo test` passes
- [ ] `cargo clippy` clean
