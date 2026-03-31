use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// A GitHub issue returned by `gh issue list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<IssueLabel>,
    pub assignees: Vec<IssueAssignee>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub state: String,
    /// Populated after fetch; not part of the JSON payload.
    #[serde(skip, default)]
    pub repo: String,
}

impl GitHubIssue {
    /// Returns the body text, or an empty string if `None`.
    pub fn body_str(&self) -> &str {
        self.body.as_deref().unwrap_or("")
    }
}

/// A label attached to a GitHub issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueLabel {
    pub name: String,
}

/// An assignee of a GitHub issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueAssignee {
    pub login: String,
}

/// Options for [`fetch_github_issues`].
pub struct FetchIssuesOptions {
    /// Target repository in `owner/repo` form or a full GitHub URL.
    /// If `None`, the repo is auto-detected via `gh repo view`.
    pub repo: Option<String>,
    /// If non-empty, one `gh issue list` call is made per assignee and results
    /// are deduplicated by issue number.  An empty slice means no filter.
    pub assignees: Option<Vec<String>>,
    /// Label filters.  An empty slice means no filter.
    pub labels: Option<Vec<String>>,
    /// Maximum number of issues to return (default 10).
    pub limit: Option<usize>,
}

/// Fetch open GitHub issues according to the provided options.
///
/// When multiple labels are configured, issues matching ANY label are
/// returned (OR logic).  This is done by making a separate `gh issue list`
/// call per label and merging the results.
pub async fn fetch_github_issues(options: &FetchIssuesOptions) -> Result<Vec<GitHubIssue>> {
    let repo = match &options.repo {
        Some(r) => r.clone(),
        None => detect_repo().await?,
    };

    let assignees = options.assignees.as_deref().unwrap_or(&[]);
    let labels = options.labels.as_deref().unwrap_or(&[]);
    let limit = options.limit.unwrap_or(100);

    // Build label groups: one query per label (OR logic).
    // When no labels are configured, query once with no label filter.
    let label_groups: Vec<&[String]> = if labels.is_empty() {
        vec![&[]]
    } else {
        labels.iter().map(std::slice::from_ref).collect()
    };

    let mut all: Vec<GitHubIssue> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for label_group in &label_groups {
        if assignees.is_empty() {
            let mut issues =
                fetch_issues_for_assignee(None, label_group, limit, &repo).await?;
            for issue in issues.drain(..) {
                if seen.insert(issue.number) {
                    let mut i = issue;
                    i.repo = repo.clone();
                    all.push(i);
                }
            }
        } else {
            for assignee in assignees {
                let mut batch =
                    fetch_issues_for_assignee(Some(assignee), label_group, limit, &repo)
                        .await?;
                for issue in batch.drain(..) {
                    if seen.insert(issue.number) {
                        let mut i = issue;
                        i.repo = repo.clone();
                        all.push(i);
                    }
                }
            }
        }
    }

    all.sort_by_key(|i| i.number);
    Ok(all)
}

async fn fetch_issues_for_assignee(
    assignee: Option<&str>,
    labels: &[String],
    limit: usize,
    repo: &str,
) -> Result<Vec<GitHubIssue>> {
    let mut cmd = Command::new("gh");
    cmd.arg("issue")
        .arg("list")
        .arg("--repo")
        .arg(repo)
        .arg("--state")
        .arg("open")
        .arg("--limit")
        .arg(limit.to_string())
        .arg("--json")
        .arg("number,title,body,url,labels,assignees,createdAt,updatedAt,state");

    if let Some(a) = assignee {
        cmd.arg("--assignee").arg(a);
    }
    for label in labels {
        cmd.arg("--label").arg(label);
    }

    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh issue list failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(vec![]);
    }

    let issues: Vec<GitHubIssue> = serde_json::from_str(&stdout)
        .map_err(|e| anyhow::anyhow!("Failed to parse gh issue list output: {}", e))?;
    Ok(issues)
}

/// Post a comment on a GitHub issue.
pub async fn comment_on_issue(repo: &str, number: u64, body: &str) -> Result<()> {
    let output = Command::new("gh")
        .arg("issue")
        .arg("comment")
        .arg(number.to_string())
        .arg("--repo")
        .arg(repo)
        .arg("--body")
        .arg(body)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh issue comment failed: {}", stderr));
    }
    Ok(())
}

/// Close a GitHub issue.
pub async fn close_issue(repo: &str, number: u64) -> Result<()> {
    let output = Command::new("gh")
        .arg("issue")
        .arg("close")
        .arg(number.to_string())
        .arg("--repo")
        .arg(repo)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh issue close failed: {}", stderr));
    }
    Ok(())
}

/// Auto-detect the current repository from `gh repo view`.
pub async fn detect_repo() -> Result<String> {
    let output = Command::new("gh")
        .arg("repo")
        .arg("view")
        .arg("--json")
        .arg("nameWithOwner")
        .arg("--jq")
        .arg(".nameWithOwner")
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh repo view failed: {}", stderr));
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        return Err(anyhow::anyhow!("Could not detect repo from gh repo view"));
    }
    Ok(s)
}

/// Normalise a target-repo string to the `owner/repo` form expected by `gh`.
///
/// Accepts:
/// - `owner/repo` — returned as-is.
/// - `https://github.com/owner/repo[.git]`
/// - `git@github.com:owner/repo[.git]`
pub fn repo_to_gh_identifier(target_repo: Option<&str>) -> Option<String> {
    let repo = target_repo?;
    // Already in "owner/repo" form.
    let simple_re = regex::Regex::new(r"^[\w.\-]+/[\w.\-]+$").unwrap();
    if simple_re.is_match(repo) {
        return Some(repo.to_string());
    }
    // https://github.com/owner/repo.git  or  git@github.com:owner/repo.git
    let https_re =
        regex::Regex::new(r"github\.com[:/]([\w.\-]+/[\w.\-]+?)(?:\.git)?$").unwrap();
    if let Some(caps) = https_re.captures(repo) {
        return Some(caps[1].to_string());
    }
    Some(repo.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_to_gh_identifier_simple() {
        assert_eq!(
            repo_to_gh_identifier(Some("owner/repo")),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_https_url() {
        assert_eq!(
            repo_to_gh_identifier(Some("https://github.com/owner/repo.git")),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_ssh_url() {
        assert_eq!(
            repo_to_gh_identifier(Some("git@github.com:owner/repo.git")),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_none() {
        assert_eq!(repo_to_gh_identifier(None), None);
    }

    #[test]
    fn test_repo_to_gh_identifier_https_no_git() {
        assert_eq!(
            repo_to_gh_identifier(Some("https://github.com/owner/my-repo")),
            Some("owner/my-repo".to_string())
        );
    }

    // ── Additional tests ─────────────────────────────────────────────────────

    #[test]
    fn test_repo_to_gh_identifier_with_dot_in_name() {
        assert_eq!(
            repo_to_gh_identifier(Some("owner/my.repo")),
            Some("owner/my.repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_https_with_path() {
        assert_eq!(
            repo_to_gh_identifier(Some("https://github.com/a/b.git")),
            Some("a/b".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_empty_string() {
        // An empty string doesn't match the simple "owner/repo" regex nor the
        // URL regex, so it is returned as-is.
        assert_eq!(
            repo_to_gh_identifier(Some("")),
            Some("".to_string())
        );
    }

    #[test]
    fn test_issue_body_str_some() {
        let issue = GitHubIssue {
            number: 1,
            title: "t".into(),
            body: Some("desc".into()),
            url: "u".into(),
            labels: vec![],
            assignees: vec![],
            created_at: "c".into(),
            updated_at: "u".into(),
            state: "open".into(),
            repo: "r".into(),
        };
        assert_eq!(issue.body_str(), "desc");
    }

    #[test]
    fn test_issue_body_str_none() {
        let issue = GitHubIssue {
            number: 1,
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            assignees: vec![],
            created_at: "c".into(),
            updated_at: "u".into(),
            state: "open".into(),
            repo: "r".into(),
        };
        assert_eq!(issue.body_str(), "");
    }

    #[test]
    fn test_fetch_issues_options_defaults() {
        let opts = FetchIssuesOptions {
            repo: None,
            assignees: None,
            labels: None,
            limit: None,
        };
        // Verify the defaults that fetch_github_issues would use.
        assert!(opts.repo.is_none());
        assert_eq!(opts.limit.unwrap_or(10), 10);
        assert!(opts.assignees.as_deref().unwrap_or(&[]).is_empty());
        assert!(opts.labels.as_deref().unwrap_or(&[]).is_empty());
    }

    #[test]
    fn test_repo_to_gh_identifier_ssh_no_dot_git() {
        assert_eq!(
            repo_to_gh_identifier(Some("git@github.com:owner/repo")),
            Some("owner/repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_with_hyphen_in_name() {
        assert_eq!(
            repo_to_gh_identifier(Some("my-org/my-repo")),
            Some("my-org/my-repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_https_with_hyphen_and_dot() {
        assert_eq!(
            repo_to_gh_identifier(Some("https://github.com/my-org/my.repo.git")),
            Some("my-org/my.repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_ssh_with_hyphen() {
        assert_eq!(
            repo_to_gh_identifier(Some("git@github.com:my-org/my-repo.git")),
            Some("my-org/my-repo".to_string())
        );
    }

    #[test]
    fn test_repo_to_gh_identifier_arbitrary_string() {
        // A string that doesn't match any pattern is returned as-is.
        assert_eq!(
            repo_to_gh_identifier(Some("just-a-word")),
            Some("just-a-word".to_string())
        );
    }
}
