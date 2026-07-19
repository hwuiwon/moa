//! Deterministic standing contact and tenant digest rendering and rebuilds.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use moa_core::{config::MemoryDigestConfig, types::identifiers::StoragePartitionId};
use moa_memory_types::FactCategory;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::consolidate::{Error, Result};

/// Version of the deterministic digest renderer stored with every row.
pub const DIGEST_RENDER_VERSION: u32 = 1;

const DEFAULT_DECAY_FLOOR: f64 = 0.1;
const EPSILON: f64 = 1e-9;

/// Digest scope rendered into the stable header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestScopeKind {
    /// Tenant-bound facts for one contact.
    User,
    /// Tenant-wide facts with no contact owner.
    Tenant,
}

impl DigestScopeKind {
    fn header(self) -> &'static str {
        match self {
            Self::User => "What I know about this user:",
            Self::Tenant => "What I know about this tenant:",
        }
    }
}

/// Fact row consumed by deterministic digest rendering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DigestFact {
    /// Stable Fact node uid.
    pub uid: Uuid,
    /// Fact subject.
    pub subject: String,
    /// Fact predicate.
    pub predicate: String,
    /// Fact object.
    pub object: String,
    /// Fact validity start.
    pub valid_from: DateTime<Utc>,
    /// Current confidence.
    pub confidence: Option<f64>,
    /// Coarse fact category assigned by extraction. Preference-category facts
    /// lead the standing digest; every other category follows. Sourced from the
    /// stored structured field, never re-derived from predicate prose.
    #[serde(default)]
    pub category: FactCategory,
}

/// Rendered digest payload ready for storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedDigest {
    /// Digest text.
    pub content: String,
    /// Fact uids included in the rendered digest.
    pub source_fact_uids: Vec<Uuid>,
}

/// Outcome for one digest rebuild pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestStats {
    /// Digest rows rebuilt or inserted.
    pub digests_rebuilt: u64,
    /// Digest rows skipped because they are fresher than the configured interval.
    pub digests_skipped_fresh: u64,
    /// Persisted digest rows deleted because their identity has no active facts.
    pub digests_deleted: u64,
}

/// Renders one deterministic digest from active Fact rows.
#[must_use]
pub fn render_digest(
    facts: &[DigestFact],
    scope_kind: DigestScopeKind,
    max_tokens: usize,
    _version: u32,
) -> RenderedDigest {
    let mut ordered = facts
        .iter()
        .filter(|fact| confidence_above_floor(fact.confidence))
        .collect::<Vec<_>>();
    ordered.sort_by_key(|fact| {
        (
            fact.category != FactCategory::Preference,
            std::cmp::Reverse(fact.valid_from),
            fact.uid,
        )
    });

    let mut content = format!("{}\n", scope_kind.header());
    let mut source_fact_uids = Vec::new();
    let max_chars = max_tokens.saturating_mul(4);

    for fact in ordered {
        let line = digest_line(fact);
        let candidate = format!("{content}{line}");
        if estimated_tokens(&candidate) > max_tokens || candidate.chars().count() > max_chars {
            break;
        }
        content.push_str(&line);
        source_fact_uids.push(fact.uid);
    }

    RenderedDigest {
        content,
        source_fact_uids,
    }
}

/// Rebuilds tenant and contact digest rows for one storage partition.
pub(crate) async fn rebuild_storage_digests(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    now: DateTime<Utc>,
    config: &MemoryDigestConfig,
) -> Result<DigestStats> {
    let rows = active_digest_fact_rows(pool, storage_partition_id).await?;
    let mut groups = BTreeMap::<DigestIdentity, Vec<DigestFact>>::new();
    for row in rows {
        let identity = match row.scope.as_str() {
            "tenant" if row.user_id.is_none() => DigestIdentity {
                scope_kind: DigestScopeKind::Tenant,
                user_id: None,
            },
            "contact" => {
                let Some(user_id) = row.user_id.clone() else {
                    tracing::warn!(
                        uid = %row.fact.uid,
                        storage_partition_id = %storage_partition_id,
                        "skipping contact-scope digest fact without user_id"
                    );
                    continue;
                };
                DigestIdentity {
                    scope_kind: DigestScopeKind::User,
                    user_id: Some(user_id),
                }
            }
            _ => continue,
        };
        groups.entry(identity).or_default().push(row.fact);
    }

    let mut stats = DigestStats::default();

    // Reconcile persisted identities against the desired active-fact set. An
    // identity whose facts were all erased, forgotten, or decayed forms zero
    // groups above, so iterating only the formed groups can never reach it —
    // its rendered digest text would otherwise survive and be re-injected into
    // prompts. Enumerate persisted identities and delete the orphans.
    let desired = groups
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for identity in persisted_digest_identities(pool, storage_partition_id).await? {
        if !desired.contains(&identity) {
            delete_digest_identity(pool, storage_partition_id, identity.user_id.as_deref()).await?;
            stats.digests_deleted += 1;
        }
    }

    for (identity, facts) in groups {
        if digest_is_fresh(
            pool,
            storage_partition_id,
            identity.user_id.as_deref(),
            now,
            config,
        )
        .await?
        {
            stats.digests_skipped_fresh += 1;
            continue;
        }
        let rendered = render_digest(
            &facts,
            identity.scope_kind,
            config.max_tokens,
            DIGEST_RENDER_VERSION,
        );
        upsert_digest(
            pool,
            storage_partition_id,
            identity.user_id.as_deref(),
            now,
            &rendered,
        )
        .await?;
        stats.digests_rebuilt += 1;
    }
    Ok(stats)
}

fn confidence_above_floor(confidence: Option<f64>) -> bool {
    confidence.unwrap_or(1.0) > DEFAULT_DECAY_FLOOR + EPSILON
}

fn digest_line(fact: &DigestFact) -> String {
    format!(
        "- {} {} {} (since {})\n",
        fact.subject,
        fact.predicate,
        fact.object,
        fact.valid_from.date_naive()
    )
}

fn estimated_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 { 0 } else { chars.div_ceil(4) }
}

fn is_fresh(updated_at: DateTime<Utc>, now: DateTime<Utc>, config: &MemoryDigestConfig) -> bool {
    let min_interval = Duration::hours(config.rebuild_min_interval_hours.max(0));
    updated_at > now - min_interval
}

async fn digest_is_fresh(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: Option<&str>,
    now: DateTime<Utc>,
    config: &MemoryDigestConfig,
) -> Result<bool> {
    let existing = sqlx::query_scalar::<_, DateTime<Utc>>(
        r#"
        SELECT updated_at
        FROM moa.memory_digests
        WHERE storage_partition_id = $1
          AND (($2::text IS NULL AND user_id IS NULL) OR user_id = $2)
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(existing.is_some_and(|updated_at| is_fresh(updated_at, now, config)))
}

async fn upsert_digest(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: Option<&str>,
    now: DateTime<Utc>,
    rendered: &RenderedDigest,
) -> Result<()> {
    let source_fact_uids = Value::Array(
        rendered
            .source_fact_uids
            .iter()
            .map(|uid| json!(uid.to_string()))
            .collect(),
    );
    sqlx::query(
        r#"
        INSERT INTO moa.memory_digests
            (storage_partition_id, user_id, content, source_fact_uids, version, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (storage_partition_id, scope, (COALESCE(user_id, '')))
        DO UPDATE SET
            content = EXCLUDED.content,
            source_fact_uids = EXCLUDED.source_fact_uids,
            version = EXCLUDED.version,
            updated_at = EXCLUDED.updated_at
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(user_id)
    .bind(&rendered.content)
    .bind(source_fact_uids)
    .bind(DIGEST_RENDER_VERSION as i32)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

async fn persisted_digest_identities(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
) -> Result<Vec<DigestIdentity>> {
    let rows = sqlx::query(
        r#"
        SELECT scope, user_id
        FROM moa.memory_digests
        WHERE storage_partition_id = $1
        "#,
    )
    .bind(storage_partition_id.to_string())
    .fetch_all(pool)
    .await?;

    let mut identities = Vec::with_capacity(rows.len());
    for row in rows {
        let scope = row.try_get::<String, _>("scope")?;
        let user_id = row.try_get::<Option<String>, _>("user_id")?;
        let scope_kind = match scope.as_str() {
            "tenant" => DigestScopeKind::Tenant,
            "contact" => DigestScopeKind::User,
            other => {
                return Err(Error::InvalidRow(format!(
                    "memory_digests row has unexpected scope `{other}`"
                )));
            }
        };
        identities.push(DigestIdentity {
            scope_kind,
            user_id,
        });
    }
    Ok(identities)
}

async fn delete_digest_identity(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    user_id: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        DELETE FROM moa.memory_digests
        WHERE storage_partition_id = $1
          AND (($2::text IS NULL AND user_id IS NULL) OR user_id = $2)
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn active_digest_fact_rows(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
) -> Result<Vec<DigestFactRow>> {
    let rows = sqlx::query(
        r#"
        SELECT uid,
               user_id,
               scope,
               confidence,
               valid_from,
               properties_summary
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND label = 'Fact'
          AND valid_to IS NULL
          AND scope IN ('contact', 'tenant')
          AND (confidence IS NULL OR confidence > $2)
        ORDER BY scope ASC, user_id ASC NULLS FIRST, valid_from DESC, uid ASC
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(DEFAULT_DECAY_FLOOR)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(digest_fact_row_from_sql).collect()
}

fn digest_fact_row_from_sql(row: sqlx::postgres::PgRow) -> Result<DigestFactRow> {
    let properties = row.try_get::<Option<Value>, _>("properties_summary")?;
    let subject = property_text(&properties, "subject")?;
    let predicate = property_text(&properties, "predicate")?;
    let object = property_text(&properties, "object")?;
    let category = property_category(&properties);
    Ok(DigestFactRow {
        user_id: row.try_get("user_id")?,
        scope: row.try_get("scope")?,
        fact: DigestFact {
            uid: row.try_get("uid")?,
            subject,
            predicate,
            object,
            valid_from: row.try_get("valid_from")?,
            confidence: row.try_get("confidence")?,
            category,
        },
    })
}

/// Reads the structured fact category from stored node properties, degrading a
/// missing or unrecognized value to [`FactCategory::Other`] so digest ordering
/// never fails on an absent field.
fn property_category(properties: &Option<Value>) -> FactCategory {
    properties
        .as_ref()
        .and_then(|properties| properties.get("category"))
        .cloned()
        .and_then(|value| serde_json::from_value::<FactCategory>(value).ok())
        .unwrap_or_default()
}

fn property_text(properties: &Option<Value>, key: &str) -> Result<String> {
    crate::property_string(properties, key)
        .ok_or_else(|| Error::InvalidRow(format!("Fact node is missing properties.{key}")))
}

#[derive(Debug, Clone, PartialEq)]
struct DigestFactRow {
    user_id: Option<String>,
    scope: String,
    fact: DigestFact,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DigestIdentity {
    scope_kind: DigestScopeKind,
    user_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;

    #[test]
    fn renderer_orders_preference_category_first_newest_within_tier() {
        // Pins: preference-category facts lead the standing digest, newest first,
        // and the newest non-preference fact still sorts after every preference.
        // Ordering keys on the structured category, not on predicate wording.
        let old_preference = preference_fact("00000000-0000-8000-8000-000000000002", 1);
        let new_preference = preference_fact("00000000-0000-8000-8000-000000000003", 3);
        let newer_non_preference = fact("00000000-0000-8000-8000-000000000001", "deploys_to", 5);

        let rendered = render_digest(
            &[newer_non_preference, old_preference, new_preference],
            DigestScopeKind::User,
            600,
            DIGEST_RENDER_VERSION,
        );

        assert_eq!(
            rendered.source_fact_uids,
            vec![
                uuid("00000000-0000-8000-8000-000000000003"),
                uuid("00000000-0000-8000-8000-000000000002"),
                uuid("00000000-0000-8000-8000-000000000001"),
            ]
        );
    }

    #[test]
    fn renderer_truncates_at_whole_line_under_token_budget() {
        // Pins: digest budget truncation never emits partial fact lines.
        let rendered = render_digest(
            &[
                fact("00000000-0000-8000-8000-000000000001", "response_style", 2),
                fact("00000000-0000-8000-8000-000000000002", "owned_by", 1),
            ],
            DigestScopeKind::User,
            24,
            DIGEST_RENDER_VERSION,
        );

        assert_eq!(rendered.source_fact_uids.len(), 1);
        assert!(rendered.content.ends_with('\n'));
        assert!(!rendered.content.contains("owned_by"));
    }

    #[test]
    fn renderer_is_byte_deterministic_for_identical_inputs() {
        // Pins: identical digest inputs render byte-identical content and source uid order.
        let facts = vec![
            fact("00000000-0000-8000-8000-000000000002", "owned_by", 1),
            fact("00000000-0000-8000-8000-000000000001", "owned_by", 1),
        ];

        let first = render_digest(&facts, DigestScopeKind::Tenant, 600, DIGEST_RENDER_VERSION);
        let second = render_digest(&facts, DigestScopeKind::Tenant, 600, DIGEST_RENDER_VERSION);

        assert_eq!(first, second);
    }

    #[test]
    fn renderer_excludes_floor_confidence_facts() {
        // Pins: floor-bound facts stay retrievable but do not enter standing digests.
        let mut floor = fact("00000000-0000-8000-8000-000000000001", "owned_by", 1);
        floor.confidence = Some(DEFAULT_DECAY_FLOOR);
        let active = fact("00000000-0000-8000-8000-000000000002", "owned_by", 2);

        let rendered = render_digest(&[floor, active], DigestScopeKind::Tenant, 600, 1);

        assert_eq!(
            rendered.source_fact_uids,
            vec![uuid("00000000-0000-8000-8000-000000000002")]
        );
    }

    #[test]
    fn rebuild_skips_rows_fresher_than_min_interval() {
        // Pins: rebuild cadence is computed against the injected clock, not wall time.
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let config = MemoryDigestConfig {
            rebuild_min_interval_hours: 6,
            ..MemoryDigestConfig::default()
        };

        assert!(is_fresh(now - Duration::hours(5), now, &config));
        assert!(!is_fresh(now - Duration::hours(7), now, &config));
    }

    #[test]
    fn tenant_digest_input_filter_excludes_user_scope_facts() {
        // Pins: tenant and user digest identities are separate before rendering.
        let rows = vec![
            DigestFactRow {
                user_id: None,
                scope: "tenant".to_string(),
                fact: fact("00000000-0000-8000-8000-000000000001", "owned_by", 1),
            },
            DigestFactRow {
                user_id: Some("user-a".to_string()),
                scope: "user".to_string(),
                fact: fact("00000000-0000-8000-8000-000000000002", "response_style", 2),
            },
        ];
        let mut identities = BTreeMap::<DigestIdentity, Vec<DigestFact>>::new();
        for row in rows {
            let identity = if row.scope == "tenant" {
                DigestIdentity {
                    scope_kind: DigestScopeKind::Tenant,
                    user_id: None,
                }
            } else {
                DigestIdentity {
                    scope_kind: DigestScopeKind::User,
                    user_id: row.user_id.clone(),
                }
            };
            identities.entry(identity).or_default().push(row.fact);
        }

        let tenant = identities
            .get(&DigestIdentity {
                scope_kind: DigestScopeKind::Tenant,
                user_id: None,
            })
            .expect("tenant digest group");

        assert_eq!(tenant.len(), 1);
        assert_eq!(tenant[0].uid, uuid("00000000-0000-8000-8000-000000000001"));
    }

    fn fact(uid: &str, predicate: &str, day: i64) -> DigestFact {
        DigestFact {
            uid: uuid(uid),
            subject: "checkout service".to_string(),
            predicate: predicate.to_string(),
            object: "rust examples".to_string(),
            valid_from: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::days(day),
            confidence: Some(0.9),
            category: FactCategory::Other,
        }
    }

    fn preference_fact(uid: &str, day: i64) -> DigestFact {
        DigestFact {
            category: FactCategory::Preference,
            ..fact(uid, "noted", day)
        }
    }

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).expect("test uuid should parse")
    }
}
