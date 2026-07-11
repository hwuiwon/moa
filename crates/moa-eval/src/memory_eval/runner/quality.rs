//! Consolidation, digest, extraction, and fragmentation quality phases.

use super::*;

pub(super) async fn run_eval_consolidation(
    pool: &PgPool,
    ledger: &[LedgerFact],
    gold_resolution: &GoldResolutionReport,
    fact_ids_by_uid: &HashMap<Uuid, String>,
    embedder: Arc<dyn EmbeddingProvider>,
    reference_time: DateTime<Utc>,
    digest_config: MemoryDigestConfig,
) -> Result<ConsolidationOutcome> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    let mut outcome = ConsolidationOutcome::default();
    for storage_partition_id in &storage_partition_ids {
        let tenant_id = tenant_id_from_storage_partition(storage_partition_id);
        let workspace_outcome = moa_memory_lifecycle::consolidate_tenant(
            pool,
            tenant_id,
            ConsolidationOptions {
                digest: digest_config.clone(),
                ..ConsolidationOptions::default()
            },
            reference_time,
            Some(embedder.clone()),
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory consolidation failed for storage partition {storage_partition_id}: {error}"
            ))
        })?;
        add_consolidation_outcome(&mut outcome, workspace_outcome);
    }

    verify_restatement_pairs_collapsed(pool, ledger, gold_resolution, fact_ids_by_uid).await?;

    let mut second = ConsolidationOutcome::default();
    for storage_partition_id in &storage_partition_ids {
        let tenant_id = tenant_id_from_storage_partition(storage_partition_id);
        let second_outcome = moa_memory_lifecycle::consolidate_tenant(
            pool,
            tenant_id,
            ConsolidationOptions {
                digest: digest_config.clone(),
                ..ConsolidationOptions::default()
            },
            reference_time,
            Some(embedder.clone()),
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "second memory consolidation failed for storage partition {storage_partition_id}: {error}"
            ))
        })?;
        add_consolidation_outcome(&mut second, second_outcome);
    }
    if !second.has_no_work() {
        return Err(EvalError::InvalidConfig(format!(
            "second consolidation pass was not idempotent: {second:?}"
        )));
    }

    Ok(outcome)
}

pub(super) fn add_consolidation_outcome(
    total: &mut ConsolidationOutcome,
    next: ConsolidationOutcome,
) {
    total.merged += next.merged;
    total.decayed += next.decayed;
    total.at_floor += next.at_floor;
    total.expired_idle += next.expired_idle;
    total.contradiction_supersessions += next.contradiction_supersessions;
    total.entity_embeddings_backfilled += next.entity_embeddings_backfilled;
    total.aliases_promoted += next.aliases_promoted;
    total.duplicates_remaining += next.duplicates_remaining;
    total.digests_rebuilt += next.digests_rebuilt;
    total.digests_skipped_fresh += next.digests_skipped_fresh;
}

pub(super) async fn run_eval_digest_rebuild(
    pool: &PgPool,
    ledger: &[LedgerFact],
    reference_time: DateTime<Utc>,
) -> Result<ConsolidationOutcome> {
    let mut outcome = ConsolidationOutcome::default();
    let config = digest_config_for_eval(true);
    for storage_partition_id in eval_storage_partition_ids(ledger) {
        let tenant_id = tenant_id_from_storage_partition(&storage_partition_id);
        let stats = moa_memory_lifecycle::rebuild_digests(
            pool,
            &tenant_id,
            reference_time,
            &config,
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory digest rebuild failed for storage partition {storage_partition_id}: {error}"
            ))
        })?;
        outcome.digests_rebuilt += stats.digests_rebuilt;
        outcome.digests_skipped_fresh += stats.digests_skipped_fresh;
    }
    Ok(outcome)
}

pub(super) fn digest_config_for_eval(enabled: bool) -> MemoryDigestConfig {
    MemoryDigestConfig {
        enabled,
        ..MemoryDigestConfig::default()
    }
}

pub(super) async fn verify_restatement_pairs_collapsed(
    pool: &PgPool,
    ledger: &[LedgerFact],
    gold_resolution: &GoldResolutionReport,
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<()> {
    let records = gold_records_by_fact_id(gold_resolution);
    for fact in ledger.iter().filter(|fact| fact.restates.is_some()) {
        let canonical_id = fact
            .restates
            .as_deref()
            .expect("filtered restating facts should have canonical ids");
        let canonical = records.get(canonical_id).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "restating fact {} references missing gold record {}",
                fact.fact_id, canonical_id
            ))
        })?;
        let restating = records.get(&fact.fact_id).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "restating fact {} has no gold record",
                fact.fact_id
            ))
        })?;
        let mut uids = canonical
            .node_uids
            .iter()
            .chain(restating.node_uids.iter())
            .copied()
            .collect::<Vec<_>>();
        uids.sort_unstable();
        uids.dedup();
        for uid in &uids {
            if !fact_ids_by_uid.contains_key(uid) {
                return Err(EvalError::InvalidConfig(format!(
                    "restatement pair {} -> {} resolved uid {} missing from fact_ids_by_uid",
                    fact.fact_id, canonical_id, uid
                )));
            }
        }
        let active = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1) AND valid_to IS NULL",
        )
        .bind(&uids)
        .fetch_one(pool)
        .await
        .map_err(crate::eval_sqlx_error)?;
        if active != 1 {
            return Err(EvalError::InvalidConfig(format!(
                "restatement pair {} -> {} has {active} active nodes after consolidation",
                fact.fact_id, canonical_id
            )));
        }
    }
    Ok(())
}

pub(super) async fn extraction_precision_counts(
    pool: &PgPool,
    ledger: &[LedgerFact],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<ExtractionPrecisionCounts> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    let total_fact_nodes = if storage_partition_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.node_index WHERE label = 'Fact' AND storage_partition_id = ANY($1)",
        )
        .bind(&storage_partition_ids)
        .fetch_one(pool)
        .await
        .map_err(crate::eval_sqlx_error)?
    };
    Ok(ExtractionPrecisionCounts {
        mapped_fact_nodes: fact_ids_by_uid.len(),
        total_fact_nodes: usize::try_from(total_fact_nodes).map_err(|_| {
            EvalError::InvalidConfig(format!(
                "stored Fact node count {total_fact_nodes} cannot fit usize"
            ))
        })?,
    })
}

pub(super) async fn entity_fragmentation_counts(
    pool: &PgPool,
    ledger: &[LedgerFact],
) -> Result<crate::memory_eval::EntityFragmentationCounts> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    let active_entity_nodes = if storage_partition_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM moa.node_index
            WHERE label = 'Entity'
              AND valid_to IS NULL
              AND storage_partition_id = ANY($1)
            "#,
        )
        .bind(&storage_partition_ids)
        .fetch_one(pool)
        .await
        .map_err(crate::eval_sqlx_error)?
    };
    let distinct_ledger_mentions = ledger
        .iter()
        .flat_map(|fact| {
            [&fact.subject, &fact.object].into_iter().map(|mention| {
                let user_id = match fact.scope {
                    ScopeTier::Contact => fact.user_id.to_string(),
                    ScopeTier::Tenant => String::new(),
                };
                (
                    scope_tier_name(fact.scope).to_string(),
                    fact.storage_partition_id.to_string(),
                    user_id,
                    normalize_entity_name(mention),
                )
            })
        })
        .filter(|(_, _, _, mention)| !mention.trim().is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Ok(crate::memory_eval::EntityFragmentationCounts {
        active_entity_nodes: usize::try_from(active_entity_nodes).map_err(|_| {
            EvalError::InvalidConfig(format!(
                "stored Entity node count {active_entity_nodes} cannot fit usize"
            ))
        })?,
        distinct_ledger_mentions,
    })
}

pub(super) fn scope_tier_name(scope: ScopeTier) -> &'static str {
    match scope {
        ScopeTier::Tenant => "tenant",
        ScopeTier::Contact => "contact",
    }
}

pub(super) fn contact_id_from_user_id(user_id: &UserId) -> ContactId {
    uuid::Uuid::parse_str(user_id.as_str())
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id.as_str())))
}

pub(super) fn gold_records_by_fact_id(
    gold_resolution: &GoldResolutionReport,
) -> HashMap<String, crate::memory_eval::GoldNodeRecord> {
    gold_resolution
        .records
        .iter()
        .map(|record| (record.fact_id.clone(), record.clone()))
        .collect()
}

pub(super) fn ledger_by_fact_id(ledger: &[LedgerFact]) -> HashMap<String, LedgerFact> {
    ledger
        .iter()
        .map(|fact| (fact.fact_id.clone(), fact.clone()))
        .collect()
}

pub(super) async fn digest_context_by_user(
    pool: &PgPool,
    ledger: &[LedgerFact],
) -> Result<HashMap<(String, String), String>> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT storage_partition_id, user_id, scope, content
        FROM moa.memory_digests
        WHERE storage_partition_id = ANY($1)
        ORDER BY storage_partition_id ASC, CASE scope WHEN 'user' THEN 0 ELSE 1 END, user_id ASC NULLS FIRST
        "#,
    )
    .bind(&storage_partition_ids)
    .fetch_all(pool)
    .await
    .map_err(crate::eval_sqlx_error)?;
    let mut tenant_content = HashMap::<String, String>::new();
    let mut user_content = HashMap::<(String, String), String>::new();
    for row in rows {
        let storage_partition_id: String = row
            .try_get("storage_partition_id")
            .map_err(crate::eval_sqlx_error)?;
        let user_id: Option<String> = row.try_get("user_id").map_err(crate::eval_sqlx_error)?;
        let scope: String = row.try_get("scope").map_err(crate::eval_sqlx_error)?;
        let content: String = row.try_get("content").map_err(crate::eval_sqlx_error)?;
        if scope == "tenant" {
            tenant_content.insert(storage_partition_id, content);
        } else if scope == "contact"
            && let Some(user_id) = user_id
        {
            user_content.insert((storage_partition_id, user_id), content);
        }
    }

    let users = ledger
        .iter()
        .map(|fact| {
            (
                fact.storage_partition_id.to_string(),
                fact.user_id.to_string(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut contexts = HashMap::new();
    for (storage_partition_id, user_id) in users {
        let mut content = String::new();
        if let Some(user_digest) =
            user_content.get(&(storage_partition_id.clone(), user_id.clone()))
        {
            content.push_str(user_digest);
            content.push('\n');
        }
        if let Some(tenant_digest) = tenant_content.get(&storage_partition_id) {
            content.push_str(tenant_digest);
        }
        contexts.insert((storage_partition_id, user_id), content);
    }
    Ok(contexts)
}

pub(super) fn preference_context_hit(
    probe: &Probe,
    final_candidates: &[RetrievedCandidate],
    digest_context: &HashMap<(String, String), String>,
    ledger_by_fact_id: &HashMap<String, LedgerFact>,
) -> Option<bool> {
    if probe.probe_type != ProbeType::PreferenceApplication {
        return None;
    }

    let mut context = digest_context
        .get(&(
            probe.storage_partition_id.to_string(),
            probe.user_id.to_string(),
        ))
        .cloned()
        .unwrap_or_default();
    for candidate in final_candidates
        .iter()
        .filter(|candidate| candidate.rank > 0 && candidate.rank <= RETRIEVAL_EVAL_FINAL_K)
    {
        for fact_id in candidate
            .fact_id
            .as_deref()
            .into_iter()
            .chain(candidate.equivalent_fact_ids.iter().map(String::as_str))
        {
            if let Some(fact) = ledger_by_fact_id.get(fact_id) {
                context.push('\n');
                context.push_str(&fact.subject);
                context.push(' ');
                context.push_str(&fact.predicate);
                context.push(' ');
                context.push_str(&fact.object);
                context.push('\n');
                context.push_str(&fact.answer);
            }
        }
    }

    Some(probe.expected_fact_ids.iter().all(|fact_id| {
        ledger_by_fact_id.get(fact_id).is_some_and(|fact| {
            tokens_contained(&fact.object, &context) || tokens_contained(&fact.answer, &context)
        })
    }))
}

pub(super) fn tokens_contained(expected: &str, haystack: &str) -> bool {
    let haystack_tokens = token_set(haystack);
    let expected_tokens = token_set(expected);
    !expected_tokens.is_empty()
        && expected_tokens
            .iter()
            .all(|token| haystack_tokens.contains(token))
}

pub(super) fn token_set(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect()
}
