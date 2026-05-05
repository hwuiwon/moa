//! Lightweight entity extraction for retrieval planning.

use std::collections::HashSet;

const DEFAULT_GAZETTEER: &str = include_str!("../../assets/ner-gazetteer.txt");
const MAX_SPANS: usize = 12;
const RELATION_TRIGGERS: &[&str] = &[
    "depends on",
    "connects to",
    "connected to",
    "impacted by",
    "impacts",
    "relates to",
    "relate to",
    "upstream of",
    "downstream of",
];
const STOPWORDS: &[&str] = &[
    "a", "about", "all", "an", "and", "anything", "are", "as", "at", "be", "been", "by", "did",
    "do", "does", "for", "from", "has", "have", "how", "in", "is", "it", "of", "on", "or", "our",
    "that", "the", "this", "to", "was", "we", "what", "when", "where", "which", "who", "why",
    "with",
];

/// Lightweight NER extractor backed by code-aware rules and a small gazetteer.
#[derive(Debug, Clone)]
pub struct NerExtractor {
    gazetteer: Vec<String>,
}

impl NerExtractor {
    /// Creates an extractor with the bundled MOA development gazetteer.
    #[must_use]
    pub fn new() -> Self {
        Self::with_gazetteer(
            DEFAULT_GAZETTEER
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(ToOwned::to_owned),
        )
    }

    /// Creates an extractor with caller-supplied gazetteer entries.
    #[must_use]
    pub fn with_gazetteer(entries: impl IntoIterator<Item = String>) -> Self {
        let mut gazetteer = entries
            .into_iter()
            .map(|entry| entry.trim().to_string())
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>();
        gazetteer.sort_by_key(|entry| std::cmp::Reverse(entry.len()));
        gazetteer.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        Self { gazetteer }
    }

    /// Extracts candidate entity spans without calling an LLM.
    #[must_use]
    pub fn extract(&self, text: &str) -> Vec<NerSpan> {
        if text.trim().is_empty() {
            return Vec::new();
        }

        let tokens = tokenize(text);
        let mut spans = Vec::new();
        self.extract_gazetteer_spans(text, &mut spans);
        extract_relation_targets(text, &tokens, &mut spans);
        extract_quoted_spans(text, &mut spans);
        extract_code_like_spans(&tokens, &mut spans);
        extract_noun_phrases(&tokens, &mut spans);
        dedupe_spans(spans)
    }

    fn extract_gazetteer_spans(&self, text: &str, spans: &mut Vec<NerSpan>) {
        let lower = text.to_ascii_lowercase();
        for entry in &self.gazetteer {
            let needle = entry.to_ascii_lowercase();
            let mut offset = 0;
            while let Some(relative) = lower[offset..].find(&needle) {
                let start = offset + relative;
                let end = start + needle.len();
                if is_boundary(&lower, start) && is_boundary(&lower, end) {
                    push_span(
                        spans,
                        start,
                        end,
                        text[start..end].to_string(),
                        NerLabel::Concept,
                    );
                }
                offset = end;
            }
        }
    }
}

impl Default for NerExtractor {
    fn default() -> Self {
        Self::new()
    }
}

/// One extracted entity mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NerSpan {
    /// Byte offset where the mention starts.
    pub start: usize,
    /// Byte offset where the mention ends.
    pub end: usize,
    /// Mention text after light normalization.
    pub text: String,
    /// Coarse entity category.
    pub label: NerLabel,
}

/// Coarse entity label used by retrieval planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NerLabel {
    /// Person-like mention.
    Person,
    /// Organization-like mention.
    Org,
    /// Product, service, or package mention.
    Product,
    /// Concept or system component mention.
    Concept,
    /// Place-like mention.
    Place,
    /// Entity that does not fit another v1 category.
    Other,
}

#[derive(Debug, Clone)]
struct Token {
    start: usize,
    end: usize,
    text: String,
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        if is_token_character(character) {
            start.get_or_insert(index);
            continue;
        }

        if let Some(token_start) = start.take() {
            tokens.push(Token {
                start: token_start,
                end: index,
                text: text[token_start..index].to_string(),
            });
        }
    }

    if let Some(token_start) = start {
        tokens.push(Token {
            start: token_start,
            end: text.len(),
            text: text[token_start..].to_string(),
        });
    }
    tokens
}

fn extract_relation_targets(text: &str, tokens: &[Token], spans: &mut Vec<NerSpan>) {
    let lower = text.to_ascii_lowercase();
    for trigger in RELATION_TRIGGERS {
        let Some(trigger_start) = lower.find(trigger) else {
            continue;
        };
        let trigger_end = trigger_start + trigger.len();
        let mut selected = Vec::new();
        for token in tokens.iter().filter(|token| token.start >= trigger_end) {
            let normalized = normalize_token(&token.text);
            if selected.is_empty()
                && (normalized == "a" || normalized == "an" || normalized == "the")
            {
                continue;
            }
            if !selected.is_empty() && is_stopword(&normalized) {
                break;
            }
            if is_stopword(&normalized) || normalized.is_empty() {
                continue;
            }
            selected.push(token.clone());
            if selected.len() >= 4 {
                break;
            }
        }
        push_tokens(spans, &selected, NerLabel::Concept);
    }
}

fn extract_quoted_spans(text: &str, spans: &mut Vec<NerSpan>) {
    for quote in ['"', '\'', '`'] {
        let mut search_start = 0;
        while let Some(open_relative) = text[search_start..].find(quote) {
            let open = search_start + open_relative;
            let content_start = open + quote.len_utf8();
            let Some(close_relative) = text[content_start..].find(quote) else {
                break;
            };
            let close = content_start + close_relative;
            let content = text[content_start..close].trim();
            if content.len() >= 3 {
                let start = content_start + text[content_start..close].find(content).unwrap_or(0);
                push_span(
                    spans,
                    start,
                    start + content.len(),
                    content.to_string(),
                    NerLabel::Other,
                );
            }
            search_start = close + quote.len_utf8();
        }
    }
}

fn extract_code_like_spans(tokens: &[Token], spans: &mut Vec<NerSpan>) {
    for token in tokens {
        let value = token.text.as_str();
        if value.starts_with("http://")
            || value.starts_with("https://")
            || value.contains('@')
            || value.contains('/')
            || value.contains('_')
            || (value.contains('.') && value.chars().any(char::is_alphabetic))
        {
            push_span(
                spans,
                token.start,
                token.end,
                token.text.clone(),
                NerLabel::Product,
            );
        }
    }
}

fn extract_noun_phrases(tokens: &[Token], spans: &mut Vec<NerSpan>) {
    let mut group = Vec::<Token>::new();
    for token in tokens {
        let normalized = normalize_token(&token.text);
        if normalized.is_empty() || is_stopword(&normalized) || token.text.contains('@') {
            flush_noun_group(&group, spans);
            group.clear();
            continue;
        }
        group.push(token.clone());
        if group.len() == 4 {
            flush_noun_group(&group, spans);
            group.clear();
        }
    }
    flush_noun_group(&group, spans);
}

fn flush_noun_group(group: &[Token], spans: &mut Vec<NerSpan>) {
    if group.is_empty() {
        return;
    }

    if group.len() == 1 {
        let token = &group[0];
        if normalize_token(&token.text).len() >= 4 {
            push_span(
                spans,
                token.start,
                token.end,
                token.text.clone(),
                NerLabel::Concept,
            );
        }
        return;
    }

    let end_index = group.len().min(3);
    push_tokens(spans, &group[..end_index], NerLabel::Concept);
}

fn push_tokens(spans: &mut Vec<NerSpan>, tokens: &[Token], label: NerLabel) {
    let Some(first) = tokens.first() else {
        return;
    };
    let Some(last) = tokens.last() else {
        return;
    };
    let text = tokens
        .iter()
        .map(|token| token.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    push_span(spans, first.start, last.end, text, label);
}

fn push_span(spans: &mut Vec<NerSpan>, start: usize, end: usize, text: String, label: NerLabel) {
    let normalized = text
        .trim_matches(|character: char| {
            character.is_ascii_punctuation() || character.is_whitespace()
        })
        .trim();
    if normalized.len() < 3 || spans.len() >= MAX_SPANS.saturating_mul(2) {
        return;
    }
    spans.push(NerSpan {
        start,
        end,
        text: normalized.to_string(),
        label,
    });
}

fn dedupe_spans(mut spans: Vec<NerSpan>) -> Vec<NerSpan> {
    spans.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then_with(|| right.text.len().cmp(&left.text.len()))
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for span in spans {
        let key = span.text.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(span);
        }
        if out.len() >= MAX_SPANS {
            break;
        }
    }
    out
}

fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|character: char| character.is_ascii_punctuation())
        .to_ascii_lowercase()
}

fn is_stopword(token: &str) -> bool {
    STOPWORDS.contains(&token)
}

fn is_token_character(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            '@' | '.' | '_' | '-' | '/' | ':' | '~' | '#' | '+'
        )
}

fn is_boundary(text: &str, index: usize) -> bool {
    if index == 0 || index >= text.len() {
        return true;
    }
    let before = text[..index].chars().next_back();
    let after = text[index..].chars().next();
    !matches!((before, after), (Some(left), Some(right)) if left.is_alphanumeric() && right.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::{NerExtractor, NerLabel};

    #[test]
    fn ner_smoke_extracts_relation_target() {
        let extractor = NerExtractor::new();
        let spans = extractor.extract("What depends on the auth service?");

        assert!(
            spans
                .iter()
                .any(|span| span.text.eq_ignore_ascii_case("auth service")
                    && span.label == NerLabel::Concept),
            "{spans:?}"
        );
    }

    #[test]
    fn ner_smoke_extracts_code_identifiers() {
        let extractor = NerExtractor::new();
        let spans = extractor.extract("Check auth/refresh.rs and api_gateway_v2 today");

        assert!(
            spans.iter().any(|span| span.text == "auth/refresh.rs"),
            "{spans:?}"
        );
        assert!(
            spans.iter().any(|span| span.text == "api_gateway_v2"),
            "{spans:?}"
        );
    }
}
