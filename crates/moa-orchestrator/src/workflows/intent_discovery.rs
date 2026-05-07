//! Restate workflow that proposes tenant intents from undefined task segments.

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    CompletionRequest, IntentSource, IntentStatus, LearningEntry, MoaConfig, ModelId, ModelTask,
    TenantIntent,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
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

/// Task-segment projection used by the intent-discovery workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverySegment {
    /// Stable task-segment identifier.
    pub id: Uuid,
    /// Human-readable task summary used for clustering.
    pub text: String,
}

/// One intent cluster returned by the discovery model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredCluster {
    /// Proposed short intent label.
    pub label: String,
    /// Optional one-sentence intent description.
    pub description: Option<String>,
    /// Representative example queries for the intent.
    #[serde(default)]
    pub example_queries: Vec<String>,
    /// Zero-based segment indices that belong to this cluster.
    #[serde(default)]
    pub member_indices: Vec<usize>,
    /// Optional confidence score from the model.
    pub confidence: Option<f64>,
}

/// Deterministic configuration snapshot used by one intent-discovery run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDiscoveryWorkflowConfig {
    /// Whether intent discovery is enabled.
    pub enabled: bool,
    /// Recent undefined-segment window in days.
    pub discovery_window_days: u64,
    /// Minimum undefined segment count before discovery runs.
    pub min_segments_for_discovery: usize,
    /// Minimum cluster size accepted as a proposed intent.
    pub min_cluster_size: usize,
    /// Model used for the discovery completion request.
    pub model_id: ModelId,
}

impl IntentDiscoveryWorkflowConfig {
    /// Builds the workflow config snapshot from the shared MOA config.
    #[must_use]
    pub fn from_moa_config(config: &MoaConfig) -> Self {
        Self {
            enabled: config.intents.enabled,
            discovery_window_days: config.intents.discovery_window_days,
            min_segments_for_discovery: config.intents.min_segments_for_discovery,
            min_cluster_size: config.intents.min_cluster_size,
            model_id: ModelId::new(config.model_for_task(ModelTask::SkillDistillation)),
        }
    }
}

/// Input to the durable persistence step for discovered intents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistDiscoveredIntentsRequest {
    /// Tenant that owns the proposed intents.
    pub tenant_id: String,
    /// Candidate clusters parsed from the model response.
    pub clusters: Vec<DiscoveredCluster>,
    /// Undefined segments considered during this workflow run.
    pub segments: Vec<DiscoverySegment>,
    /// Minimum cluster size accepted as a proposed intent.
    pub min_cluster_size: usize,
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
        let config = IntentDiscoveryWorkflowConfig::from_moa_config(&runtime.config);
        let mut steps = RestateIntentDiscoverySteps { ctx: &ctx, runtime };
        let report = run_intent_discovery_workflow(&mut steps, &config, request).await?;
        Ok(Json(report))
    }
}

/// Durable operations used by the intent-discovery workflow body.
#[async_trait]
pub trait IntentDiscoveryDurableSteps {
    /// Loads the recent undefined task segments considered for discovery.
    async fn load_undefined_segments(
        &mut self,
        tenant_id: &str,
        window_days: u64,
        limit: usize,
    ) -> Result<Vec<DiscoverySegment>, HandlerError>;

    /// Calls the LLM gateway with the deterministic discovery prompt.
    async fn complete_discovery_prompt(
        &mut self,
        request: CompletionRequest,
    ) -> Result<String, HandlerError>;

    /// Persists proposed intents and returns the intents created by this run.
    async fn persist_discovered_intents(
        &mut self,
        request: PersistDiscoveredIntentsRequest,
    ) -> Result<Vec<TenantIntent>, HandlerError>;
}

/// Runs the intent-discovery workflow body against a durable-step implementation.
pub async fn run_intent_discovery_workflow(
    steps: &mut impl IntentDiscoveryDurableSteps,
    config: &IntentDiscoveryWorkflowConfig,
    request: IntentDiscoveryRequest,
) -> Result<IntentDiscoveryReport, HandlerError> {
    if !config.enabled {
        return Ok(IntentDiscoveryReport {
            tenant_id: request.tenant_id,
            proposed_intents: Vec::new(),
        });
    }

    let segments = steps
        .load_undefined_segments(
            &request.tenant_id,
            config.discovery_window_days,
            config.min_segments_for_discovery.saturating_mul(4),
        )
        .await?;

    if segments.len() < config.min_segments_for_discovery {
        return Ok(IntentDiscoveryReport {
            tenant_id: request.tenant_id,
            proposed_intents: Vec::new(),
        });
    }

    let prompt = build_discovery_prompt(&segments, config.min_cluster_size);
    let mut completion_request = CompletionRequest::simple(prompt);
    completion_request.model = Some(config.model_id.clone());
    let response_text = steps.complete_discovery_prompt(completion_request).await?;
    let clusters = parse_clusters(&response_text).map_err(|error| {
        HandlerError::from(moa_core::MoaError::ProviderError(format!(
            "parse intent discovery response: {error}"
        )))
    })?;

    let proposed = steps
        .persist_discovered_intents(PersistDiscoveredIntentsRequest {
            tenant_id: request.tenant_id.clone(),
            clusters,
            segments,
            min_cluster_size: config.min_cluster_size,
        })
        .await?;

    Ok(IntentDiscoveryReport {
        tenant_id: request.tenant_id,
        proposed_intents: proposed,
    })
}

struct RestateIntentDiscoverySteps<'ctx, 'workflow> {
    ctx: &'ctx WorkflowContext<'workflow>,
    runtime: std::sync::Arc<OrchestratorCtx>,
}

#[async_trait]
impl IntentDiscoveryDurableSteps for RestateIntentDiscoverySteps<'_, '_> {
    async fn load_undefined_segments(
        &mut self,
        tenant_id: &str,
        window_days: u64,
        limit: usize,
    ) -> Result<Vec<DiscoverySegment>, HandlerError> {
        let store = self.runtime.session_store.clone();
        let tenant_id = tenant_id.to_string();
        self.ctx
            .run(|| async move {
                let segments = store
                    .list_undefined_segments(&tenant_id, window_days, limit)
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
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn complete_discovery_prompt(
        &mut self,
        request: CompletionRequest,
    ) -> Result<String, HandlerError> {
        let response = self
            .ctx
            .service_client::<LLMGatewayClient>()
            .complete(Json(request))
            .call()
            .await?
            .into_inner();
        Ok(response.text)
    }

    async fn persist_discovered_intents(
        &mut self,
        request: PersistDiscoveredIntentsRequest,
    ) -> Result<Vec<TenantIntent>, HandlerError> {
        let store = self.runtime.session_store.clone();
        let embedding_provider = self.runtime.embedding_provider.clone();
        self.ctx
            .run(|| async move {
                persist_discovered_intents_with_store(store, embedding_provider, request).await
            })
            .name("persist_discovered_intents")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }
}

// This helper intentionally contains ID and timestamp generation, but production
// only calls it from the journaled `persist_discovered_intents` Restate step.
async fn persist_discovered_intents_with_store(
    store: std::sync::Arc<moa_session::PostgresSessionStore>,
    embedding_provider: Option<std::sync::Arc<dyn moa_core::traits::EmbeddingProvider>>,
    request: PersistDiscoveredIntentsRequest,
) -> Result<Json<Vec<TenantIntent>>, HandlerError> {
    let mut proposed = Vec::new();
    let existing_labels = store
        .list_intents(&request.tenant_id, None)
        .await
        .map_err(HandlerError::from)?
        .into_iter()
        .map(|intent| intent.label)
        .collect::<HashSet<_>>();

    for cluster in request.clusters {
        let member_segments = cluster
            .member_indices
            .iter()
            .filter_map(|index| request.segments.get(*index))
            .collect::<Vec<_>>();
        if member_segments.len() < request.min_cluster_size {
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
                let embeddings = provider.embed(&inputs).await.map_err(HandlerError::from)?;
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
            tenant_id: request.tenant_id.clone(),
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
                tenant_id: request.tenant_id.clone(),
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
                id: uuid::Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0042),
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
