//! ACE-style lesson curation for skill `Lesson` nodes.
//!
//! Skill lessons accumulate append-only: `learn_lesson` never dedups, merges,
//! or retires, so an active skill's rendered addenda drift into near-identical
//! restatements and stale advice — the context rot an evolving playbook needs a
//! curator to prevent. This pass is that curator. It mirrors the entity- and
//! fact-resolution passes in [`crate::consolidate`]: a blocking, deterministic,
//! off-hot-path batch that closes redundant nodes by supersession (never
//! delete) and bitemporally retires nodes that have aged out.
//!
//! v1 is intentionally minimal: exact-normalized-summary grouping only (no
//! embeddings, no LLM verifier), oldest-canonical supersession, and age-based
//! retirement with a single renewal exemption.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    types::contact::ContactId, types::identifiers::TenantId, types::memory::RlsContext,
};
use moa_memory_graph::{ExistingSupersessionIntent, GraphStore, PostgresGraphStore};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::consolidate::Result;

/// Actor recorded on supersession and invalidation writes issued by curation.
const CURATION_ACTOR: &str = "consolidation";
/// Actor kind recorded on curation writes.
const CURATION_ACTOR_KIND: &str = "system";

/// Tuning for the skill-lesson curation pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonCurationOptions {
    /// Age past which an un-renewed lesson is bitemporally retired.
    ///
    /// A lesson whose `valid_from` is older than `now - retire_after` is closed
    /// unless it is a canonical that absorbed a merge from a duplicate created
    /// within that same window.
    #[serde(with = "duration_secs")]
    pub retire_after: Duration,
}

impl Default for LessonCurationOptions {
    fn default() -> Self {
        Self {
            retire_after: Duration::days(180),
        }
    }
}

/// Serializable outcome of one tenant lesson-curation pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LessonCurationStats {
    /// Duplicate lessons superseded into an older canonical lesson.
    pub merged: u64,
    /// Stale lessons bitemporally invalidated by retirement.
    pub retired: u64,
}

/// Curates a tenant's active skill lessons: exact-normalized-summary dedup by
/// supersession into the oldest canonical, then age-based retirement.
///
/// Lessons are grouped per `(storage_partition, contact, scope, skill_uid,
/// normalized summary)` so grouping never crosses ownership and every merge
/// stays within one supersession scope. Within a group the oldest lesson
/// (`valid_from`, uid tie-break — the [`crate::consolidate`] convention) is
/// canonical and the rest are superseded into it. Retirement then closes any
/// lesson older than `opts.retire_after` unless it is a canonical that absorbed
/// a duplicate created within that window (a "renewal"). Nodes are never
/// deleted, so provenance is preserved.
pub async fn curate_skill_lessons(
    pool: &PgPool,
    tenant_id: TenantId,
    now: DateTime<Utc>,
    opts: &LessonCurationOptions,
) -> Result<LessonCurationStats> {
    let lessons = active_lesson_rows(pool, &tenant_id).await?;
    if lessons.is_empty() {
        return Ok(LessonCurationStats::default());
    }
    let cutoff = now - opts.retire_after;

    let mut stores: BTreeMap<Option<Uuid>, PostgresGraphStore> = BTreeMap::new();
    let mut stats = LessonCurationStats::default();
    let mut closed = BTreeSet::new();
    let mut renewed_canonicals = BTreeSet::new();

    for group in duplicate_groups(&lessons) {
        let canonical = &group[0];
        for duplicate in group.iter().skip(1) {
            scoped_store(&mut stores, pool, tenant_id, duplicate.contact_id)
                .close_existing_node_with_supersession(ExistingSupersessionIntent {
                    old_uid: duplicate.uid,
                    replacement_uid: canonical.uid,
                    valid_to: duplicate.valid_from,
                    invalidated_at: now,
                    reason: "lesson_curation".to_string(),
                    actor_id: CURATION_ACTOR.to_string(),
                    actor_kind: CURATION_ACTOR_KIND.to_string(),
                })
                .await?;
            closed.insert(duplicate.uid);
            stats.merged += 1;
            // A canonical that absorbs a duplicate created within the retention
            // window is treated as renewed, so an old canonical still gathering
            // fresh restatements is not retired out from under them.
            if duplicate.valid_from >= cutoff {
                renewed_canonicals.insert(canonical.uid);
            }
        }
    }

    for lesson in &lessons {
        if closed.contains(&lesson.uid) || renewed_canonicals.contains(&lesson.uid) {
            continue;
        }
        if lesson.valid_from < cutoff {
            scoped_store(&mut stores, pool, tenant_id, lesson.contact_id)
                .invalidate_node(lesson.uid, "lesson_retired")
                .await?;
            stats.retired += 1;
        }
    }

    Ok(stats)
}

/// Normalizes a lesson summary for exact-match grouping.
///
/// Lowercases, collapses internal whitespace to single spaces, and strips
/// trailing ASCII punctuation so cosmetic restatements ("Rotate keys." vs
/// "rotate  keys") collide into one group.
#[must_use]
pub fn normalize_lesson_summary(summary: &str) -> String {
    let collapsed = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .to_lowercase()
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .to_string()
}

/// Groups duplicate lessons, oldest canonical first, dropping singleton groups.
fn duplicate_groups(rows: &[LessonRow]) -> Vec<Vec<LessonRow>> {
    let mut groups = BTreeMap::<LessonKey, Vec<LessonRow>>::new();
    for row in rows {
        groups
            .entry(LessonKey {
                contact_id: row.contact_id,
                scope: row.scope.clone(),
                skill_uid: row.skill_uid.clone(),
                normalized_summary: normalize_lesson_summary(&row.summary),
            })
            .or_default()
            .push(row.clone());
    }

    groups
        .into_values()
        .filter(|group| group.len() > 1)
        .map(|mut group| {
            group.sort_by_key(|row| (row.valid_from, row.uid));
            group
        })
        .collect()
}

/// Returns a scoped graph store for one lesson's ownership, cached per contact.
///
/// Lessons carry no embeddings, so no vector backend is attached; the store is
/// used only for bitemporal supersession and invalidation writes.
fn scoped_store<'a>(
    stores: &'a mut BTreeMap<Option<Uuid>, PostgresGraphStore>,
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: Option<Uuid>,
) -> &'a PostgresGraphStore {
    stores.entry(contact_id).or_insert_with(|| {
        let scope = match contact_id {
            Some(contact_id) => RlsContext::contact(tenant_id, ContactId(contact_id)),
            None => RlsContext::tenant(tenant_id),
        };
        PostgresGraphStore::scoped(pool.clone(), scope)
    })
}

/// Loads active skill-`Lesson` index rows for one tenant.
async fn active_lesson_rows(pool: &PgPool, tenant_id: &TenantId) -> Result<Vec<LessonRow>> {
    let rows = sqlx::query(
        r#"
        SELECT node.uid,
               node.contact_id,
               node.scope,
               node.valid_from,
               node.properties_summary->>'skill_uid' AS skill_uid,
               COALESCE(node.properties_summary->>'summary', node.name) AS summary
        FROM moa.node_index AS node
        WHERE node.tenant_id = $1
          AND node.label = 'Lesson'
          AND node.valid_to IS NULL
          AND node.properties_summary->>'skill_uid' IS NOT NULL
        ORDER BY node.valid_from ASC, node.uid ASC
        "#,
    )
    .bind(tenant_id.0)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(LessonRow {
                uid: row.try_get("uid")?,
                contact_id: row.try_get("contact_id")?,
                scope: row.try_get("scope")?,
                valid_from: row.try_get("valid_from")?,
                skill_uid: row.try_get("skill_uid")?,
                summary: row.try_get("summary")?,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LessonRow {
    uid: Uuid,
    contact_id: Option<Uuid>,
    scope: String,
    valid_from: DateTime<Utc>,
    skill_uid: String,
    summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LessonKey {
    contact_id: Option<Uuid>,
    scope: String,
    skill_uid: String,
    normalized_summary: String,
}

/// Serde helper: `chrono::Duration` as whole seconds.
mod duration_secs {
    use chrono::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(value.num_seconds())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        let secs = i64::deserialize(deserializer)?;
        Ok(Duration::seconds(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace_case_and_trailing_punctuation() {
        // Pins: cosmetic restatements normalize to one grouping key.
        assert_eq!(
            normalize_lesson_summary("Rotate  keys before deploy."),
            normalize_lesson_summary("rotate keys before deploy")
        );
        assert_eq!(
            normalize_lesson_summary("Check rotation first!!"),
            "check rotation first"
        );
    }

    #[test]
    fn duplicate_groups_keep_oldest_canonical_and_drop_singletons() {
        // Pins: only multi-member groups survive, oldest (valid_from, uid) first.
        let skill = Uuid::from_u128(0x11).to_string();
        let group = vec![
            lesson(
                "00000000-0000-8000-8000-000000000003",
                &skill,
                "Rotate keys.",
                2,
            ),
            lesson(
                "00000000-0000-8000-8000-000000000001",
                &skill,
                "rotate  keys",
                2,
            ),
            lesson(
                "00000000-0000-8000-8000-000000000002",
                &skill,
                "Rotate keys!",
                0,
            ),
            lesson(
                "00000000-0000-8000-8000-000000000004",
                &skill,
                "unique lesson",
                1,
            ),
        ];

        let groups = duplicate_groups(&group);

        assert_eq!(groups.len(), 1, "singleton lesson is excluded");
        let uids = groups[0].iter().map(|row| row.uid).collect::<Vec<_>>();
        assert_eq!(
            uids,
            vec![
                Uuid::parse_str("00000000-0000-8000-8000-000000000002").unwrap(),
                Uuid::parse_str("00000000-0000-8000-8000-000000000001").unwrap(),
                Uuid::parse_str("00000000-0000-8000-8000-000000000003").unwrap(),
            ],
            "oldest valid_from is canonical, uid breaks ties"
        );
    }

    #[test]
    fn different_skills_never_group_together() {
        // Pins: an identical summary under two skills stays two singleton groups.
        let rows = vec![
            lesson(
                "00000000-0000-8000-8000-000000000001",
                &Uuid::from_u128(0x1).to_string(),
                "same summary",
                0,
            ),
            lesson(
                "00000000-0000-8000-8000-000000000002",
                &Uuid::from_u128(0x2).to_string(),
                "same summary",
                1,
            ),
        ];

        assert!(duplicate_groups(&rows).is_empty());
    }

    fn lesson(uid: &str, skill_uid: &str, summary: &str, day_offset: i64) -> LessonRow {
        use chrono::TimeZone;
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        LessonRow {
            uid: Uuid::parse_str(uid).expect("test uuid"),
            contact_id: None,
            scope: "tenant".to_string(),
            valid_from: base + Duration::days(day_offset),
            skill_uid: skill_uid.to_string(),
            summary: summary.to_string(),
        }
    }
}
