use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Top-level harness config (deserialised from harness.config.toml)
// ---------------------------------------------------------------------------

/// Root configuration for the agent harness.
///
/// Fields that cannot be serialised (e.g. closure-based pipeline configs) are
/// skipped during TOML deserialisation and must be set programmatically.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HarnessConfig {
    /// LLM provider identifier: `"anthropic"`, `"openai"`, or `"ollama"`.
    pub provider: String,
    /// Model name passed to the provider (e.g. `"claude-opus-4-6"`).
    pub model: String,
    /// Optional default target repository in `owner/repo` form.
    pub target_repo: Option<String>,
    /// Git transport method: `"ssh"` or `"https"` (default `"https"`).
    pub git_method: Option<String>,
    /// Structured logging options.
    pub logging: Option<LoggingConfig>,
    /// Prompt template overrides.
    pub prompts: Option<PromptConfig>,
    /// Per-technique option overrides.
    pub techniques: Option<TechniquesConfig>,
    /// Pipeline definitions — populated programmatically; skipped in TOML.
    #[serde(skip)]
    pub pipelines: Vec<PipelineConfig>,
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

/// Controls log verbosity and formatting.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggingConfig {
    /// Tracing filter string, e.g. `"info"` or `"debug"`.
    pub level: Option<String>,
    /// Emit human-readable pretty-printed logs instead of JSON lines.
    pub pretty: Option<bool>,
}

// ---------------------------------------------------------------------------
// Prompt overrides
// ---------------------------------------------------------------------------

/// Paths or inline content for each named prompt template.
///
/// A value ending in `.md`, `.txt`, or `.prompt` is treated as a file path
/// relative to `harness_root`; any other value is used as the literal
/// template string.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptConfig {
    pub code_review: Option<String>,
    pub review_feedback: Option<String>,
    pub pr_description: Option<String>,
    pub pr_review: Option<String>,
    pub pr_comment_response: Option<String>,
    pub react_system: Option<String>,
    pub reflexion_system: Option<String>,
    pub reflexion_reflect: Option<String>,
}

// ---------------------------------------------------------------------------
// Technique option overrides
// ---------------------------------------------------------------------------

/// Container for all per-technique option blocks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TechniquesConfig {
    pub react: Option<ReactConfig>,
    pub reflexion: Option<ReflexionConfig>,
    pub tree_of_thoughts: Option<TreeOfThoughtsConfig>,
    pub self_consistency: Option<SelfConsistencyConfig>,
    pub critique_loop: Option<CritiqueLoopConfig>,
    pub plan_and_solve: Option<PlanAndSolveConfig>,
    pub skeleton_of_thought: Option<SkeletonOfThoughtConfig>,
    pub multi_agent_debate: Option<MultiAgentDebateConfig>,
}

/// Options for the ReAct (Think → Act → Observe) technique.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReactConfig {
    /// Maximum Think-Act-Observe iterations before giving up.
    pub max_iterations: Option<usize>,
}

/// Options for the Reflexion technique (ReAct + written self-reflection).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReflexionConfig {
    /// Maximum number of trial attempts.
    pub max_trials: Option<usize>,
    /// Number of past reflections kept in the context window.
    pub memory_window: Option<usize>,
}

/// Options for Tree of Thoughts search.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TreeOfThoughtsConfig {
    /// Number of branches to expand at each node.
    pub branching: Option<usize>,
    /// Search strategy: `"bfs"` or `"dfs"`.
    pub strategy: Option<String>,
    /// Maximum tree depth.
    pub max_depth: Option<usize>,
    /// Minimum score for a node to be kept.
    pub score_threshold: Option<f64>,
}

/// Options for Self-Consistency sampling.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SelfConsistencyConfig {
    /// Number of independent reasoning chains to sample.
    pub samples: Option<usize>,
    /// Sampling temperature used for each chain.
    pub temperature: Option<f64>,
}

/// Options for the Critique Loop technique.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CritiqueLoopConfig {
    /// Maximum generate → critique → revise rounds.
    pub max_rounds: Option<usize>,
    /// Evaluation criteria injected into the critique prompt.
    pub criteria: Option<Vec<String>>,
}

/// Options for Plan-and-Solve (PS+).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PlanAndSolveConfig {
    /// Emit a detailed step-by-step plan before executing.
    pub detailed_plan: Option<bool>,
}

/// Options for Skeleton-of-Thought.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkeletonOfThoughtConfig {
    /// Maximum number of outline points to generate.
    pub max_points: Option<usize>,
    /// Expand outline points in parallel (requires async runtime).
    pub parallel_expansion: Option<bool>,
}

/// Options for Multi-Agent Debate.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MultiAgentDebateConfig {
    /// Number of debating agents.
    pub num_agents: Option<usize>,
    /// Number of debate rounds.
    pub rounds: Option<usize>,
    /// Final-answer aggregation strategy: `"judge"` or `"majority"`.
    pub aggregation: Option<String>,
}

// ---------------------------------------------------------------------------
// Pipeline config  (set programmatically — not deserialised from TOML)
// ---------------------------------------------------------------------------

/// Full configuration for a single pipeline instance.
///
/// `PipelineConfig` is constructed in application code (not loaded from TOML)
/// because it may embed closures for `buildTask`.  The nested sub-configs
/// (`GitHubIssuesPipelineConfig`, etc.) *are* serialisable for logging/
/// debugging purposes.
#[derive(Debug, Clone, Default)]
pub struct PipelineConfig {
    /// Human-readable pipeline name used in logs and run-store keys.
    pub name: String,
    /// cron expression (e.g. `"0 */5 * * * *"`).
    pub schedule: String,
    /// GitHub PR-response trigger settings.
    pub github_pr_responses: Option<GitHubPRResponsePipelineConfig>,
    /// GitHub PR trigger settings.
    pub github_prs: Option<GitHubPRsPipelineConfig>,
    /// GitHub Issues trigger settings.
    pub github_issues: Option<GitHubIssuesPipelineConfig>,
    /// Generic shell-command trigger.
    pub trigger: Option<TriggerConfig>,
    /// Agent loop configuration.
    pub agent_loop: AgentLoopConfig,
    /// Workspace (git clone) configuration.
    pub workspace: Option<WorkspaceConfig>,
    /// Pull-request creation configuration.
    pub pull_request: Option<PullRequestConfig>,
    /// Retry policy for the pipeline.
    pub retry: Option<RetryConfig>,
    /// Per-pipeline prompt overrides (take precedence over harness-level).
    pub prompts: Option<PipelinePromptConfig>,
}

/// Per-pipeline prompt overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelinePromptConfig {
    pub code_review: Option<String>,
    pub review_feedback: Option<String>,
    pub pr_description: Option<String>,
}

// ---------------------------------------------------------------------------
// GitHub trigger sub-configs
// ---------------------------------------------------------------------------

/// Trigger pipeline runs from open GitHub Issues.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubIssuesPipelineConfig {
    /// `owner/repo` — falls back to `HarnessConfig::target_repo` when absent.
    pub repo: Option<String>,
    /// Only process issues assigned to these users; empty = no filter.
    pub assignees: Option<Vec<String>>,
    /// Only process issues carrying all of these labels; empty = no filter.
    pub labels: Option<Vec<String>>,
    /// Maximum number of issues to process per cron tick.
    pub limit: Option<usize>,
}

/// Trigger pipeline runs from open GitHub Pull Requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubPRsPipelineConfig {
    /// `owner/repo`.
    pub repo: Option<String>,
    /// `gh pr list --search` expression.
    pub search: Option<String>,
    /// Maximum number of PRs to process per cron tick.
    pub limit: Option<usize>,
}

/// Trigger pipeline runs from review comments on GitHub Pull Requests.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitHubPRResponsePipelineConfig {
    /// `owner/repo`.
    pub repo: Option<String>,
    /// `gh pr list --search` expression.
    pub search: Option<String>,
    /// Maximum number of PRs to process per cron tick.
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Generic shell-command trigger
// ---------------------------------------------------------------------------

/// Runs a shell command and feeds its stdout to the parse stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// Shell command to execute.
    pub command: String,
    /// Working directory for the command; defaults to `harness_root`.
    pub cwd: Option<String>,
    /// Kill the command after this many milliseconds.
    pub timeout_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

/// Controls which reasoning technique (or external agent CLI) drives the loop.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentLoopConfig {
    /// Technique name: `"react"`, `"reflexion"`, `"external"`, etc.
    pub technique: String,
    /// Used when `technique = "external"`.
    pub external_agent: Option<ExternalAgentConfig>,
    /// Regex pattern in the agent's output that signals task completion.
    pub stop_criteria: Option<String>,
    /// Maximum agent-loop retries on transient errors.
    pub max_retries: Option<usize>,
    /// Base back-off delay in milliseconds between retries.
    pub backoff_ms: Option<u64>,
    /// Maximum reviewer → agent feedback rounds before the pipeline fails.
    pub max_review_rounds: Option<usize>,
    /// Maximum inner reasoning iterations passed to the technique.
    pub max_iterations: Option<usize>,
}

/// Configuration for spawning an external agent CLI (e.g. `claude`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExternalAgentConfig {
    /// Kill the agent process after this many seconds.
    pub timeout_secs: Option<u64>,
    /// Model override passed to the external CLI.
    pub model: Option<String>,
    /// Additional CLI flags appended verbatim.
    pub extra_flags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Workspace
// ---------------------------------------------------------------------------

/// Git workspace (clone) settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Remote URL to clone; overrides the URL derived from `target_repo`.
    pub repo_url: Option<String>,
    /// Branch to check out after cloning (default `"main"`).
    pub base_branch: Option<String>,
    /// Prefix for the harness-created feature branch (default `"harness/"`).
    pub branch_prefix: Option<String>,
    /// Parent directory for workspace clones (default `~/agent-harness/workspaces`).
    pub workspaces_dir: Option<String>,
}

// ---------------------------------------------------------------------------
// Pull request
// ---------------------------------------------------------------------------

/// Metadata applied when the harness opens a pull request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PullRequestConfig {
    /// Static PR title; overridden by the LLM-generated title when absent.
    pub title: Option<String>,
    /// Static PR body; overridden by the LLM-generated body when absent.
    pub body: Option<String>,
    /// Open the PR as a draft.
    pub draft: Option<bool>,
    /// Labels to apply to the newly created PR.
    pub labels: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Retry
// ---------------------------------------------------------------------------

/// Retry policy applied at the pipeline level.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Total attempts (initial + retries).
    pub max_attempts: Option<usize>,
    /// Base back-off delay in milliseconds (may be multiplied for exponential back-off).
    pub backoff_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_config_default_is_empty() {
        let cfg = HarnessConfig::default();
        assert_eq!(cfg.provider, "");
        assert_eq!(cfg.model, "");
        assert!(cfg.pipelines.is_empty());
    }

    #[test]
    fn harness_config_toml_roundtrip() {
        let toml_src = r#"
provider = "anthropic"
model = "claude-opus-4-6"
target_repo = "owner/repo"
git_method = "ssh"

[logging]
level = "debug"
pretty = true

[techniques.react]
max_iterations = 10

[techniques.reflexion]
max_trials = 3
memory_window = 5
"#;
        let cfg: HarnessConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.provider, "anthropic");
        assert_eq!(cfg.model, "claude-opus-4-6");
        assert_eq!(cfg.target_repo.as_deref(), Some("owner/repo"));
        assert_eq!(cfg.git_method.as_deref(), Some("ssh"));

        let log = cfg.logging.as_ref().unwrap();
        assert_eq!(log.level.as_deref(), Some("debug"));
        assert_eq!(log.pretty, Some(true));

        let tech = cfg.techniques.as_ref().unwrap();
        assert_eq!(tech.react.as_ref().unwrap().max_iterations, Some(10));
        let refl = tech.reflexion.as_ref().unwrap();
        assert_eq!(refl.max_trials, Some(3));
        assert_eq!(refl.memory_window, Some(5));

        // pipelines are skipped — must stay empty after deserialisation
        assert!(cfg.pipelines.is_empty());
    }

    #[test]
    fn pipeline_config_default() {
        let pc = PipelineConfig::default();
        assert_eq!(pc.name, "");
        assert_eq!(pc.schedule, "");
        assert!(pc.github_issues.is_none());
    }

    #[test]
    fn github_issues_config_serde() {
        let toml_src = r#"
repo = "owner/repo"
assignees = ["alice", "bob"]
labels = ["bug"]
limit = 5
"#;
        let cfg: GitHubIssuesPipelineConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(cfg.repo.as_deref(), Some("owner/repo"));
        assert_eq!(cfg.assignees.as_ref().unwrap().len(), 2);
        assert_eq!(cfg.limit, Some(5));
    }

    #[test]
    fn retry_config_serde() {
        let json = r#"{"max_attempts":3,"backoff_ms":500}"#;
        let cfg: RetryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.max_attempts, Some(3));
        assert_eq!(cfg.backoff_ms, Some(500));
    }

    #[test]
    fn agent_loop_config_default() {
        let cfg = AgentLoopConfig::default();
        assert_eq!(cfg.technique, "");
        assert!(cfg.external_agent.is_none());
    }
}
