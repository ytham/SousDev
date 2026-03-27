use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::process::Command;
use crate::types::config::WorkspaceConfig;
use crate::utils::logger::Logger;
use crate::pipelines::github_prs::GitHubPR;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Information about a prepared workspace returned by [`WorkspaceManager::setup`].
pub struct WorkspaceInfo {
    /// Absolute path of the workspace directory on disk.
    pub dir: PathBuf,
    /// Feature branch that was created (or checked out) in this workspace.
    pub branch: String,
    /// Remote URL that was cloned.
    pub repo_url: String,
}

/// Manages the lifecycle of a git workspace for a single pipeline run.
///
/// On `setup` the manager either clones the target repository fresh or resets
/// an existing clone to the configured base branch and creates a new feature
/// branch.  On `teardown` the directory is removed.
pub struct WorkspaceManager {
    config: WorkspaceConfig,
    logger: Logger,
    /// Optional `owner/repo` string (or full URL) from the harness config.
    target_repo: Option<String>,
    /// Git transport method: `"ssh"` or `"https"`.
    git_method: String,
}

impl WorkspaceManager {
    /// Create a new [`WorkspaceManager`].
    pub fn new(
        config: WorkspaceConfig,
        logger: Logger,
        target_repo: Option<String>,
        git_method: impl Into<String>,
    ) -> Self {
        Self {
            config,
            logger,
            target_repo,
            git_method: git_method.into(),
        }
    }

    /// Prepare a workspace for a regular issue/trigger run.
    ///
    /// If an existing workspace is found with a `.git` directory it is reset
    /// to the base branch and a fresh feature branch is created.  Otherwise
    /// the repo is cloned fresh.
    pub async fn setup(&self, run_id: &str, issue_number: Option<u64>) -> Result<WorkspaceInfo> {
        let repo_url = self.resolve_repo_url().await?;
        let repo_slug = repo_slug_from_url(&repo_url);
        let base_branch = self.config.base_branch.as_deref().unwrap_or("main");
        let workspaces_dir = self.workspaces_dir();

        let (dir_name, branch) = if let Some(n) = issue_number {
            let prefix = self
                .config
                .branch_prefix
                .as_deref()
                .unwrap_or("sousdev/");
            let dir = format!("{}-issue{}", repo_slug, n);
            let br = format!("{}issue-{}", prefix, n);
            (dir, br)
        } else {
            let short = &run_id[..run_id.len().min(12)];
            let prefix = self
                .config
                .branch_prefix
                .as_deref()
                .unwrap_or("sousdev/auto-");
            let dir = format!("{}-{}", repo_slug, short);
            let br = format!("{}{}", prefix, short);
            (dir, br)
        };

        let dir = workspaces_dir.join(&dir_name);
        fs::create_dir_all(&dir).await?;

        let git_dir = dir.join(".git");
        if git_dir.exists() {
            self.logger.info(&format!(
                "Found existing workspace at {} — resetting to {}",
                dir.display(),
                base_branch
            ));
            self.reset_workspace(&dir, base_branch, &branch, &repo_url)
                .await?;
            self.logger
                .info(&format!("Workspace reset. Branch: {}", branch));
        } else {
            // Check if dir is non-empty (exists but has no .git — stale from a
            // prior failed run that didn't finish cloning).
            let mut read_dir = fs::read_dir(&dir).await?;
            let is_empty = read_dir.next_entry().await?.is_none();
            if !is_empty {
                self.logger.info(
                    "Workspace directory exists but has no .git — cleaning up before cloning",
                );
                fs::remove_dir_all(&dir).await?;
                fs::create_dir_all(&dir).await?;
            }

            self.logger.info(&format!(
                "Cloning {} → {}  (base: {})",
                repo_url,
                dir.display(),
                base_branch
            ));
            self.exec(
                &[
                    "git",
                    "clone",
                    "--depth=50",
                    "--filter=blob:none",
                    "--single-branch",
                    "--branch",
                    base_branch,
                    &repo_url,
                    ".",
                ],
                &dir,
            )
            .await?;
            self.exec(&["git", "checkout", "-b", &branch], &dir)
                .await?;
        }

        Ok(WorkspaceInfo {
            dir,
            branch,
            repo_url,
        })
    }

    /// Prepare a workspace for reviewing an existing pull request.
    ///
    /// Clones (or reuses) the repository and runs `gh pr checkout` to check
    /// out the PR's head branch.
    pub async fn setup_for_pr_review(
        &self,
        pr: &GitHubPR,
        _run_id: &str,
    ) -> Result<WorkspaceInfo> {
        let repo_url = self.resolve_repo_url().await?;
        let repo_slug = repo_slug_from_url(&repo_url);
        let base_branch = self.config.base_branch.as_deref().unwrap_or("main");
        let workspaces_dir = self.workspaces_dir();

        let dir_name = format!("{}-pr{}", repo_slug, pr.number);
        let dir = workspaces_dir.join(&dir_name);
        fs::create_dir_all(&dir).await?;

        let git_dir = dir.join(".git");
        if git_dir.exists() {
            self.logger
                .info(&format!("Reusing existing PR workspace: {}", dir.display()));
            // Fetch latest refs (best-effort; ignore transient network errors).
            if let Err(e) = self
                .exec(&["git", "fetch", "--depth=50", "origin"], &dir)
                .await
            {
                self.logger.info(&format!(
                    "fetch origin failed (continuing with cached state): {}",
                    e
                ));
            }
        } else {
            let mut read_dir = fs::read_dir(&dir).await?;
            let is_empty = read_dir.next_entry().await?.is_none();
            if !is_empty {
                self.logger.info(
                    "PR workspace directory exists but has no .git — cleaning up before cloning",
                );
                fs::remove_dir_all(&dir).await?;
                fs::create_dir_all(&dir).await?;
            }
            self.logger.info(&format!(
                "Cloning {} → {}  (base: {})",
                repo_url,
                dir.display(),
                base_branch
            ));
            self.exec(
                &[
                    "git",
                    "clone",
                    "--depth=50",
                    "--filter=blob:none",
                    "--single-branch",
                    "--branch",
                    base_branch,
                    &repo_url,
                    ".",
                ],
                &dir,
            )
            .await?;
        }

        // Check out the PR branch via `gh pr checkout`.
        self.logger.info(&format!(
            "Checking out PR #{} ({})",
            pr.number, pr.head_ref_name
        ));
        let pr_number_str = pr.number.to_string();
        self.exec(
            &[
                "gh",
                "pr",
                "checkout",
                &pr_number_str,
                "--repo",
                &pr.repo,
            ],
            &dir,
        )
        .await?;

        self.logger.info(&format!(
            "PR workspace ready: {}  branch: {}",
            dir.display(),
            pr.head_ref_name
        ));
        Ok(WorkspaceInfo {
            dir,
            branch: pr.head_ref_name.clone(),
            repo_url,
        })
    }

    /// Remove the workspace directory from disk.
    pub async fn teardown(&self, info: &WorkspaceInfo) -> Result<()> {
        self.logger
            .info(&format!("Tearing down workspace: {}", info.dir.display()));
        fs::remove_dir_all(&info.dir).await?;
        Ok(())
    }

    // ── Private helpers ──────────────────────────────────────────────────────

    /// Reset an existing workspace: fetch origin, hard-reset to the base
    /// branch, delete any stale harness branch, and create a fresh one.
    async fn reset_workspace(
        &self,
        dir: &Path,
        base_branch: &str,
        branch: &str,
        _repo_url: &str,
    ) -> Result<()> {
        // Fetch latest base branch (best-effort).
        if let Err(e) = self
            .exec(
                &["git", "fetch", "--depth=50", "origin", base_branch],
                dir,
            )
            .await
        {
            self.logger.info(&format!(
                "fetch origin failed (continuing with cached state): {}",
                e
            ));
        }

        // Checkout base branch.
        self.exec(&["git", "checkout", base_branch], dir).await?;

        // Hard reset to remote tip (best-effort).
        let origin_ref = format!("origin/{}", base_branch);
        if let Err(e) = self
            .exec(&["git", "reset", "--hard", &origin_ref], dir)
            .await
        {
            self.logger.info(&format!(
                "reset --hard {} failed — using local state: {}",
                origin_ref, e
            ));
        }

        // Delete existing harness branch if present (ignore error).
        let _ = self.exec(&["git", "branch", "-D", branch], dir).await;

        // Create fresh harness branch.
        self.exec(&["git", "checkout", "-b", branch], dir).await?;
        Ok(())
    }

    /// Resolve the remote URL to clone, preferring `config.repo_url`, then
    /// `target_repo`, then failing with a clear error message.
    async fn resolve_repo_url(&self) -> Result<String> {
        if let Some(url) = &self.config.repo_url {
            return Ok(self.expand_repo_url(url));
        }
        if let Some(repo) = &self.target_repo {
            return Ok(self.expand_repo_url(repo));
        }
        Err(anyhow::anyhow!(
            "No target_repo configured. Set target_repo in sousdev.config.toml \
             or workspace.repo_url in the pipeline config."
        ))
    }

    /// Expand an `owner/repo` shorthand or leave a full URL untouched.
    fn expand_repo_url(&self, raw: &str) -> String {
        if raw.starts_with("https://") || raw.starts_with("git@") {
            return raw.to_string();
        }
        // `owner/repo` shorthand.
        match self.git_method.as_str() {
            "ssh" => format!("git@github.com:{}.git", raw),
            _ => format!("https://github.com/{}.git", raw),
        }
    }

    /// Return the parent directory for all workspace clones.
    fn workspaces_dir(&self) -> PathBuf {
        if let Some(dir) = &self.config.workspaces_dir {
            let expanded = if dir.starts_with("~/") {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(&dir[2..])
            } else {
                PathBuf::from(dir)
            };
            return expanded;
        }
        dirs::home_dir()
            .unwrap_or_default()
            .join("sousdev")
            .join("workspaces")
    }

    /// Run a command in `cwd` and return stdout, or an error with stderr.
    async fn exec(&self, args: &[&str], cwd: &Path) -> Result<String> {
        if args.is_empty() {
            return Err(anyhow::anyhow!("exec: empty args"));
        }
        self.logger.debug(&format!("exec: {}", args.join(" ")));
        let output = Command::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .output()
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "Command failed: {}\nstderr: {}",
                args.join(" "),
                stderr
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

// ---------------------------------------------------------------------------
// Helper: derive a short repo slug from a URL
// ---------------------------------------------------------------------------

fn repo_slug_from_url(url: &str) -> String {
    // Strip trailing slash and .git suffix, then take the last path component.
    let name = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("repo");
    // Handle git@github.com:owner/repo — "rsplit('/').next()" already gives
    // "repo" but the colon variant might leave "owner" if there's no slash.
    // A second rsplit on ':' handles that case.
    let name = name.rsplit(':').next().unwrap_or(name);
    name.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager(method: &str) -> WorkspaceManager {
        WorkspaceManager::new(
            WorkspaceConfig::default(),
            Logger::new("test"),
            None,
            method,
        )
    }

    #[test]
    fn test_repo_slug_from_https_url() {
        assert_eq!(
            repo_slug_from_url("https://github.com/owner/my-repo.git"),
            "my-repo"
        );
    }

    #[test]
    fn test_repo_slug_from_ssh_url() {
        assert_eq!(
            repo_slug_from_url("git@github.com:owner/my-repo.git"),
            "my-repo"
        );
    }

    #[test]
    fn test_repo_slug_no_git_suffix() {
        assert_eq!(
            repo_slug_from_url("https://github.com/owner/my-repo"),
            "my-repo"
        );
    }

    #[test]
    fn test_expand_repo_url_https_shorthand() {
        let m = make_manager("https");
        assert_eq!(
            m.expand_repo_url("owner/repo"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn test_expand_repo_url_ssh_shorthand() {
        let m = make_manager("ssh");
        assert_eq!(
            m.expand_repo_url("owner/repo"),
            "git@github.com:owner/repo.git"
        );
    }

    #[test]
    fn test_expand_repo_url_already_https() {
        let m = make_manager("https");
        let full = "https://github.com/owner/repo.git";
        assert_eq!(m.expand_repo_url(full), full);
    }

    #[test]
    fn test_expand_repo_url_already_ssh() {
        let m = make_manager("ssh");
        let full = "git@github.com:owner/repo.git";
        assert_eq!(m.expand_repo_url(full), full);
    }

    #[tokio::test]
    async fn test_resolve_repo_url_from_config() {
        let mut config = WorkspaceConfig::default();
        config.repo_url = Some("owner/from-config".to_string());
        let m = WorkspaceManager::new(config, Logger::new("t"), None, "https");
        let url = m.resolve_repo_url().await.unwrap();
        assert_eq!(url, "https://github.com/owner/from-config.git");
    }

    #[tokio::test]
    async fn test_resolve_repo_url_from_target_repo() {
        let m = WorkspaceManager::new(
            WorkspaceConfig::default(),
            Logger::new("t"),
            Some("owner/from-target".to_string()),
            "https",
        );
        let url = m.resolve_repo_url().await.unwrap();
        assert_eq!(url, "https://github.com/owner/from-target.git");
    }

    #[tokio::test]
    async fn test_resolve_repo_url_missing_errors() {
        let m = make_manager("https");
        let result = m.resolve_repo_url().await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("No target_repo"),
            "expected 'No target_repo' in error"
        );
    }

    #[test]
    fn test_workspaces_dir_custom() {
        let mut config = WorkspaceConfig::default();
        config.workspaces_dir = Some("/tmp/my-workspaces".to_string());
        let m = WorkspaceManager::new(config, Logger::new("t"), None, "https");
        assert_eq!(m.workspaces_dir(), PathBuf::from("/tmp/my-workspaces"));
    }

    // ── Additional tests ─────────────────────────────────────────────────────

    #[test]
    fn test_repo_slug_simple_name() {
        assert_eq!(repo_slug_from_url("repo"), "repo");
    }

    #[test]
    fn test_repo_slug_with_dashes() {
        assert_eq!(
            repo_slug_from_url("https://github.com/owner/my-cool-repo.git"),
            "my-cool-repo"
        );
    }

    #[test]
    fn test_expand_repo_url_shorthand_https_default() {
        // When git_method is something unexpected, it should default to https.
        let m = make_manager("");
        assert_eq!(
            m.expand_repo_url("owner/repo"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn test_workspaces_dir_default() {
        let m = make_manager("https");
        let expected = dirs::home_dir()
            .unwrap()
            .join("sousdev")
            .join("workspaces");
        assert_eq!(m.workspaces_dir(), expected);
    }

    #[test]
    fn test_workspaces_dir_tilde_expansion() {
        let mut config = WorkspaceConfig::default();
        config.workspaces_dir = Some("~/custom-ws".to_string());
        let m = WorkspaceManager::new(config, Logger::new("t"), None, "https");
        let expected = dirs::home_dir().unwrap().join("custom-ws");
        assert_eq!(m.workspaces_dir(), expected);
    }

    #[test]
    fn test_repo_slug_trailing_slash() {
        assert_eq!(
            repo_slug_from_url("https://github.com/owner/repo/"),
            "repo"
        );
    }
}
