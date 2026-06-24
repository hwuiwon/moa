//! Deterministic fallback for agent self-assessment scoring.

/// Converts the agent's final response into a conservative fallback self-assessment signal.
#[must_use]
pub fn score(last_response: Option<&str>) -> Option<f64> {
    let response = last_response?.trim();
    if response.is_empty() {
        return None;
    }

    if response.ends_with('?') {
        return Some(0.4);
    }

    Some(0.5)
}

#[cfg(test)]
mod tests {
    use super::score;

    #[test]
    fn declarative_response_is_neutral_without_llm() {
        // Pins: wording changes do not change segment outcomes without deterministic evidence.
        assert_eq!(score(Some("Done, the file has been updated.")), Some(0.5));
        assert_eq!(
            score(Some("I couldn't find the requested file.")),
            Some(0.5)
        );
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
