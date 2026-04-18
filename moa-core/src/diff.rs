//! Unified diff helpers shared across tool implementations.

use similar::TextDiff;

/// Computes a unified diff for one text file using the provided context radius.
pub fn compute_unified_diff(path: &str, before: &str, after: &str, context: usize) -> String {
    TextDiff::from_lines(before, after)
        .unified_diff()
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .context_radius(context)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::compute_unified_diff;

    fn approximate_tokens(text: &str) -> u32 {
        let chars = text.chars().count() as u32;
        if chars == 0 { 0 } else { chars.div_ceil(4) }
    }

    #[test]
    fn unified_diff_contains_standard_headers_and_hunks() {
        let before = "fn demo() {\n    alpha();\n    beta();\n}\n";
        let after = "fn demo() {\n    gamma();\n    beta();\n}\n";

        let diff = compute_unified_diff("src/lib.rs", before, after, 3);

        assert!(diff.starts_with("--- a/src/lib.rs\n+++ b/src/lib.rs\n"));
        assert!(diff.contains("@@"));
        assert!(diff.contains("-    alpha();"));
        assert!(diff.contains("+    gamma();"));
    }

    #[test]
    fn small_edit_diff_is_substantially_smaller_than_full_file() {
        let before = (1..=500)
            .map(|line| format!("{line:03}: {}", "x".repeat(64)))
            .collect::<Vec<_>>()
            .join("\n");
        let mut after_lines = before.lines().map(str::to_string).collect::<Vec<_>>();
        after_lines[119] = "120: changed alpha".to_string();
        after_lines[120] = "121: changed beta".to_string();
        after_lines[121] = "122: changed gamma".to_string();
        let after = after_lines.join("\n");

        let diff = compute_unified_diff("notes.txt", &before, &after, 3);

        assert!(approximate_tokens(&diff) * 10 <= approximate_tokens(&after) * 3);
    }
}
