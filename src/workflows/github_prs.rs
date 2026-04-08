use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A GitHub pull request returned by `gh pr list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubPR {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(rename = "headRefOid")]
    pub head_ref_oid: String,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: String,
    pub author: PRAuthor,
    #[serde(default)]
    pub labels: Vec<PRLabel>,
    #[serde(rename = "reviewDecision", default)]
    pub review_decision: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    /// Logins of users whose review has been requested on this PR.
    #[serde(default)]
    pub requested_reviewers: Vec<String>,
    /// Team slugs whose review has been requested on this PR.
    #[serde(default)]
    pub requested_teams: Vec<String>,
    /// Logins of users assigned to this PR.
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Number of lines added in this PR.
    #[serde(default)]
    pub additions: u64,
    /// Number of lines deleted in this PR.
    #[serde(default)]
    pub deletions: u64,
    /// Populated after fetch; not part of the JSON payload.
    #[serde(skip, default)]
    pub repo: String,
}

impl GitHubPR {
    /// Returns the body text, or an empty string if `None`.
    pub fn body_str(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }
}

/// The author of a pull request.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PRAuthor {
    pub login: String,
}

/// A label attached to a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PRLabel {
    pub name: String,
}

/// A timeline (issue) comment on a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PRComment {
    pub id: u64,
    pub login: String,
    pub body: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// An inline review comment on a pull request diff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineReviewComment {
    pub id: u64,
    pub login: String,
    pub body: String,
    pub path: String,
    pub line: Option<u64>,
    #[serde(rename = "diffHunk")]
    pub diff_hunk: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "inReplyToId")]
    pub in_reply_to_id: Option<u64>,
}

/// Options for [`fetch_github_prs`].
pub struct FetchPRsOptions {
    /// Target repository.  If `None`, auto-detected via `gh repo view`.
    pub repo: Option<String>,
    /// Additional search query terms appended to the default filter.
    pub search: Option<String>,
    /// Maximum number of PRs to return (default 10).
    pub limit: Option<usize>,
    /// Use a raw search query instead of the reviewer-specific dual search.
    /// When `true`, only `search` + `is:open` is used (no `user-review-requested`
    /// or `assignee` searches).  Used by the pr-responder which searches by author.
    pub raw_search: bool,
}

/// Fetch open pull requests where the authenticated user is a requested reviewer.
///
/// Uses `user-review-requested:@me` to match only individual review requests
/// (not team auto-assignments).  Falls back to `review-requested:@me` when
/// combined with an assignee search.
pub async fn fetch_github_prs(options: &FetchPRsOptions) -> Result<Vec<GitHubPR>> {
    let repo = match &options.repo {
        Some(r) => r.clone(),
        None => super::github_issues::detect_repo().await?,
    };

    let extra_search = options
        .search
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let limit = options.limit.unwrap_or(100);
    let json_fields = "number,title,body,url,headRefName,headRefOid,baseRefName,author,labels,reviewDecision,reviewRequests,assignees,additions,deletions,createdAt,updatedAt";

    // Raw search mode: use the provided search query directly (for pr-responder).
    if options.raw_search {
        let search = if extra_search.is_empty() {
            "is:open".to_string()
        } else {
            format!("{} is:open", extra_search)
        };

        let output = Command::new("gh")
            .arg("pr").arg("list")
            .arg("--repo").arg(&repo)
            .arg("--search").arg(&search)
            .arg("--limit").arg(limit.to_string())
            .arg("--json").arg(json_fields)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("gh pr list failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return Ok(vec![]);
        }

        let raw: Vec<RawGhPR> = serde_json::from_str(&stdout)
            .map_err(|e| anyhow::anyhow!("Failed to parse gh pr list: {}", e))?;

        return Ok(raw
            .into_iter()
            .map(|r| map_raw_pr(r, &repo))
            .collect());
    }

    // Reviewer search mode (default): dual search.
    // Search 1: PRs where the user is individually requested as reviewer.
    let search1 = if extra_search.is_empty() {
        "user-review-requested:@me is:open".to_string()
    } else {
        format!("user-review-requested:@me is:open {}", extra_search)
    };

    let output1 = Command::new("gh")
        .arg("pr").arg("list")
        .arg("--repo").arg(&repo)
        .arg("--search").arg(&search1)
        .arg("--limit").arg(limit.to_string())
        .arg("--json").arg(json_fields)
        .output()
        .await?;

    if !output1.status.success() {
        let stderr = String::from_utf8_lossy(&output1.stderr);
        return Err(anyhow::anyhow!("gh pr list failed: {}", stderr));
    }

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    let mut raw: Vec<RawGhPR> = if stdout1.trim().is_empty() {
        vec![]
    } else {
        serde_json::from_str(&stdout1)
            .map_err(|e| anyhow::anyhow!("Failed to parse gh pr list: {}", e))?
    };

    // Search 2: PRs assigned to the user (may overlap with search 1).
    let search2 = if extra_search.is_empty() {
        "assignee:@me is:open".to_string()
    } else {
        format!("assignee:@me is:open {}", extra_search)
    };

    let output2 = Command::new("gh")
        .arg("pr").arg("list")
        .arg("--repo").arg(&repo)
        .arg("--search").arg(&search2)
        .arg("--limit").arg(limit.to_string())
        .arg("--json").arg(json_fields)
        .output()
        .await;

    if let Ok(output2) = output2 {
        if output2.status.success() {
            let stdout2 = String::from_utf8_lossy(&output2.stdout);
            if !stdout2.trim().is_empty() {
                let raw2: Vec<RawGhPR> = serde_json::from_str(&stdout2).unwrap_or_default();
                // Merge, deduplicating by PR number.
                let existing: std::collections::HashSet<u64> =
                    raw.iter().map(|r| r.number).collect();
                for r in raw2 {
                    if !existing.contains(&r.number) {
                        raw.push(r);
                    }
                }
            }
        }
    }

    // Search 3: broader review-requested:@me (includes team requests).
    // Catches cases where user-review-requested:@me misses due to
    // GitHub search index delays.
    let search3 = if extra_search.is_empty() {
        "review-requested:@me is:open".to_string()
    } else {
        format!("review-requested:@me is:open {}", extra_search)
    };

    let output3 = Command::new("gh")
        .arg("pr").arg("list")
        .arg("--repo").arg(&repo)
        .arg("--search").arg(&search3)
        .arg("--limit").arg(limit.to_string())
        .arg("--json").arg(json_fields)
        .output()
        .await;

    if let Ok(output3) = output3 {
        if output3.status.success() {
            let stdout3 = String::from_utf8_lossy(&output3.stdout);
            if !stdout3.trim().is_empty() {
                let raw3: Vec<RawGhPR> = serde_json::from_str(&stdout3).unwrap_or_default();
                let existing: std::collections::HashSet<u64> =
                    raw.iter().map(|r| r.number).collect();
                for r in raw3 {
                    if !existing.contains(&r.number) {
                        raw.push(r);
                    }
                }
            }
        }
    }

    // Search 4: PRs already reviewed by the user (reviewed-by:@me).
    // Once a review is submitted, GitHub removes the user from
    // `reviewRequests`, so searches 1-3 no longer find the PR.
    // This search ensures reviewed PRs stay visible in the Info pane
    // while they remain open.
    let search4 = if extra_search.is_empty() {
        "reviewed-by:@me is:open".to_string()
    } else {
        format!("reviewed-by:@me is:open {}", extra_search)
    };

    let output4 = Command::new("gh")
        .arg("pr").arg("list")
        .arg("--repo").arg(&repo)
        .arg("--search").arg(&search4)
        .arg("--limit").arg(limit.to_string())
        .arg("--json").arg(json_fields)
        .output()
        .await;

    if let Ok(output4) = output4 {
        if output4.status.success() {
            let stdout4 = String::from_utf8_lossy(&output4.stdout);
            if !stdout4.trim().is_empty() {
                let raw4: Vec<RawGhPR> = serde_json::from_str(&stdout4).unwrap_or_default();
                let existing: std::collections::HashSet<u64> =
                    raw.iter().map(|r| r.number).collect();
                for r in raw4 {
                    if !existing.contains(&r.number) {
                        raw.push(r);
                    }
                }
            }
        }
    }

    Ok(raw
        .into_iter()
        .map(|r| map_raw_pr(r, &repo))
        .collect())
}

/// Map a raw GitHub PR response to the internal representation.
fn map_raw_pr(r: RawGhPR, repo: &str) -> GitHubPR {
    GitHubPR {
        number: r.number,
        title: r.title,
        body: r.body,
        url: r.url,
        head_ref_name: r.head_ref_name,
        head_ref_oid: r.head_ref_oid,
        base_ref_name: r.base_ref_name,
        author: r.author.unwrap_or_default(),
        labels: r.labels.unwrap_or_default(),
        review_decision: r.review_decision.unwrap_or_default(),
        requested_reviewers: r
            .review_requests
            .iter()
            .filter_map(|rr| rr.login.clone())
            .collect(),
        requested_teams: r
            .review_requests
            .iter()
            .filter_map(|rr| rr.slug.clone())
            .collect(),
        assignees: r
            .assignees
            .iter()
            .map(|a| a.login.clone())
            .collect(),
        additions: r.additions,
        deletions: r.deletions,
        created_at: r.created_at,
        updated_at: r.updated_at,
        repo: repo.to_string(),
    }
}

/// Fetch timeline (issue-level) comments for a pull request.
///
/// If `after_id` is provided, only comments with an `id` greater than that
/// value are returned.
///
/// Uses `--paginate` to ensure all comments are returned even when
/// the PR has more than 30 comments (GitHub's default page size).
pub async fn fetch_pr_comments(
    repo: &str,
    pr_number: u64,
    after_id: Option<u64>,
) -> Result<Vec<PRComment>> {
    let endpoint = format!("/repos/{}/issues/{}/comments?per_page=100", repo, pr_number);
    let jq = "[.[] | {id: .id, login: .user.login, body: .body, createdAt: .created_at}]";
    let output = Command::new("gh")
        .arg("api")
        .arg("--paginate")
        .arg(&endpoint)
        .arg("--jq")
        .arg(jq)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "gh api {} failed: {}",
            endpoint.split('?').next().unwrap_or(&endpoint),
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() || stdout.trim() == "null" {
        return Ok(vec![]);
    }

    let all: Vec<PRComment> = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse PR comments: {}", e))?;

    if let Some(cursor) = after_id {
        Ok(all.into_iter().filter(|c| c.id > cursor).collect())
    } else {
        Ok(all)
    }
}

/// Fetch PR review bodies (submitted reviews with a body comment).
///
/// These are reviews submitted via "Submit review" on GitHub — they have
/// a body but are NOT inline diff comments or timeline comments.  They
/// live under the `/pulls/{pr}/reviews` API.
///
/// If `after_id` is provided, only reviews with an `id` greater than that
/// are returned.
///
/// Uses `--paginate` to ensure all reviews are returned.
pub async fn fetch_pr_review_comments(
    repo: &str,
    pr_number: u64,
    after_id: Option<u64>,
) -> Result<Vec<PRComment>> {
    let endpoint = format!("/repos/{}/pulls/{}/reviews?per_page=100", repo, pr_number);
    let jq = r#"[.[] | select(.body != null and .body != "") | {id: .id, login: .user.login, body: .body, createdAt: .submitted_at}]"#;
    let output = Command::new("gh")
        .arg("api")
        .arg("--paginate")
        .arg(&endpoint)
        .arg("--jq")
        .arg(jq)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "gh api {} failed: {}",
            endpoint.split('?').next().unwrap_or(&endpoint),
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() || stdout.trim() == "null" {
        return Ok(vec![]);
    }

    let all: Vec<PRComment> = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse PR review comments: {}", e))?;

    if let Some(cursor) = after_id {
        Ok(all.into_iter().filter(|c| c.id > cursor).collect())
    } else {
        Ok(all)
    }
}

/// Fetch inline review comments (diff-level) for a pull request.
///
/// Only root comments (those without an `in_reply_to_id`) are returned.
/// If `after_id` is provided, only comments with an `id` greater than that
/// value are included.
///
/// Uses `--paginate` to ensure all comments are returned.
pub async fn fetch_inline_review_comments(
    repo: &str,
    pr_number: u64,
    after_id: Option<u64>,
) -> Result<Vec<InlineReviewComment>> {
    let endpoint = format!("/repos/{}/pulls/{}/comments?per_page=100", repo, pr_number);
    let jq = "[.[] | {id: .id, login: .user.login, body: .body, path: .path, line: .line, diffHunk: .diff_hunk, createdAt: .created_at, inReplyToId: .in_reply_to_id}]";
    let output = Command::new("gh")
        .arg("api")
        .arg("--paginate")
        .arg(&endpoint)
        .arg("--jq")
        .arg(jq)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "gh api {} failed: {}",
            endpoint.split('?').next().unwrap_or(&endpoint),
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() || stdout.trim() == "null" {
        return Ok(vec![]);
    }

    let all: Vec<InlineReviewComment> = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse inline review comments: {}", e))?;

    // Only root comments (no in_reply_to_id).
    let roots: Vec<InlineReviewComment> =
        all.into_iter().filter(|c| c.in_reply_to_id.is_none()).collect();

    if let Some(cursor) = after_id {
        Ok(roots.into_iter().filter(|c| c.id > cursor).collect())
    } else {
        Ok(roots)
    }
}

/// Fetch inline comments from a specific PR review.
///
/// This returns only the inline (diff-level) comments that were part of a
/// particular submitted review, identified by `review_id`.  Useful for
/// fetching all feedback from an approval review.
///
/// Uses `--paginate` to ensure all comments are returned.
pub async fn fetch_review_inline_comments(
    repo: &str,
    pr_number: u64,
    review_id: u64,
) -> Result<Vec<InlineReviewComment>> {
    let endpoint = format!(
        "/repos/{}/pulls/{}/reviews/{}/comments?per_page=100",
        repo, pr_number, review_id
    );
    let jq = "[.[] | {id: .id, login: .user.login, body: .body, path: .path, line: .line, diffHunk: .diff_hunk, createdAt: .created_at, inReplyToId: .in_reply_to_id}]";
    let output = Command::new("gh")
        .arg("api")
        .arg("--paginate")
        .arg(&endpoint)
        .arg("--jq")
        .arg(jq)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "gh api {} failed: {}",
            endpoint.split('?').next().unwrap_or(&endpoint),
            stderr
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() || stdout.trim() == "null" {
        return Ok(vec![]);
    }

    let all: Vec<InlineReviewComment> = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse review inline comments: {}", e))?;

    Ok(all)
}

/// Post an inline review comment on a specific diff line.
///
/// Uses the GitHub REST API directly via `reqwest` (not `gh` CLI) to avoid
/// CLI parameter encoding issues.  Sends proper JSON body with
/// `subject_type: "line"`.
pub async fn post_inline_comment(
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    path: &str,
    line: u64,
    body: &str,
) -> Result<()> {
    // Get the GitHub token from gh CLI auth.
    let token = get_github_token().await?;

    let url = format!(
        "https://api.github.com/repos/{}/pulls/{}/comments",
        repo, pr_number
    );
    let json_body = serde_json::json!({
        "body": body,
        "commit_id": head_sha,
        "path": path,
        "line": line,
        "side": "RIGHT"
    });

    let client = reqwest::Client::new();
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("Content-Type", "application/json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "SousDev")
        .json(&json_body)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Failed to post inline comment on {}:{} — HTTP {} — {}",
            path, line, status, body
        ));
    }
    Ok(())
}

/// Get the GitHub auth token from the `gh` CLI.
async fn get_github_token() -> Result<String> {
    // Check GITHUB_TOKEN env var first.
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    // Fall back to `gh auth token`.
    let output = Command::new("gh")
        .arg("auth")
        .arg("token")
        .output()
        .await?;
    if !output.status.success() {
        return Err(anyhow::anyhow!("Failed to get GitHub token from gh CLI"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Post a top-level (summary) comment on a pull request.
///
/// Returns the comment ID on success (useful for later deletion/editing).
pub async fn post_summary_comment(repo: &str, pr_number: u64, body: &str) -> Result<u64> {
    let endpoint = format!("/repos/{}/issues/{}/comments", repo, pr_number);
    let output = Command::new("gh")
        .arg("api")
        .arg("--method").arg("POST")
        .arg(&endpoint)
        .arg("-f").arg(format!("body={}", body))
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to post summary comment: {}", stderr));
    }
    // Parse the comment ID from the JSON response.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let id = serde_json::from_str::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|v| v["id"].as_u64())
        .unwrap_or(0);
    Ok(id)
}

/// Delete a comment by ID.
pub async fn delete_comment(repo: &str, comment_id: u64) -> Result<()> {
    let endpoint = format!("/repos/{}/issues/comments/{}", repo, comment_id);
    let output = Command::new("gh")
        .arg("api")
        .arg("--method").arg("DELETE")
        .arg(&endpoint)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to delete comment: {}", stderr));
    }
    Ok(())
}

/// Reply to an existing inline review comment.
pub async fn reply_to_inline_comment(repo: &str, comment_id: u64, body: &str) -> Result<()> {
    let endpoint = format!("/repos/{}/pulls/comments/{}/replies", repo, comment_id);
    let output = Command::new("gh")
        .arg("api")
        .arg("--method").arg("POST")
        .arg(&endpoint)
        .arg("-f").arg(format!("body={}", body))
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to reply to inline comment: {}", stderr));
    }
    Ok(())
}

/// Fetch the state of a PR (`"OPEN"`, `"CLOSED"`, or `"MERGED"`).
pub async fn fetch_pr_state(repo: &str, pr_number: u64) -> Result<String> {
    let output = Command::new("gh")
        .arg("pr")
        .arg("view")
        .arg(pr_number.to_string())
        .arg("--repo")
        .arg(repo)
        .arg("--json")
        .arg("state")
        .arg("--jq")
        .arg(".state")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Failed to fetch PR state: {}", stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Detect the login of the currently authenticated GitHub user.
pub async fn detect_github_login() -> Result<String> {
    let output = Command::new("gh")
        .arg("api")
        .arg("user")
        .arg("--jq")
        .arg(".login")
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh api user failed: {}", stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Single-quote escape a string for use in a shell command.
#[cfg(test)]
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Internal raw deserialization types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawGhPR {
    number: u64,
    title: String,
    body: Option<String>,
    url: String,
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(rename = "baseRefName")]
    base_ref_name: String,
    author: Option<PRAuthor>,
    labels: Option<Vec<PRLabel>>,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(rename = "reviewRequests", default)]
    review_requests: Vec<RawReviewRequest>,
    #[serde(default)]
    assignees: Vec<PRAuthor>,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}

/// A review request entry from `gh pr list --json reviewRequests`.
///
/// Each entry has a `__typename` of `"User"` or `"Team"`, plus a `login`
/// (for users) or `name`/`slug` (for teams).  We only extract `login`.
#[derive(Deserialize)]
struct RawReviewRequest {
    login: Option<String>,
    /// Team slug (e.g. `"org/eng"`).  Present when `__typename == "Team"`.
    slug: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn test_shell_escape_with_single_quote() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_pr_body_str_none() {
        let pr = GitHubPR {
            number: 1,
            title: "t".into(),
            body: None,
            url: "u".into(),
            head_ref_name: "h".into(),
            head_ref_oid: "o".into(),
            base_ref_name: "b".into(),
            author: PRAuthor { login: "a".into() },
            labels: vec![],
            review_decision: "".into(),
            created_at: "c".into(),
            updated_at: "u".into(),
            repo: "r".into(),
            requested_reviewers: vec![],
            requested_teams: vec![],
            assignees: vec![],
            additions: 0,
            deletions: 0,
        };
        assert_eq!(pr.body_str(), "");
    }

    #[test]
    fn test_pr_body_str_some() {
        let pr = GitHubPR {
            number: 1,
            title: "t".into(),
            body: Some("desc".into()),
            url: "u".into(),
            head_ref_name: "h".into(),
            head_ref_oid: "o".into(),
            base_ref_name: "b".into(),
            author: PRAuthor { login: "a".into() },
            labels: vec![],
            review_decision: "".into(),
            created_at: "c".into(),
            updated_at: "u".into(),
            repo: "r".into(),
            requested_reviewers: vec![],
            requested_teams: vec![],
            assignees: vec![],
            additions: 0,
            deletions: 0,
        };
        assert_eq!(pr.body_str(), "desc");
    }

    // ── Additional tests ─────────────────────────────────────────────────────

    #[test]
    fn test_pr_body_str_with_value() {
        let pr = GitHubPR {
            number: 42,
            title: "feat: add metrics".into(),
            body: Some("This PR adds usage metrics tracking.".into()),
            url: "https://github.com/o/r/pull/42".into(),
            head_ref_name: "feature/metrics".into(),
            head_ref_oid: "abc1234".into(),
            base_ref_name: "main".into(),
            author: PRAuthor { login: "alice".into() },
            labels: vec![PRLabel { name: "enhancement".into() }],
            review_decision: "REVIEW_REQUIRED".into(),
            created_at: "2025-01-01".into(),
            updated_at: "2025-01-02".into(),
            repo: "o/r".into(),
            requested_reviewers: vec![],
            requested_teams: vec![],
            assignees: vec![],
            additions: 0,
            deletions: 0,
        };
        assert_eq!(pr.body_str(), "This PR adds usage metrics tracking.");
    }

    #[test]
    fn test_pr_body_str_without_value() {
        let pr = GitHubPR {
            number: 1,
            title: "t".into(),
            body: None,
            url: "u".into(),
            head_ref_name: "h".into(),
            head_ref_oid: "o".into(),
            base_ref_name: "b".into(),
            author: PRAuthor::default(),
            labels: vec![],
            review_decision: String::new(),
            created_at: "c".into(),
            updated_at: "u".into(),
            repo: "r".into(),
            requested_reviewers: vec![],
            requested_teams: vec![],
            assignees: vec![],
            additions: 0,
            deletions: 0,
        };
        assert_eq!(pr.body_str(), "");
    }

    #[test]
    fn test_shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn test_shell_escape_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn test_shell_escape_special_chars() {
        assert_eq!(
            shell_escape("/path/to/file with spaces"),
            "'/path/to/file with spaces'"
        );
    }

    #[test]
    fn test_inline_comment_defaults() {
        let comment = InlineReviewComment {
            id: 1,
            login: "user".into(),
            body: "fix this".into(),
            path: "src/main.rs".into(),
            line: None,
            diff_hunk: None,
            created_at: "2025-01-01".into(),
            in_reply_to_id: None,
        };
        assert!(comment.line.is_none());
        assert!(comment.diff_hunk.is_none());
        assert!(comment.in_reply_to_id.is_none());
    }

    #[test]
    fn test_pr_author_default() {
        let author = PRAuthor::default();
        assert_eq!(author.login, "");
    }

    #[test]
    fn test_pr_label_name() {
        let label = PRLabel { name: "bug".into() };
        assert_eq!(label.name, "bug");
    }

    #[test]
    fn test_pr_comment_fields() {
        let comment = PRComment {
            id: 42,
            login: "reviewer".into(),
            body: "LGTM".into(),
            created_at: "2025-06-01T12:00:00Z".into(),
        };
        assert_eq!(comment.id, 42);
        assert_eq!(comment.login, "reviewer");
        assert_eq!(comment.body, "LGTM");
    }

    // ── map_raw_pr ────────────────────────────────────────────────────────

    #[test]
    fn test_map_raw_pr_extracts_reviewers_and_teams() {
        let raw = RawGhPR {
            number: 99,
            title: "Test PR".into(),
            body: Some("Body".into()),
            url: "https://github.com/o/r/pull/99".into(),
            head_ref_name: "feat".into(),
            head_ref_oid: "abc123".into(),
            base_ref_name: "main".into(),
            author: Some(PRAuthor { login: "alice".into() }),
            labels: Some(vec![PRLabel { name: "bug".into() }]),
            review_decision: Some("REVIEW_REQUIRED".into()),
            review_requests: vec![
                RawReviewRequest { login: Some("bob".into()), slug: None },
                RawReviewRequest { login: None, slug: Some("org/eng".into()) },
            ],
            assignees: vec![PRAuthor { login: "carol".into() }],
            additions: 42,
            deletions: 10,
            created_at: "2025-01-01".into(),
            updated_at: "2025-01-02".into(),
        };
        let pr = map_raw_pr(raw, "o/r");
        assert_eq!(pr.number, 99);
        assert_eq!(pr.requested_reviewers, vec!["bob"]);
        assert_eq!(pr.requested_teams, vec!["org/eng"]);
        assert_eq!(pr.assignees, vec!["carol"]);
        assert_eq!(pr.repo, "o/r");
        assert_eq!(pr.author.login, "alice");
        assert_eq!(pr.labels.len(), 1);
    }

    #[test]
    fn test_map_raw_pr_handles_missing_fields() {
        let raw = RawGhPR {
            number: 1,
            title: "t".into(),
            body: None,
            url: "u".into(),
            head_ref_name: "h".into(),
            head_ref_oid: "o".into(),
            base_ref_name: "b".into(),
            author: None,
            labels: None,
            review_decision: None,
            review_requests: vec![],
            assignees: vec![],
            additions: 0,
            deletions: 0,
            created_at: "c".into(),
            updated_at: "u".into(),
        };
        let pr = map_raw_pr(raw, "r");
        assert_eq!(pr.author.login, "");
        assert!(pr.labels.is_empty());
        assert!(pr.requested_reviewers.is_empty());
        assert!(pr.requested_teams.is_empty());
        assert!(pr.assignees.is_empty());
    }
}
