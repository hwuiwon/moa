//! User-continuation signal for deferred task-resolution scoring.

use std::collections::{HashMap, HashSet};

/// Inputs used to score how a user continued after a segment ended.
#[derive(Debug, Clone, Copy)]
pub struct ContinuationInput<'a> {
    /// Next user message, if one exists.
    pub next_user_message: Option<&'a str>,
    /// Initial user query for the completed segment.
    pub initial_query: Option<&'a str>,
    /// Whether the query rewriter classified the next message as a new task.
    pub is_new_task: bool,
}

/// Scores the next user message as implicit continuation feedback.
#[must_use]
pub fn score(input: ContinuationInput<'_>, rephrase_similarity_threshold: f64) -> Option<f64> {
    let message = input.next_user_message?.trim();
    if message.is_empty() {
        return None;
    }

    let lower = message.to_ascii_lowercase();
    if is_correction(&lower) {
        return Some(0.15);
    }
    if is_acknowledgment(&lower) {
        return Some(0.85);
    }
    if let Some(initial_query) = input.initial_query
        && lexical_cosine_similarity(initial_query, message) >= rephrase_similarity_threshold
    {
        return Some(0.1);
    }
    if input.is_new_task {
        return Some(0.75);
    }

    Some(0.7)
}

fn is_acknowledgment(message: &str) -> bool {
    let trimmed = message.trim_matches(|character: char| {
        character.is_ascii_punctuation() || character.is_whitespace()
    });
    [
        "thanks",
        "thank you",
        "got it",
        "perfect",
        "works",
        "looks good",
        "great",
        "done",
    ]
    .iter()
    .any(|phrase| trimmed == *phrase || trimmed.starts_with(&format!("{phrase} ")))
}

fn is_correction(message: &str) -> bool {
    [
        "no ",
        "no,",
        "wrong",
        "not what i meant",
        "that's not what i meant",
        "that is not what i meant",
        "try again",
        "doesn't work",
        "does not work",
        "still failing",
        "still broken",
    ]
    .iter()
    .any(|phrase| message.starts_with(phrase) || message.contains(phrase))
}

fn lexical_cosine_similarity(left: &str, right: &str) -> f64 {
    let left_tokens = token_counts(left);
    let right_tokens = token_counts(right);
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0.0;
    }

    let vocabulary = left_tokens
        .keys()
        .chain(right_tokens.keys())
        .collect::<HashSet<_>>();
    let dot = vocabulary
        .iter()
        .map(|token| {
            let left_count = f64::from(*left_tokens.get(*token).unwrap_or(&0));
            let right_count = f64::from(*right_tokens.get(*token).unwrap_or(&0));
            left_count * right_count
        })
        .sum::<f64>();
    let left_norm = left_tokens
        .values()
        .map(|count| f64::from(*count).powi(2))
        .sum::<f64>()
        .sqrt();
    let right_norm = right_tokens
        .values()
        .map(|count| f64::from(*count).powi(2))
        .sum::<f64>()
        .sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * right_norm)
    }
}

fn token_counts(value: &str) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for token in value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 2)
    {
        *counts.entry(token).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::{ContinuationInput, score};

    #[test]
    fn thanks_scores_acknowledgment_high() {
        let input = ContinuationInput {
            next_user_message: Some("thanks"),
            initial_query: Some("Fix cargo test"),
            is_new_task: false,
        };

        assert_eq!(score(input, 0.85), Some(0.85));
    }

    #[test]
    fn correction_scores_low() {
        let input = ContinuationInput {
            next_user_message: Some("no that's wrong"),
            initial_query: Some("Fix cargo test"),
            is_new_task: false,
        };

        assert_eq!(score(input, 0.85), Some(0.15));
    }

    #[test]
    fn rephrase_scores_low() {
        let input = ContinuationInput {
            next_user_message: Some("please fix cargo test failure"),
            initial_query: Some("please fix cargo test failure"),
            is_new_task: false,
        };

        assert_eq!(score(input, 0.85), Some(0.1));
    }

    #[test]
    fn new_task_scores_likely_resolved() {
        let input = ContinuationInput {
            next_user_message: Some("now update the README"),
            initial_query: Some("Fix cargo test"),
            is_new_task: true,
        };

        assert_eq!(score(input, 0.85), Some(0.75));
    }

    #[test]
    fn no_next_message_defers_signal() {
        let input = ContinuationInput {
            next_user_message: None,
            initial_query: Some("Fix cargo test"),
            is_new_task: false,
        };

        assert_eq!(score(input, 0.85), None);
    }
}
