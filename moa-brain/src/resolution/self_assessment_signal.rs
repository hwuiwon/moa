//! Agent self-assessment signal for task-resolution scoring.

/// Scores the agent's final response in a segment using deterministic language patterns.
#[must_use]
pub fn score(last_response: Option<&str>) -> Option<f64> {
    let response = last_response?.trim();
    if response.is_empty() {
        return None;
    }

    let lower = response.to_ascii_lowercase();
    if contains_any(
        &lower,
        &[
            "i couldn't",
            "i could not",
            "couldn't complete",
            "could not complete",
            "this failed",
            "failed:",
            "error:",
            "unable to complete",
        ],
    ) {
        return Some(0.15);
    }
    if contains_any(
        &lower,
        &[
            "i wasn't able",
            "i was not able",
            "not sure if",
            "might not work",
            "may not work",
            "couldn't verify",
            "could not verify",
        ],
    ) {
        return Some(0.3);
    }
    if contains_any(
        &lower,
        &[
            "could you clarify",
            "can you clarify",
            "what do you mean",
            "which file",
            "which one",
        ],
    ) {
        return Some(0.3);
    }
    if lower.ends_with('?') {
        return Some(0.4);
    }
    if contains_any(
        &lower,
        &[
            "done",
            "completed",
            "i've completed",
            "i have completed",
            "here's the result",
            "changes have been applied",
            "implemented",
            "fixed",
            "updated",
        ],
    ) {
        return Some(0.7);
    }

    Some(0.5)
}

fn contains_any(value: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| value.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn completion_language_scores_high() {
        assert_eq!(score(Some("Done, the file has been updated.")), Some(0.7));
    }

    #[test]
    fn failure_language_scores_low() {
        assert_eq!(
            score(Some("I couldn't find the requested file.")),
            Some(0.15)
        );
    }

    #[test]
    fn clarification_scores_low() {
        assert_eq!(score(Some("Could you clarify which file?")), Some(0.3));
    }

    #[test]
    fn question_scores_below_neutral() {
        assert_eq!(score(Some("Should I keep going?")), Some(0.4));
    }

    #[test]
    fn unclear_response_is_neutral() {
        assert_eq!(score(Some("Here are some notes.")), Some(0.5));
    }
}
