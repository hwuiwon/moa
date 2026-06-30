//! Deterministic block and chunk construction.

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    domain::{DocumentElement, KnowledgeBlock, KnowledgeChunk},
    graph_delta::stable_uid,
    normalize::normalize_text,
};

/// Chunking thresholds for tenant knowledge chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkingConfig {
    /// Target chunk token count.
    pub target_tokens: usize,
    /// Maximum chunk token count.
    pub max_tokens: usize,
    /// Minimum chunk token count.
    pub min_tokens: usize,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            target_tokens: 700,
            max_tokens: 1_000,
            min_tokens: 120,
        }
    }
}

/// Converts parser elements into normalized blocks.
#[must_use]
pub fn elements_to_blocks(version_uid: Uuid, elements: &[DocumentElement]) -> Vec<KnowledgeBlock> {
    elements
        .iter()
        .filter_map(|element| {
            let normalized_text = normalize_text(&element.text);
            if normalized_text.is_empty() {
                return None;
            }
            let block_hash = content_hash(&normalized_text);
            Some(KnowledgeBlock {
                block_uid: stable_uid(&format!(
                    "{}:{}:{}",
                    version_uid, element.element_id, block_hash
                )),
                version_uid,
                element_id: element.element_id.clone(),
                block_hash,
                normalized_text,
                heading_path: element.heading_path.clone(),
                ordinal: element.ordinal,
                metadata: element.metadata.clone(),
            })
        })
        .collect()
}

/// Converts ordered blocks into retrieval-sized chunks.
#[must_use]
pub fn blocks_to_chunks(
    version_uid: Uuid,
    blocks: &[KnowledgeBlock],
    config: ChunkingConfig,
) -> Vec<KnowledgeChunk> {
    let mut chunks = Vec::new();
    let mut current: Vec<ChunkPart> = Vec::new();
    let mut current_tokens = 0usize;

    for block in blocks {
        let block_tokens = estimate_tokens(&block.normalized_text);
        if block_tokens > config.max_tokens {
            flush_current(version_uid, &mut chunks, &mut current, &mut current_tokens);
            for part in split_oversized_block(block, config.max_tokens) {
                chunks.push(build_chunk(version_uid, chunks.len() as u32, &[part]));
            }
            continue;
        }

        let would_exceed = current_tokens + block_tokens > config.max_tokens;
        let reached_target = current_tokens >= config.target_tokens;
        if !current.is_empty()
            && (would_exceed || (reached_target && current_tokens >= config.min_tokens))
        {
            chunks.push(build_chunk(version_uid, chunks.len() as u32, &current));
            current.clear();
            current_tokens = 0;
        }
        current.push(ChunkPart::from_block(block));
        current_tokens += block_tokens;
    }

    if !current.is_empty() {
        chunks.push(build_chunk(version_uid, chunks.len() as u32, &current));
    }
    chunks
}

/// Returns a blake3 hex hash for normalized content.
#[must_use]
pub fn content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn build_chunk(version_uid: Uuid, ordinal: u32, blocks: &[ChunkPart]) -> KnowledgeChunk {
    let text = blocks
        .iter()
        .map(|block| block.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let block_hashes = blocks
        .iter()
        .map(|block| block.block_hash.clone())
        .collect::<Vec<_>>();
    let chunk_seed = block_hashes.join("");
    let parent_blocks = blocks
        .iter()
        .filter_map(|block| block.parent_block_hash.clone())
        .collect::<Vec<_>>();
    let metadata = if parent_blocks.is_empty() {
        Value::Null
    } else {
        json!({
            "split_parent_block_hashes": parent_blocks,
            "split": true
        })
    };
    KnowledgeChunk {
        chunk_uid: stable_uid(&format!("{}:{}:{}", version_uid, ordinal, chunk_seed)),
        version_uid,
        graph_node_uid: None,
        chunk_hash: content_hash(&chunk_seed),
        block_hashes,
        token_count: estimate_tokens(&text),
        text,
        heading_path: blocks
            .first()
            .map(|block| block.heading_path.clone())
            .unwrap_or_default(),
        ordinal,
        metadata,
    }
}

fn estimate_tokens(text: &str) -> usize {
    let mut tokens = 0usize;
    let mut in_token = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if !in_token {
                tokens = tokens.saturating_add(1);
                in_token = true;
            }
        } else {
            in_token = false;
            if matches!(
                ch,
                '.' | ',' | ';' | ':' | '!' | '?' | '(' | ')' | '[' | ']'
            ) {
                tokens = tokens.saturating_add(1);
            }
        }
    }
    tokens.max(1)
}

fn split_oversized_block(block: &KnowledgeBlock, max_tokens: usize) -> Vec<ChunkPart> {
    let token_budget = max_tokens.max(1);
    let words = block.normalized_text.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() {
        return Vec::new();
    }

    words
        .chunks(token_budget)
        .enumerate()
        .map(|(index, words)| {
            let text = words.join(" ");
            let hash = content_hash(&format!("{}:{}:{}", block.block_hash, index, text));
            ChunkPart {
                text,
                block_hash: hash,
                heading_path: block.heading_path.clone(),
                parent_block_hash: Some(block.block_hash.clone()),
            }
        })
        .collect()
}

fn flush_current(
    version_uid: Uuid,
    chunks: &mut Vec<KnowledgeChunk>,
    current: &mut Vec<ChunkPart>,
    current_tokens: &mut usize,
) {
    if !current.is_empty() {
        chunks.push(build_chunk(version_uid, chunks.len() as u32, current));
        current.clear();
        *current_tokens = 0;
    }
}

#[derive(Debug, Clone)]
struct ChunkPart {
    text: String,
    block_hash: String,
    heading_path: Vec<String>,
    parent_block_hash: Option<String>,
}

impl ChunkPart {
    fn from_block(block: &KnowledgeBlock) -> Self {
        Self {
            text: block.normalized_text.clone(),
            block_hash: block.block_hash.clone(),
            heading_path: block.heading_path.clone(),
            parent_block_hash: None,
        }
    }
}
