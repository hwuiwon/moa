//! Shared memory-admission policy for prompt and agentic retrieval surfaces.

use std::str::FromStr;

use moa_core::types::memory::{SOURCE_ACL_EPOCH_UNRESOLVED, SourceAclContext};
use moa_core::types::security::SensitivityClass;
use moa_core::{
    error::MoaError, error::Result, types::agent::AgentKnowledgePolicy,
    types::agent::AgentKnowledgeScopeMode, types::contact::ContactId,
    types::context::WorkingContext, types::identifiers::TenantId, types::session::SessionMeta,
};
use moa_memory_graph::{NodeIndexRow, NodeLabel};
use moa_memory_types::MemoryScope;

use super::{RetrievalHit, SourceTier};

const TENANT_KNOWLEDGE_LABELS: [NodeLabel; 3] = [
    NodeLabel::Document,
    NodeLabel::Chunk,
    NodeLabel::ContactGroup,
];

/// One scoped retrieval leg and the source tier it may contribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalScopePlan {
    scope: MemoryScope,
    source_tier: SourceTier,
    label_filter: Option<Vec<NodeLabel>>,
}

impl RetrievalScopePlan {
    /// Returns the RLS scope used for this retrieval leg.
    #[must_use]
    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    /// Returns the source tier assigned to admitted hits from this leg.
    #[must_use]
    pub const fn source_tier(&self) -> SourceTier {
        self.source_tier
    }

    /// Returns the graph-label allowlist applied before final admission.
    #[must_use]
    pub fn label_filter(&self) -> Option<&[NodeLabel]> {
        self.label_filter.as_deref()
    }
}

/// One authoritative admission policy for prompt injection and agentic memory tools.
#[derive(Debug, Clone)]
pub struct MemoryAdmissionPolicy {
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    agent_policy: AgentKnowledgePolicy,
    plans: Vec<RetrievalScopePlan>,
    source_acl: SourceAclContext,
}

impl MemoryAdmissionPolicy {
    /// Builds the policy pinned into a context-pipeline working context.
    pub fn from_working_context(ctx: &WorkingContext) -> Result<Self> {
        let agent_policy = ctx
            .agent_policy_snapshot()?
            .map(|snapshot| snapshot.knowledge_policy)
            .unwrap_or_default();
        Ok(Self::new(
            ctx.tenant_id,
            ctx.contact.as_ref().map(|contact| contact.contact_id),
            agent_policy,
        ))
    }

    /// Builds the policy pinned into durable session metadata.
    pub fn from_session(session: &SessionMeta) -> Result<Self> {
        let agent_policy = session
            .agent_context
            .as_ref()
            .map(|context| context.parsed_policy_snapshot())
            .transpose()?
            .map(|snapshot| snapshot.knowledge_policy)
            .unwrap_or_default();
        Ok(Self::new(
            session.tenant_id,
            session.contact.as_ref().map(|contact| contact.contact_id),
            agent_policy,
        ))
    }

    fn new(
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        agent_policy: AgentKnowledgePolicy,
    ) -> Self {
        let plans = if agent_policy.mode == AgentKnowledgeScopeMode::Disabled {
            Vec::new()
        } else {
            let mut plans = vec![RetrievalScopePlan {
                scope: MemoryScope::Tenant { tenant_id },
                source_tier: SourceTier::TenantKnowledge,
                label_filter: Some(TENANT_KNOWLEDGE_LABELS.to_vec()),
            }];
            if let Some(contact_id) = contact_id {
                plans.push(RetrievalScopePlan {
                    scope: MemoryScope::Contact {
                        tenant_id,
                        contact_id,
                    },
                    source_tier: SourceTier::UserMemory,
                    label_filter: None,
                });
            }
            plans
        };
        Self {
            tenant_id,
            contact_id,
            agent_policy,
            plans,
            // Deliberately unresolved: the agent knowledge policy is authored
            // configuration, while provider-source admission is durable identity
            // state that only a database read can establish. Until the retrieval
            // entry point attaches it, this policy admits tenant-public sources
            // only and its results are not cacheable.
            source_acl: SourceAclContext::empty(SOURCE_ACL_EPOCH_UNRESOLVED),
        }
    }

    /// Returns this policy carrying the caller's resolved source-ACL context.
    ///
    /// Attached once per turn from durable identity state, never from a request
    /// payload. Tenant role or operator status does not widen it: an operator
    /// authorized to list a connection's control-plane metadata still needs the
    /// source's own permission to read a single chunk of its content.
    #[must_use]
    pub fn with_source_acl(mut self, source_acl: SourceAclContext) -> Self {
        self.source_acl = source_acl;
        self
    }

    /// Returns the caller's resolved provider-source admission context.
    #[must_use]
    pub fn source_acl(&self) -> &SourceAclContext {
        &self.source_acl
    }

    /// Returns the tenant this policy admits for.
    #[must_use]
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Returns the contact this policy admits for, when the session has one.
    #[must_use]
    pub fn contact_id(&self) -> Option<ContactId> {
        self.contact_id
    }

    /// Returns whether memory retrieval is enabled by the pinned agent policy.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.agent_policy.mode != AgentKnowledgeScopeMode::Disabled
    }

    /// Returns the exact scoped legs that may contribute admitted memory.
    #[must_use]
    pub fn plans(&self) -> &[RetrievalScopePlan] {
        &self.plans
    }

    /// Returns the RLS scope used for graph navigation.
    #[must_use]
    pub fn traversal_scope(&self) -> MemoryScope {
        match self.contact_id {
            Some(contact_id) => MemoryScope::Contact {
                tenant_id: self.tenant_id,
                contact_id,
            },
            None => MemoryScope::Tenant {
                tenant_id: self.tenant_id,
            },
        }
    }

    /// Returns the tenant-knowledge labels visible to contact-facing retrieval.
    #[must_use]
    pub const fn tenant_knowledge_labels() -> &'static [NodeLabel] {
        &TENANT_KNOWLEDGE_LABELS
    }

    /// Returns the pinned configured-agent knowledge policy.
    #[must_use]
    pub const fn agent_policy(&self) -> &AgentKnowledgePolicy {
        &self.agent_policy
    }

    /// Resolves the effective result limit under the pinned knowledge policy.
    #[must_use]
    pub fn result_limit(&self, default_limit: usize) -> usize {
        self.agent_policy
            .retrieval_budget
            .and_then(|limit| usize::try_from(limit).ok())
            .filter(|limit| *limit > 0)
            .map_or(default_limit, |budget| budget.min(default_limit))
    }

    /// Resolves the maximum PII class visible under the pinned knowledge policy.
    pub fn max_pii_class(&self) -> Result<SensitivityClass> {
        let Some(value) = self
            .agent_policy
            .pii_floor
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(SensitivityClass::Restricted);
        };
        SensitivityClass::from_str(value)
            .map_err(|error| MoaError::ValidationError(format!("invalid agent pii_floor: {error}")))
    }

    /// Assigns the plan's source tier when one retrieval hit is admitted.
    #[must_use]
    pub fn admit_hit(
        &self,
        mut hit: RetrievalHit,
        plan: &RetrievalScopePlan,
    ) -> Option<RetrievalHit> {
        if !self.plans.contains(plan)
            || self.source_tier_for_node(&hit.node) != Some(plan.source_tier)
            || plan
                .label_filter
                .as_ref()
                .is_some_and(|labels| !labels.contains(&hit.node.label))
        {
            return None;
        }
        hit.source_tier = plan.source_tier;
        Some(hit)
    }

    /// Returns whether one graph node may be exposed by this policy.
    #[must_use]
    pub fn admits_node(&self, node: &NodeIndexRow) -> bool {
        self.source_tier_for_node(node).is_some()
    }

    /// Classifies one admitted node into its prompt-visible source tier.
    #[must_use]
    pub fn source_tier_for_node(&self, node: &NodeIndexRow) -> Option<SourceTier> {
        if !self.is_enabled() || !self.matches_agent_filters(node) {
            return None;
        }
        let tenant_id = self.tenant_id.to_string();
        if node.storage_partition_id.as_deref() != Some(tenant_id.as_str()) {
            return None;
        }
        if node.scope == "tenant"
            && node.contact_id.is_none()
            && TENANT_KNOWLEDGE_LABELS.contains(&node.label)
        {
            return Some(SourceTier::TenantKnowledge);
        }
        let contact_id = self.contact_id?.to_string();
        (node.scope == "contact" && node.contact_id.as_deref() == Some(contact_id.as_str()))
            .then_some(SourceTier::UserMemory)
    }

    fn matches_agent_filters(&self, node: &NodeIndexRow) -> bool {
        let filters = &self.agent_policy.filters;
        matches_string_filter(filters, "labels", node.label.as_str())
            && matches_string_filter(filters, "names", &node.name)
            && matches_string_filter(filters, "scopes", &node.scope)
            && matches_string_filter(filters, "pii_classes", node.pii_class.as_str())
            && self
                .max_pii_class()
                .is_ok_and(|max_pii_class| node.pii_class.rank() <= max_pii_class.rank())
    }
}

/// Deduplicates admitted hits and preserves the established ranking semantics.
#[must_use]
pub fn dedupe_and_rank_hits(mut hits: Vec<RetrievalHit>, result_limit: usize) -> Vec<RetrievalHit> {
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                source_tier_rank(left.source_tier).cmp(&source_tier_rank(right.source_tier))
            })
            .then_with(|| left.uid.cmp(&right.uid))
    });
    hits.dedup_by_key(|hit| hit.uid);
    hits.truncate(result_limit);
    hits
}

fn source_tier_rank(tier: SourceTier) -> u8 {
    match tier {
        SourceTier::TenantKnowledge => 0,
        SourceTier::UserMemory => 1,
    }
}

fn matches_string_filter(filters: &serde_json::Value, key: &str, candidate: &str) -> bool {
    let Some(values) = filters.get(key).and_then(serde_json::Value::as_array) else {
        return true;
    };
    values.is_empty()
        || values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|value| value == candidate)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::{
        types::agent::AgentContext, types::agent::AgentKnowledgeScopeMode,
        types::agent::AgentPolicySnapshot, types::channel::Channel, types::contact::ContactRef,
        types::contact::ContactVerificationState, types::identifiers::ModelId,
        types::identifiers::SessionId,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::retrieval::LegSources;

    #[test]
    fn contact_policy_admits_only_tenant_knowledge_and_current_contact_memory() {
        // Pins: every contact-facing memory surface shares the same tenant-knowledge/current-contact boundary.
        let tenant_id = TenantId::new();
        let other_tenant_id = TenantId::new();
        let contact_id = ContactId::new();
        let other_contact_id = ContactId::new();
        let session = contact_session(tenant_id, contact_id, AgentKnowledgePolicy::default());
        let policy = MemoryAdmissionPolicy::from_session(&session).expect("policy should parse");
        let rows = [
            node(
                1,
                tenant_id,
                None,
                NodeLabel::Document,
                "tenant",
                SensitivityClass::None,
                "doc",
            ),
            node(
                2,
                tenant_id,
                None,
                NodeLabel::Chunk,
                "tenant",
                SensitivityClass::None,
                "chunk",
            ),
            node(
                3,
                tenant_id,
                None,
                NodeLabel::ContactGroup,
                "tenant",
                SensitivityClass::None,
                "group",
            ),
            node(
                4,
                tenant_id,
                None,
                NodeLabel::Fact,
                "tenant",
                SensitivityClass::None,
                "admin fact",
            ),
            node(
                5,
                tenant_id,
                Some(contact_id),
                NodeLabel::Fact,
                "contact",
                SensitivityClass::None,
                "mine",
            ),
            node(
                6,
                tenant_id,
                Some(other_contact_id),
                NodeLabel::Fact,
                "contact",
                SensitivityClass::None,
                "other",
            ),
            node(
                7,
                other_tenant_id,
                None,
                NodeLabel::Chunk,
                "tenant",
                SensitivityClass::None,
                "other tenant",
            ),
        ];

        let admitted = rows
            .iter()
            .filter_map(|row| policy.source_tier_for_node(row).map(|tier| (row.uid, tier)))
            .collect::<Vec<_>>();

        assert_eq!(
            admitted,
            vec![
                (Uuid::from_u128(1), SourceTier::TenantKnowledge),
                (Uuid::from_u128(2), SourceTier::TenantKnowledge),
                (Uuid::from_u128(3), SourceTier::TenantKnowledge),
                (Uuid::from_u128(5), SourceTier::UserMemory),
            ]
        );
        assert_eq!(policy.plans().len(), 2);
        let contact_plan = policy
            .plans()
            .iter()
            .find(|plan| plan.source_tier() == SourceTier::UserMemory)
            .expect("contact plan should exist");
        let admitted_hit = policy
            .admit_hit(hit(rows[4].clone()), contact_plan)
            .expect("current-contact hit should be admitted");
        assert_eq!(admitted_hit.source_tier, SourceTier::UserMemory);
    }

    #[test]
    fn tenant_only_and_disabled_policies_fail_closed() {
        // Pins: tenant-only sessions see knowledge only, while disabled policies admit nothing.
        let tenant_id = TenantId::new();
        let contact_id = ContactId::new();
        let tenant_only_session = tenant_session(tenant_id, AgentKnowledgePolicy::default());
        let tenant_policy =
            MemoryAdmissionPolicy::from_session(&tenant_only_session).expect("policy should parse");
        let knowledge = node(
            10,
            tenant_id,
            None,
            NodeLabel::Chunk,
            "tenant",
            SensitivityClass::None,
            "chunk",
        );
        let operational = node(
            11,
            tenant_id,
            None,
            NodeLabel::Fact,
            "tenant",
            SensitivityClass::None,
            "fact",
        );
        let contact = node(
            12,
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact",
            SensitivityClass::None,
            "contact",
        );

        assert_eq!(tenant_policy.plans().len(), 1);
        assert!(tenant_policy.admits_node(&knowledge));
        assert!(!tenant_policy.admits_node(&operational));
        assert!(!tenant_policy.admits_node(&contact));

        let disabled = tenant_session(
            tenant_id,
            AgentKnowledgePolicy {
                mode: AgentKnowledgeScopeMode::Disabled,
                ..AgentKnowledgePolicy::default()
            },
        );
        let disabled_policy =
            MemoryAdmissionPolicy::from_session(&disabled).expect("policy should parse");
        assert!(!disabled_policy.is_enabled());
        assert!(disabled_policy.plans().is_empty());
        assert!(!disabled_policy.admits_node(&knowledge));
    }

    #[test]
    fn explicit_filters_and_pii_floor_apply_to_every_source_tier() {
        // Pins: pinned labels, names, scopes, PII classes, and PII floor are final admission gates.
        let tenant_id = TenantId::new();
        let contact_id = ContactId::new();
        let policy = AgentKnowledgePolicy {
            filters: json!({
                "labels": ["Fact"],
                "names": ["allowed"],
                "scopes": ["contact"],
                "pii_classes": ["pii", "phi"]
            }),
            retrieval_budget: Some(2),
            pii_floor: Some("pii".to_string()),
            ..AgentKnowledgePolicy::default()
        };
        let session = contact_session(tenant_id, contact_id, policy);
        let admission = MemoryAdmissionPolicy::from_session(&session).expect("policy should parse");
        let allowed = node(
            20,
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact",
            SensitivityClass::Pii,
            "allowed",
        );
        let wrong_name = node(
            21,
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact",
            SensitivityClass::Pii,
            "blocked",
        );
        let too_sensitive = node(
            22,
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact",
            SensitivityClass::Phi,
            "allowed",
        );
        let tenant_chunk = node(
            23,
            tenant_id,
            None,
            NodeLabel::Chunk,
            "tenant",
            SensitivityClass::Pii,
            "allowed",
        );

        assert!(admission.admits_node(&allowed));
        assert!(!admission.admits_node(&wrong_name));
        assert!(!admission.admits_node(&too_sensitive));
        assert!(!admission.admits_node(&tenant_chunk));
        assert_eq!(admission.result_limit(8), 2);
        assert_eq!(admission.result_limit(1), 1);
        assert_eq!(
            admission.max_pii_class().expect("PII floor should parse"),
            SensitivityClass::Pii
        );
    }

    fn contact_session(
        tenant_id: TenantId,
        contact_id: ContactId,
        policy: AgentKnowledgePolicy,
    ) -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("test"),
            contact: Some(ContactRef {
                contact_id,
                tenant_id,
                state: ContactVerificationState::Verified,
                canonical_contact_id: None,
                linked_contact_ids: Vec::new(),
                scopes: Vec::new(),
                permissions: serde_json::Value::Null,
                agent_ids: Vec::new(),
                session_ids: Vec::new(),
                verified_contact_point_ids: Vec::new(),
            }),
            agent_context: Some(agent_context(policy)),
            ..SessionMeta::default()
        }
    }

    fn tenant_session(tenant_id: TenantId, policy: AgentKnowledgePolicy) -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant_id,
            channel: Channel::Chat,
            model: ModelId::new("test"),
            contact: None,
            agent_context: Some(agent_context(policy)),
            ..SessionMeta::default()
        }
    }

    fn agent_context(policy: AgentKnowledgePolicy) -> AgentContext {
        let snapshot = AgentPolicySnapshot {
            knowledge_policy: policy,
            ..AgentPolicySnapshot::default()
        };
        let mut context = AgentContext::system_default();
        context.policy_snapshot = serde_json::to_value(snapshot).expect("policy should serialize");
        context
    }

    #[allow(clippy::too_many_arguments)]
    fn node(
        uid: u128,
        tenant_id: TenantId,
        contact_id: Option<ContactId>,
        label: NodeLabel,
        scope: &str,
        pii_class: SensitivityClass,
        name: &str,
    ) -> NodeIndexRow {
        let now = Utc
            .with_ymd_and_hms(2026, 7, 9, 12, 0, 0)
            .single()
            .expect("fixed time should parse");
        NodeIndexRow {
            uid: Uuid::from_u128(uid),
            label,
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: contact_id.map(|id| id.to_string()),
            scope: scope.to_string(),
            name: name.to_string(),
            pii_class,
            valid_to: None,
            valid_from: now,
            properties_summary: Some(json!({"summary": name})),
            last_accessed_at: now,
            quality_score: 0.5,
        }
    }

    fn hit(node: NodeIndexRow) -> RetrievalHit {
        RetrievalHit {
            uid: node.uid,
            score: 1.0,
            legs: LegSources::default(),
            similarity: None,
            lexical_backend: None,
            source_tier: SourceTier::TenantKnowledge,
            knowledge_chunk: None,
            node,
        }
    }
}
