use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use crate::providers::provider::{LLMProvider, Message};

/// A single round of the critique loop: an agent response, its critique, and
/// the numeric quality score (0–10) the critic assigned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CritiqueRound {
    /// The agent's output for this round.
    pub response: String,
    /// The critic's assessment of that output.
    pub critique: String,
    /// Quality score assigned by the critic (0.0 – 10.0).
    pub score: f64,
}

/// The final result of a complete critique-loop run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CritiqueLoopResult {
    /// The best answer produced (from the last accepted round).
    pub answer: String,
    /// All rounds executed, in order.
    pub rounds: Vec<CritiqueRound>,
}

/// Options for [`run_critique_loop`].
pub struct CritiqueLoopOptions {
    /// The task or question to address.
    pub task: String,
    /// The LLM provider to use for both generation and critique.
    pub provider: Arc<dyn LLMProvider>,
    /// Maximum generate → critique → revise rounds.
    pub max_rounds: usize,
    /// Evaluation criteria injected into the critique prompt.
    pub criteria: Vec<String>,
    /// Score threshold above which the answer is accepted without further
    /// revision (0.0 – 10.0).
    pub satisfaction_threshold: f64,
}

/// Run the critique loop: generate an answer, critique it, optionally revise.
///
/// Returns early if the critic assigns a score ≥ `satisfaction_threshold`.
pub async fn run_critique_loop(options: CritiqueLoopOptions) -> Result<CritiqueLoopResult> {
    let mut rounds: Vec<CritiqueRound> = Vec::new();
    let mut current_response = String::new();
    let mut history: Vec<Message> = Vec::new();

    for round in 0..options.max_rounds.max(1) {
        // ── Generation ────────────────────────────────────────────────────────
        let generation_prompt = if round == 0 {
            options.task.clone()
        } else {
            let last_critique = rounds.last().map(|r| r.critique.as_str()).unwrap_or("");
            format!(
                "{}\n\nPrevious critique:\n{}\n\nPlease revise your answer accordingly.",
                options.task, last_critique
            )
        };

        history.push(Message::user(generation_prompt));
        let gen_result = options.provider.complete(&history, None).await?;
        current_response = gen_result.content.clone();
        history.push(Message::assistant(&current_response));

        // ── Critique ──────────────────────────────────────────────────────────
        let criteria_text = if options.criteria.is_empty() {
            "correctness, completeness, and clarity".to_string()
        } else {
            options
                .criteria
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{}. {}", i + 1, c))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let critique_prompt = format!(
            "You are a rigorous reviewer. Evaluate the following answer on a scale of 0–10.\n\
             Criteria:\n{}\n\n\
             Answer to evaluate:\n{}\n\n\
             Respond with:\n\
             SCORE: <integer 0-10>\n\
             CRITIQUE: <detailed feedback>",
            criteria_text, current_response
        );

        let critique_messages = vec![Message::user(critique_prompt)];
        let critique_result = options.provider.complete(&critique_messages, None).await?;
        let critique_text = critique_result.content.clone();

        let score = parse_score(&critique_text);

        rounds.push(CritiqueRound {
            response: current_response.clone(),
            critique: critique_text,
            score,
        });

        if score >= options.satisfaction_threshold {
            break;
        }
    }

    Ok(CritiqueLoopResult {
        answer: current_response,
        rounds,
    })
}

/// Parse `SCORE: <n>` from a critique response.  Returns 0.0 on parse failure.
fn parse_score(text: &str) -> f64 {
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SCORE:") {
            if let Ok(n) = rest.trim().parse::<f64>() {
                return n.clamp(0.0, 10.0);
            }
        }
    }
    0.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_score_found() {
        assert_eq!(parse_score("SCORE: 8\nCRITIQUE: Good work"), 8.0);
    }

    #[test]
    fn test_parse_score_float() {
        assert_eq!(parse_score("SCORE: 7.5\nCRITIQUE: Almost there"), 7.5);
    }

    #[test]
    fn test_parse_score_not_found() {
        assert_eq!(parse_score("No score here"), 0.0);
    }

    #[test]
    fn test_parse_score_clamps() {
        assert_eq!(parse_score("SCORE: 15"), 10.0);
        assert_eq!(parse_score("SCORE: -3"), 0.0);
    }

    #[test]
    fn test_critique_round_serde() {
        let round = CritiqueRound {
            response: "answer".into(),
            critique: "good".into(),
            score: 9.0,
        };
        let json = serde_json::to_string(&round).unwrap();
        let decoded: CritiqueRound = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.score, 9.0);
    }
}
