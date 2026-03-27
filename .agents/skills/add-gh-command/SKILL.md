---
name: add-gh-command
description: Add a new GitHub CLI wrapper function to SousDev using the established async Command pattern with JSON parsing and error handling.
---

## When to use

Use this when adding a new function that shells out to the `gh` CLI to interact with
GitHub (e.g. fetching data, posting comments, creating resources).

## Steps

1. **Decide which file** the function belongs in:
   - Issue-related → `src/workflows/github_issues.rs`
   - PR-related → `src/workflows/github_prs.rs`
   - New resource type → consider a new `github_<resource>.rs` module

2. **Write the function** following the established pattern:

```rust
/// <Brief description of what this does.>
///
/// # Arguments
/// * `repo` — "owner/repo" format
/// * `<param>` — <description>
pub async fn <function_name>(repo: &str, <params>) -> Result<ReturnType> {
    let output = Command::new("gh")
        .arg("<subcommand>")
        .arg("<action>")
        .arg("--repo").arg(repo)
        // ... additional flags
        .arg("--json").arg("<fields>")  // for queries
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh <subcommand> <action> failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(Default::default());  // or vec![] for list operations
    }

    let result: ReturnType = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse gh output: {}", e))?;
    Ok(result)
}
```

3. **For `gh api` calls** (when `gh <subcommand>` doesn't cover the need):

```rust
let cmd_str = format!(
    "gh api /repos/{}/pulls/{}/comments --jq '[.[] | {{id: .id, body: .body}}]'",
    repo, pr_number
);
let output = Command::new("sh").arg("-c").arg(&cmd_str).output().await?;
```

Use `sh -c` for `gh api` commands because they need shell expansion for the `--jq` flag.

4. **For mutation commands** (POST/create/comment):
   - Use `shell_escape()` for any user-provided string values
   - Check `!output.status.success()` and return the stderr as the error
   - Return `Result<()>` (no parsed output needed)

5. **Add unit tests** — since these call real `gh`, tests should verify:
   - The function signature and types are correct (sync tests on types)
   - Helper functions like `shell_escape` work correctly
   - Data type defaults and accessor methods work

   Do NOT write tests that actually call `gh` — all CLI tests are mocked at the
   integration level in the executor tests.

6. **If the return type is new**, define it in the same file with serde derives:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyNewType {
    pub id: u64,
    pub name: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}
```

Use `#[serde(rename = "camelCase")]` for fields that come from GitHub's JSON API.
Use `#[serde(skip, default)]` for fields you populate after deserialization (like `repo`).

## Conventions

- All gh commands use `tokio::process::Command`
- Timeouts are not set on individual commands (the parent stage handles timeouts)
- Error messages always include the command that failed: `"gh pr list failed: {stderr}"`
- Empty stdout → return empty vec or default, not an error
- `"null"` stdout (from `--jq` on empty results) → treat as empty
