//! Sentence-aware transcript chunking for slow-path ingestion.

use crate::extract::{SessionTurn, TurnChunk};
use crate::{IngestError, Result};

const APPROX_CHARS_PER_TOKEN: usize = 4;

/// Chunks a finalized turn transcript into semantically coherent units.
///
/// Outside fenced code blocks, chunk boundaries prefer explicit fact lines,
/// blank-line paragraph boundaries, and sentence endings. Fenced code blocks are
/// kept intact even when they exceed the target size, because splitting a code
/// block tends to destroy the evidence needed by extraction.
pub fn chunk_turn(
    turn: &SessionTurn,
    target_tokens: usize,
    overlap_tokens: usize,
) -> Result<Vec<TurnChunk>> {
    if target_tokens == 0 {
        return Err(IngestError::InvalidChunkTarget);
    }
    let transcript = turn.transcript.trim();
    if transcript.is_empty() {
        return Err(IngestError::EmptyTranscript);
    }

    let target_chars = target_tokens.saturating_mul(APPROX_CHARS_PER_TOKEN).max(1);
    let overlap_chars = overlap_tokens.saturating_mul(APPROX_CHARS_PER_TOKEN);
    let units = semantic_units(transcript);
    let mut chunks = Vec::new();
    let mut current = Vec::<String>::new();

    for unit in units {
        if !current.is_empty() && joined_len_with(&current, &unit) > target_chars {
            push_chunk(&mut chunks, &current);
            current = overlap_units(&current, overlap_chars);
        }
        current.push(unit);
    }

    if !current.is_empty() {
        push_chunk(&mut chunks, &current);
    }

    Ok(chunks)
}

fn semantic_units(transcript: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut paragraph = String::new();
    let mut fence = Vec::<String>::new();
    let mut in_fence = false;

    for line in transcript.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_fence {
                fence.push(line.to_string());
                push_joined_lines(&mut units, &mut fence);
                in_fence = false;
            } else {
                flush_paragraph(&mut units, &mut paragraph);
                fence.push(line.to_string());
                in_fence = true;
            }
            continue;
        }

        if in_fence {
            fence.push(line.to_string());
            continue;
        }

        if trimmed.is_empty() {
            flush_paragraph(&mut units, &mut paragraph);
        } else if is_explicit_fact_line(trimmed) {
            flush_paragraph(&mut units, &mut paragraph);
            units.push(trimmed.to_string());
        } else {
            if !paragraph.is_empty() {
                paragraph.push(' ');
            }
            paragraph.push_str(trimmed);
        }
    }

    if in_fence {
        push_joined_lines(&mut units, &mut fence);
    }
    flush_paragraph(&mut units, &mut paragraph);

    units
}

fn flush_paragraph(units: &mut Vec<String>, paragraph: &mut String) {
    let text = paragraph.trim();
    if !text.is_empty() {
        units.extend(split_sentences(text));
    }
    paragraph.clear();
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        if !matches!(character, '.' | '!' | '?') {
            continue;
        }
        let boundary = index + character.len_utf8();
        let next_is_boundary = chars
            .peek()
            .map(|(_, next)| next.is_whitespace())
            .unwrap_or(true);
        if next_is_boundary {
            push_trimmed_slice(&mut sentences, text, start, boundary);
            start = boundary;
            while let Some((next_index, next)) = chars.peek().copied() {
                if !next.is_whitespace() {
                    start = next_index;
                    break;
                }
                chars.next();
                start = text.len();
            }
        }
    }
    if start < text.len() {
        push_trimmed_slice(&mut sentences, text, start, text.len());
    }
    if sentences.is_empty() {
        vec![text.to_string()]
    } else {
        sentences
    }
}

fn push_trimmed_slice(sentences: &mut Vec<String>, text: &str, start: usize, end: usize) {
    let sentence = text[start..end].trim();
    if !sentence.is_empty() {
        sentences.push(sentence.to_string());
    }
}

fn is_explicit_fact_line(line: &str) -> bool {
    line.starts_with("Fact:") || line.starts_with("- Fact:") || line.starts_with("* Fact:")
}

fn push_joined_lines(units: &mut Vec<String>, lines: &mut Vec<String>) {
    let text = lines.join("\n").trim().to_string();
    if !text.is_empty() {
        units.push(text);
    }
    lines.clear();
}

fn push_chunk(chunks: &mut Vec<TurnChunk>, units: &[String]) {
    let text = join_units(units);
    if text.is_empty() {
        return;
    }
    chunks.push(TurnChunk {
        index: chunks.len(),
        token_estimate: estimate_tokens(&text),
        text,
    });
}

fn overlap_units(units: &[String], max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return Vec::new();
    }

    let mut selected = Vec::new();
    let mut total = 0;
    for unit in units.iter().rev() {
        let projected = if selected.is_empty() {
            unit.len()
        } else {
            total + 1 + unit.len()
        };
        if !selected.is_empty() && projected > max_chars {
            break;
        }
        selected.push(unit.clone());
        total = projected;
        if total >= max_chars {
            break;
        }
    }
    selected.reverse();
    selected
}

fn joined_len_with(units: &[String], next: &str) -> usize {
    if units.is_empty() {
        next.len()
    } else {
        units.iter().map(String::len).sum::<usize>() + units.len() + next.len()
    }
}

fn join_units(units: &[String]) -> String {
    units
        .iter()
        .map(|unit| unit.trim())
        .filter(|unit| !unit.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(APPROX_CHARS_PER_TOKEN).max(1)
}
