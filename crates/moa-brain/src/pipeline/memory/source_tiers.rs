//! Source-tier planning, filtering, and final ranking for graph-memory retrieval.

use std::str::FromStr;

use moa_core::{AgentKnowledgePolicy, AgentKnowledgeScopeMode, MoaError, Result, WorkingContext};
use moa_memory_graph::{NodeLabel, PiiClass};
use moa_memory_types::MemoryScope;

/// One scoped retrieval leg and the source tier it may contribute to prompt context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RetrievalScopePlan {
    pub(super) scope: MemoryScope,
    pub(super) source_tier: crate::retrieval::SourceTier,
    pub(super) label_filter: Option<Vec<NodeLabel>>,
}

/// Reads the configured-agent knowledge policy pinned into the working context.
pub(super) fn agent_knowledge_policy(ctx: &WorkingContext) -> Result<AgentKnowledgePolicy> {
    Ok(ctx
        .agent_policy_snapshot()?
        .map(|snapshot| snapshot.knowledge_policy)
        .unwrap_or_default())
}

/// Builds the default tenant-knowledge/current-contact memory retrieval plan.
pub(super) fn default_retrieval_plan(
    ctx: &WorkingContext,
    policy: &AgentKnowledgePolicy,
) -> Vec<RetrievalScopePlan> {
    if policy.mode == AgentKnowledgeScopeMode::Disabled {
        return Vec::new();
    }

    let mut plan = vec![RetrievalScopePlan {
        scope: MemoryScope::Tenant {
            tenant_id: ctx.tenant_id,
        },
        source_tier: crate::retrieval::SourceTier::TenantKnowledge,
        label_filter: Some(tenant_knowledge_label_filter()),
    }];
    if let Some(contact) = &ctx.contact {
        plan.push(RetrievalScopePlan {
            scope: MemoryScope::Contact {
                tenant_id: ctx.tenant_id,
                contact_id: contact.contact_id,
            },
            source_tier: crate::retrieval::SourceTier::UserMemory,
            label_filter: None,
        });
    }
    plan
}

/// Returns graph labels that may surface as tenant knowledge.
pub(super) fn tenant_knowledge_label_filter() -> Vec<NodeLabel> {
    vec![
        NodeLabel::Document,
        NodeLabel::Chunk,
        NodeLabel::ContactGroup,
    ]
}

/// Resolves the effective prompt result limit under the pinned knowledge policy.
pub(super) fn effective_result_limit(policy: &AgentKnowledgePolicy, default_limit: usize) -> usize {
    policy
        .retrieval_budget
        .and_then(|limit| usize::try_from(limit).ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(default_limit)
}

/// Resolves the effective maximum PII class under the pinned knowledge policy.
pub(super) fn effective_max_pii_class(policy: &AgentKnowledgePolicy) -> Result<PiiClass> {
    let Some(value) = policy
        .pii_floor
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(PiiClass::Restricted);
    };
    PiiClass::from_str(value)
        .map_err(|error| MoaError::ValidationError(format!("invalid agent pii_floor: {error}")))
}

/// Applies source-tier, scope, label, and policy filters to one retrieved hit.
pub(super) fn admit_retrieval_hit(
    mut hit: crate::retrieval::RetrievalHit,
    plan: &RetrievalScopePlan,
    policy: &AgentKnowledgePolicy,
) -> Option<crate::retrieval::RetrievalHit> {
    if !hit_matches_retrieval_plan(&hit, plan) || !hit_matches_knowledge_policy(&hit, policy) {
        return None;
    }
    hit.source_tier = plan.source_tier;
    Some(hit)
}

/// Deduplicates selected hits and preserves the established ranking semantics.
pub(super) fn dedupe_and_rank_hits(
    hits: Vec<crate::retrieval::RetrievalHit>,
    result_limit: usize,
) -> Vec<crate::retrieval::RetrievalHit> {
    let mut hits = hits;
    hits.sort_by(compare_retrieval_hits);
    hits.dedup_by_key(|hit| hit.uid);
    hits.truncate(result_limit);
    hits
}

fn compare_retrieval_hits(
    left: &crate::retrieval::RetrievalHit,
    right: &crate::retrieval::RetrievalHit,
) -> std::cmp::Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| source_tier_rank(left.source_tier).cmp(&source_tier_rank(right.source_tier)))
        .then_with(|| left.uid.cmp(&right.uid))
}

fn source_tier_rank(tier: crate::retrieval::SourceTier) -> u8 {
    match tier {
        crate::retrieval::SourceTier::TenantKnowledge => 0,
        crate::retrieval::SourceTier::UserMemory => 1,
    }
}

fn hit_matches_retrieval_plan(
    hit: &crate::retrieval::RetrievalHit,
    plan: &RetrievalScopePlan,
) -> bool {
    match plan.source_tier {
        crate::retrieval::SourceTier::TenantKnowledge => {
            let MemoryScope::Tenant { tenant_id } = &plan.scope else {
                return false;
            };
            let tenant_id = tenant_id.to_string();
            hit.node.scope == "tenant"
                && hit.node.storage_partition_id.as_deref() == Some(tenant_id.as_str())
                && plan
                    .label_filter
                    .as_ref()
                    .is_some_and(|labels| labels.contains(&hit.node.label))
        }
        crate::retrieval::SourceTier::UserMemory => contact_hit_matches_scope(hit, &plan.scope),
    }
}

fn contact_hit_matches_scope(hit: &crate::retrieval::RetrievalHit, scope: &MemoryScope) -> bool {
    let MemoryScope::Contact {
        tenant_id,
        contact_id,
    } = scope
    else {
        return false;
    };
    let tenant_id = tenant_id.to_string();
    let contact_id = contact_id.to_string();
    hit.node.scope == "contact"
        && hit.node.storage_partition_id.as_deref() == Some(tenant_id.as_str())
        && hit.node.contact_id.as_deref() == Some(contact_id.as_str())
}

fn hit_matches_knowledge_policy(
    hit: &crate::retrieval::RetrievalHit,
    policy: &AgentKnowledgePolicy,
) -> bool {
    let filters = &policy.filters;
    matches_string_filter(filters, "labels", hit.node.label.as_str())
        && matches_string_filter(filters, "names", &hit.node.name)
        && matches_string_filter(filters, "scopes", &hit.node.scope)
        && matches_string_filter(filters, "pii_classes", hit.node.pii_class.as_str())
        && policy
            .pii_floor
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|value| PiiClass::from_str(value).ok())
            .is_none_or(|max_pii_class| pii_rank(hit.node.pii_class) <= pii_rank(max_pii_class))
}

fn pii_rank(class: PiiClass) -> i32 {
    match class {
        PiiClass::None => 0,
        PiiClass::Pii => 1,
        PiiClass::Phi => 2,
        PiiClass::Restricted => 3,
    }
}

fn matches_string_filter(filters: &serde_json::Value, key: &str, candidate: &str) -> bool {
    let Some(values) = filters.get(key).and_then(serde_json::Value::as_array) else {
        return true;
    };
    if values.is_empty() {
        return true;
    }
    values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .any(|value| value == candidate)
}
