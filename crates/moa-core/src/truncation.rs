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

/// Truncates text by line count using head+tail preservation.
pub fn truncate_head_tail_lines(text: &str, max_lines: usize, head_ratio: f64) -> (String, bool) {
    if max_lines == 0 {
        return (String::new(), !text.is_empty());
    }

    let lines = text.lines().collect::<Vec<_>>();
    if lines.len() <= max_lines {
        return (text.to_string(), false);
    }

    let ratio = head_ratio.clamp(0.0, 1.0);
    let head_lines = ((max_lines as f64) * ratio).round() as usize;
    let head_lines = head_lines.min(max_lines.saturating_sub(1));
    let tail_lines = max_lines.saturating_sub(head_lines);
    let omitted = lines.len().saturating_sub(head_lines + tail_lines);

    let mut result = lines[..head_lines].join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    result.push_str(&format!("[... {} lines omitted ...]", omitted));
    if tail_lines > 0 {
        result.push('\n');
        result.push_str(&lines[lines.len() - tail_lines..].join("\n"));
    }

    (result, true)
}

#[cfg(test)]
mod tests {
    use super::{truncate_head_tail, truncate_head_tail_lines};

    #[test]
    fn head_tail_preserves_both_ends() {
        let input = (1..=100)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");

        let (result, truncated) = truncate_head_tail_lines(&input, 20, 0.4);

        assert!(truncated);
        assert!(result.contains("line 1"));
        assert!(result.contains("line 100"));
        assert!(result.contains("[... 80 lines omitted ...]"));
        assert!(!result.contains("line 50"));
    }

    #[test]
    fn small_output_not_truncated() {
        let input = "hello\nworld\n";
        let (result, truncated) = truncate_head_tail_lines(input, 200, 0.4);

        assert!(!truncated);
        assert_eq!(result, input);
    }

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
