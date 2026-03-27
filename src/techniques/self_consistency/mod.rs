//! Self-Consistency: sample N independent reasoning chains, majority-vote answer.
//!
//! Each chain is a single zero-shot completion at the given temperature.
//! The final answer is selected by:
//! - **exact majority vote** when any answer string appears in more than half
//!   the chains, or
//! - **semantic clustering**: if no majority exists, ask the LLM to pick the
//!   most consistent answer from the set.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

const DEFAULT_SAMPLES: usize = 5;
const DEFAULT_TEMPERATURE: f64 = 0.7;

/// Options for [`run_self_consistency`].
pub struct Options {
    /// The question or task.
    pub task: String,
    /// LLM provider.
    pub provider: Arc<dyn LLMProvider>,
    /// Number of independent chains to sample.
    pub samples: Option<usize>,
    /// Sampling temperature.  Higher values increase diversity.
    pub temperature: Option<f64>,
}

/// Run the Self-Consistency technique.
pub async fn run_self_consistency(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let samples = opts.samples.unwrap_or(DEFAULT_SAMPLES);
    let temperature = opts.temperature.unwrap_or(DEFAULT_TEMPERATURE);

    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls: usize = 0;
    let mut answers: Vec<String> = Vec::new();

    let complete_opts = CompleteOptions {
        temperature: Some(temperature),
        max_tokens: Some(1024),
    };

    // Sample `samples` independent chains.
    for i in 0..samples {
        let msgs = vec![Message::user(&opts.task)];
        let completion = opts.provider.complete(&msgs, Some(&complete_opts)).await?;
        llm_calls += 1;

        let chain_answer = completion.content.trim().to_string();
        answers.push(chain_answer.clone());

        trajectory.push(TrajectoryStep {
            index: i,
            step_type: StepType::Thought,
            content: chain_answer,
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("chain".to_string(), serde_json::json!(i));
                m.insert("temperature".to_string(), serde_json::json!(temperature));
                m
            },
        });
    }

    // Attempt exact majority vote.
    let final_answer = match majority_vote(&answers) {
        Some(answer) => answer,
        None => {
            // Fall back to LLM-based consensus selection.
            let answer = llm_consensus(&opts.task, &answers, &opts.provider).await?;
            llm_calls += 1;
            answer
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    // Record the aggregation step.
    trajectory.push(TrajectoryStep {
        index: samples,
        step_type: StepType::Observation,
        content: format!("Consensus answer: {}", final_answer),
        timestamp: chrono::Utc::now().to_rfc3339(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("aggregation".to_string(), serde_json::json!("majority-vote"));
            m
        },
    });

    Ok(RunResult::success(
        "self-consistency",
        final_answer,
        trajectory,
        llm_calls,
        duration_ms,
    ))
}

// ---------------------------------------------------------------------------
// Aggregation helpers
// ---------------------------------------------------------------------------

/// Return the majority answer if one exists (appears in > samples/2 chains).
pub fn majority_vote(answers: &[String]) -> Option<String> {
    if answers.is_empty() {
        return None;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for a in answers {
        *counts.entry(a.as_str()).or_insert(0) += 1;
    }
    let threshold = answers.len() / 2 + 1;
    counts
        .into_iter()
        .filter(|(_, c)| *c >= threshold)
        .max_by_key(|(_, c)| *c)
        .map(|(answer, _)| answer.to_string())
}

/// Ask the LLM to pick the most consistent answer from `answers`.
async fn llm_consensus(
    task: &str,
    answers: &[String],
    provider: &Arc<dyn LLMProvider>,
) -> Result<String> {
    let numbered: String = answers
        .iter()
        .enumerate()
        .map(|(i, a)| format!("{}. {}", i + 1, a))
        .collect::<Vec<_>>()
        .join("\n");

    let prompt = format!(
        "Task: {}\n\nThe following answers were independently generated:\n{}\n\n\
         Select or synthesise the most consistent and correct answer. \
         Respond with only the final answer text.",
        task, numbered
    );
    let msgs = vec![Message::user(prompt)];
    let result = provider
        .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(512), ..Default::default() }))
        .await?;
    Ok(result.content.trim().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_majority_vote_clear_winner() {
        let answers = vec![
            "42".to_string(),
            "42".to_string(),
            "43".to_string(),
            "42".to_string(),
        ];
        assert_eq!(majority_vote(&answers), Some("42".to_string()));
    }

    #[test]
    fn test_majority_vote_no_majority() {
        let answers = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
        ];
        assert!(majority_vote(&answers).is_none());
    }

    #[test]
    fn test_majority_vote_empty() {
        assert!(majority_vote(&[]).is_none());
    }

    #[test]
    fn test_majority_vote_single() {
        let answers = vec!["only".to_string()];
        assert_eq!(majority_vote(&answers), Some("only".to_string()));
    }

    #[test]
    fn test_majority_vote_exact_half_no_majority() {
        let answers = vec!["yes".to_string(), "yes".to_string(), "no".to_string(), "no".to_string()];
        // 2 out of 4 — threshold is 3, so no majority.
        assert!(majority_vote(&answers).is_none());
    }

    #[tokio::test]
    async fn test_run_self_consistency_returns_majority() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SteppedProvider {
            call: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl LLMProvider for SteppedProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, _msgs: &[Message], _opts: Option<&CompleteOptions>) -> Result<CompletionResult> {
                let n = self.call.fetch_add(1, Ordering::SeqCst);
                // Chains 0,1,2 say "42"; chain 3 says "43" — majority is "42".
                let content = if n < 3 { "42" } else { "43" };
                Ok(CompletionResult { content: content.into(), done: true })
            }
        }

        let result = run_self_consistency(Options {
            task: "What is 6*7?".into(),
            provider: Arc::new(SteppedProvider { call: Arc::new(AtomicUsize::new(0)) }),
            samples: Some(4),
            temperature: Some(0.7),
        })
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(result.answer, "42");
        assert_eq!(result.technique, "self-consistency");
        assert_eq!(result.llm_calls, 4); // 4 chains, no consensus call needed.
    }
}
