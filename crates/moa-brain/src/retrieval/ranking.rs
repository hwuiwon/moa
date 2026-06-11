//! Deterministic post-hydration ranking features for graph-memory retrieval.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use moa_core::{MemoryRankingConfig, MemoryRankingMode, MemoryRankingWeights, MemoryScope};
use moa_memory_graph::NodeIndexRow;
use serde::{Deserialize, Serialize};

/// Ranking pipeline version included in cache fingerprints.
pub const RANKING_PIPELINE_VERSION: u32 = 3;

/// Ranking mode for hydrated hybrid retrieval candidates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RankingMode {
    /// Preserve the legacy RRF plus layer-bias ranking path.
    Legacy,
    /// Apply deterministic feature scoring after candidate hydration.
    #[default]
    FeatureV1,
}

/// Weights used by the FeatureV1 deterministic scorer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankingWeights {
    /// Normalized reciprocal-rank fusion contribution.
    pub rrf: f64,
    /// Valid-from recency contribution.
    pub recency: f64,
    /// Last-access recency contribution.
    pub access: f64,
    /// Exact subject-token match contribution.
    pub subject_match: f64,
    /// Query-to-summary token overlap contribution.
    pub overlap: f64,
    /// Additive score for user-scoped rows.
    pub scope_user: f64,
    /// Additive score for workspace-scoped rows.
    pub scope_workspace: f64,
    /// Half-life in days for valid-from recency.
    pub recency_half_life_days: f64,
    /// Half-life in days for access recency.
    pub access_half_life_days: f64,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            rrf: 1.0,
            recency: 0.3,
            access: 0.15,
            subject_match: 0.5,
            overlap: 0.35,
            scope_user: 0.2,
            scope_workspace: 0.1,
            recency_half_life_days: 90.0,
            access_half_life_days: 14.0,
        }
    }
}

/// Ranking configuration applied after candidate hydration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankingConfig {
    /// Ranking mode.
    pub mode: RankingMode,
    /// Feature weights used when `mode` is `FeatureV1`.
    pub weights: RankingWeights,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            mode: RankingMode::FeatureV1,
            weights: RankingWeights::default(),
        }
    }
}

impl From<&MemoryRankingConfig> for RankingConfig {
    fn from(value: &MemoryRankingConfig) -> Self {
        Self {
            mode: match value.mode {
                MemoryRankingMode::Legacy => RankingMode::Legacy,
                MemoryRankingMode::FeatureV1 => RankingMode::FeatureV1,
            },
            weights: RankingWeights::from(&value.weights),
        }
    }
}

impl From<&MemoryRankingWeights> for RankingWeights {
    fn from(value: &MemoryRankingWeights) -> Self {
        Self {
            rrf: value.rrf,
            recency: value.recency,
            access: value.access,
            subject_match: value.subject_match,
            overlap: value.overlap,
            scope_user: value.scope_user,
            scope_workspace: value.scope_workspace,
            recency_half_life_days: value.recency_half_life_days,
            access_half_life_days: value.access_half_life_days,
        }
    }
}

/// Deterministic scorer for hydrated retrieval candidates.
pub struct FeatureRanker<'a> {
    config: &'a RankingConfig,
    reference_time: DateTime<Utc>,
    request_scope: Option<&'a MemoryScope>,
}

impl<'a> FeatureRanker<'a> {
    /// Creates a ranker for one retrieval request.
    #[must_use]
    pub fn new(config: &'a RankingConfig, reference_time: DateTime<Utc>) -> Self {
        Self {
            config,
            reference_time,
            request_scope: None,
        }
    }

    /// Attaches the caller's request scope for scope-aware scoring.
    #[must_use]
    pub fn with_request_scope(mut self, request_scope: &'a MemoryScope) -> Self {
        self.request_scope = Some(request_scope);
        self
    }

    /// Scores one hydrated candidate against the fused candidate set.
    #[must_use]
    pub fn score(
        &self,
        fused_score: f64,
        max_fused_score: f64,
        query_tokens: &BTreeSet<String>,
        row: &NodeIndexRow,
    ) -> f64 {
        let weights = &self.config.weights;
        let rrf_norm = if max_fused_score > 0.0 {
            fused_score / max_fused_score
        } else {
            0.0
        };
        let recency = decay_score(
            self.reference_time,
            row.valid_from,
            weights.recency_half_life_days,
        );
        let access = decay_score(
            self.reference_time,
            row.last_accessed_at,
            weights.access_half_life_days,
        );
        let subject = subject_match_score(query_tokens, &row.name);
        let overlap = overlap_score(query_tokens, row);
        let scope_term = match row.scope.as_str() {
            "user" if self.user_row_matches_request(row) => weights.scope_user,
            "user" if self.request_scope.is_none() => weights.scope_user,
            "workspace" => weights.scope_workspace,
            _ => 0.0,
        };

        weights.rrf * rrf_norm
            + weights.recency * recency
            + weights.access * access
            + weights.subject_match * subject
            + weights.overlap * overlap
            + scope_term
    }

    fn user_row_matches_request(&self, row: &NodeIndexRow) -> bool {
        let Some(MemoryScope::User {
            workspace_id,
            user_id,
        }) = self.request_scope
        else {
            return false;
        };
        row.workspace_id.as_deref() == Some(workspace_id.as_str())
            && row.user_id.as_deref() == Some(user_id.as_str())
    }
}

/// Tokenizes text using lowercase ASCII alphanumeric token splits.
#[must_use]
pub fn normalize_tokens(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|raw| {
            let token = raw.to_ascii_lowercase();
            (token.len() >= 3 || token.chars().any(|ch| ch.is_ascii_digit())).then_some(token)
        })
        .collect()
}

/// Returns a stable hash over the ranking pipeline version and configuration.
#[must_use]
pub fn ranking_fingerprint(config: &RankingConfig) -> [u8; 32] {
    let mut canonical = format!("version={RANKING_PIPELINE_VERSION}|");
    canonical.push_str(
        &serde_json::to_string(config)
            .expect("ranking config contains only serializable primitive fields"),
    );
    *blake3::hash(canonical.as_bytes()).as_bytes()
}

fn subject_match_score(query_tokens: &BTreeSet<String>, name: &str) -> f64 {
    let name_tokens = normalize_tokens(name);
    if name_tokens.is_empty() {
        return 0.0;
    }
    if name_tokens.iter().all(|token| query_tokens.contains(token))
        || name_tokens
            .iter()
            .any(|token| is_identifier_token(token) && query_tokens.contains(token))
    {
        1.0
    } else {
        0.0
    }
}

fn is_identifier_token(token: &str) -> bool {
    token.len() >= 3 && token.chars().any(|ch| ch.is_ascii_digit())
}

fn overlap_score(query_tokens: &BTreeSet<String>, row: &NodeIndexRow) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let summary_json;
    let summary = match row.properties_summary.as_ref() {
        Some(value) => match value.get("summary").and_then(serde_json::Value::as_str) {
            Some(summary) => summary,
            None => {
                summary_json = value.to_string();
                &summary_json
            }
        },
        None => &row.name,
    };
    let summary_tokens = normalize_tokens(summary);
    if summary_tokens.is_empty() {
        return 0.0;
    }
    query_tokens.intersection(&summary_tokens).count() as f64 / query_tokens.len() as f64
}

fn decay_score(
    reference_time: DateTime<Utc>,
    observed_time: DateTime<Utc>,
    half_life_days: f64,
) -> f64 {
    if half_life_days <= 0.0 {
        return 0.0;
    }
    if observed_time > reference_time {
        return 0.0;
    }
    let age_seconds = reference_time
        .signed_duration_since(observed_time)
        .num_seconds() as f64;
    let age_days = age_seconds / 86_400.0;
    2.0_f64.powf(-(age_days / half_life_days))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_memory_graph::{NodeLabel, PiiClass};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn feature_score_promotes_recent_fact_over_stale_duplicate() {
        // Pins: valid_from recency changes rank when fused scores and text features tie.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("checkout deploy target");
        let recent = row(
            "workspace",
            "checkout deploy target",
            reference_time - chrono::Duration::days(7),
            reference_time - chrono::Duration::days(30),
            Some(json!({"summary": "checkout deploy target"})),
        );
        let stale = row(
            "workspace",
            "checkout deploy target",
            reference_time - chrono::Duration::days(180),
            reference_time - chrono::Duration::days(30),
            Some(json!({"summary": "checkout deploy target"})),
        );

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &recent)
                > ranker.score(1.0, 1.0, &query_tokens, &stale)
        );
    }

    #[test]
    fn feature_score_exact_subject_match_outranks_overlap_only() {
        // Pins: exact subject tokens beat a row that only overlaps in summary text.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("what is checkout service deploy target");
        let exact = row(
            "workspace",
            "checkout service",
            reference_time,
            reference_time,
            Some(json!({"summary": "owner notes"})),
        );
        let overlap_only = row(
            "workspace",
            "billing service",
            reference_time,
            reference_time,
            Some(json!({"summary": "checkout service deploy target"})),
        );

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &exact)
                > ranker.score(1.0, 1.0, &query_tokens, &overlap_only)
        );
    }

    #[test]
    fn feature_score_identifier_token_counts_as_subject_match() {
        // Pins: explicit stable identifiers in a query match verbose fact names.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.overlap = 0.0;
        config.weights.scope_workspace = 0.0;
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("fact01 fact04 auth deploy release cadence");
        let explicit_identifier = row(
            "workspace",
            "fact01 auth-service deployment flyio bluegreen Monday release window superseded",
            reference_time,
            reference_time,
            None,
        );
        let missing_identifier = row(
            "workspace",
            "fact99 auth-service deployment flyio bluegreen Monday release window superseded",
            reference_time,
            reference_time,
            None,
        );

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &explicit_identifier)
                > ranker.score(1.0, 1.0, &query_tokens, &missing_identifier)
        );
    }

    #[test]
    fn feature_score_numeric_subject_segments_must_all_match() {
        // Pins: service-like numeric suffixes disambiguate otherwise identical subject tokens.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.overlap = 0.0;
        config.weights.scope_workspace = 0.0;
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens(
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
        );
        let exact = row(
            "workspace",
            "audit-shipper-dep-0-0-0",
            reference_time,
            reference_time,
            None,
        );
        let sibling = row(
            "workspace",
            "audit-shipper-dep-0-4-0",
            reference_time,
            reference_time,
            None,
        );

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &exact)
                > ranker.score(1.0, 1.0, &query_tokens, &sibling)
        );
    }

    #[test]
    fn feature_score_overlap_uses_structured_properties_when_summary_is_absent() {
        // Pins: overlap mirrors lexical fallback by searching structured properties text.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("contact email");
        let structured = row(
            "workspace",
            "User 00",
            reference_time,
            reference_time,
            Some(json!({"predicate": "contact_email", "object": "user@example.invalid"})),
        );
        let name_only = row("workspace", "User 00", reference_time, reference_time, None);

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &structured)
                > ranker.score(1.0, 1.0, &query_tokens, &name_only)
        );
    }

    #[test]
    fn feature_score_scope_term_orders_user_over_workspace_when_tied() {
        // Pins: FeatureV1 preserves the old preference for user facts over workspace facts.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("checkout service");
        let user = row(
            "user",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        let workspace = row(
            "workspace",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &user)
                > ranker.score(1.0, 1.0, &query_tokens, &workspace)
        );
    }

    #[test]
    fn feature_score_user_scope_applies_to_request_user_only() {
        // Pins: user-scope boost belongs to the caller's user row, not every visible user row.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let request_scope = MemoryScope::User {
            workspace_id: moa_core::WorkspaceId::new("workspace"),
            user_id: moa_core::UserId::new("user-a"),
        };
        let ranker = FeatureRanker::new(&config, reference_time).with_request_scope(&request_scope);
        let query_tokens = normalize_tokens("checkout service");
        let mut caller = row(
            "user",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        caller.user_id = Some("user-a".to_string());
        let mut other_user = row(
            "user",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        other_user.user_id = Some("user-b".to_string());

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &caller)
                > ranker.score(1.0, 1.0, &query_tokens, &other_user)
        );
    }

    #[test]
    fn feature_score_is_deterministic_for_fixed_reference_time() {
        // Pins: FeatureV1 scoring does not depend on wall clock when reference time is fixed.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("checkout service deploy target");
        let candidate = row(
            "workspace",
            "checkout service",
            reference_time - chrono::Duration::days(3),
            reference_time - chrono::Duration::days(1),
            Some(json!({"summary": "checkout service deploy target"})),
        );

        assert_eq!(
            ranker.score(0.5, 1.0, &query_tokens, &candidate),
            ranker.score(0.5, 1.0, &query_tokens, &candidate)
        );
    }

    #[test]
    fn feature_score_ignores_future_access_time() {
        // Pins: retrieval-time access bumps after the ranking reference do not affect eval scoring.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.overlap = 0.0;
        config.weights.scope_user = 0.0;
        config.weights.scope_workspace = 0.0;
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("checkout service");
        let future_access = row(
            "workspace",
            "checkout service",
            reference_time,
            reference_time + chrono::Duration::days(1),
            None,
        );
        let past_access = row(
            "workspace",
            "checkout service",
            reference_time,
            reference_time - chrono::Duration::days(1),
            None,
        );

        assert_eq!(ranker.score(1.0, 1.0, &query_tokens, &future_access), 0.0);
        assert!(ranker.score(1.0, 1.0, &query_tokens, &past_access) > 0.0);
    }

    fn row(
        scope: &str,
        name: &str,
        valid_from: DateTime<Utc>,
        last_accessed_at: DateTime<Utc>,
        properties_summary: Option<serde_json::Value>,
    ) -> NodeIndexRow {
        let uid = Uuid::now_v7();
        NodeIndexRow {
            uid,
            label: NodeLabel::Fact,
            workspace_id: Some("workspace".to_string()),
            user_id: None,
            scope: scope.to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from,
            properties_summary,
            last_accessed_at,
        }
    }
}
