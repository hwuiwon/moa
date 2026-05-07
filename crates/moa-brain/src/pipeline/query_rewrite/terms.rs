//! Token normalization helpers used by rewrite validation and trigger detection.

pub(super) fn entity_terms(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_entity_token)
        .filter(|token| is_entity_like(token))
        .collect()
}

pub(super) fn normalize_entity_token(token: &str) -> String {
    token
        .trim_matches(|character: char| {
            !character.is_alphanumeric()
                && !matches!(character, '_' | '-' | '/' | '.' | ':' | '@' | '#')
        })
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_ascii_lowercase()
}

pub(super) fn is_entity_like(token: &str) -> bool {
    token.len() >= 3 && !is_rewrite_function_word(token)
}

pub(super) fn is_rewrite_function_word(token: &str) -> bool {
    const FUNCTION_WORDS: &[&str] = &[
        "about",
        "add",
        "after",
        "again",
        "all",
        "also",
        "and",
        "answer",
        "around",
        "before",
        "build",
        "can",
        "check",
        "clarify",
        "code",
        "coverage",
        "covering",
        "create",
        "debug",
        "delete",
        "describe",
        "diagnose",
        "edit",
        "explain",
        "file",
        "find",
        "fix",
        "for",
        "from",
        "help",
        "how",
        "implement",
        "into",
        "investigate",
        "issue",
        "make",
        "move",
        "need",
        "please",
        "question",
        "read",
        "remove",
        "request",
        "resolve",
        "review",
        "run",
        "search",
        "show",
        "summarize",
        "task",
        "tell",
        "that",
        "the",
        "then",
        "this",
        "to",
        "update",
        "use",
        "using",
        "what",
        "when",
        "where",
        "which",
        "with",
        "write",
    ];

    FUNCTION_WORDS.contains(&token)
}
