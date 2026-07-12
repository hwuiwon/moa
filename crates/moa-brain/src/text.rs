//! Shared text helpers for prompt and context rendering.

use std::borrow::Cow;

/// Truncate `text` to at most `max_chars` characters, counting an appended
/// `"..."` ellipsis *inside* the budget: the returned string is never longer
/// than `max_chars` characters. Truncation is char-boundary safe (it counts
/// Unicode scalar values, not bytes) so it never splits a multi-byte
/// character. When the text already fits, the input is borrowed unchanged.
///
/// If `max_chars <= 3` there is no room for both content and a full ellipsis,
/// so the result is `max_chars` dots.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> Cow<'_, str> {
    // Peek one past the budget to detect overflow without counting the whole
    // string when it is long.
    let mut iter = text.chars();
    let head: String = iter.by_ref().take(max_chars + 1).collect();
    if head.chars().count() <= max_chars {
        return Cow::Borrowed(text);
    }

    if max_chars <= 3 {
        return Cow::Owned(".".repeat(max_chars));
    }

    let prefix: String = head.chars().take(max_chars - 3).collect();
    Cow::Owned(format!("{prefix}..."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_borrowed_unchanged() {
        // Pins: text within budget is returned as-is without allocation.
        assert!(matches!(
            truncate_chars("hello", 10),
            Cow::Borrowed("hello")
        ));
        assert!(matches!(truncate_chars("hello", 5), Cow::Borrowed("hello")));
    }

    #[test]
    fn ellipsis_is_counted_inside_the_budget() {
        // Pins: the truncated output never exceeds max_chars characters.
        let out = truncate_chars("abcdefghij", 5);
        assert_eq!(out, "ab...");
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn tiny_budgets_collapse_to_dots() {
        // Pins: no room for content plus a full ellipsis yields max_chars dots.
        assert_eq!(truncate_chars("abcdef", 3), "...");
        assert_eq!(truncate_chars("abcdef", 1), ".");
        assert_eq!(truncate_chars("abcdef", 0), "");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // Pins: multi-byte characters are never split mid-encoding.
        let out = truncate_chars("héllo wörld", 6);
        assert_eq!(out, "hél...");
        assert_eq!(out.chars().count(), 6);
    }
}
