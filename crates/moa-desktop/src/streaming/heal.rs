//! Streaming-markdown self-healing — port of npm `remend` (Vercel
//! Streamdown). Auto-closes unterminated emphasis, bold, italic, and
//! inline-code markers so partial AI output renders cleanly when the
//! UI feeds it through the markdown pipeline mid-stream.
//!
//! Faithful port of the core handlers (bold, italic asterisk, italic
//! double-underscore, italic single-underscore, bold-italic, inline
//! code). Less common cases (strikethrough, links, KaTeX, HTML tags,
//! setext headings, comparison operators, single-tilde escape) are
//! deferred until a concrete need surfaces.

/// Heals a streaming markdown chunk: trims a trailing single space and
/// closes any unterminated bold / italic / inline-code markers so the
/// markdown renderer doesn't consume the rest of the bubble.
pub fn heal(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    // remend trims a single trailing space (but not a hard-break "  ").
    let trimmed: &str = if text.ends_with(' ') && !text.ends_with("  ") {
        &text[..text.len() - 1]
    } else {
        text
    };

    let mut s = trimmed.to_string();
    s = handle_incomplete_bold_italic(&s);
    s = handle_incomplete_bold(&s);
    s = handle_incomplete_double_underscore_italic(&s);
    s = handle_incomplete_single_asterisk_italic(&s);
    s = handle_incomplete_single_underscore_italic(&s);
    s = handle_incomplete_inline_code(&s);
    s
}

// ---------------------------------------------------------------------------
// Character classification
// ---------------------------------------------------------------------------

fn is_whitespace_byte(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\n' || b == b'\r'
}

// ---------------------------------------------------------------------------
// Code-block awareness
// ---------------------------------------------------------------------------

/// True when `position` falls inside an open fenced (```` ``` ````) or
/// unbalanced inline (`` ` ``) code span counted from the start.
fn is_inside_code_block(text: &str, position: usize) -> bool {
    let bytes = text.as_bytes();
    let limit = position.min(bytes.len());
    let mut in_inline = false;
    let mut in_multiline = false;
    let mut i = 0;
    while i < limit {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'`' {
            i += 2;
            continue;
        }
        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"```" {
            in_multiline = !in_multiline;
            i += 3;
            continue;
        }
        if !in_multiline && bytes[i] == b'`' {
            in_inline = !in_inline;
        }
        i += 1;
    }
    in_inline || in_multiline
}

/// True when `position` is inside a fully-closed inline code span.
/// Streaming/incomplete spans don't count — we still want emphasis
/// completion to fire inside half-typed inline code.
fn is_within_complete_inline_code(text: &str, position: usize) -> bool {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut in_inline = false;
    let mut in_multiline = false;
    let mut inline_start: Option<usize> = None;
    let mut i = 0;
    while i < n {
        if bytes[i] == b'\\' && i + 1 < n && bytes[i + 1] == b'`' {
            i += 2;
            continue;
        }
        if i + 2 < n && &bytes[i..i + 3] == b"```" {
            in_multiline = !in_multiline;
            i += 3;
            continue;
        }
        if !in_multiline && bytes[i] == b'`' {
            if in_inline {
                if let Some(start) = inline_start
                    && start < position
                    && position < i
                {
                    return true;
                }
                in_inline = false;
                inline_start = None;
            } else {
                in_inline = true;
                inline_start = Some(i);
            }
        }
        i += 1;
    }
    false
}

/// True when `position` lies inside an `[text](url` URL part on the same line.
fn is_within_link_or_image_url(text: &str, position: usize) -> bool {
    let bytes = text.as_bytes();
    if position == 0 {
        return false;
    }
    let mut i = position - 1;
    loop {
        let c = bytes[i];
        if c == b')' || c == b'\n' {
            return false;
        }
        if c == b'(' {
            if i > 0 && bytes[i - 1] == b']' {
                // Inside URL; verify a closing `)` exists later on this line.
                let mut j = position;
                while j < bytes.len() {
                    match bytes[j] {
                        b')' => return true,
                        b'\n' => return false,
                        _ => {}
                    }
                    j += 1;
                }
                return false;
            }
            return false;
        }
        if i == 0 {
            return false;
        }
        i -= 1;
    }
}

/// True when `position` lies inside an `<html attr=...>` tag on the same line.
fn is_within_html_tag(text: &str, position: usize) -> bool {
    let bytes = text.as_bytes();
    if position == 0 {
        return false;
    }
    let mut i = position - 1;
    loop {
        let c = bytes[i];
        if c == b'>' || c == b'\n' {
            return false;
        }
        if c == b'<' {
            let next = bytes.get(i + 1).copied().unwrap_or(0);
            return next.is_ascii_alphabetic() || next == b'/';
        }
        if i == 0 {
            return false;
        }
        i -= 1;
    }
}

/// True when `position` falls inside a `$...$` or `$$...$$` math span.
/// Skips escaped `\$`.
fn is_within_math_block(text: &str, position: usize) -> bool {
    let bytes = text.as_bytes();
    let limit = position.min(bytes.len());
    let mut in_inline = false;
    let mut in_block = false;
    let mut i = 0;
    while i < limit {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'$' {
            i += 2;
            continue;
        }
        if bytes[i] == b'$' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                in_block = !in_block;
                i += 2;
                in_inline = false;
                continue;
            }
            if !in_block {
                in_inline = !in_inline;
            }
        }
        i += 1;
    }
    in_inline || in_block
}

/// True when the `marker` run on the line at `marker_index` is a valid
/// horizontal-rule (≥3 markers + only whitespace on the line).
fn is_horizontal_rule(text: &str, marker_index: usize, marker: u8) -> bool {
    let bytes = text.as_bytes();
    let mut line_start = 0;
    if marker_index > 0 {
        let mut i = marker_index - 1;
        loop {
            if bytes[i] == b'\n' {
                line_start = i + 1;
                break;
            }
            if i == 0 {
                break;
            }
            i -= 1;
        }
    }
    let mut line_end = bytes.len();
    let mut j = marker_index;
    while j < bytes.len() {
        if bytes[j] == b'\n' {
            line_end = j;
            break;
        }
        j += 1;
    }
    let mut marker_count = 0usize;
    for &c in &bytes[line_start..line_end] {
        if c == marker {
            marker_count += 1;
        } else if c != b' ' && c != b'\t' {
            return false;
        }
    }
    marker_count >= 3
}

// ---------------------------------------------------------------------------
// Counters (skip content inside fenced code blocks)
// ---------------------------------------------------------------------------

fn count_double_asterisks_outside_code(text: &str) -> usize {
    count_double_marker_outside_code(text, b'*')
}

fn count_double_underscores_outside_code(text: &str) -> usize {
    count_double_marker_outside_code(text, b'_')
}

fn count_double_marker_outside_code(text: &str, marker: u8) -> usize {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut count = 0;
    let mut in_code = false;
    let mut i = 0;
    while i < n {
        if i + 2 < n && &bytes[i..i + 3] == b"```" {
            in_code = !in_code;
            i += 3;
            continue;
        }
        if in_code {
            i += 1;
            continue;
        }
        if bytes[i] == marker && i + 1 < n && bytes[i + 1] == marker {
            count += 1;
            i += 2;
        } else {
            i += 1;
        }
    }
    count
}

fn count_triple_asterisks(text: &str) -> usize {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut count = 0;
    let mut consecutive = 0usize;
    let mut in_code = false;
    let mut i = 0;
    while i < n {
        if i + 2 < n && &bytes[i..i + 3] == b"```" {
            count += consecutive / 3;
            consecutive = 0;
            in_code = !in_code;
            i += 3;
            continue;
        }
        if in_code {
            i += 1;
            continue;
        }
        if bytes[i] == b'*' {
            consecutive += 1;
        } else {
            count += consecutive / 3;
            consecutive = 0;
        }
        i += 1;
    }
    count + consecutive / 3
}

fn count_single_asterisks(text: &str) -> usize {
    count_single_marker(text, b'*', should_skip_asterisk)
}

fn count_single_underscores(text: &str) -> usize {
    count_single_marker(text, b'_', should_skip_underscore)
}

fn count_single_marker<F>(text: &str, marker: u8, mut skip: F) -> usize
where
    F: FnMut(&str, usize, u8, u8) -> bool,
{
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut count = 0;
    let mut in_code = false;
    let mut i = 0;
    while i < n {
        if i + 2 < n && &bytes[i..i + 3] == b"```" {
            in_code = !in_code;
            i += 3;
            continue;
        }
        if in_code {
            i += 1;
            continue;
        }
        if bytes[i] != marker {
            i += 1;
            continue;
        }
        let prev = if i > 0 { bytes[i - 1] } else { 0 };
        let next = if i + 1 < n { bytes[i + 1] } else { 0 };
        if !skip(text, i, prev, next) {
            count += 1;
        }
        i += 1;
    }
    count
}

fn should_skip_asterisk(text: &str, index: usize, prev: u8, next: u8) -> bool {
    if prev == b'\\' {
        return true;
    }
    if text.as_bytes().contains(&b'$') && is_within_math_block(text, index) {
        return true;
    }
    // *** sequences: the first * in *** can close a single-* italic.
    if prev != b'*' && next == b'*' {
        let next_next = text.as_bytes().get(index + 2).copied().unwrap_or(0);
        if next_next == b'*' {
            return false;
        }
        return true;
    }
    if prev == b'*' {
        return true;
    }
    if prev != 0 && next != 0 && byte_is_word_char(prev) && byte_is_word_char(next) {
        return true;
    }
    let prev_ws = prev == 0 || is_whitespace_byte(prev);
    let next_ws = next == 0 || is_whitespace_byte(next);
    prev_ws && next_ws
}

fn should_skip_underscore(text: &str, index: usize, prev: u8, next: u8) -> bool {
    if prev == b'\\' {
        return true;
    }
    if text.as_bytes().contains(&b'$') && is_within_math_block(text, index) {
        return true;
    }
    if is_within_link_or_image_url(text, index) {
        return true;
    }
    if is_within_html_tag(text, index) {
        return true;
    }
    if prev == b'_' || next == b'_' {
        return true;
    }
    if prev != 0 && next != 0 && byte_is_word_char(prev) && byte_is_word_char(next) {
        return true;
    }
    false
}

fn byte_is_word_char(b: u8) -> bool {
    if b == b'_' {
        return true;
    }
    if b.is_ascii_alphanumeric() {
        return true;
    }
    // Rare: non-ASCII bytes — fall back to char check via the leading
    // byte. Multi-byte starts are >= 0xC0; we accept them as "word"
    // since the typical case (letters/digits in any script) qualifies.
    b >= 0xC0
}

fn count_single_backticks(text: &str) -> usize {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut count = 0;
    let mut i = 0;
    while i < n {
        if bytes[i] == b'\\' && i + 1 < n && bytes[i + 1] == b'`' {
            i += 2;
            continue;
        }
        if bytes[i] == b'`' && !is_part_of_triple_backtick(bytes, i) {
            count += 1;
        }
        i += 1;
    }
    count
}

fn is_part_of_triple_backtick(bytes: &[u8], i: usize) -> bool {
    let n = bytes.len();
    let three = |start: usize| -> bool {
        start + 2 < n
            && bytes[start] == b'`'
            && bytes[start + 1] == b'`'
            && bytes[start + 2] == b'`'
    };
    let triple_start = three(i);
    let triple_middle = i > 0 && three(i - 1);
    let triple_end = i > 1 && three(i - 2);
    triple_start || triple_middle || triple_end
}

// ---------------------------------------------------------------------------
// Pattern matchers (anchored at end of string)
// ---------------------------------------------------------------------------

/// Returns the trailing content after the rightmost `**` if the
/// substring matches `[^*]*\*?$`. Mirrors `boldPattern`.
fn match_bold_at_end(text: &str) -> Option<(usize, &str)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n < 2 {
        return None;
    }
    // Rightmost `**` such that what follows has 0 or 1 `*` (only at end).
    let mut idx = text.rfind("**")?;
    loop {
        let after = &text[idx + 2..];
        let asterisks = after.bytes().filter(|&b| b == b'*').count();
        if asterisks == 0 || (asterisks == 1 && after.ends_with('*')) {
            return Some((idx, after));
        }
        if idx == 0 {
            return None;
        }
        idx = text[..idx].rfind("**")?;
    }
}

fn match_double_underscore_at_end(text: &str) -> Option<(usize, &str)> {
    let idx = text.rfind("__")?;
    let after = &text[idx + 2..];
    if !after.contains('_') {
        Some((idx, after))
    } else {
        None
    }
}

fn match_half_complete_underscore(text: &str) -> Option<(usize, &str)> {
    // /(__)([^_]+)_$/
    if !text.ends_with('_') || text.ends_with("__") {
        return None;
    }
    let body = &text[..text.len() - 1];
    let idx = body.rfind("__")?;
    let middle = &body[idx + 2..];
    if middle.is_empty() || middle.contains('_') {
        return None;
    }
    Some((idx, middle))
}

fn match_bold_italic_at_end(text: &str) -> Option<(usize, &str)> {
    let idx = text.rfind("***")?;
    let after = &text[idx + 3..];
    if !after.contains('*') {
        Some((idx, after))
    } else {
        None
    }
}

fn match_single_asterisk_at_end(text: &str) -> Option<(usize, &str)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return None;
    }
    // The rightmost single `*` not adjacent to another `*` such that the
    // tail after it has no further `*`.
    let mut i = n;
    while i > 0 {
        i -= 1;
        if bytes[i] != b'*' {
            continue;
        }
        let prev = if i > 0 { bytes[i - 1] } else { 0 };
        let next = if i + 1 < n { bytes[i + 1] } else { 0 };
        if prev == b'*' || next == b'*' {
            continue;
        }
        let after = &text[i + 1..];
        if !after.contains('*') {
            return Some((i, after));
        }
    }
    None
}

fn match_single_underscore_at_end(text: &str) -> Option<(usize, &str)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return None;
    }
    let mut i = n;
    while i > 0 {
        i -= 1;
        if bytes[i] != b'_' {
            continue;
        }
        let prev = if i > 0 { bytes[i - 1] } else { 0 };
        let next = if i + 1 < n { bytes[i + 1] } else { 0 };
        if prev == b'_' || next == b'_' {
            continue;
        }
        let after = &text[i + 1..];
        if !after.contains('_') {
            return Some((i, after));
        }
    }
    None
}

fn match_inline_code_at_end(text: &str) -> Option<(usize, &str)> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return None;
    }
    let mut i = n;
    while i > 0 {
        i -= 1;
        if bytes[i] != b'`' {
            continue;
        }
        let after = &text[i + 1..];
        if !after.contains('`') {
            return Some((i, after));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Skip predicates
// ---------------------------------------------------------------------------

fn content_is_empty_or_only_markers(content: &str) -> bool {
    content
        .chars()
        .all(|c| c.is_whitespace() || matches!(c, '_' | '~' | '*' | '`'))
}

fn line_is_list_item_marker(text: &str, marker_index: usize) -> bool {
    // /^[\s]*[-*+][\s]+$/
    let bytes = text.as_bytes();
    let line_start = bytes[..marker_index]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    let line = &text[line_start..marker_index];
    let trimmed = line.trim_start_matches([' ', '\t']);
    let mut chars = trimmed.chars();
    let Some(marker) = chars.next() else {
        return false;
    };
    if !matches!(marker, '-' | '*' | '+') {
        return false;
    }
    let rest = chars.as_str();
    !rest.is_empty() && rest.chars().all(|c| c == ' ' || c == '\t')
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_incomplete_bold(text: &str) -> String {
    let Some((marker_index, content_after)) = match_bold_at_end(text) else {
        return text.to_string();
    };
    if is_inside_code_block(text, marker_index)
        || is_within_complete_inline_code(text, marker_index)
    {
        return text.to_string();
    }
    if content_is_empty_or_only_markers(content_after) {
        return text.to_string();
    }
    if line_is_list_item_marker(text, marker_index) && content_after.contains('\n') {
        return text.to_string();
    }
    if is_horizontal_rule(text, marker_index, b'*') {
        return text.to_string();
    }
    let pairs = count_double_asterisks_outside_code(text);
    if pairs % 2 == 1 {
        if content_after.ends_with('*') {
            return format!("{text}*");
        }
        return format!("{text}**");
    }
    text.to_string()
}

fn handle_incomplete_double_underscore_italic(text: &str) -> String {
    if let Some((marker_index, content_after)) = match_double_underscore_at_end(text) {
        if is_inside_code_block(text, marker_index)
            || is_within_complete_inline_code(text, marker_index)
        {
            return text.to_string();
        }
        if content_is_empty_or_only_markers(content_after) {
            return text.to_string();
        }
        if line_is_list_item_marker(text, marker_index) && content_after.contains('\n') {
            return text.to_string();
        }
        if is_horizontal_rule(text, marker_index, b'_') {
            return text.to_string();
        }
        let pairs = count_double_underscores_outside_code(text);
        if pairs % 2 == 1 {
            return format!("{text}__");
        }
        return text.to_string();
    }
    // Half-complete __content_ → __content__.
    if let Some((marker_index, _)) = match_half_complete_underscore(text)
        && !(is_inside_code_block(text, marker_index)
            || is_within_complete_inline_code(text, marker_index))
        && count_double_underscores_outside_code(text) % 2 == 1
    {
        return format!("{text}_");
    }
    text.to_string()
}

fn handle_incomplete_single_asterisk_italic(text: &str) -> String {
    let Some((index, content_after)) = match_single_asterisk_at_end(text) else {
        return text.to_string();
    };
    if is_inside_code_block(text, index) || is_within_complete_inline_code(text, index) {
        return text.to_string();
    }
    if content_is_empty_or_only_markers(content_after) {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let prev = if index > 0 { bytes[index - 1] } else { 0 };
    let next = bytes.get(index + 1).copied().unwrap_or(0);
    let prev_ws = prev == 0 || is_whitespace_byte(prev);
    let next_ws = next == 0 || is_whitespace_byte(next);
    if prev_ws && next_ws {
        return text.to_string();
    }
    if byte_is_word_char(prev) && byte_is_word_char(next) {
        return text.to_string();
    }
    if count_single_asterisks(text) % 2 == 1 {
        return format!("{text}*");
    }
    text.to_string()
}

fn handle_incomplete_single_underscore_italic(text: &str) -> String {
    let Some((index, content_after)) = match_single_underscore_at_end(text) else {
        return text.to_string();
    };
    if content_is_empty_or_only_markers(content_after) {
        return text.to_string();
    }
    if is_inside_code_block(text, index) || is_within_complete_inline_code(text, index) {
        return text.to_string();
    }
    let bytes = text.as_bytes();
    let prev = if index > 0 { bytes[index - 1] } else { 0 };
    let next = bytes.get(index + 1).copied().unwrap_or(0);
    if byte_is_word_char(prev) && byte_is_word_char(next) {
        return text.to_string();
    }
    if count_single_underscores(text) % 2 == 1 {
        // Insert before any trailing newlines so the closer doesn't
        // fall on a blank line and break paragraph continuity.
        let trail_newlines = text.bytes().rev().take_while(|&b| b == b'\n').count();
        if trail_newlines == 0 {
            return format!("{text}_");
        }
        let cut = text.len() - trail_newlines;
        let mut s = String::with_capacity(text.len() + 1);
        s.push_str(&text[..cut]);
        s.push('_');
        s.push_str(&text[cut..]);
        return s;
    }
    text.to_string()
}

fn handle_incomplete_bold_italic(text: &str) -> String {
    // /^\*{4,}$/ — text consisting of only 4+ asterisks → no completion.
    if !text.is_empty() && text.bytes().all(|b| b == b'*') && text.len() >= 4 {
        return text.to_string();
    }
    let Some((marker_index, content_after)) = match_bold_italic_at_end(text) else {
        return text.to_string();
    };
    if content_is_empty_or_only_markers(content_after) {
        return text.to_string();
    }
    if is_inside_code_block(text, marker_index)
        || is_within_complete_inline_code(text, marker_index)
    {
        return text.to_string();
    }
    if is_horizontal_rule(text, marker_index, b'*') {
        return text.to_string();
    }
    let triples = count_triple_asterisks(text);
    if triples % 2 == 1 {
        // If both ** pairs and singles are balanced, the *** is overlap
        // (e.g. **bold and *italic***) — don't append.
        let pairs = count_double_asterisks_outside_code(text);
        let singles = count_single_asterisks(text);
        if pairs.is_multiple_of(2) && singles.is_multiple_of(2) {
            return text.to_string();
        }
        return format!("{text}***");
    }
    text.to_string()
}

fn handle_incomplete_inline_code(text: &str) -> String {
    // Half-complete inline triple backtick: "```code``" → "```code```"
    // (and only on single-line strings).
    if !text.contains('\n')
        && text.starts_with("```")
        && text.ends_with("``")
        && !text.ends_with("```")
    {
        // Make sure it's *only* one inline triple span, not a block.
        let stripped = &text[3..];
        if !stripped.contains('\n') {
            return format!("{text}`");
        }
    }

    let Some((index, content_after)) = match_inline_code_at_end(text) else {
        return text.to_string();
    };
    // Don't close inside an open fenced block — it would prematurely end
    // a streaming code fence.
    let triples = text.matches("```").count();
    if triples % 2 == 1 {
        return text.to_string();
    }
    if content_is_empty_or_only_markers(content_after) {
        return text.to_string();
    }
    let _ = index;
    if count_single_backticks(text) % 2 == 1 {
        return format!("{text}`");
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_noop() {
        assert_eq!(heal(""), "");
    }

    #[test]
    fn closes_unterminated_bold() {
        assert_eq!(heal("hello **world"), "hello **world**");
    }

    #[test]
    fn completes_half_open_bold_close() {
        // ** ... * → append a single * to finish the closer.
        assert_eq!(heal("**word*"), "**word**");
    }

    #[test]
    fn balanced_bold_is_left_alone() {
        assert_eq!(heal("**foo** bar"), "**foo** bar");
    }

    #[test]
    fn closes_unterminated_italic_asterisk() {
        assert_eq!(heal("a *word"), "a *word*");
    }

    #[test]
    fn closes_unterminated_italic_underscore() {
        assert_eq!(heal("a _word"), "a _word_");
    }

    #[test]
    fn closes_unterminated_double_underscore() {
        assert_eq!(heal("a __word"), "a __word__");
    }

    #[test]
    fn closes_unterminated_inline_code() {
        assert_eq!(heal("call `func"), "call `func`");
    }

    #[test]
    fn does_not_touch_inside_open_fenced_block() {
        // Open ``` then unterminated *italic — italic must stay open
        // because we're inside code. Inline code completion also skipped.
        let input = "```rust\nfn x() { *not italic";
        assert_eq!(heal(input), input);
    }

    #[test]
    fn skips_word_internal_underscore() {
        // snake_case must not be italicized.
        assert_eq!(heal("snake_case_value"), "snake_case_value");
    }

    #[test]
    fn skips_horizontal_rule() {
        assert_eq!(heal("---"), "---");
        assert_eq!(heal("***"), "***");
    }

    #[test]
    fn closes_bold_italic_triple() {
        assert_eq!(heal("***bold and italic"), "***bold and italic***");
    }

    #[test]
    fn trims_single_trailing_space_but_not_double() {
        assert_eq!(heal("hello "), "hello");
        assert_eq!(heal("hello  "), "hello  ");
    }

    #[test]
    fn skips_link_url_internal_underscore() {
        assert_eq!(
            heal("see [docs](https://example.com/some_page"),
            "see [docs](https://example.com/some_page"
        );
    }

    #[test]
    fn does_not_close_when_only_whitespace_after_marker() {
        assert_eq!(heal("foo ** "), "foo **");
    }

    #[test]
    fn nested_emphasis_overlap_balances() {
        // **bold and *italic*** — counts balance, so leave triple alone.
        assert_eq!(heal("**bold and *italic***"), "**bold and *italic***");
    }
}
