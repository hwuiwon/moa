//! Chunking coverage for deterministic block and chunk identity.

use moa_knowledge::{
    chunking::{ChunkingConfig, blocks_to_chunks, elements_to_blocks},
    domain::{DocumentElement, DocumentElementKind},
};
use serde_json::json;
use uuid::Uuid;

fn element(ordinal: u32, text: &str, heading_path: Vec<&str>) -> DocumentElement {
    DocumentElement {
        element_id: format!("element-{ordinal}"),
        kind: DocumentElementKind::Paragraph,
        text: text.to_string(),
        heading_path: heading_path.into_iter().map(ToOwned::to_owned).collect(),
        ordinal,
        page_number: None,
        layout: None,
        metadata: json!({
            "volatile_parser_duration_ms": ordinal + 100
        }),
    }
}

#[test]
fn reparsing_same_input_produces_identical_blocks_and_hashes() {
    // Pins: normalized block rows and block hashes are deterministic for the same version/input.
    let version_uid = Uuid::from_u128(10);
    let elements = vec![
        element(0, "Cafe\u{301}   launch\r\nnotes", vec!["Guide"]),
        element(1, "Second paragraph", vec!["Guide"]),
    ];

    let first = elements_to_blocks(version_uid, &elements);
    let second = elements_to_blocks(version_uid, &elements);

    assert_eq!(first, second);
    assert_eq!(first[0].normalized_text, "Café launch\nnotes");
    assert_eq!(first[0].block_hash, second[0].block_hash);
}

#[test]
fn editing_one_paragraph_changes_one_block_and_one_chunk() {
    // Pins: a paragraph edit invalidates only its block and the chunk containing that block.
    let version_uid = Uuid::from_u128(11);
    let base = vec![
        element(0, "alpha beta gamma delta epsilon", vec!["Guide"]),
        element(1, "bravo beta gamma delta epsilon", vec!["Guide"]),
        element(2, "charlie beta gamma delta epsilon", vec!["Guide"]),
        element(3, "delta beta gamma delta epsilon", vec!["Guide"]),
        element(4, "echo beta gamma delta epsilon", vec!["Guide"]),
        element(5, "foxtrot beta gamma delta epsilon", vec!["Guide"]),
    ];
    let mut edited = base.clone();
    edited[2].text = "charlie beta gamma updated epsilon".to_string();

    let config = ChunkingConfig {
        target_tokens: 10,
        max_tokens: 12,
        min_tokens: 1,
    };
    let base_blocks = elements_to_blocks(version_uid, &base);
    let edited_blocks = elements_to_blocks(version_uid, &edited);
    let changed_blocks = base_blocks
        .iter()
        .zip(&edited_blocks)
        .filter(|(left, right)| left.block_hash != right.block_hash)
        .count();
    assert_eq!(changed_blocks, 1);

    let base_chunks = blocks_to_chunks(version_uid, &base_blocks, config);
    let edited_chunks = blocks_to_chunks(version_uid, &edited_blocks, config);
    assert_eq!(base_chunks.len(), edited_chunks.len());
    let changed_chunks = base_chunks
        .iter()
        .zip(&edited_chunks)
        .filter(|(left, right)| left.chunk_hash != right.chunk_hash)
        .count();
    assert_eq!(changed_chunks, 1);
}

#[test]
fn heading_path_does_not_participate_in_chunk_hash() {
    // Pins: heading paths are citation metadata and do not change block or chunk identity.
    let version_uid = Uuid::from_u128(12);
    let left = elements_to_blocks(
        version_uid,
        &[element(0, "same paragraph text", vec!["Old heading"])],
    );
    let right = elements_to_blocks(
        version_uid,
        &[element(0, "same paragraph text", vec!["New heading"])],
    );

    assert_eq!(left[0].block_hash, right[0].block_hash);
    let left_chunk = blocks_to_chunks(version_uid, &left, ChunkingConfig::default());
    let right_chunk = blocks_to_chunks(version_uid, &right, ChunkingConfig::default());
    assert_eq!(left_chunk[0].chunk_hash, right_chunk[0].chunk_hash);
    assert_ne!(left_chunk[0].heading_path, right_chunk[0].heading_path);
}

#[test]
fn oversized_single_block_is_split_with_parent_provenance() {
    // Pins: oversized blocks split deterministically and carry parent block provenance in chunk metadata.
    let version_uid = Uuid::from_u128(13);
    let text = (0..25)
        .map(|index| format!("word{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let blocks = elements_to_blocks(version_uid, &[element(0, &text, vec!["Large"])]);
    let chunks = blocks_to_chunks(
        version_uid,
        &blocks,
        ChunkingConfig {
            target_tokens: 5,
            max_tokens: 8,
            min_tokens: 1,
        },
    );

    assert_eq!(chunks.len(), 4);
    assert!(chunks.iter().all(|chunk| chunk.token_count <= 8));
    assert!(chunks.iter().all(|chunk| chunk.metadata["split"] == true));
    assert!(
        chunks.iter().all(|chunk| {
            chunk.metadata["split_parent_block_hashes"][0] == blocks[0].block_hash
        })
    );
    assert_eq!(
        chunks,
        blocks_to_chunks(
            version_uid,
            &blocks,
            ChunkingConfig {
                target_tokens: 5,
                max_tokens: 8,
                min_tokens: 1,
            },
        )
    );
}
