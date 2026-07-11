//! Segment metadata helpers shared by turn workflows.

use std::collections::HashMap;

use moa_core::{
    types::completion::CompletionRequest, types::query_rewrite::QueryRewriteResult,
    types::segments::ActiveSegment,
};

/// Completion metadata key for the active segment identifier.
pub(crate) const ACTIVE_SEGMENT_ID_METADATA_KEY: &str = "_moa.segment_id";
/// Completion metadata key for the active segment index.
pub(crate) const ACTIVE_SEGMENT_INDEX_METADATA_KEY: &str = "_moa.segment_index";
/// Completion metadata key carrying the query-rewrite result.
pub(crate) const QUERY_REWRITE_METADATA_KEY: &str = "query_rewrite";

/// Inserts active segment metadata into a completion request.
pub(crate) fn insert_active_segment_metadata(
    request: &mut CompletionRequest,
    segment: &ActiveSegment,
) {
    request.metadata.insert(
        ACTIVE_SEGMENT_ID_METADATA_KEY.to_string(),
        serde_json::json!(segment.id.to_string()),
    );
    request.metadata.insert(
        ACTIVE_SEGMENT_INDEX_METADATA_KEY.to_string(),
        serde_json::json!(segment.segment_index),
    );
}

/// Reads query-rewrite metadata from a completion request metadata map.
pub(crate) fn query_rewrite_from_metadata(
    metadata: &HashMap<String, serde_json::Value>,
) -> Option<QueryRewriteResult> {
    metadata
        .get(QUERY_REWRITE_METADATA_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        types::completion::CompletionRequest, types::identifiers::ModelId,
        types::identifiers::SegmentId, types::query_rewrite::QueryRewriteResult,
    };

    use super::{
        ACTIVE_SEGMENT_ID_METADATA_KEY, ACTIVE_SEGMENT_INDEX_METADATA_KEY,
        QUERY_REWRITE_METADATA_KEY, insert_active_segment_metadata, query_rewrite_from_metadata,
    };

    #[test]
    fn active_segment_metadata_uses_owned_keys() {
        // Pins: root and worker completion requests use one segment metadata key owner.
        let segment = moa_core::types::segments::ActiveSegment {
            id: SegmentId(uuid::Uuid::now_v7()),
            segment_index: 3,
            task_summary: None,
            started_at: Utc::now(),
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            turn_count: 0,
            token_cost: 0,
        };
        let mut request = CompletionRequest::new("hello");
        request.model = Some(ModelId::new("model"));

        insert_active_segment_metadata(&mut request, &segment);

        assert_eq!(
            request.metadata.get(ACTIVE_SEGMENT_ID_METADATA_KEY),
            Some(&serde_json::json!(segment.id.to_string()))
        );
        assert_eq!(
            request.metadata.get(ACTIVE_SEGMENT_INDEX_METADATA_KEY),
            Some(&serde_json::json!(3))
        );
    }

    #[test]
    fn query_rewrite_metadata_round_trips() {
        // Pins: segment assessment reads the same query-rewrite metadata key as the context pipeline writes.
        let rewrite = QueryRewriteResult::original("lookup this");
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            QUERY_REWRITE_METADATA_KEY.to_string(),
            serde_json::to_value(&rewrite).expect("serialize query rewrite"),
        );

        assert_eq!(query_rewrite_from_metadata(&metadata), Some(rewrite));
    }
}
