//! Deterministic post-hydration ranking features for graph-memory retrieval.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use moa_core::{MemoryRankingConfig, MemoryRankingWeights};
use moa_memory_graph::NodeIndexRow;
use moa_memory_types::MemoryScope;
use serde::{Deserialize, Serialize};

/// Ranking pipeline version included in cache fingerprints.
pub const RANKING_PIPELINE_VERSION: u32 = 7;

/// Weights used by the FeatureV1 deterministic scorer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
    /// Rescue bonus for candidates only graph expansion found.
    pub graph_rescue: f64,
    /// Outcome-derived quality prior contribution.
    pub quality: f64,
    /// Additive score for contact-scoped rows.
    pub scope_user: f64,
    /// Additive score for tenant-scoped rows.
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
            graph_rescue: 0.6,
            quality: 0.6,
            scope_user: 0.2,
            scope_workspace: 0.1,
            recency_half_life_days: 90.0,
            access_half_life_days: 14.0,
        }
    }
}

/// Ranking configuration applied after candidate hydration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RankingConfig {
    /// Feature weights used by deterministic post-hydration ranking.
    pub weights: RankingWeights,
}

impl From<&MemoryRankingConfig> for RankingConfig {
    fn from(value: &MemoryRankingConfig) -> Self {
        Self {
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
            graph_rescue: value.graph_rescue,
            quality: value.quality,
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
    first_person_query: bool,
}

impl<'a> FeatureRanker<'a> {
    /// Creates a ranker for one retrieval request.
    #[must_use]
    pub fn new(config: &'a RankingConfig, reference_time: DateTime<Utc>) -> Self {
        Self {
            config,
            reference_time,
            request_scope: None,
            first_person_query: false,
        }
    }

    /// Attaches the caller's request scope for scope-aware scoring.
    #[must_use]
    pub fn with_request_scope(mut self, request_scope: &'a MemoryScope) -> Self {
        self.request_scope = Some(request_scope);
        self
    }

    /// Doubles the caller's user-scope term for first-person queries.
    ///
    /// "What do I prefer" should favor the caller's own facts over
    /// workspace facts with similar text.
    #[must_use]
    pub fn with_first_person_query(mut self, query_text: &str) -> Self {
        self.first_person_query = is_first_person_query(query_text);
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
        let user_scope_weight = if self.first_person_query {
            weights.scope_user * 2.0
        } else {
            weights.scope_user
        };
        let scope_term = match row.scope.as_str() {
            "contact" if self.contact_row_matches_request(row) => user_scope_weight,
            "contact" if self.request_scope.is_none() => user_scope_weight,
            "tenant" => weights.scope_workspace,
            _ => 0.0,
        };

        weights.rrf * rrf_norm
            + weights.recency * recency
            + weights.access * access
            + weights.subject_match * subject
            + weights.overlap * overlap
            + weights.quality * (row.quality_score - 0.5) * 2.0
            + scope_term
    }

    fn contact_row_matches_request(&self, row: &NodeIndexRow) -> bool {
        let Some(MemoryScope::Contact {
            tenant_id,
            contact_id,
        }) = self.request_scope
        else {
            return false;
        };
        let tenant_id = tenant_id.to_string();
        let contact_id = contact_id.to_string();
        row.workspace_id.as_deref() == Some(tenant_id.as_str())
            && row.user_id.as_deref() == Some(contact_id.as_str())
    }
}

/// Tokenizes text using lowercase ASCII alphanumeric token splits.
///
/// Pure-alphabetic tokens are Snowball-stemmed so morphological variants
/// match across query and fact text (`deploys` ↔ `deploy`, `required` ↔
/// `require`). Tokens containing digits are kept verbatim because they act
/// as stable identifiers.
#[must_use]
pub fn normalize_tokens(text: &str) -> BTreeSet<String> {
    let stemmer = rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::English);
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter_map(|raw| {
            let token = raw.to_ascii_lowercase();
            if token.len() < 3 && !token.chars().any(|ch| ch.is_ascii_digit()) {
                return None;
            }
            if token.chars().any(|ch| ch.is_ascii_digit()) {
                return Some(token);
            }
            Some(stemmer.stem(&token).into_owned())
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

fn is_first_person_query(query_text: &str) -> bool {
    query_text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token.to_ascii_lowercase().as_str(),
                "i" | "me" | "my" | "mine"
            )
        })
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
    fn feature_score_scope_term_orders_contact_over_tenant_when_tied() {
        // Pins: FeatureV1 prefers contact facts over tenant facts when text features tie.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("checkout service");
        let contact = row(
            "contact",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        let tenant = row(
            "tenant",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &contact)
                > ranker.score(1.0, 1.0, &query_tokens, &tenant)
        );
    }

    #[test]
    fn feature_score_contact_scope_applies_to_request_contact_only() {
        // Pins: contact-scope boost belongs to the caller's contact row, not every visible contact row.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let request_scope = MemoryScope::Contact {
            tenant_id: moa_core::TenantId::from(Uuid::from_u128(0x100)),
            contact_id: moa_core::ContactId(Uuid::from_u128(0x101)),
        };
        let ranker = FeatureRanker::new(&config, reference_time).with_request_scope(&request_scope);
        let query_tokens = normalize_tokens("checkout service");
        let mut caller = row(
            "contact",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        caller.workspace_id = Some(Uuid::from_u128(0x100).to_string());
        caller.user_id = Some(Uuid::from_u128(0x101).to_string());
        let mut other_contact = row(
            "contact",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        other_contact.workspace_id = Some(Uuid::from_u128(0x100).to_string());
        other_contact.user_id = Some(Uuid::from_u128(0x102).to_string());

        assert!(
            ranker.score(1.0, 1.0, &query_tokens, &caller)
                > ranker.score(1.0, 1.0, &query_tokens, &other_contact)
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

    #[test]
    fn quality_term_contributes_zero_at_neutral_default() {
        // Pins: the default migrated quality score is behavior-preserving.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let mut config_with_quality = RankingConfig::default();
        let mut config_without_quality = config_with_quality.clone();
        config_without_quality.weights.quality = 0.0;
        let query_tokens = normalize_tokens("checkout service");
        let candidate = row(
            "workspace",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );

        assert_eq!(
            FeatureRanker::new(&config_with_quality, reference_time).score(
                1.0,
                1.0,
                &query_tokens,
                &candidate,
            ),
            FeatureRanker::new(&config_without_quality, reference_time).score(
                1.0,
                1.0,
                &query_tokens,
                &candidate,
            )
        );
        config_with_quality.weights.quality = 0.8;
        assert_eq!(
            FeatureRanker::new(&config_with_quality, reference_time).score(
                1.0,
                1.0,
                &query_tokens,
                &candidate,
            ),
            FeatureRanker::new(&config_without_quality, reference_time).score(
                1.0,
                1.0,
                &query_tokens,
                &candidate,
            )
        );
    }

    #[test]
    fn quality_term_is_centered_and_symmetric() {
        // Pins: high and low priors move the score by equal opposite amounts.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let config = RankingConfig::default();
        let ranker = FeatureRanker::new(&config, reference_time);
        let query_tokens = normalize_tokens("checkout service");
        let mut high = row(
            "workspace",
            "checkout service",
            reference_time,
            reference_time,
            None,
        );
        let mut neutral = high.clone();
        let mut low = high.clone();
        high.quality_score = 0.8;
        neutral.quality_score = 0.5;
        low.quality_score = 0.2;

        let high_delta = ranker.score(1.0, 1.0, &query_tokens, &high)
            - ranker.score(1.0, 1.0, &query_tokens, &neutral);
        let low_delta = ranker.score(1.0, 1.0, &query_tokens, &neutral)
            - ranker.score(1.0, 1.0, &query_tokens, &low);

        assert!((high_delta - low_delta).abs() < f64::EPSILON);
        assert!((high_delta - 0.36).abs() < f64::EPSILON);
    }

    #[test]
    fn feature_ranking_with_all_neutral_scores_matches_prompt_eleven_ordering() {
        // Pins: neutral quality priors do not perturb FeatureV1 candidate ordering.
        let reference_time = Utc
            .with_ymd_and_hms(2026, 6, 1, 0, 0, 0)
            .single()
            .expect("test timestamp should be valid");
        let mut without_quality = RankingConfig::default();
        without_quality.weights.quality = 0.0;
        let with_quality = RankingConfig::default();
        let query_tokens = normalize_tokens("checkout deploy target");
        let candidates = [
            row(
                "workspace",
                "checkout",
                reference_time,
                reference_time,
                None,
            ),
            row(
                "user",
                "deploy target",
                reference_time - chrono::Duration::days(7),
                reference_time,
                Some(json!({"summary": "checkout deploy target"})),
            ),
            row(
                "workspace",
                "billing",
                reference_time - chrono::Duration::days(30),
                reference_time,
                Some(json!({"summary": "checkout deploy target"})),
            ),
        ];
        let order = |config: &RankingConfig| {
            let ranker = FeatureRanker::new(config, reference_time);
            let mut scored = candidates
                .iter()
                .map(|row| (row.uid, ranker.score(1.0, 1.0, &query_tokens, row)))
                .collect::<Vec<_>>();
            scored.sort_by(|left, right| {
                right
                    .1
                    .total_cmp(&left.1)
                    .then_with(|| left.0.cmp(&right.0))
            });
            scored.into_iter().map(|(uid, _)| uid).collect::<Vec<_>>()
        };

        assert_eq!(order(&with_quality), order(&without_quality));
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
            quality_score: 0.5,
        }
    }
}
