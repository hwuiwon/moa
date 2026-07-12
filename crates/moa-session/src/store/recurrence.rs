//! Exact-fingerprint recurrence-mining queries for the Postgres session store.
//!
//! These read-only aggregates back the background cron that treats *recurrence*
//! as skill-learning evidence: an experience record persists for every assessed
//! segment regardless of the single-session dispatch gate, so the same task
//! recurring across many sub-gate sessions is already recorded here. The queries
//! group resolved/partial `experience_records` by `task_fingerprint` and read the
//! `learning_candidates` decision history for a fingerprint. All rate/qualify
//! logic stays in `moa-skills`; only the I/O lives here, mirroring the
//! regression-monitor split.

use chrono::{DateTime, Utc};
use moa_core::types::experience::LearningCandidateStatus;
use moa_core::types::experience::LearningCandidateType;
use moa_core::types::segment_assessment::SegmentOutcome;

use super::*;

/// One resolved/partial experience that shares a task fingerprint with siblings.
///
/// The recurrence cron ranks these to pick a cluster exemplar and feeds the rest
/// in as siblings. `tool_count` is the number of distinct tool names the segment
/// used (`cardinality(tools_used)`); it is the record-native proxy the cron ranks
/// on, while the authoritative per-session tool-call floor is re-checked against
/// real events in the distiller at dispatch time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurrenceExperienceMember {
    /// Experience record identifier.
    pub experience_id: Uuid,
    /// Session that produced the experience.
    pub session_id: SessionId,
    /// Assessed outcome (`resolved` or `partial`).
    pub outcome: SegmentOutcome,
    /// Confidence in the assessed outcome, scaled to per-mille for a total order.
    pub confidence_milli: i64,
    /// Number of distinct tool names the segment used.
    pub tool_count: usize,
    /// Time the experience record was created.
    pub created_at: DateTime<Utc>,
}

/// A group of resolved/partial experiences sharing one task fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurringExperienceCluster {
    /// Stable task-fingerprint hash the members share.
    pub fingerprint_hash: String,
    /// Member experiences, ordered by creation time ascending.
    pub members: Vec<RecurrenceExperienceMember>,
}

/// One decided-or-open skill learning candidate observed for a task fingerprint.
///
/// The cron consults these to suppress a fingerprint that already has an open
/// candidate, a promoted candidate, or a recently rejected candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidateDecision {
    /// Candidate status (`proposed`, `evaluating`, `promoted`, or `rejected`).
    pub status: LearningCandidateStatus,
    /// Last time the candidate row changed status.
    pub updated_at: DateTime<Utc>,
}

impl PostgresSessionStore {
    /// Lists tenant IDs with at least one resolved/partial experience since `since`.
    ///
    /// The recurrence cron drives per-tenant grouping only for tenants that have
    /// recent learnable experiences, so a tenant with no activity in the window is
    /// skipped entirely.
    pub async fn list_tenants_with_recent_learnable_experiences(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<TenantId>> {
        let experience_records = self.table_name("experience_records");
        let rows = sqlx::query(&format!(
            "SELECT DISTINCT tenant_id FROM {experience_records} \
             WHERE outcome IN ('resolved', 'partial') \
               AND created_at >= $1"
        ))
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let raw: String = row.try_get("tenant_id").map_err(map_sqlx_error)?;
                Uuid::parse_str(&raw).map(TenantId::from).map_err(|error| {
                    MoaError::StorageError(format!("invalid tenant id `{raw}`: {error}"))
                })
            })
            .collect()
    }

    /// Groups a tenant's resolved/partial experiences into per-fingerprint groups,
    /// bounded by recency, as candidates for semantic clustering.
    ///
    /// Every fingerprint with at least one resolved/partial member in
    /// `[since, now]` is a candidate group — including single-occurrence
    /// fingerprints — because the occurrence threshold is applied *after* semantic
    /// clustering pools differently-worded aliases, not before. Filtering by
    /// `HAVING COUNT(*) >= min_occurrences` here would discard three
    /// distinct-fingerprint aliases of the same recurring task that collectively
    /// clear the threshold; the merge would never see them.
    ///
    /// To keep loading every low-count group finite, only the `max_groups` most
    /// recently active fingerprints (by latest member time) are returned, each with
    /// all its members ordered by creation time. A cut group simply does not
    /// participate in this tick's clustering. The
    /// `idx_experience_records_tenant_task` index covers the
    /// `(tenant_id, task_fingerprint)` grouping.
    pub async fn list_candidate_experience_groups(
        &self,
        tenant_id: &TenantId,
        since: DateTime<Utc>,
        max_groups: usize,
    ) -> Result<Vec<RecurringExperienceCluster>> {
        let experience_records = self.table_name("experience_records");
        let rows = sqlx::query(&format!(
            "SELECT \
                 id, \
                 session_id, \
                 task_fingerprint, \
                 outcome, \
                 confidence::DOUBLE PRECISION AS confidence, \
                 COALESCE(cardinality(tools_used), 0)::BIGINT AS tool_count, \
                 created_at \
             FROM {experience_records} \
             WHERE tenant_id = $1 \
               AND outcome IN ('resolved', 'partial') \
               AND created_at >= $2 \
               AND task_fingerprint IN ( \
                   SELECT task_fingerprint FROM {experience_records} \
                   WHERE tenant_id = $1 \
                     AND outcome IN ('resolved', 'partial') \
                     AND created_at >= $2 \
                   GROUP BY task_fingerprint \
                   ORDER BY MAX(created_at) DESC, task_fingerprint ASC \
                   LIMIT $3 \
               ) \
             ORDER BY task_fingerprint ASC, created_at ASC, id ASC"
        ))
        .bind(tenant_id.to_string())
        .bind(since)
        .bind(max_groups as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let mut clusters: Vec<RecurringExperienceCluster> = Vec::new();
        for row in &rows {
            let fingerprint_hash: String =
                row.try_get("task_fingerprint").map_err(map_sqlx_error)?;
            let confidence: f64 = row.try_get("confidence").map_err(map_sqlx_error)?;
            let tool_count: i64 = row.try_get("tool_count").map_err(map_sqlx_error)?;
            let member = RecurrenceExperienceMember {
                experience_id: row.try_get("id").map_err(map_sqlx_error)?,
                session_id: SessionId(row.try_get("session_id").map_err(map_sqlx_error)?),
                outcome: from_db(
                    "segment outcome",
                    &row.try_get::<String, _>("outcome")
                        .map_err(map_sqlx_error)?,
                )?,
                // Per-mille scaling gives the pure ranker a stable total order over
                // confidence without comparing floats.
                confidence_milli: (confidence * 1000.0).round() as i64,
                tool_count: tool_count.max(0) as usize,
                created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
            };
            match clusters.last_mut() {
                Some(cluster) if cluster.fingerprint_hash == fingerprint_hash => {
                    cluster.members.push(member);
                }
                _ => clusters.push(RecurringExperienceCluster {
                    fingerprint_hash,
                    members: vec![member],
                }),
            }
        }
        Ok(clusters)
    }

    /// Lists a tenant's skill learning candidate decisions for one task fingerprint.
    ///
    /// Returns every skill candidate (open or decided) whose `task_fingerprint`
    /// matches, so the recurrence cron can suppress a fingerprint that already has
    /// an open proposal, a promotion, or a rejection still inside its cooldown.
    pub async fn list_skill_candidate_decisions_for_fingerprint(
        &self,
        tenant_id: &TenantId,
        fingerprint_hash: &str,
    ) -> Result<Vec<SkillCandidateDecision>> {
        let learning_candidates = self.table_name("learning_candidates");
        let rows = sqlx::query(&format!(
            "SELECT status, updated_at FROM {learning_candidates} \
             WHERE tenant_id = $1 \
               AND candidate_type = $2 \
               AND task_fingerprint = $3"
        ))
        .bind(tenant_id.to_string())
        .bind(LearningCandidateType::Skill.as_str())
        .bind(fingerprint_hash)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(SkillCandidateDecision {
                    status: from_db(
                        "learning candidate status",
                        &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
                    )?,
                    updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }

    /// Lists candidate decisions for many fingerprints in one query, keyed by hash.
    ///
    /// The batched form of [`Self::list_skill_candidate_decisions_for_fingerprint`]:
    /// the recurrence cron gathers suppression history for every merged fingerprint
    /// across all of a tenant's clusters, so a single `= ANY($3)` scan replaces the
    /// per-fingerprint N+1. Each returned pair is `(task_fingerprint, decision)`;
    /// the caller groups by hash. Duplicate hashes in the input are harmless (the
    /// scan is set-valued). Returns an empty vector for an empty input without
    /// hitting the database.
    pub async fn list_skill_candidate_decisions_for_fingerprints(
        &self,
        tenant_id: &TenantId,
        fingerprint_hashes: &[String],
    ) -> Result<Vec<(String, SkillCandidateDecision)>> {
        if fingerprint_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let learning_candidates = self.table_name("learning_candidates");
        let rows = sqlx::query(&format!(
            "SELECT task_fingerprint, status, updated_at FROM {learning_candidates} \
             WHERE tenant_id = $1 \
               AND candidate_type = $2 \
               AND task_fingerprint = ANY($3)"
        ))
        .bind(tenant_id.to_string())
        .bind(LearningCandidateType::Skill.as_str())
        .bind(fingerprint_hashes)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                let fingerprint_hash: String =
                    row.try_get("task_fingerprint").map_err(map_sqlx_error)?;
                let decision = SkillCandidateDecision {
                    status: from_db(
                        "learning candidate status",
                        &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
                    )?,
                    updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
                };
                Ok((fingerprint_hash, decision))
            })
            .collect()
    }
}
