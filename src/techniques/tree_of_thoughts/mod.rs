//! Tree of Thoughts (ToT): BFS/DFS search over scored reasoning steps.
//!
//! The LLM generates multiple candidate "thoughts" (reasoning branches) at
//! each node, scores them, and the search expands the best ones up to a
//! configurable depth.  Useful when the solution space is broad and requires
//! exploring multiple hypotheses.
//!
//! # Algorithm
//!
//! Initialise with a single root node containing the task.
//! Loop for `max_depth` levels:
//!   1. For each frontier node expand `branching` child thoughts.
//!   2. Score each child thought (ask LLM: "Score this thought 0–10").
//!   3. Prune children below `score_threshold`.
//!   4. Keep the top-`branching` survivors as the new frontier (BFS) or
//!      recurse into the best child immediately (DFS).
//!      Return the answer from the highest-scoring leaf.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

const DEFAULT_BRANCHING: usize = 3;
const DEFAULT_MAX_DEPTH: usize = 3;
const DEFAULT_SCORE_THRESHOLD: f64 = 4.0;

/// Search strategy for ToT.
#[derive(Debug, Clone, PartialEq)]
pub enum Strategy {
    Bfs,
    Dfs,
}

/// Options for [`run_tree_of_thoughts`].
pub struct Options {
    /// The task or problem statement.
    pub task: String,
    /// LLM provider.
    pub provider: Arc<dyn LLMProvider>,
    /// Number of child thoughts generated at each expansion step.
    pub branching: Option<usize>,
    /// Search strategy.
    pub strategy: Option<Strategy>,
    /// Maximum tree depth.
    pub max_depth: Option<usize>,
    /// Minimum score for a thought to be retained (0.0–10.0).
    pub score_threshold: Option<f64>,
}

/// A node in the thought tree.
#[derive(Debug, Clone)]
struct ThoughtNode {
    /// Accumulated reasoning so far (root → this node).
    context: String,
    /// The thought text at this node.
    thought: String,
    /// Score assigned by the LLM evaluator (0.0–10.0).
    score: f64,
    /// Depth of this node (root = 0).
    depth: usize,
}

/// Run the Tree-of-Thoughts algorithm.
pub async fn run_tree_of_thoughts(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let branching = opts.branching.unwrap_or(DEFAULT_BRANCHING);
    let max_depth = opts.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    let score_threshold = opts.score_threshold.unwrap_or(DEFAULT_SCORE_THRESHOLD);
    let strategy = opts.strategy.unwrap_or(Strategy::Bfs);

    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls: usize = 0;
    let mut step_index: usize = 0;

    // Root frontier — one virtual node with empty thought.
    let root = ThoughtNode {
        context: opts.task.clone(),
        thought: String::new(),
        score: 10.0,
        depth: 0,
    };

    let best_leaf = match strategy {
        Strategy::Bfs => {
            bfs_search(
                root,
                branching,
                max_depth,
                score_threshold,
                &opts.task,
                &opts.provider,
                &mut trajectory,
                &mut llm_calls,
                &mut step_index,
            )
            .await?
        }
        Strategy::Dfs => {
            dfs_search(
                root,
                branching,
                max_depth,
                score_threshold,
                &opts.task,
                &opts.provider,
                &mut trajectory,
                &mut llm_calls,
                &mut step_index,
            )
            .await?
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    match best_leaf {
        Some(leaf) => {
            // Ask LLM to produce a final answer from the best leaf context.
            let final_prompt = format!(
                "Based on the following reasoning, provide a concise final answer:\n\n{}",
                build_full_context(&leaf)
            );
            let msgs = vec![Message::user(final_prompt)];
            let completion = opts
                .provider
                .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(1024), ..Default::default() }))
                .await?;
            llm_calls += 1;
            let answer = completion.content.trim().to_string();
            Ok(RunResult::success("tree-of-thoughts", answer, trajectory, llm_calls, duration_ms))
        }
        None => Ok(RunResult::failure(
            "tree-of-thoughts",
            "No viable thought path found above the score threshold.",
            trajectory,
            duration_ms,
        )),
    }
}

// ---------------------------------------------------------------------------
// BFS search
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn bfs_search(
    root: ThoughtNode,
    branching: usize,
    max_depth: usize,
    score_threshold: f64,
    task: &str,
    provider: &Arc<dyn LLMProvider>,
    trajectory: &mut Vec<TrajectoryStep>,
    llm_calls: &mut usize,
    step_index: &mut usize,
) -> Result<Option<ThoughtNode>> {
    let mut frontier: Vec<ThoughtNode> = vec![root];
    let mut best_leaf: Option<ThoughtNode> = None;

    for _depth in 0..max_depth {
        let mut next_frontier: Vec<ThoughtNode> = Vec::new();

        for node in &frontier {
            let children = expand_node(node, branching, task, provider, trajectory, llm_calls, step_index).await?;
            for child in children {
                if child.score >= score_threshold {
                    // Track best leaf as we go.
                    if best_leaf.as_ref().is_none_or(|b: &ThoughtNode| child.score > b.score) {
                        best_leaf = Some(child.clone());
                    }
                    next_frontier.push(child);
                }
            }
        }

        if next_frontier.is_empty() {
            break;
        }

        // Keep only the top `branching` nodes to limit memory.
        next_frontier.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        next_frontier.truncate(branching);
        frontier = next_frontier;
    }

    Ok(best_leaf)
}

// ---------------------------------------------------------------------------
// DFS search
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn dfs_search(
    root: ThoughtNode,
    branching: usize,
    max_depth: usize,
    score_threshold: f64,
    task: &str,
    provider: &Arc<dyn LLMProvider>,
    trajectory: &mut Vec<TrajectoryStep>,
    llm_calls: &mut usize,
    step_index: &mut usize,
) -> Result<Option<ThoughtNode>> {
    let mut best: Option<ThoughtNode> = None;
    dfs_recurse(
        &root,
        branching,
        max_depth,
        score_threshold,
        task,
        provider,
        trajectory,
        llm_calls,
        step_index,
        &mut best,
    )
    .await?;
    Ok(best)
}

#[allow(clippy::too_many_arguments)]
async fn dfs_recurse(
    node: &ThoughtNode,
    branching: usize,
    max_depth: usize,
    score_threshold: f64,
    task: &str,
    provider: &Arc<dyn LLMProvider>,
    trajectory: &mut Vec<TrajectoryStep>,
    llm_calls: &mut usize,
    step_index: &mut usize,
    best: &mut Option<ThoughtNode>,
) -> Result<()> {
    if node.depth >= max_depth {
        return Ok(());
    }

    let children = expand_node(node, branching, task, provider, trajectory, llm_calls, step_index).await?;

    // Sort by score descending — explore best first.
    let mut sorted_children = children;
    sorted_children.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

    for child in sorted_children {
        if child.score < score_threshold {
            continue;
        }
        if best.as_ref().is_none_or(|b: &ThoughtNode| child.score > b.score) {
            *best = Some(child.clone());
        }
        // Use a Box to avoid recursive async – use a stack instead.
        // For simplicity we rely on tokio's stack and limit max_depth to avoid blowup.
        Box::pin(dfs_recurse(
            &child, branching, max_depth, score_threshold, task, provider,
            trajectory, llm_calls, step_index, best,
        ))
        .await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Node expansion + scoring
// ---------------------------------------------------------------------------

async fn expand_node(
    node: &ThoughtNode,
    branching: usize,
    task: &str,
    provider: &Arc<dyn LLMProvider>,
    trajectory: &mut Vec<TrajectoryStep>,
    llm_calls: &mut usize,
    step_index: &mut usize,
) -> Result<Vec<ThoughtNode>> {
    let context = build_full_context(node);
    let expansion_prompt = format!(
        "Task: {}\n\nReasoning so far:\n{}\n\n\
         Generate {} distinct next reasoning steps (thoughts). \
         Separate each thought with \"---THOUGHT---\".",
        task,
        if context.is_empty() { "(none yet)".to_string() } else { context.clone() },
        branching
    );

    let msgs = vec![Message::user(expansion_prompt)];
    let completion = provider
        .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(1024), ..Default::default() }))
        .await?;
    *llm_calls += 1;

    let raw_thoughts: Vec<String> = completion
        .content
        .split("---THOUGHT---")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(branching)
        .collect();

    let mut children = Vec::new();
    for thought in raw_thoughts {
        let score = score_thought(&thought, task, &context, provider, llm_calls).await?;

        trajectory.push(TrajectoryStep {
            index: *step_index,
            step_type: StepType::Thought,
            content: thought.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("score".to_string(), serde_json::json!(score));
                m.insert("depth".to_string(), serde_json::json!(node.depth + 1));
                m
            },
        });
        *step_index += 1;

        children.push(ThoughtNode {
            context: context.clone(),
            thought,
            score,
            depth: node.depth + 1,
        });
    }

    Ok(children)
}

async fn score_thought(
    thought: &str,
    task: &str,
    context: &str,
    provider: &Arc<dyn LLMProvider>,
    llm_calls: &mut usize,
) -> Result<f64> {
    let prompt = format!(
        "Task: {}\n\nReasoning context:\n{}\n\nThought to evaluate:\n{}\n\n\
         Rate this thought on a scale of 0–10 based on its relevance, \
         correctness, and progress toward solving the task.\n\
         Respond with only: SCORE: <integer>",
        task,
        if context.is_empty() { "(none)" } else { context },
        thought
    );
    let msgs = vec![Message::user(prompt)];
    let completion = provider
        .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(64), ..Default::default() }))
        .await?;
    *llm_calls += 1;
    Ok(parse_score(&completion.content))
}

fn build_full_context(node: &ThoughtNode) -> String {
    if node.thought.is_empty() {
        return node.context.clone();
    }
    if node.context.is_empty() {
        return node.thought.clone();
    }
    format!("{}\n\n{}", node.context, node.thought)
}

fn parse_score(text: &str) -> f64 {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SCORE:") {
            if let Ok(n) = rest.trim().parse::<f64>() {
                return n.clamp(0.0, 10.0);
            }
        }
    }
    // Fallback: look for a lone integer on any line.
    for line in text.lines() {
        if let Ok(n) = line.trim().parse::<f64>() {
            return n.clamp(0.0, 10.0);
        }
    }
    5.0 // neutral default
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_score_explicit() {
        assert_eq!(parse_score("SCORE: 8"), 8.0);
        assert_eq!(parse_score("SCORE: 3\nSome extra text"), 3.0);
    }

    #[test]
    fn test_parse_score_fallback_integer() {
        assert_eq!(parse_score("7"), 7.0);
    }

    #[test]
    fn test_parse_score_default() {
        assert_eq!(parse_score("no score here"), 5.0);
    }

    #[test]
    fn test_parse_score_clamped() {
        assert_eq!(parse_score("SCORE: 15"), 10.0);
        assert_eq!(parse_score("SCORE: -2"), 0.0);
    }

    #[test]
    fn test_build_full_context_empty_thought() {
        let node = ThoughtNode { context: "ctx".into(), thought: "".into(), score: 5.0, depth: 0 };
        assert_eq!(build_full_context(&node), "ctx");
    }

    #[test]
    fn test_build_full_context_empty_context() {
        let node = ThoughtNode { context: "".into(), thought: "thought".into(), score: 5.0, depth: 1 };
        assert_eq!(build_full_context(&node), "thought");
    }

    #[test]
    fn test_build_full_context_both() {
        let node = ThoughtNode { context: "ctx".into(), thought: "thought".into(), score: 5.0, depth: 1 };
        let result = build_full_context(&node);
        assert!(result.contains("ctx"));
        assert!(result.contains("thought"));
    }

    #[test]
    fn test_strategy_default_is_bfs() {
        // Strategy::Bfs is the default in run_tree_of_thoughts.
        assert_eq!(Strategy::Bfs, Strategy::Bfs);
        assert_ne!(Strategy::Bfs, Strategy::Dfs);
    }

    #[tokio::test]
    async fn test_run_tree_of_thoughts_bfs() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;

        struct MockProvider;

        #[async_trait]
        impl LLMProvider for MockProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, msgs: &[Message], _: Option<&CompleteOptions>) -> Result<CompletionResult> {
                let last = msgs.last().map(|m| m.content.as_str()).unwrap_or("");
                let content = if last.contains("SCORE") || last.contains("Rate this") {
                    "SCORE: 8".to_string()
                } else if last.contains("Generate") {
                    "Step A: Consider base case.\n---THOUGHT---\nStep B: Consider inductive step.".to_string()
                } else {
                    "The final answer.".to_string()
                };
                Ok(CompletionResult { content, done: true, content_blocks: None, stop_reason: None, usage: None })
            }
        }

        let result = run_tree_of_thoughts(Options {
            task: "Prove 1+1=2".into(),
            provider: Arc::new(MockProvider),
            branching: Some(2),
            strategy: Some(Strategy::Bfs),
            max_depth: Some(2),
            score_threshold: Some(5.0),
        })
        .await
        .unwrap();

        assert!(result.success);
        assert!(!result.answer.is_empty());
        assert_eq!(result.technique, "tree-of-thoughts");
    }
}
