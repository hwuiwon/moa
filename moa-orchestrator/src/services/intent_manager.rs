//! Restate service for tenant intent taxonomy administration.

use std::sync::Arc;

use chrono::Utc;
use moa_brain::intents::IntentClassifier;
use moa_core::{
    CatalogIntent, IntentSource, IntentStatus, LearningEntry, MoaConfig, MoaError,
    Result as MoaResult, TenantIntent,
};
use moa_providers::EmbeddingProvider;
use moa_session::PostgresSessionStore;
use restate_sdk::prelude::*;
use uuid::Uuid;

use crate::observability::annotate_restate_handler_span;

/// Request payload for listing tenant intents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListTenantIntentsRequest {
    /// Tenant whose taxonomy should be listed.
    pub tenant_id: String,
    /// Optional lifecycle status filter.
    pub status: Option<IntentStatus>,
}

/// Request payload for one intent id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IntentIdRequest {
    /// Intent identifier.
    pub intent_id: Uuid,
}

/// Request payload for renaming an intent.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RenameIntentRequest {
    /// Intent identifier.
    pub intent_id: Uuid,
    /// New tenant-facing label.
    pub new_label: String,
    /// Optional replacement description.
    pub description: Option<String>,
}

/// Request payload for merging intents.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MergeIntentsRequest {
    /// Source intent ids to merge and deprecate.
    pub source_ids: Vec<Uuid>,
    /// Label for the merged replacement intent.
    pub target_label: String,
}

/// Request payload for manual intent creation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreateManualIntentRequest {
    /// Tenant that owns the new intent.
    pub tenant_id: String,
    /// Tenant-facing label.
    pub label: String,
    /// Intent description.
    pub description: String,
    /// Representative examples used to compute the centroid.
    pub examples: Vec<String>,
}

/// Request payload for catalog adoption.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AdoptCatalogIntentRequest {
    /// Tenant adopting the catalog intent.
    pub tenant_id: String,
    /// Catalog intent identifier.
    pub catalog_id: Uuid,
}

/// Request payload for catalog listing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListCatalogIntentsRequest {
    /// Optional category filter.
    pub category: Option<String>,
}

/// Request payload for learning-log reads.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GetLearningLogRequest {
    /// Tenant whose current learning log should be listed.
    pub tenant_id: String,
    /// Optional learning type filter.
    pub learning_type: Option<String>,
    /// Maximum entries to return.
    pub limit: usize,
}

/// Request payload for rolling back one learning batch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RollbackLearningBatchRequest {
    /// Batch identifier to invalidate.
    pub batch_id: Uuid,
}

/// Restate service surface for tenant intent management.
#[restate_sdk::service]
pub trait IntentManager {
    /// Lists tenant intents.
    async fn list_tenant_intents(
        request: Json<ListTenantIntentsRequest>,
    ) -> Result<Json<Vec<TenantIntent>>, HandlerError>;

    /// Confirms a proposed intent and retroactively classifies recent matches.
    async fn confirm_intent(request: Json<IntentIdRequest>) -> Result<Json<Uuid>, HandlerError>;

    /// Rejects a proposed intent.
    async fn reject_intent(request: Json<IntentIdRequest>) -> Result<u64, HandlerError>;

    /// Renames a tenant intent.
    async fn rename_intent(
        request: Json<RenameIntentRequest>,
    ) -> Result<Json<TenantIntent>, HandlerError>;

    /// Merges multiple intents into a new active intent.
    async fn merge_intents(request: Json<MergeIntentsRequest>) -> Result<Json<Uuid>, HandlerError>;

    /// Deprecates a tenant intent.
    async fn deprecate_intent(request: Json<IntentIdRequest>) -> Result<(), HandlerError>;

    /// Creates a manual tenant intent.
    async fn create_manual_intent(
        request: Json<CreateManualIntentRequest>,
    ) -> Result<Json<Uuid>, HandlerError>;

    /// Adopts a global catalog intent for a tenant.
    async fn adopt_catalog_intent(
        request: Json<AdoptCatalogIntentRequest>,
    ) -> Result<Json<Uuid>, HandlerError>;

    /// Lists global catalog intents.
    async fn list_catalog_intents(
        request: Json<ListCatalogIntentsRequest>,
    ) -> Result<Json<Vec<CatalogIntent>>, HandlerError>;

    /// Lists current learning-log entries.
    async fn get_learning_log(
        request: Json<GetLearningLogRequest>,
    ) -> Result<Json<Vec<LearningEntry>>, HandlerError>;

    /// Invalidates one learning batch.
    async fn rollback_learning_batch(
        request: Json<RollbackLearningBatchRequest>,
    ) -> Result<u64, HandlerError>;
}

/// Concrete intent manager backed by `PostgresSessionStore`.
#[derive(Clone)]
pub struct IntentManagerImpl {
    store: Arc<PostgresSessionStore>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    config: Arc<MoaConfig>,
}

impl IntentManagerImpl {
    /// Creates a tenant intent manager service.
    #[must_use]
    pub fn new(
        store: Arc<PostgresSessionStore>,
        embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
        config: Arc<MoaConfig>,
    ) -> Self {
        Self {
            store,
            embedding_provider,
            config,
        }
    }

    async fn confirm_intent_inner(&self, intent_id: Uuid) -> MoaResult<Uuid> {
        self.store
            .update_intent_status(intent_id, IntentStatus::Active)
            .await?;
        let intent = self.store.get_intent(intent_id).await?;
        let batch_id = Uuid::now_v7();
        self.store
            .append_learning(&LearningEntry {
                id: Uuid::now_v7(),
                tenant_id: intent.tenant_id.clone(),
                learning_type: "intent_confirmed".to_string(),
                target_id: intent.id.to_string(),
                target_label: Some(intent.label.clone()),
                payload: serde_json::json!({ "intent_id": intent.id }),
                confidence: None,
                source_refs: Vec::new(),
                actor: "admin".to_string(),
                valid_from: Utc::now(),
                valid_to: None,
                batch_id: Some(batch_id),
                version: 1,
            })
            .await?;
        self.retroactively_classify(&intent, batch_id).await?;
        Ok(batch_id)
    }

    async fn retroactively_classify(
        &self,
        intent: &TenantIntent,
        batch_id: Uuid,
    ) -> MoaResult<u64> {
        let Some(embedding_provider) = self.embedding_provider.clone() else {
            return Ok(0);
        };
        if intent.embedding.is_none() {
            return Ok(0);
        }

        let classifier = IntentClassifier::with_threshold(
            self.store.clone(),
            embedding_provider,
            self.config.intents.classification_threshold,
        );
        let candidates = self
            .store
            .list_undefined_segments(
                &intent.tenant_id,
                self.config.intents.deprecation_after_days,
                10_000,
            )
            .await?;
        let mut matched = 0_u64;
        for segment in candidates {
            let task_summary = segment.task_summary.as_deref().unwrap_or_default();
            let Some((matched_intent, confidence)) = classifier
                .classify(&intent.tenant_id, task_summary, "")
                .await?
            else {
                continue;
            };
            if matched_intent.id != intent.id
                || confidence < self.config.intents.retroactive_threshold
            {
                continue;
            }

            self.store
                .classify_segment(segment.id.0, intent.id, confidence)
                .await?;
            self.store
                .append_learning(&LearningEntry {
                    id: Uuid::now_v7(),
                    tenant_id: intent.tenant_id.clone(),
                    learning_type: "intent_classified".to_string(),
                    target_id: segment.id.to_string(),
                    target_label: Some(intent.label.clone()),
                    payload: serde_json::json!({
                        "intent_id": intent.id,
                        "retroactive": true,
                    }),
                    confidence: Some(confidence),
                    source_refs: vec![segment.id.0],
                    actor: "system".to_string(),
                    valid_from: Utc::now(),
                    valid_to: None,
                    batch_id: Some(batch_id),
                    version: 1,
                })
                .await?;
            matched = matched.saturating_add(1);
        }
        Ok(matched)
    }

    async fn create_manual_intent_inner(
        &self,
        request: CreateManualIntentRequest,
    ) -> MoaResult<Uuid> {
        let embedding = centroid_embedding(
            self.embedding_provider.as_deref(),
            &request.label,
            &request.description,
            &request.examples,
        )
        .await?;
        let intent = TenantIntent {
            id: Uuid::now_v7(),
            tenant_id: request.tenant_id,
            label: request.label,
            description: Some(request.description),
            status: IntentStatus::Active,
            source: IntentSource::Manual,
            catalog_ref: None,
            example_queries: request.examples,
            embedding,
            segment_count: 0,
            resolution_rate: None,
        };
        self.store.create_intent(&intent).await?;
        self.store
            .append_learning(&LearningEntry {
                id: Uuid::now_v7(),
                tenant_id: intent.tenant_id.clone(),
                learning_type: "intent_confirmed".to_string(),
                target_id: intent.id.to_string(),
                target_label: Some(intent.label.clone()),
                payload: serde_json::json!({ "source": "manual" }),
                confidence: None,
                source_refs: Vec::new(),
                actor: "admin".to_string(),
                valid_from: Utc::now(),
                valid_to: None,
                batch_id: None,
                version: 1,
            })
            .await?;
        Ok(intent.id)
    }

    async fn merge_intents_inner(&self, request: MergeIntentsRequest) -> MoaResult<Uuid> {
        let mut sources = Vec::with_capacity(request.source_ids.len());
        for source_id in request.source_ids {
            sources.push(self.store.get_intent(source_id).await?);
        }
        let first = sources.first().ok_or_else(|| {
            MoaError::StorageError("merge_intents requires at least one source intent".to_string())
        })?;
        let tenant_id = first.tenant_id.clone();
        let examples = sources
            .iter()
            .flat_map(|intent| intent.example_queries.clone())
            .collect::<Vec<_>>();
        let description = sources
            .iter()
            .filter_map(|intent| intent.description.clone())
            .collect::<Vec<_>>()
            .join(" ");
        let embedding = average_embeddings(
            sources
                .iter()
                .filter_map(|intent| intent.embedding.as_deref()),
        );
        let intent = TenantIntent {
            id: Uuid::now_v7(),
            tenant_id,
            label: request.target_label,
            description: (!description.is_empty()).then_some(description),
            status: IntentStatus::Active,
            source: IntentSource::Manual,
            catalog_ref: None,
            example_queries: examples,
            embedding,
            segment_count: sources.iter().map(|intent| intent.segment_count).sum(),
            resolution_rate: None,
        };
        self.store.create_intent(&intent).await?;
        for source in sources {
            self.store
                .update_intent_status(source.id, IntentStatus::Deprecated)
                .await?;
        }
        Ok(intent.id)
    }
}

impl IntentManager for IntentManagerImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_tenant_intents(
        &self,
        ctx: Context<'_>,
        request: Json<ListTenantIntentsRequest>,
    ) -> Result<Json<Vec<TenantIntent>>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "list_tenant_intents");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .list_intents(&request.tenant_id, request.status)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_tenant_intents")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn confirm_intent(
        &self,
        ctx: Context<'_>,
        request: Json<IntentIdRequest>,
    ) -> Result<Json<Uuid>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "confirm_intent");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .confirm_intent_inner(request.intent_id)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("confirm_intent")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn reject_intent(
        &self,
        ctx: Context<'_>,
        request: Json<IntentIdRequest>,
    ) -> Result<u64, HandlerError> {
        annotate_restate_handler_span("IntentManager", "reject_intent");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .reject_intent(request.intent_id)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("reject_intent")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn rename_intent(
        &self,
        ctx: Context<'_>,
        request: Json<RenameIntentRequest>,
    ) -> Result<Json<TenantIntent>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "rename_intent");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .rename_intent(
                        request.intent_id,
                        &request.new_label,
                        request.description.as_deref(),
                    )
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("rename_intent")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn merge_intents(
        &self,
        ctx: Context<'_>,
        request: Json<MergeIntentsRequest>,
    ) -> Result<Json<Uuid>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "merge_intents");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .merge_intents_inner(request)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("merge_intents")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn deprecate_intent(
        &self,
        ctx: Context<'_>,
        request: Json<IntentIdRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("IntentManager", "deprecate_intent");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .update_intent_status(request.intent_id, IntentStatus::Deprecated)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("deprecate_intent")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn create_manual_intent(
        &self,
        ctx: Context<'_>,
        request: Json<CreateManualIntentRequest>,
    ) -> Result<Json<Uuid>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "create_manual_intent");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .create_manual_intent_inner(request)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("create_manual_intent")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn adopt_catalog_intent(
        &self,
        ctx: Context<'_>,
        request: Json<AdoptCatalogIntentRequest>,
    ) -> Result<Json<Uuid>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "adopt_catalog_intent");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                let intent = store
                    .adopt_catalog_intent(&request.tenant_id, request.catalog_id)
                    .await
                    .map_err(HandlerError::from)?;
                store
                    .append_learning(&LearningEntry {
                        id: Uuid::now_v7(),
                        tenant_id: intent.tenant_id.clone(),
                        learning_type: "intent_confirmed".to_string(),
                        target_id: intent.id.to_string(),
                        target_label: Some(intent.label),
                        payload: serde_json::json!({
                            "source": "catalog",
                            "catalog_id": request.catalog_id,
                        }),
                        confidence: None,
                        source_refs: Vec::new(),
                        actor: "admin".to_string(),
                        valid_from: Utc::now(),
                        valid_to: None,
                        batch_id: None,
                        version: 1,
                    })
                    .await
                    .map_err(HandlerError::from)?;
                Ok(Json::from(intent.id))
            })
            .name("adopt_catalog_intent")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_catalog_intents(
        &self,
        ctx: Context<'_>,
        request: Json<ListCatalogIntentsRequest>,
    ) -> Result<Json<Vec<CatalogIntent>>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "list_catalog_intents");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .list_catalog_intents(request.category.as_deref())
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("list_catalog_intents")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn get_learning_log(
        &self,
        ctx: Context<'_>,
        request: Json<GetLearningLogRequest>,
    ) -> Result<Json<Vec<LearningEntry>>, HandlerError> {
        annotate_restate_handler_span("IntentManager", "get_learning_log");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .list_learnings(
                        &request.tenant_id,
                        request.learning_type.as_deref(),
                        request.limit,
                    )
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("get_learning_log")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn rollback_learning_batch(
        &self,
        ctx: Context<'_>,
        request: Json<RollbackLearningBatchRequest>,
    ) -> Result<u64, HandlerError> {
        annotate_restate_handler_span("IntentManager", "rollback_learning_batch");
        let store = self.store.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                store
                    .rollback_batch(request.batch_id)
                    .await
                    .map_err(HandlerError::from)
            })
            .name("rollback_learning_batch")
            .await?)
    }
}

async fn centroid_embedding(
    embedding_provider: Option<&dyn EmbeddingProvider>,
    label: &str,
    description: &str,
    examples: &[String],
) -> MoaResult<Option<Vec<f32>>> {
    let Some(embedding_provider) = embedding_provider else {
        return Ok(None);
    };
    let mut inputs = Vec::with_capacity(examples.len().saturating_add(1));
    inputs.push(format!("{label} {description}"));
    inputs.extend(examples.iter().cloned());
    let embeddings = embedding_provider.embed(&inputs).await?;
    Ok(average_embeddings(embeddings.iter().map(Vec::as_slice)))
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
