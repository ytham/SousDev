//! Skeleton-of-Thought (SoT): outline first, then expand points in parallel.
//!
//! # Algorithm
//! 1. **Skeleton phase**: ask the LLM for a numbered outline (up to
//!    `max_points` items).
//! 2. **Expansion phase**: for each outline point, call the LLM independently
//!    to expand it into full prose.  When `parallel_expansion` is `true` the
//!    calls run concurrently via `tokio::join!` / `futures::join_all`.
//! 3. **Assembly**: join the expanded sections in order to form the final
//!    answer.
//!
//! This mirrors the original paper's intent of reducing sequential token
//! generation latency by parallelising the expansion step.

use anyhow::Result;
use futures::future::join_all;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

const DEFAULT_MAX_POINTS: usize = 6;

/// Options for [`run_skeleton_of_thought`].
pub struct Options {
    /// The question or task to answer.
    pub task: String,
    /// LLM provider.
    pub provider: Arc<dyn LLMProvider>,
    /// Maximum number of outline points to generate.
    pub max_points: Option<usize>,
    /// Expand outline points concurrently.
    pub parallel_expansion: Option<bool>,
}

/// Run the Skeleton-of-Thought technique.
pub async fn run_skeleton_of_thought(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let max_points = opts.max_points.unwrap_or(DEFAULT_MAX_POINTS);
    let parallel = opts.parallel_expansion.unwrap_or(true);

    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls: usize = 0;

    // ── Phase 1: Skeleton ─────────────────────────────────────────────────────
    let skeleton_prompt = format!(
        "Question: {}\n\n\
         First, write a brief numbered outline of the key points you would \
         cover in a complete answer. List at most {} points.\n\
         Example format:\n1. First point\n2. Second point\n\n\
         Write ONLY the outline — no explanations yet.",
        opts.task, max_points
    );

    let skeleton_completion = opts
        .provider
        .complete(
            &[Message::user(&skeleton_prompt)],
            Some(&CompleteOptions { max_tokens: Some(512), ..Default::default() }),
        )
        .await?;
    llm_calls += 1;

    let skeleton_text = skeleton_completion.content.trim().to_string();
    let points = parse_outline_points(&skeleton_text, max_points);

    trajectory.push(TrajectoryStep {
        index: 0,
        step_type: StepType::Thought,
        content: format!("Skeleton outline:\n{}", skeleton_text),
        timestamp: chrono::Utc::now().to_rfc3339(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("phase".to_string(), serde_json::json!("skeleton"));
            m.insert("point_count".to_string(), serde_json::json!(points.len()));
            m
        },
    });

    if points.is_empty() {
        // Degenerate case: no outline extracted — fall back to direct answer.
        let msgs = vec![Message::user(&opts.task)];
        let completion = opts.provider.complete(&msgs, Some(&CompleteOptions { max_tokens: Some(2048), ..Default::default() })).await?;
        llm_calls += 1;
        let duration_ms = start.elapsed().as_millis() as u64;
        return Ok(RunResult::success("skeleton-of-thought", completion.content.trim(), trajectory, llm_calls, duration_ms));
    }

    // ── Phase 2: Parallel or sequential expansion ─────────────────────────────
    let expanded: Vec<String> = if parallel {
        let futs: Vec<_> = points
            .iter()
            .map(|point| {
                let provider = opts.provider.clone();
                let task = opts.task.clone();
                let point = point.clone();
                async move {
                    expand_point(&task, &point, &provider).await
                }
            })
            .collect();

        let results: Vec<Result<String>> = join_all(futs).await;
        llm_calls += results.len();
        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|e| format!("(expansion error: {})", e)))
            .collect()
    } else {
        let mut expansions = Vec::new();
        for point in &points {
            let text = expand_point(&opts.task, point, &opts.provider).await?;
            llm_calls += 1;
            expansions.push(text);
        }
        expansions
    };

    // Record expansion steps.
    for (i, (point, expansion)) in points.iter().zip(expanded.iter()).enumerate() {
        trajectory.push(TrajectoryStep {
            index: i + 1,
            step_type: StepType::Action,
            content: format!("Point {}: {}\n\n{}", i + 1, point, expansion),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("point_index".to_string(), serde_json::json!(i + 1));
                m.insert("phase".to_string(), serde_json::json!("expansion"));
                m
            },
        });
    }

    // ── Phase 3: Assembly ─────────────────────────────────────────────────────
    let assembled = assemble_answer(&opts.task, &points, &expanded);

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(RunResult::success(
        "skeleton-of-thought",
        assembled,
        trajectory,
        llm_calls,
        duration_ms,
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn expand_point(
    task: &str,
    point: &str,
    provider: &Arc<dyn LLMProvider>,
) -> Result<String> {
    let prompt = format!(
        "You are expanding a single point from the outline of an answer.\n\
         Original question: {}\n\
         Outline point: {}\n\n\
         Write a concise, complete paragraph expanding this point. \
         Do NOT address other parts of the question.",
        task, point
    );
    let msgs = vec![Message::user(prompt)];
    let result = provider
        .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(512), ..Default::default() }))
        .await?;
    Ok(result.content.trim().to_string())
}

/// Assemble the final answer from expanded sections.
pub fn assemble_answer(task: &str, points: &[String], expansions: &[String]) -> String {
    let body: String = points
        .iter()
        .zip(expansions.iter())
        .enumerate()
        .map(|(i, (point, expansion))| {
            format!("**{}. {}**\n{}", i + 1, point, expansion)
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    if body.is_empty() {
        return task.to_string();
    }
    body
}

/// Extract numbered outline points from the LLM's skeleton response.
pub fn parse_outline_points(skeleton: &str, max_points: usize) -> Vec<String> {
    let numbered_re = regex::Regex::new(r"^\d+[.)]\s+(.+)$").unwrap();
    let mut points: Vec<String> = Vec::new();
    for line in skeleton.lines() {
        let trimmed = line.trim();
        if let Some(caps) = numbered_re.captures(trimmed) {
            if let Some(m) = caps.get(1) {
                points.push(m.as_str().trim().to_string());
                if points.len() >= max_points {
                    break;
                }
            }
        }
    }
    points
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_outline_points_numbered_dot() {
        let skeleton = "1. Introduction\n2. Main argument\n3. Conclusion";
        let pts = parse_outline_points(skeleton, 10);
        assert_eq!(pts, vec!["Introduction", "Main argument", "Conclusion"]);
    }

    #[test]
    fn test_parse_outline_points_numbered_paren() {
        let skeleton = "1) Point one\n2) Point two";
        let pts = parse_outline_points(skeleton, 10);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0], "Point one");
    }

    #[test]
    fn test_parse_outline_points_respects_max() {
        let skeleton = "1. A\n2. B\n3. C\n4. D\n5. E";
        let pts = parse_outline_points(skeleton, 3);
        assert_eq!(pts.len(), 3);
    }

    #[test]
    fn test_parse_outline_points_empty() {
        assert!(parse_outline_points("", 5).is_empty());
    }

    #[test]
    fn test_assemble_answer_basic() {
        let points = vec!["Intro".to_string(), "Body".to_string()];
        let expansions = vec!["This is the intro.".to_string(), "This is the body.".to_string()];
        let result = assemble_answer("What is this?", &points, &expansions);
        assert!(result.contains("Intro"));
        assert!(result.contains("Body"));
        assert!(result.contains("intro."));
        assert!(result.contains("body."));
    }

    #[test]
    fn test_assemble_answer_empty_fallback() {
        let result = assemble_answer("My task", &[], &[]);
        assert_eq!(result, "My task");
    }

    #[tokio::test]
    async fn test_run_skeleton_of_thought_basic() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;

        struct MockProvider;
        #[async_trait]
        impl LLMProvider for MockProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, msgs: &[Message], _: Option<&CompleteOptions>) -> Result<CompletionResult> {
                let last = msgs.last().map(|m| m.content.as_str()).unwrap_or("");
                let content = if last.contains("outline") || last.contains("ONLY the outline") {
                    "1. First point\n2. Second point".to_string()
                } else {
                    "Expanded text for this point.".to_string()
                };
                Ok(CompletionResult { content, done: true })
            }
        }

        let result = run_skeleton_of_thought(Options {
            task: "Explain recursion.".into(),
            provider: Arc::new(MockProvider),
            max_points: Some(2),
            parallel_expansion: Some(false),
        })
        .await
        .unwrap();

        assert!(result.success);
        assert!(!result.answer.is_empty());
        assert_eq!(result.technique, "skeleton-of-thought");
        // 1 skeleton call + 2 expansion calls = 3
        assert_eq!(result.llm_calls, 3);
    }
}
