//! Restate workflow that proposes tenant intents from undefined task segments.

use std::collections::HashSet;

use chrono::Utc;
use moa_core::{
    CompletionRequest, IntentSource, IntentStatus, LearningEntry, ModelId, ModelTask, TenantIntent,
};
use restate_sdk::prelude::*;
use serde::Deserialize;
use uuid::Uuid;

use crate::ctx::OrchestratorCtx;
use crate::observability::annotate_restate_handler_span;
use crate::services::llm_gateway::LLMGatewayClient;

/// Workflow input for one tenant intent-discovery run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntentDiscoveryRequest {
    /// Tenant whose undefined recent segments should be clustered.
    pub tenant_id: String,
}

/// Workflow output for one tenant intent-discovery run.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct IntentDiscoveryReport {
    /// Tenant that was inspected.
    pub tenant_id: String,
    /// Proposed intents created during the run.
    pub proposed_intents: Vec<TenantIntent>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DiscoverySegment {
    id: Uuid,
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DiscoveredCluster {
    label: String,
    description: Option<String>,
    #[serde(default)]
    example_queries: Vec<String>,
    #[serde(default)]
    member_indices: Vec<usize>,
    confidence: Option<f64>,
}

/// Restate workflow surface for tenant intent discovery.
#[restate_sdk::workflow]
pub trait IntentDiscovery {
    /// Discovers proposed intents for one tenant.
    async fn run(
        request: Json<IntentDiscoveryRequest>,
    ) -> Result<Json<IntentDiscoveryReport>, HandlerError>;
}

/// Concrete tenant intent discovery workflow.
pub struct IntentDiscoveryImpl;

impl IntentDiscovery for IntentDiscoveryImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<IntentDiscoveryRequest>,
    ) -> Result<Json<IntentDiscoveryReport>, HandlerError> {
        annotate_restate_handler_span("IntentDiscovery", "run");
        let request = request.into_inner();
        let runtime = OrchestratorCtx::current();
        if !runtime.config.intents.enabled {
            return Ok(Json(IntentDiscoveryReport {
                tenant_id: request.tenant_id,
                proposed_intents: Vec::new(),
            }));
        }

        let store = runtime.session_store.clone();
        let config = runtime.config.intents.clone();
        let tenant_id = request.tenant_id.clone();
        let segments = ctx
            .run(|| async move {
                let segments = store
                    .list_undefined_segments(
                        &tenant_id,
                        config.discovery_window_days,
                        config.min_segments_for_discovery.saturating_mul(4),
                    )
                    .await
                    .map_err(HandlerError::from)?;
                Ok(Json::from(
                    segments
                        .into_iter()
                        .filter_map(|segment| {
                            segment.task_summary.and_then(|summary| {
                                let text = summary.trim().to_string();
                                (!text.is_empty()).then_some(DiscoverySegment {
                                    id: segment.id.0,
                                    text,
                                })
                            })
                        })
                        .collect::<Vec<_>>(),
                ))
            })
            .name("load_undefined_segments")
            .await?
            .into_inner();

        if segments.len() < runtime.config.intents.min_segments_for_discovery {
            return Ok(Json(IntentDiscoveryReport {
                tenant_id: request.tenant_id,
                proposed_intents: Vec::new(),
            }));
        }

        let prompt = build_discovery_prompt(&segments, runtime.config.intents.min_cluster_size);
        let mut completion_request = CompletionRequest::simple(prompt);
        completion_request.model = Some(ModelId::new(
            runtime
                .config
                .model_for_task(ModelTask::SkillDistillation)
                .to_string(),
        ));
        let response = ctx
            .service_client::<LLMGatewayClient>()
            .complete(Json(completion_request))
            .call()
            .await?
            .into_inner();
        let clusters = parse_clusters(&response.text).map_err(|error| {
            HandlerError::from(moa_core::MoaError::ProviderError(format!(
                "parse intent discovery response: {error}"
            )))
        })?;

        let store = runtime.session_store.clone();
        let embedding_provider = runtime.embedding_provider.clone();
        let tenant_id = request.tenant_id.clone();
        let min_cluster_size = runtime.config.intents.min_cluster_size;
        let proposed = ctx
            .run(|| async move {
                let mut proposed = Vec::new();
                let existing_labels = store
                    .list_intents(&tenant_id, None)
                    .await
                    .map_err(HandlerError::from)?
                    .into_iter()
                    .map(|intent| intent.label)
                    .collect::<HashSet<_>>();

                for cluster in clusters {
                    let member_segments = cluster
                        .member_indices
                        .iter()
                        .filter_map(|index| segments.get(*index))
                        .collect::<Vec<_>>();
                    if member_segments.len() < min_cluster_size {
                        continue;
                    }
                    let label = cluster.label.trim().to_string();
                    if label.is_empty() || existing_labels.contains(&label) {
                        continue;
                    }
                    let embedding = match embedding_provider.as_ref() {
                        Some(provider) => {
                            let inputs = member_segments
                                .iter()
                                .map(|segment| segment.text.clone())
                                .collect::<Vec<_>>();
                            let embeddings =
                                provider.embed(&inputs).await.map_err(HandlerError::from)?;
                            average_embeddings(embeddings.iter().map(Vec::as_slice))
                        }
                        None => None,
                    };
                    let source_refs = member_segments
                        .iter()
                        .map(|segment| segment.id)
                        .collect::<Vec<_>>();
                    let intent = TenantIntent {
                        id: Uuid::now_v7(),
                        tenant_id: tenant_id.clone(),
                        label,
                        description: cluster.description.clone(),
                        status: IntentStatus::Proposed,
                        source: IntentSource::Discovered,
                        catalog_ref: None,
                        example_queries: cluster.example_queries.clone(),
                        embedding,
                        segment_count: member_segments.len() as u32,
                        resolution_rate: None,
                    };
                    store
                        .create_intent(&intent)
                        .await
                        .map_err(HandlerError::from)?;
                    store
                        .append_learning(&LearningEntry {
                            id: Uuid::now_v7(),
                            tenant_id: tenant_id.clone(),
                            learning_type: "intent_discovered".to_string(),
                            target_id: intent.id.to_string(),
                            target_label: Some(intent.label.clone()),
                            payload: serde_json::json!({
                                "description": intent.description.clone(),
                                "example_queries": intent.example_queries.clone(),
                                "segment_count": intent.segment_count,
                            }),
                            confidence: cluster.confidence,
                            source_refs,
                            actor: "system".to_string(),
                            valid_from: Utc::now(),
                            valid_to: None,
                            batch_id: None,
                            version: 1,
                        })
                        .await
                        .map_err(HandlerError::from)?;
                    proposed.push(intent);
                }
                Ok(Json::from(proposed))
            })
            .name("persist_discovered_intents")
            .await?
            .into_inner();

        Ok(Json(IntentDiscoveryReport {
            tenant_id: request.tenant_id,
            proposed_intents: proposed,
        }))
    }
}

fn build_discovery_prompt(segments: &[DiscoverySegment], min_cluster_size: usize) -> String {
    let items = segments
        .iter()
        .enumerate()
        .map(|(index, segment)| format!("{index}. {}", segment.text))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Given these task descriptions from a single team, identify groups of similar tasks. \
         For each group of at least {min_cluster_size} similar tasks, suggest a short intent label \
         of 2-4 words, a one-sentence description, 3 representative example queries, member_indices \
         using the zero-based numbers below, and confidence from 0.0 to 1.0. \
         Respond with only a JSON array of objects with keys label, description, example_queries, \
         member_indices, confidence. Only include groups with at least {min_cluster_size} members.\n\n{items}"
    )
}

fn parse_clusters(text: &str) -> serde_json::Result<Vec<DiscoveredCluster>> {
    serde_json::from_str(extract_json_array(text))
}

fn extract_json_array(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(start) = trimmed.find('[')
        && let Some(end) = trimmed.rfind(']')
    {
        return &trimmed[start..=end];
    }
    trimmed
}

fn average_embeddings<'a>(embeddings: impl Iterator<Item = &'a [f32]>) -> Option<Vec<f32>> {
    let mut count = 0_usize;
    let mut sum = Vec::<f32>::new();
    for embedding in embeddings {
        if embedding.is_empty() {
            continue;
        }
        if sum.is_empty() {
            sum.resize(embedding.len(), 0.0);
        }
        if embedding.len() != sum.len() {
            continue;
        }
        for (index, value) in embedding.iter().enumerate() {
            sum[index] += value;
        }
        count = count.saturating_add(1);
    }
    if count == 0 {
        return None;
    }
    for value in &mut sum {
        *value /= count as f32;
    }
    Some(sum)
}

#[cfg(test)]
mod tests {
    use super::{DiscoverySegment, average_embeddings, build_discovery_prompt, parse_clusters};

    #[test]
    fn parse_clusters_accepts_fenced_json() {
        let clusters = parse_clusters(
            "```json\n[{\"label\":\"Debugging\",\"description\":\"Fix failures\",\"example_queries\":[\"fix test\"],\"member_indices\":[0,1,2,3,4],\"confidence\":0.82}]\n```",
        )
        .expect("cluster JSON should parse");
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].label, "Debugging");
        assert_eq!(clusters[0].member_indices.len(), 5);
    }

    #[test]
    fn prompt_requires_minimum_cluster_size_and_member_indices() {
        let prompt = build_discovery_prompt(
            &[DiscoverySegment {
                id: uuid::Uuid::now_v7(),
                text: "Fix flaky deploy".to_string(),
            }],
            5,
        );
        assert!(prompt.contains("at least 5"));
        assert!(prompt.contains("member_indices"));
    }

    #[test]
    fn average_embeddings_skips_mismatched_vectors() {
        let averaged =
            average_embeddings([&[1.0_f32, 3.0][..], &[3.0, 5.0][..], &[9.0][..]].into_iter())
                .expect("valid vectors should average");
        assert_eq!(averaged, vec![2.0, 4.0]);
    }
}
