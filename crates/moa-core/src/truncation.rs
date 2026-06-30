//! Shared text truncation utilities for tool output handling.

/// Truncates text using head+tail preservation.
pub fn truncate_head_tail(text: &str, max_chars: usize, head_ratio: f64) -> (String, bool) {
    if max_chars == 0 {
        return (String::new(), !text.is_empty());
    }

    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return (text.to_string(), false);
    }

    let ratio = head_ratio.clamp(0.0, 1.0);
    let marker_budget = 40usize.min(max_chars);
    let content_budget = max_chars.saturating_sub(marker_budget).max(1);
    let head_budget = ((content_budget as f64) * ratio).round() as usize;
    let head_budget = head_budget.min(content_budget.saturating_sub(1));
    let tail_budget = content_budget.saturating_sub(head_budget);

    let head_raw = text.chars().take(head_budget).collect::<String>();
    let tail_raw = text
        .chars()
        .skip(total_chars.saturating_sub(tail_budget))
        .collect::<String>();

    let head_clean = match head_raw.rfind('\n') {
        Some(index) if index > 0 => head_raw[..index].trim_end(),
        _ => head_raw.trim_end(),
    };
    let tail_clean = match tail_raw.find('\n') {
        Some(index) if index + 1 < tail_raw.len() => tail_raw[index + 1..].trim_start(),
        _ => tail_raw.trim_start(),
    };

    let omitted_chars = total_chars
        .saturating_sub(head_clean.chars().count())
        .saturating_sub(tail_clean.chars().count());
    let marker = format!("[... ~{} chars omitted ...]", omitted_chars);

    let truncated = if head_clean.is_empty() {
        format!("{marker}\n{tail_clean}")
    } else if tail_clean.is_empty() {
        format!("{head_clean}\n{marker}")
    } else {
        format!("{head_clean}\n{marker}\n{tail_clean}")
    };

    (truncated, true)
}

#[cfg(test)]
mod tests {
    use super::truncate_head_tail;

    #[test]
    fn char_truncation_preserves_head_and_tail() {
        let input = format!(
            "{}\n{}\n{}",
            "head".repeat(200),
            "middle".repeat(400),
            "tail".repeat(200)
        );

        let (result, truncated) = truncate_head_tail(&input, 200, 0.4);

        assert!(truncated);
        assert!(result.contains("head"));
        assert!(result.contains("tail"));
        assert!(result.contains("[... ~"));
    }
}
