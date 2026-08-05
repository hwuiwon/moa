//! Central `PostgreSQL` migrations for MOA.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    ExecutionPlanDefinition, PlanAmendment, PlanAmendmentOperation,
};
use moa_artifacts::{
    canonical::canonical_hash,
    document::{ArtifactDocument, ArtifactStatus},
    validation::validate_for_status,
};
use moa_execution::{
    CanonicalExecutionPlan,
    capability::{amendment_hash, plan_hash},
};
use serde_json::Value;
use sqlx::{Acquire, PgConnection, PgPool, raw_sql};
use tokio_postgres::{Client, NoTls};

/// Refinery migrations embedded into the migration runner binary.
mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations/postgres");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MigrationIdentity {
    version: i32,
    name: String,
    checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryRow {
    version: Option<String>,
    name: Option<String>,
    checksum: Option<String>,
}

#[derive(Clone, Copy)]
enum HistoryRequirement {
    Prefix,
    Complete,
}

#[derive(Clone, Copy)]
struct SchemaFragment {
    name: &'static str,
    sql: &'static str,
}

const TENANT_CONNECTOR_CONNECTIONS_SQL: &str =
    include_str!("../migrations/postgres/V000050__tenant_connector_connections.sql");
const CONNECTOR_CONNECTION_USE_GRANTS_SQL: &str =
    include_str!("../migrations/postgres/V000051__connector_connection_use_grants.sql");
const CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_BEGIN: &str =
    "-- BEGIN TENANT CONNECTOR CREDENTIAL SLOT AUTH FRAGMENT";
const CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_END: &str =
    "-- END TENANT CONNECTOR CREDENTIAL SLOT AUTH FRAGMENT";
const STAGED_CREDENTIAL_OPERATION_FRAGMENT_BEGIN: &str =
    "-- BEGIN STAGED TENANT CREDENTIAL OPERATION AUTH FRAGMENT";
const STAGED_CREDENTIAL_OPERATION_FRAGMENT_END: &str =
    "-- END STAGED TENANT CREDENTIAL OPERATION AUTH FRAGMENT";

const AUTH_SCHEMA_FRAGMENTS: &[SchemaFragment] = &[
    SchemaFragment {
        name: "auth_baseline",
        sql: include_str!("../migrations/postgres/V000003__auth_baseline.sql"),
    },
    SchemaFragment {
        name: "authz_outbox_claims",
        sql: include_str!("../migrations/postgres/V000013__authz_outbox_claims.sql"),
    },
    SchemaFragment {
        name: "oauth_authorization_server",
        sql: include_str!("../migrations/postgres/V000032__oauth_authorization_server.sql"),
    },
    SchemaFragment {
        name: "tenant_credential_vault",
        sql: include_str!("../migrations/postgres/V000036__tenant_credential_vault.sql"),
    },
];

fn auth_schema_fragments() -> Result<Vec<SchemaFragment>> {
    let mut fragments = AUTH_SCHEMA_FRAGMENTS.to_vec();
    fragments.push(SchemaFragment {
        name: "tenant_connector_connections",
        sql: extract_marked_schema_fragment(
            TENANT_CONNECTOR_CONNECTIONS_SQL,
            CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_BEGIN,
            CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_END,
        )?,
    });
    fragments.push(SchemaFragment {
        name: "connector_connection_use_grants",
        sql: extract_marked_schema_fragment(
            CONNECTOR_CONNECTION_USE_GRANTS_SQL,
            STAGED_CREDENTIAL_OPERATION_FRAGMENT_BEGIN,
            STAGED_CREDENTIAL_OPERATION_FRAGMENT_END,
        )?,
    });
    Ok(fragments)
}

fn extract_marked_schema_fragment<'a>(
    source: &'a str,
    begin_marker: &str,
    end_marker: &str,
) -> Result<&'a str> {
    let begin_offsets = source
        .match_indices(begin_marker)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let end_offsets = source
        .match_indices(end_marker)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if begin_offsets.len() != 1 || end_offsets.len() != 1 {
        bail!(
            "schema fragment markers must occur exactly once: begin={}, end={}",
            begin_offsets.len(),
            end_offsets.len()
        );
    }
    let fragment_start = begin_offsets[0] + begin_marker.len();
    let fragment_end = end_offsets[0];
    if fragment_start >= fragment_end {
        bail!("schema fragment end marker must follow its begin marker");
    }
    let fragment = source[fragment_start..fragment_end].trim();
    if fragment.is_empty() {
        bail!("schema fragment between markers must not be empty");
    }
    Ok(fragment)
}

const ORCHESTRATOR_SCHEMA_FRAGMENTS: &[SchemaFragment] = &[SchemaFragment {
    name: "orchestrator_baseline",
    sql: include_str!("../migrations/postgres/V000004__orchestrator_baseline.sql"),
}];

const OCSF_SCHEMA_FRAGMENTS: &[SchemaFragment] = &[SchemaFragment {
    name: "ocsf_baseline",
    sql: include_str!("../migrations/postgres/V000005__ocsf_baseline.sql"),
}];

const REFINERY_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5246_4e59;
const SHARED_CATALOG_RETRY_LIMIT: usize = 5;
const REFINERY_HISTORY_TABLE: &str = "public.refinery_schema_history";
const DESTRUCTIVE_RESET_REQUIRED: &str =
    "the database must be destructively rebuilt or reset for the contiguous migration epoch";

/// Advisory lock used by schema-isolated migration helpers.
pub(crate) const SCHEMA_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5343_4845;

/// Runs all central refinery migrations.
pub async fn run(database_url: &str) -> Result<()> {
    run_embedded_migrations(database_url)
        .await
        .map(|_report| ())
}

/// Runs all central refinery migrations and returns the labels of the migrations
/// newly applied by this call.
///
/// On a database that is already up to date the returned list is empty, which is
/// the observable signal callers (and idempotency tests) use to confirm a re-run
/// applied nothing.
pub async fn run_reporting_applied(database_url: &str) -> Result<Vec<String>> {
    let report = run_embedded_migrations(database_url).await?;
    Ok(report
        .applied_migrations()
        .iter()
        .map(|migration| migration.to_string())
        .collect())
}

/// Validates that a database has the exact complete embedded migration history.
///
/// Runtime consumers use this instead of applying migrations themselves. A
/// missing, partial, legacy, or checksum-divergent history fails closed.
pub async fn validate_complete_history(pool: &PgPool) -> Result<()> {
    let history_exists: bool = sqlx::query_scalar(
        "SELECT pg_catalog.to_regclass('public.refinery_schema_history') IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("check central migration history table")?;
    if !history_exists {
        bail!("central migration history is missing; {DESTRUCTIVE_RESET_REQUIRED}");
    }

    let rows = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
        "SELECT history.version::TEXT, history.name::TEXT, history.checksum::TEXT \
         FROM public.refinery_schema_history AS history ORDER BY history.version",
    )
    .fetch_all(pool)
    .await
    .context("read central migration history")?
    .into_iter()
    .map(|(version, name, checksum)| HistoryRow {
        version,
        name,
        checksum,
    })
    .collect::<Vec<_>>();

    validate_history_rows(
        &rows,
        &expected_migration_identities(),
        HistoryRequirement::Complete,
    )
}

/// Connects to Postgres, takes the refinery advisory lock, validates the
/// migration epoch, runs the embedded migrations, and returns the report.
async fn run_embedded_migrations(database_url: &str) -> Result<refinery::Report> {
    let (mut client, connection) = tokio_postgres::connect(database_url, NoTls)
        .await
        .context("connect to Postgres for refinery migrations")?;
    let connection_task = tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(error = %error, "refinery migration connection task failed");
        }
    });

    let result = match client
        .execute(
            "SELECT pg_advisory_lock($1)",
            &[&REFINERY_MIGRATION_LOCK_ID],
        )
        .await
        .context("acquire refinery migration advisory lock")
    {
        Err(error) => Err(error),
        Ok(_) => {
            let run_result = async {
                prepare_execution_plan_v2_cutover(&mut client).await?;
                let report = run_with_shared_catalog_retry(&mut client).await?;
                let rewritten = rewrite_archived_session_statuses(&mut client).await?;
                if rewritten > 0 {
                    tracing::info!(
                        rewritten,
                        "rewrote archived session status payloads to idle"
                    );
                }
                verify_execution_plan_v2_cutover(&client).await?;
                Ok(report)
            }
            .await;
            let unlock_result = client
                .execute(
                    "SELECT pg_advisory_unlock($1)",
                    &[&REFINERY_MIGRATION_LOCK_ID],
                )
                .await
                .context("release refinery migration advisory lock");

            match (run_result, unlock_result) {
                (Ok(report), Ok(_)) => Ok(report),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        }
    };

    drop(client);
    let _ = connection_task.await;

    if let Ok(report) = &result {
        tracing::info!(
            applied = report.applied_migrations().len(),
            "refinery migrations complete"
        );
    }
    result
}

async fn run_with_shared_catalog_retry(client: &mut Client) -> Result<refinery::Report> {
    let mut applied_migrations = Vec::new();
    for attempt in 1..=SHARED_CATALOG_RETRY_LIMIT {
        validate_history_before_migration(client).await?;
        let mut runner = central_migration_runner();
        runner.set_migration_table_name(REFINERY_HISTORY_TABLE);
        match runner.run_async(&mut *client).await {
            Ok(report) => {
                applied_migrations.extend(report.applied_migrations().iter().cloned());
                return Ok(refinery::Report::new(applied_migrations));
            }
            Err(error)
                if attempt < SHARED_CATALOG_RETRY_LIMIT
                    && is_shared_catalog_concurrency_error(&error) =>
            {
                if let Some(report) = error.report() {
                    applied_migrations.extend(report.applied_migrations().iter().cloned());
                }
                tracing::warn!(
                    attempt,
                    retry_limit = SHARED_CATALOG_RETRY_LIMIT,
                    "retrying migration after concurrent cluster-role catalog update"
                );
                tokio::time::sleep(Duration::from_millis(25 * attempt as u64)).await;
            }
            Err(error) => return Err(error).context("run refinery migrations"),
        }
    }
    unreachable!("the bounded migration retry loop always returns")
}

const EXECUTION_PLAN_HASH_DOMAIN: &str = "moa.execution.plan";
const EXECUTION_AMENDMENT_HASH_DOMAIN: &str = "moa.execution.amendment";
const EXECUTION_NONTERMINAL_RUN_STATUSES: &[&str] = &[
    "awaiting_confirmation",
    "queued",
    "running",
    "waiting_input",
    "waiting_review",
    "waiting_replan",
    "compensating",
];

/// Performs the one-way persisted execution-plan v1 to v2 cutover before V55.
///
/// V55 installs v2-only constraints, so an existing V54 database must be
/// rewritten while the migration advisory lock is held and before refinery
/// applies the SQL migration. Fresh databases have no execution tables yet and
/// take the ordinary empty-schema path.
async fn prepare_execution_plan_v2_cutover(client: &mut Client) -> Result<()> {
    if !relation_exists(client, "moa.execution_run").await? {
        return Ok(());
    }

    migrate_v1_skill_execution_templates(client).await?;
    reject_invalid_execution_plan_versions(client).await?;

    let active_rows = client
        .query(
            "SELECT run_uid::TEXT, status \
             FROM moa.execution_run \
             WHERE (initial_plan #>> '{definition,schema_version}' = '1' \
                    OR active_plan #>> '{definition,schema_version}' = '1') \
               AND status = ANY($1::TEXT[]) \
             ORDER BY run_uid LIMIT 11",
            &[&EXECUTION_NONTERMINAL_RUN_STATUSES],
        )
        .await
        .context("inspect active v1 execution runs")?;
    if !active_rows.is_empty() {
        let ids = active_rows
            .iter()
            .take(10)
            .map(|row| format!("{} ({})", row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect::<Vec<_>>();
        bail!(
            "execution-plan v2 cutover requires all v1 runs to be inactive; found: {}",
            ids.join(", ")
        );
    }

    let active_tasks = client
        .query(
            "SELECT task.task_id::TEXT, task.run_uid::TEXT, task.status \
             FROM moa.execution_task AS task \
             JOIN moa.execution_run AS run ON run.run_uid = task.run_uid \
             WHERE run.initial_plan #>> '{definition,schema_version}' = '1' \
               AND task.status IN ( \
                   'pending','reserved','running','waiting_input','waiting_replan' \
               ) \
             ORDER BY task.task_id LIMIT 11",
            &[],
        )
        .await
        .context("inspect active tasks beneath inactive v1 execution runs")?;
    if !active_tasks.is_empty() {
        let ids = active_tasks
            .iter()
            .take(10)
            .map(|row| {
                format!(
                    "{} under {} ({})",
                    row.get::<_, String>(0),
                    row.get::<_, String>(1),
                    row.get::<_, String>(2)
                )
            })
            .collect::<Vec<_>>();
        bail!(
            "execution-plan v2 cutover found nonterminal tasks beneath inactive v1 runs: {}",
            ids.join(", ")
        );
    }

    let tx = client
        .transaction()
        .await
        .context("begin execution-plan v2 cutover")?;
    let rows = tx
        .query(
            "SELECT run_uid::TEXT, initial_plan::TEXT, active_plan::TEXT, \
                    initial_plan_hash, active_plan_hash, confirmed_plan_hash, \
                    plan_history::TEXT, source_provenance::TEXT \
             FROM moa.execution_run \
             WHERE initial_plan #>> '{definition,schema_version}' = '1' \
                OR active_plan #>> '{definition,schema_version}' = '1' \
             ORDER BY run_uid FOR UPDATE",
            &[],
        )
        .await
        .context("lock inactive v1 execution runs")?;
    if rows.is_empty() {
        tx.commit()
            .await
            .context("commit empty execution-plan v2 cutover")?;
        return Ok(());
    }

    tx.batch_execute("ALTER TABLE moa.execution_run DISABLE TRIGGER execution_run_update_guard")
        .await
        .context("disable execution-run update guard for v2 cutover")?;

    let mut rewritten = 0_u64;
    for row in rows {
        let run_uid: String = row.get(0);
        let initial_text: String = row.get(1);
        let active_text: String = row.get(2);
        let old_initial_hash: String = row.get(3);
        let old_active_hash: String = row.get(4);
        let old_confirmed_hash: Option<String> = row.get(5);
        let history_text: String = row.get(6);
        let provenance_text: String = row.get(7);

        let (initial, initial_hash) = upgrade_execution_plan_snapshot(
            &run_uid,
            "initial_plan",
            &initial_text,
            &old_initial_hash,
        )?;
        let (active, active_hash) = upgrade_execution_plan_snapshot(
            &run_uid,
            "active_plan",
            &active_text,
            &old_active_hash,
        )?;
        if old_confirmed_hash
            .as_deref()
            .is_some_and(|hash| hash != old_initial_hash)
        {
            bail!(
                "execution run {run_uid} confirmed_plan_hash is not bound to its initial v1 plan"
            );
        }

        let history = upgrade_execution_plan_history(
            &run_uid,
            &history_text,
            &initial.definition,
            &active.definition,
        )?;
        let provenance = upgrade_execution_source_provenance(
            &run_uid,
            &provenance_text,
            &initial_hash.to_string(),
        )?;
        let initial_hash_text = initial_hash.to_string();
        let active_hash_text = active_hash.to_string();
        let initial_json = serde_json::to_string(&initial)
            .with_context(|| format!("encode rewritten initial plan for run {run_uid}"))?;
        let active_json = serde_json::to_string(&active)
            .with_context(|| format!("encode rewritten active plan for run {run_uid}"))?;
        let history_json = serde_json::to_string(&history)
            .with_context(|| format!("encode rewritten plan history for run {run_uid}"))?;
        let provenance_json = serde_json::to_string(&provenance)
            .with_context(|| format!("encode rewritten source provenance for run {run_uid}"))?;
        let confirmed_hash = old_confirmed_hash
            .as_ref()
            .map(|_| initial_hash_text.clone());
        let updated = tx
            .execute(
                "UPDATE moa.execution_run \
                 SET initial_plan = $2::TEXT::JSONB, active_plan = $3::TEXT::JSONB, \
                     initial_plan_hash = $4, active_plan_hash = $5, \
                     confirmed_plan_hash = $6, plan_history = $7::TEXT::JSONB, \
                     source_provenance = $8::TEXT::JSONB, updated_at = NOW() \
                 WHERE run_uid::TEXT = $1",
                &[
                    &run_uid,
                    &initial_json,
                    &active_json,
                    &initial_hash_text,
                    &active_hash_text,
                    &confirmed_hash,
                    &history_json,
                    &provenance_json,
                ],
            )
            .await
            .with_context(|| format!("rewrite execution run {run_uid} to plan v2"))?;
        if updated != 1 {
            bail!("execution-plan v2 cutover updated {updated} rows for run {run_uid}");
        }
        rewritten = rewritten
            .checked_add(1)
            .context("execution-plan v2 rewrite count overflow")?;
    }

    tx.batch_execute("ALTER TABLE moa.execution_run ENABLE TRIGGER execution_run_update_guard")
        .await
        .context("restore execution-run update guard after v2 cutover")?;
    verify_execution_plan_v2_cutover_tx(&tx).await?;
    tx.commit()
        .await
        .context("commit execution-plan v2 cutover")?;
    tracing::info!(rewritten, "rewrote inactive execution plans to schema v2");
    Ok(())
}

async fn relation_exists(client: &Client, relation: &str) -> Result<bool> {
    client
        .query_one(
            "SELECT pg_catalog.to_regclass($1) IS NOT NULL",
            &[&relation],
        )
        .await
        .with_context(|| format!("check relation {relation}"))
        .map(|row| row.get(0))
}

async fn migrate_v1_skill_execution_templates(client: &mut Client) -> Result<()> {
    if !relation_exists(client, "moa.artifact_revision").await? {
        return Ok(());
    }
    let tx = client
        .transaction()
        .await
        .context("begin skill execution-template v2 cutover")?;
    let rows = tx
        .query(
            "SELECT revision.revision_uid::TEXT, revision.definition::TEXT, \
                    revision.source_format, revision.status \
             FROM moa.artifact_revision AS revision \
             JOIN moa.artifact AS artifact \
               ON artifact.artifact_uid = revision.artifact_uid \
             WHERE artifact.kind = 'skill' \
               AND revision.definition \
                   #>> '{definition,spec,execution_plan,plan,schema_version}' = '1' \
             ORDER BY revision.revision_uid FOR UPDATE",
            &[],
        )
        .await
        .context("lock stored v1 skill execution templates")?;
    let rewritten = rows.len();
    for row in rows {
        let revision_uid: String = row.get(0);
        let definition_text: String = row.get(1);
        let source_format: String = row.get(2);
        let status_text: String = row.get(3);
        let mut definition: Value = serde_json::from_str(&definition_text)
            .with_context(|| format!("decode skill revision {revision_uid} definition"))?;
        let plan = definition
            .pointer_mut("/definition/spec/execution_plan/plan")
            .with_context(|| {
                format!("skill revision {revision_uid} lost its execution template plan")
            })?;
        upgrade_plan_definition_json(plan, &revision_uid, "skill execution template")?;
        let document: ArtifactDocument = serde_json::from_value(definition.clone())
            .with_context(|| format!("decode rewritten skill revision {revision_uid}"))?;
        let status = status_text.parse::<ArtifactStatus>().map_err(|error| {
            anyhow::anyhow!("skill revision {revision_uid} has invalid status: {error}")
        })?;
        let canonical_hash = canonical_hash(&document)
            .with_context(|| format!("hash rewritten skill revision {revision_uid}"))?;
        let validation_report = serde_json::to_string(&validate_for_status(&document, status))
            .with_context(|| format!("encode validation for skill revision {revision_uid}"))?;
        let source_text = match source_format.as_str() {
            "json" => serde_json::to_vec_pretty(&document)
                .with_context(|| format!("render JSON source for skill revision {revision_uid}"))?,
            "yaml" => serde_yaml::to_string(&document)
                .with_context(|| format!("render YAML source for skill revision {revision_uid}"))?
                .into_bytes(),
            other => bail!("skill revision {revision_uid} has unsupported source format {other}"),
        };
        let definition_json = serde_json::to_string(&document)
            .with_context(|| format!("encode rewritten skill revision {revision_uid}"))?;
        let updated = tx
            .execute(
                "UPDATE moa.artifact_revision \
                 SET definition = $2::TEXT::JSONB, canonical_hash = $3, source_text = $4, \
                     validation_report = $5::TEXT::JSONB, updated_at = NOW() \
                 WHERE revision_uid::TEXT = $1",
                &[
                    &revision_uid,
                    &definition_json,
                    &&canonical_hash[..],
                    &source_text,
                    &validation_report,
                ],
            )
            .await
            .with_context(|| format!("rewrite skill revision {revision_uid} to plan v2"))?;
        if updated != 1 {
            bail!("skill execution-template cutover updated {updated} rows for {revision_uid}");
        }
    }
    let remaining: i64 = tx
        .query_one(
            "SELECT COUNT(*) FROM moa.artifact_revision AS revision \
             JOIN moa.artifact AS artifact ON artifact.artifact_uid = revision.artifact_uid \
             WHERE artifact.kind = 'skill' \
               AND revision.definition \
                   #>> '{definition,spec,execution_plan,plan,schema_version}' = '1'",
            &[],
        )
        .await
        .context("verify zero v1 skill execution templates")?
        .get(0);
    if remaining != 0 {
        bail!("skill execution-template cutover left {remaining} v1 revisions");
    }
    tx.commit()
        .await
        .context("commit skill execution-template v2 cutover")?;
    tracing::info!(rewritten, "rewrote skill execution templates to schema v2");
    Ok(())
}

async fn reject_invalid_execution_plan_versions(client: &Client) -> Result<()> {
    let rows = client
        .query(
            "SELECT run_uid::TEXT, \
                    initial_plan #>> '{definition,schema_version}', \
                    active_plan #>> '{definition,schema_version}' \
             FROM moa.execution_run \
             WHERE COALESCE(initial_plan #>> '{definition,schema_version}', '') \
                       NOT IN ('1','2') \
                OR COALESCE(active_plan #>> '{definition,schema_version}', '') \
                       NOT IN ('1','2') \
                OR (initial_plan #>> '{definition,schema_version}') \
                     <> (active_plan #>> '{definition,schema_version}') \
             ORDER BY run_uid LIMIT 11",
            &[],
        )
        .await
        .context("inspect persisted execution-plan versions")?;
    if rows.is_empty() {
        return Ok(());
    }
    let facts = rows
        .iter()
        .take(10)
        .map(|row| {
            format!(
                "{} (initial={:?}, active={:?})",
                row.get::<_, String>(0),
                row.get::<_, Option<String>>(1),
                row.get::<_, Option<String>>(2)
            )
        })
        .collect::<Vec<_>>();
    bail!(
        "execution-plan v2 cutover found malformed or mixed plan versions: {}",
        facts.join(", ")
    )
}

fn upgrade_execution_plan_snapshot(
    run_uid: &str,
    field: &str,
    text: &str,
    stored_hash: &str,
) -> Result<(CanonicalExecutionPlan, moa_execution::ExecutionHash)> {
    let mut value: Value = serde_json::from_str(text)
        .with_context(|| format!("decode {field} for execution run {run_uid}"))?;
    let embedded_hash = value
        .get("plan_hash")
        .and_then(Value::as_str)
        .with_context(|| format!("execution run {run_uid} {field} has no plan_hash"))?
        .to_string();
    let definition = value
        .get_mut("definition")
        .with_context(|| format!("execution run {run_uid} {field} has no definition"))?;
    let old_hash = legacy_execution_hash(EXECUTION_PLAN_HASH_DOMAIN, definition, true)?;
    if old_hash != stored_hash || old_hash != embedded_hash {
        bail!(
            "execution run {run_uid} {field} v1 hashes disagree: computed={old_hash}, embedded={embedded_hash}, column={stored_hash}"
        );
    }
    upgrade_plan_definition_json(definition, run_uid, field)?;
    let mut snapshot: CanonicalExecutionPlan = serde_json::from_value(value)
        .with_context(|| format!("decode rewritten {field} for execution run {run_uid}"))?;
    let hash = plan_hash(&snapshot.definition)
        .with_context(|| format!("hash rewritten {field} for execution run {run_uid}"))?;
    snapshot.plan_hash = hash;
    Ok((snapshot, hash))
}

fn upgrade_plan_definition_json(value: &mut Value, run_uid: &str, field: &str) -> Result<()> {
    let object = value
        .as_object_mut()
        .with_context(|| format!("execution run {run_uid} {field} definition is not an object"))?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        bail!("execution run {run_uid} {field} is not an exact v1 plan");
    }
    object.insert("schema_version".to_string(), Value::from(2));
    object.insert(
        "cancel_policy".to_string(),
        Value::String("retain_effects".to_string()),
    );
    let nodes = object
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .with_context(|| format!("execution run {run_uid} {field} has no nodes array"))?;
    for (index, node) in nodes.iter_mut().enumerate() {
        add_empty_compensation(node, &format!("{field}.nodes[{index}]"))?;
    }
    Ok(())
}

fn add_empty_compensation(node: &mut Value, path: &str) -> Result<()> {
    let object = node
        .as_object_mut()
        .with_context(|| format!("{path} is not an object"))?;
    if object.contains_key("compensation") {
        bail!("{path} already carries a compensation field in schema v1");
    }
    object.insert("compensation".to_string(), Value::Null);
    Ok(())
}

fn upgrade_execution_plan_history(
    run_uid: &str,
    text: &str,
    initial: &ExecutionPlanDefinition,
    expected_active: &ExecutionPlanDefinition,
) -> Result<Value> {
    let mut history: Value = serde_json::from_str(text)
        .with_context(|| format!("decode plan history for execution run {run_uid}"))?;
    let entries = history
        .as_array_mut()
        .with_context(|| format!("execution run {run_uid} plan_history is not an array"))?;
    let mut replay = initial.clone();
    for (index, entry) in entries.iter_mut().enumerate() {
        let expected_base = u64::try_from(index)
            .context("plan history index exceeds u64")?
            .checked_add(1)
            .context("plan history revision overflow")?;
        let expected_revision = expected_base
            .checked_add(1)
            .context("plan history revision overflow")?;
        let object = entry.as_object_mut().with_context(|| {
            format!("execution run {run_uid} plan_history[{index}] is not an object")
        })?;
        if object.get("base_plan_revision").and_then(Value::as_u64) != Some(expected_base)
            || object.get("plan_revision").and_then(Value::as_u64) != Some(expected_revision)
            || object.get("outcome").and_then(Value::as_str) != Some("applied")
        {
            bail!(
                "execution run {run_uid} plan_history[{index}] has invalid revision or outcome metadata"
            );
        }
        let stored_amendment_hash = object
            .get("amendment_hash")
            .and_then(Value::as_str)
            .with_context(|| {
                format!("execution run {run_uid} plan_history[{index}] has no amendment_hash")
            })?
            .to_string();
        let stored_active_hash = object
            .get("active_plan_hash")
            .and_then(Value::as_str)
            .with_context(|| {
                format!("execution run {run_uid} plan_history[{index}] has no active_plan_hash")
            })?
            .to_string();
        let amendment_value = object.get_mut("amendment").with_context(|| {
            format!("execution run {run_uid} plan_history[{index}] has no amendment")
        })?;
        let old_amendment_hash =
            legacy_execution_hash(EXECUTION_AMENDMENT_HASH_DOMAIN, amendment_value, false)?;
        if old_amendment_hash != stored_amendment_hash {
            bail!("execution run {run_uid} plan_history[{index}] amendment hash is corrupt");
        }
        upgrade_amendment_json(amendment_value, run_uid, index)?;
        let amendment: PlanAmendment = serde_json::from_value(amendment_value.clone())
            .with_context(|| {
                format!("decode rewritten amendment {index} for execution run {run_uid}")
            })?;
        if amendment.base_plan_revision != expected_base {
            bail!(
                "execution run {run_uid} plan_history[{index}] has base revision {}, expected {expected_base}",
                amendment.base_plan_revision
            );
        }
        apply_migrated_amendment(&mut replay, &amendment, run_uid, index)?;
        let legacy_active_hash = legacy_plan_hash_from_v2(&replay).with_context(|| {
            format!("reconstruct legacy active plan hash for run {run_uid} history {index}")
        })?;
        if legacy_active_hash != stored_active_hash {
            bail!("execution run {run_uid} plan_history[{index}] active plan hash is corrupt");
        }
        let next_plan_hash = plan_hash(&replay)
            .with_context(|| format!("hash replayed plan history for run {run_uid}"))?;
        let next_amendment_hash = amendment_hash(&amendment)
            .with_context(|| format!("hash rewritten amendment for run {run_uid}"))?;
        object.insert(
            "amendment_hash".to_string(),
            Value::String(next_amendment_hash.to_string()),
        );
        object.insert(
            "active_plan_hash".to_string(),
            Value::String(next_plan_hash.to_string()),
        );
    }
    if &replay != expected_active {
        bail!("execution run {run_uid} plan history does not replay to its active v2 definition");
    }
    Ok(history)
}

fn legacy_plan_hash_from_v2(plan: &ExecutionPlanDefinition) -> Result<String> {
    let mut value = serde_json::to_value(plan).context("encode v2 plan for legacy hash replay")?;
    let object = value
        .as_object_mut()
        .context("encoded v2 execution plan is not an object")?;
    object.insert("schema_version".to_string(), Value::from(1));
    object.remove("cancel_policy");
    let nodes = object
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .context("encoded v2 execution plan has no nodes")?;
    for node in nodes {
        let object = node
            .as_object_mut()
            .context("encoded v2 execution-plan node is not an object")?;
        if object.get("compensation") != Some(&Value::Null) {
            bail!("migrated v1 plan replay unexpectedly carries compensation");
        }
        object.remove("compensation");
    }
    legacy_execution_hash(EXECUTION_PLAN_HASH_DOMAIN, &value, true)
}

fn upgrade_amendment_json(value: &mut Value, run_uid: &str, index: usize) -> Result<()> {
    let object = value.as_object_mut().with_context(|| {
        format!("execution run {run_uid} plan_history[{index}].amendment is not an object")
    })?;
    if object.get("schema_version").and_then(Value::as_u64) != Some(1) {
        bail!("execution run {run_uid} plan_history[{index}] is not an exact v1 amendment");
    }
    object.insert("schema_version".to_string(), Value::from(2));
    let operations = object
        .get_mut("operations")
        .and_then(Value::as_array_mut)
        .with_context(|| format!("execution run {run_uid} amendment {index} has no operations"))?;
    for (operation_index, operation) in operations.iter_mut().enumerate() {
        let Some(node) = operation.get_mut("node") else {
            continue;
        };
        add_empty_compensation(
            node,
            &format!("plan_history[{index}].operations[{operation_index}].node"),
        )?;
    }
    Ok(())
}

fn apply_migrated_amendment(
    plan: &mut ExecutionPlanDefinition,
    amendment: &PlanAmendment,
    run_uid: &str,
    history_index: usize,
) -> Result<()> {
    for operation in &amendment.operations {
        match operation {
            PlanAmendmentOperation::AddNode { node } => {
                if plan.nodes.iter().any(|existing| existing.id == node.id) {
                    bail!(
                        "execution run {run_uid} plan_history[{history_index}] reuses node {}",
                        node.id
                    );
                }
                plan.nodes.push(node.clone());
            }
            PlanAmendmentOperation::ReplacePendingNode { node_id, node } => {
                let position = plan
                    .nodes
                    .iter()
                    .position(|existing| existing.id == *node_id)
                    .with_context(|| {
                        format!(
                            "execution run {run_uid} plan_history[{history_index}] replaces missing node {node_id}"
                        )
                    })?;
                plan.nodes[position] = node.clone();
            }
            PlanAmendmentOperation::RemovePendingNode { node_id } => {
                let before = plan.nodes.len();
                plan.nodes.retain(|node| node.id != *node_id);
                if before == plan.nodes.len() {
                    bail!(
                        "execution run {run_uid} plan_history[{history_index}] removes missing node {node_id}"
                    );
                }
            }
        }
    }
    Ok(())
}

fn upgrade_execution_source_provenance(
    run_uid: &str,
    text: &str,
    initial_plan_hash: &str,
) -> Result<Value> {
    let mut provenance: Value = serde_json::from_str(text)
        .with_context(|| format!("decode source provenance for execution run {run_uid}"))?;
    if provenance.get("kind").and_then(Value::as_str) == Some("generated_plan") {
        let final_hash = provenance
            .get_mut("planner")
            .and_then(Value::as_object_mut)
            .with_context(|| {
                format!("generated source provenance for run {run_uid} has no planner")
            })?
            .get_mut("final_plan_hash")
            .with_context(|| {
                format!("generated source provenance for run {run_uid} has no final_plan_hash")
            })?;
        *final_hash = Value::String(initial_plan_hash.to_string());
    }
    Ok(provenance)
}

fn legacy_execution_hash(domain: &str, value: &Value, sort_nodes: bool) -> Result<String> {
    let mut canonical = value.clone();
    if sort_nodes {
        let nodes = canonical
            .get_mut("nodes")
            .and_then(Value::as_array_mut)
            .context("legacy execution plan has no nodes array")?;
        nodes.sort_by(|left, right| {
            left.get("id")
                .and_then(Value::as_str)
                .cmp(&right.get("id").and_then(Value::as_str))
        });
    }
    let bytes = moa_core::canonical_json::canonical_json_bytes(&canonical)
        .context("canonicalize legacy execution document")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

async fn verify_execution_plan_v2_cutover(client: &Client) -> Result<()> {
    if !relation_exists(client, "moa.execution_run").await? {
        return Ok(());
    }
    let count: i64 = client
        .query_one(
            "SELECT COUNT(*) \
             FROM moa.execution_run \
             WHERE initial_plan #>> '{definition,schema_version}' <> '2' \
                OR active_plan #>> '{definition,schema_version}' <> '2'",
            &[],
        )
        .await
        .context("verify zero persisted execution-plan v1 rows")?
        .get(0);
    if count != 0 {
        bail!("execution-plan v2 cutover left {count} non-v2 execution runs");
    }
    let skill_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM moa.artifact_revision AS revision \
             JOIN moa.artifact AS artifact ON artifact.artifact_uid = revision.artifact_uid \
             WHERE artifact.kind = 'skill' \
               AND revision.definition \
                   #>> '{definition,spec,execution_plan,plan,schema_version}' = '1'",
            &[],
        )
        .await
        .context("verify zero persisted skill execution-template v1 rows")?
        .get(0);
    if skill_count != 0 {
        bail!("execution-plan v2 cutover left {skill_count} v1 skill revisions");
    }
    Ok(())
}

async fn verify_execution_plan_v2_cutover_tx(tx: &tokio_postgres::Transaction<'_>) -> Result<()> {
    let count: i64 = tx
        .query_one(
            "SELECT COUNT(*) \
             FROM moa.execution_run \
             WHERE initial_plan #>> '{definition,schema_version}' <> '2' \
                OR active_plan #>> '{definition,schema_version}' <> '2'",
            &[],
        )
        .await
        .context("verify rewritten execution-plan versions")?
        .get(0);
    if count != 0 {
        bail!("execution-plan v2 transaction left {count} non-v2 runs");
    }
    let trigger_enabled: bool = tx
        .query_one(
            "SELECT trigger.tgenabled <> 'D' \
             FROM pg_catalog.pg_trigger AS trigger \
             WHERE trigger.tgrelid = 'moa.execution_run'::REGCLASS \
               AND trigger.tgname = 'execution_run_update_guard'",
            &[],
        )
        .await
        .context("verify execution-run update trigger restoration")?
        .get(0);
    if !trigger_enabled {
        bail!("execution-run update guard remained disabled after plan v2 cutover");
    }
    Ok(())
}

/// Rewrites retired session status labels inside immutable archive BYTEA payloads.
///
/// SQL cannot derive BLAKE3, so V54 migrates live JSONB rows and this runner-owned
/// post-step handles the archive format. The trigger is disabled only inside the
/// same transaction that locks, verifies, rewrites, re-digests, and re-enables it.
/// A corrupt pre-existing digest fails closed rather than being laundered into a
/// newly valid digest by the migration.
async fn rewrite_archived_session_statuses(client: &mut Client) -> Result<u64> {
    let tx = client
        .transaction()
        .await
        .context("begin archived session status rewrite")?;
    let rows = tx
        .query(
            "SELECT session_id::TEXT AS session_id, payload, content_digest \
             FROM public.session_event_archives \
             ORDER BY session_id FOR UPDATE",
            &[],
        )
        .await
        .context("lock session event archives for status rewrite")?;

    let mut rewrites = Vec::new();
    for row in rows {
        let session_id: String = row.get("session_id");
        let payload: Vec<u8> = row.get("payload");
        let stored_digest: Vec<u8> = row.get("content_digest");
        let actual_digest = blake3::hash(&payload);
        if stored_digest.as_slice() != actual_digest.as_bytes() {
            bail!(
                "session event archive {session_id} failed digest verification before status rewrite"
            );
        }

        let mut body: Value = serde_json::from_slice(&payload)
            .with_context(|| format!("decode session event archive {session_id}"))?;
        if rewrite_archive_body_session_status(&mut body)
            .with_context(|| format!("inspect session event archive {session_id}"))?
        {
            let rewritten = serde_json::to_vec(&body)
                .with_context(|| format!("encode session event archive {session_id}"))?;
            let digest = blake3::hash(&rewritten);
            rewrites.push((session_id, rewritten, digest.as_bytes().to_vec()));
        }
    }

    if rewrites.is_empty() {
        tx.commit()
            .await
            .context("commit empty archived session status rewrite")?;
        return Ok(0);
    }

    tx.batch_execute(
        "ALTER TABLE public.session_event_archives \
             DISABLE TRIGGER session_event_archives_no_update",
    )
    .await
    .context("disable archive immutable trigger for status rewrite")?;

    for (session_id, payload, digest) in &rewrites {
        let updated = tx
            .execute(
                "UPDATE public.session_event_archives \
                 SET payload = $2, content_digest = $3 \
                 WHERE session_id = $1::UUID",
                &[session_id, payload, digest],
            )
            .await
            .with_context(|| format!("rewrite session event archive {session_id}"))?;
        if updated != 1 {
            bail!(
                "session event archive {session_id} status rewrite updated {updated} rows instead of one"
            );
        }
    }

    tx.batch_execute(
        "ALTER TABLE public.session_event_archives \
             ENABLE TRIGGER session_event_archives_no_update",
    )
    .await
    .context("re-enable archive immutable trigger after status rewrite")?;

    let verification_rows = tx
        .query(
            "SELECT session_id::TEXT AS session_id, payload, content_digest \
             FROM public.session_event_archives ORDER BY session_id",
            &[],
        )
        .await
        .context("verify rewritten session event archives")?;
    for row in verification_rows {
        let session_id: String = row.get("session_id");
        let payload: Vec<u8> = row.get("payload");
        let stored_digest: Vec<u8> = row.get("content_digest");
        if stored_digest.as_slice() != blake3::hash(&payload).as_bytes() {
            bail!("session event archive {session_id} digest is invalid after status rewrite");
        }
        let body: Value = serde_json::from_slice(&payload)
            .with_context(|| format!("verify session event archive {session_id}"))?;
        if archive_body_contains_retired_session_status(&body)? {
            bail!("session event archive {session_id} still contains paused session status");
        }
    }

    let rewritten = rewrites.len() as u64;
    tx.commit()
        .await
        .context("commit archived session status rewrite")?;
    Ok(rewritten)
}

/// Rewrites exact `SessionStatusChanged` from/to values in one decoded archive body.
fn rewrite_archive_body_session_status(body: &mut Value) -> Result<bool> {
    let events = body
        .get_mut("events")
        .and_then(Value::as_array_mut)
        .context("archive body has no events array")?;
    let mut changed = false;
    for event in events {
        if event.get("event_type").and_then(Value::as_str) != Some("SessionStatusChanged") {
            continue;
        }
        let Some(data) = event
            .get_mut("payload")
            .and_then(|payload| payload.get_mut("data"))
            .and_then(Value::as_object_mut)
        else {
            bail!("SessionStatusChanged archive event has no payload.data object");
        };
        for field in ["from", "to"] {
            if data.get(field).and_then(Value::as_str) == Some("paused") {
                data.insert(field.to_string(), Value::String("idle".to_string()));
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// Returns whether an archive still contains the retired session status label.
fn archive_body_contains_retired_session_status(body: &Value) -> Result<bool> {
    let events = body
        .get("events")
        .and_then(Value::as_array)
        .context("archive body has no events array")?;
    Ok(events.iter().any(|event| {
        event.get("event_type").and_then(Value::as_str) == Some("SessionStatusChanged")
            && event
                .get("payload")
                .and_then(|payload| payload.get("data"))
                .and_then(Value::as_object)
                .is_some_and(|data| {
                    ["from", "to"]
                        .into_iter()
                        .any(|field| data.get(field).and_then(Value::as_str) == Some("paused"))
                })
    }))
}

fn is_shared_catalog_concurrency_error(error: &refinery::Error) -> bool {
    let refinery::error::Kind::Connection(_, source) = error.kind() else {
        return false;
    };
    source
        .downcast_ref::<tokio_postgres::Error>()
        .and_then(tokio_postgres::Error::as_db_error)
        .is_some_and(|error| error.message() == "tuple concurrently updated")
}

async fn validate_history_before_migration(client: &Client) -> Result<()> {
    let expected = expected_migration_identities();
    validate_expected_migrations(&expected)?;

    let history_exists: bool = client
        .query_one(
            "SELECT pg_catalog.to_regclass('public.refinery_schema_history') IS NOT NULL",
            &[],
        )
        .await
        .context("check central migration history table")?
        .get(0);

    let rows = if history_exists {
        client
            .query(
                "SELECT history.version::TEXT, history.name::TEXT, history.checksum::TEXT \
                 FROM public.refinery_schema_history AS history ORDER BY history.version",
                &[],
            )
            .await
            .context("read central migration history")?
            .into_iter()
            .map(|row| HistoryRow {
                version: row.get(0),
                name: row.get(1),
                checksum: row.get(2),
            })
            .collect()
    } else {
        Vec::new()
    };

    if rows.is_empty() {
        reject_untracked_product_relations(client).await?;
        return Ok(());
    }

    validate_history_rows(&rows, &expected, HistoryRequirement::Prefix)
}

async fn reject_untracked_product_relations(client: &Client) -> Result<()> {
    let has_product_relations: bool = client
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 \
                 FROM pg_catalog.pg_class AS c \
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
                 WHERE n.nspname IN ('public', 'moa', 'analytics', 'pii_vault') \
                   AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
                   AND NOT (n.nspname = 'public' AND c.relname = 'refinery_schema_history') \
                   AND NOT EXISTS ( \
                       SELECT 1 \
                       FROM pg_catalog.pg_depend AS d \
                       WHERE d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass \
                         AND d.objid = c.oid \
                         AND d.deptype = 'e' \
                   ) \
             )",
            &[],
        )
        .await
        .context("inspect database for untracked product relations")?
        .get(0);

    if has_product_relations {
        bail!(
            "product relations exist without contiguous central migration history; {DESTRUCTIVE_RESET_REQUIRED}"
        );
    }
    Ok(())
}

fn expected_migration_identities() -> Vec<MigrationIdentity> {
    central_migration_runner()
        .get_migrations()
        .iter()
        .map(|migration| MigrationIdentity {
            version: migration.version(),
            name: migration.name().to_string(),
            checksum: migration.checksum(),
        })
        .collect()
}

fn central_migration_runner() -> refinery::Runner {
    let mut migrations = embedded::migrations::runner().get_migrations().clone();
    migrations.sort_by_key(refinery::Migration::version);
    refinery::Runner::new(&migrations)
}

fn validate_expected_migrations(expected: &[MigrationIdentity]) -> Result<()> {
    if expected.is_empty() {
        bail!("the embedded central migration set is empty");
    }
    for (index, migration) in expected.iter().enumerate() {
        let expected_version = i32::try_from(index + 1).context("migration count exceeds i32")?;
        if migration.version != expected_version {
            bail!(
                "embedded central migrations must be exactly contiguous from V000001; expected version {expected_version}, found {}",
                migration.version
            );
        }
    }
    Ok(())
}

fn validate_history_rows(
    rows: &[HistoryRow],
    expected: &[MigrationIdentity],
    requirement: HistoryRequirement,
) -> Result<()> {
    validate_expected_migrations(expected)?;
    if rows.len() > expected.len() {
        bail!(
            "central migration history has {} rows but this build embeds only {}; {DESTRUCTIVE_RESET_REQUIRED}",
            rows.len(),
            expected.len()
        );
    }
    if matches!(requirement, HistoryRequirement::Complete) && rows.len() != expected.len() {
        bail!(
            "central migration history is incomplete: found {} of {} expected rows; {DESTRUCTIVE_RESET_REQUIRED}",
            rows.len(),
            expected.len()
        );
    }

    for (index, (row, expected_row)) in rows.iter().zip(expected).enumerate() {
        let position = index + 1;
        let version = row
            .version
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("central migration history row {position} has a null version; {DESTRUCTIVE_RESET_REQUIRED}"))?
            .parse::<i32>()
            .with_context(|| {
                format!(
                    "central migration history row {position} has a malformed version; {DESTRUCTIVE_RESET_REQUIRED}"
                )
            })?;
        let name = row.name.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "central migration history row {position} has a null name; {DESTRUCTIVE_RESET_REQUIRED}"
            )
        })?;
        let checksum = row
            .checksum
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("central migration history row {position} has a null checksum; {DESTRUCTIVE_RESET_REQUIRED}"))?
            .parse::<u64>()
            .with_context(|| {
                format!(
                    "central migration history row {position} has a malformed checksum; {DESTRUCTIVE_RESET_REQUIRED}"
                )
            })?;

        if version != expected_row.version
            || name != expected_row.name
            || checksum != expected_row.checksum
        {
            bail!(
                "central migration history diverges at row {position}: found V{version:06}__{name} checksum {checksum}, expected V{:06}__{} checksum {}; {DESTRUCTIVE_RESET_REQUIRED}",
                expected_row.version,
                expected_row.name,
                expected_row.checksum
            );
        }
    }
    Ok(())
}

/// Returns a stable fingerprint of the complete database template contents.
///
/// The fingerprint is derived directly from refinery's embedded migration
/// metadata, so adding, renaming, reordering, or changing any central migration
/// invalidates the cached template without a second hand-maintained list.
#[must_use]
pub fn full_database_template_fingerprint() -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    fn write_bytes(state: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *state ^= u64::from(*byte);
            *state = state.wrapping_mul(FNV_PRIME);
        }
    }

    let mut fingerprint = FNV_OFFSET_BASIS;
    for migration in expected_migration_identities() {
        write_bytes(&mut fingerprint, migration.version.to_string().as_bytes());
        write_bytes(&mut fingerprint, &[0]);
        write_bytes(&mut fingerprint, migration.name.as_bytes());
        write_bytes(&mut fingerprint, &[0]);
        write_bytes(&mut fingerprint, &migration.checksum.to_le_bytes());
        write_bytes(&mut fingerprint, &[0xff]);
    }
    format!("{fingerprint:016x}")
}

/// Runs the auth DDL fragments inside an isolated schema.
pub async fn run_auth_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    let fragments = auth_schema_fragments()?;
    run_schema_fragments(pool, schema_name, &fragments).await
}

/// Runs the orchestrator DDL fragments inside an isolated schema.
pub async fn run_orchestrator_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_fragments(pool, schema_name, ORCHESTRATOR_SCHEMA_FRAGMENTS).await
}

/// Runs the OCSF DDL fragments inside an isolated schema.
pub async fn run_ocsf_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_fragments(pool, schema_name, OCSF_SCHEMA_FRAGMENTS).await
}

async fn run_schema_fragments(
    pool: &PgPool,
    schema_name: &str,
    fragments: &[SchemaFragment],
) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquire schema fragment connection")?;
    let conn: &mut PgConnection = &mut conn;

    // Each bootstrap targets a unique schema name, so creating the schema and
    // replaying its DDL never conflicts with concurrent bootstraps and needs no
    // global lock. Only `CREATE EXTENSION` touches database-global catalog state
    // shared across schemas, so that step alone is serialized below.
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(schema_name)
    ))
    .execute(&mut *conn)
    .await
    .with_context(|| format!("create schema {schema_name}"))?;

    install_shared_extensions(conn).await?;
    apply_schema_fragments(conn, schema_name, fragments).await
}

/// Installs the database-global extensions shared by every isolated schema.
///
/// Concurrent `CREATE EXTENSION IF NOT EXISTS` for the same extension can error
/// or deadlock on the shared catalog, so a short advisory lock serializes just
/// this step (a fast no-op once the extension already exists) rather than the
/// whole fragment replay.
async fn install_shared_extensions(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *conn)
        .await
        .context("acquire schema extension advisory lock")?;

    let result = install_shared_extensions_locked(conn).await;

    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *conn)
        .await
        .context("release schema extension advisory lock");

    match (result, unlock_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

async fn install_shared_extensions_locked(conn: &mut PgConnection) -> Result<()> {
    raw_sql("CREATE EXTENSION IF NOT EXISTS pgcrypto;")
        .execute(&mut *conn)
        .await
        .context("install pgcrypto extension")?;
    ensure_shared_database_roles(conn).await
}

async fn ensure_shared_database_roles(conn: &mut PgConnection) -> Result<()> {
    // Roles are cluster-global catalog state, exactly like extensions. The
    // per-schema fragment lists reference `moa_app` (RLS policies and grants)
    // but deliberately exclude `V000002__session_baseline.sql`, which creates
    // the roles in a full replay. On a pristine cluster a schema-scoped
    // bootstrap can run first, so the same guarded creation lives here.
    const ROLE_SQL: &str = r#"
        DO $$
        BEGIN
            CREATE ROLE moa_app NOLOGIN NOBYPASSRLS;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        DO $$
        BEGIN
            CREATE ROLE moa_promoter NOLOGIN NOBYPASSRLS;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        DO $$
        BEGIN
            CREATE ROLE moa_owner NOLOGIN;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        ALTER ROLE moa_app NOLOGIN NOBYPASSRLS;
        ALTER ROLE moa_promoter NOLOGIN NOBYPASSRLS;
        "#;

    for attempt in 1..=SHARED_CATALOG_RETRY_LIMIT {
        match raw_sql(ROLE_SQL).execute(&mut *conn).await {
            Ok(_) => return Ok(()),
            Err(error)
                if attempt < SHARED_CATALOG_RETRY_LIMIT
                    && is_sqlx_shared_catalog_concurrency_error(&error) =>
            {
                tracing::warn!(
                    attempt,
                    retry_limit = SHARED_CATALOG_RETRY_LIMIT,
                    "retrying schema bootstrap after concurrent cluster-role catalog update"
                );
                tokio::time::sleep(Duration::from_millis(25 * attempt as u64)).await;
            }
            Err(error) => return Err(error).context("ensure shared database roles"),
        }
    }
    unreachable!("the bounded schema-role retry loop always returns")
}

fn is_sqlx_shared_catalog_concurrency_error(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(error) if error.message() == "tuple concurrently updated")
}

async fn apply_schema_fragments(
    conn: &mut PgConnection,
    schema_name: &str,
    fragments: &[SchemaFragment],
) -> Result<()> {
    let mut tx = conn
        .begin()
        .await
        .context("begin schema fragment transaction")?;
    // Keep destructive unqualified DDL from resolving to public objects before
    // the isolated schema has created its own relation of the same name.
    let search_path = quote_identifier(schema_name);
    for fragment in fragments {
        sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .context("set schema fragment search_path")?;
        sqlx::query("SELECT pg_catalog.set_config('moa.migration_search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .context("set schema fragment search_path GUC")?;
        raw_sql(fragment.sql)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("run schema fragment {} for {schema_name}", fragment.name))?;
    }

    tx.commit().await.context("commit schema fragments")?;
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryRequirement, HistoryRow, MigrationIdentity, OCSF_SCHEMA_FRAGMENTS,
        ORCHESTRATOR_SCHEMA_FRAGMENTS, auth_schema_fragments, central_migration_runner,
        expected_migration_identities, extract_marked_schema_fragment,
        rewrite_archive_body_session_status, validate_history_rows,
    };

    fn row(identity: &MigrationIdentity) -> HistoryRow {
        HistoryRow {
            version: Some(identity.version.to_string()),
            name: Some(identity.name.clone()),
            checksum: Some(identity.checksum.to_string()),
        }
    }

    #[test]
    fn actual_runner_orders_migrations_exactly_contiguous_from_one() {
        // Pins: the actual runner passed to Refinery receives one gap-free,
        // numerically ordered V1..N epoch regardless of macro/filesystem order.
        let migrations = central_migration_runner();
        let versions = migrations
            .get_migrations()
            .iter()
            .map(refinery::Migration::version)
            .collect::<Vec<_>>();
        let expected = (1..=i32::try_from(versions.len()).expect("migration count fits i32"))
            .collect::<Vec<_>>();

        assert!(!versions.is_empty(), "expected central migrations");
        assert_eq!(versions, expected);
    }

    #[test]
    fn schema_fragments_are_retained_embedded_migrations_in_order() {
        // Pins: isolated-schema helpers reuse retained final-shape SQL only.
        let embedded_names = expected_migration_identities()
            .into_iter()
            .map(|migration| migration.name)
            .collect::<Vec<_>>();

        let auth_fragments = auth_schema_fragments()
            .expect("the V50 credential-slot fragment markers must be exact");

        for fragments in [
            auth_fragments.as_slice(),
            ORCHESTRATOR_SCHEMA_FRAGMENTS,
            OCSF_SCHEMA_FRAGMENTS,
        ] {
            let positions = fragments
                .iter()
                .map(|fragment| {
                    embedded_names
                        .iter()
                        .position(|name| name == fragment.name)
                        .unwrap_or_else(|| panic!("missing embedded fragment {}", fragment.name))
                })
                .collect::<Vec<_>>();
            assert!(
                positions.windows(2).all(|window| window[0] < window[1]),
                "schema fragments must preserve embedded order"
            );
        }
    }

    #[test]
    fn marked_schema_fragment_requires_one_ordered_nonempty_pair() {
        // Pins: isolated auth bootstrap cannot silently omit or ambiguously select
        // the V50 credential-slot DDL when marker comments drift.
        assert_eq!(
            extract_marked_schema_fragment("before BEGIN\nSELECT 1;\nEND after", "BEGIN", "END")
                .expect("one ordered marker pair should extract"),
            "SELECT 1;"
        );

        for malformed in [
            "SELECT 1; END",
            "BEGIN SELECT 1;",
            "BEGIN SELECT 1; BEGIN SELECT 2; END",
            "BEGIN SELECT 1; END END",
            "END SELECT 1; BEGIN",
            "BEGIN   END",
        ] {
            extract_marked_schema_fragment(malformed, "BEGIN", "END")
                .expect_err("missing, duplicate, reversed, or empty markers must fail closed");
        }
    }

    #[test]
    fn archive_status_rewrite_targets_only_session_status_fields() {
        // Pins: the raw-BYTEA post-step changes both exact lifecycle coordinates
        // while leaving unrelated `paused` text and other event kinds untouched.
        let mut body = serde_json::json!({
            "events": [
                {
                    "event_type": "SessionStatusChanged",
                    "payload": {
                        "data": {
                            "from": "paused",
                            "to": "paused",
                            "note": "paused"
                        }
                    }
                },
                {
                    "event_type": "Warning",
                    "payload": {"data": {"from": "paused", "to": "paused"}}
                }
            ]
        });

        assert!(
            rewrite_archive_body_session_status(&mut body)
                .expect("well-formed archive should rewrite")
        );
        assert_eq!(body["events"][0]["payload"]["data"]["from"], "idle");
        assert_eq!(body["events"][0]["payload"]["data"]["to"], "idle");
        assert_eq!(body["events"][0]["payload"]["data"]["note"], "paused");
        assert_eq!(body["events"][1]["payload"]["data"]["from"], "paused");
        assert!(
            !rewrite_archive_body_session_status(&mut body)
                .expect("the rewrite should be idempotent")
        );
    }

    #[test]
    fn complete_history_requires_every_exact_identity() {
        // Pins: runtime startup fails closed on partial history.
        let expected = expected_migration_identities();
        let rows = expected[..expected.len() - 1]
            .iter()
            .map(row)
            .collect::<Vec<_>>();

        let error = validate_history_rows(&rows, &expected, HistoryRequirement::Complete)
            .expect_err("partial history must fail");
        assert!(error.to_string().contains("history is incomplete"));
        assert!(error.to_string().contains("destructively rebuilt or reset"));
    }

    #[test]
    fn migration_preflight_accepts_an_exact_prefix() {
        // Pins: a clean interrupted rollout may continue from an exact prefix.
        let expected = expected_migration_identities();
        let rows = expected[..3].iter().map(row).collect::<Vec<_>>();

        validate_history_rows(&rows, &expected, HistoryRequirement::Prefix)
            .expect("exact prefix is resumable");
    }

    #[test]
    fn migration_preflight_rejects_legacy_epoch_identity() {
        // Pins: the old sparse V1 session baseline cannot be mistaken for the epoch marker.
        let expected = expected_migration_identities();
        let rows = vec![HistoryRow {
            version: Some("1".to_string()),
            name: Some("session_baseline".to_string()),
            checksum: Some("0".to_string()),
        }];

        let error = validate_history_rows(&rows, &expected, HistoryRequirement::Prefix)
            .expect_err("legacy history must fail");
        assert!(error.to_string().contains("diverges at row 1"));
    }

    #[test]
    fn migration_preflight_rejects_malformed_and_divergent_rows() {
        // Pins: corrupt history is diagnosed before refinery can panic or execute DDL.
        let expected = expected_migration_identities();
        let cases = [
            HistoryRow {
                version: Some("not-a-version".to_string()),
                name: Some(expected[0].name.clone()),
                checksum: Some(expected[0].checksum.to_string()),
            },
            HistoryRow {
                version: Some(expected[0].version.to_string()),
                name: None,
                checksum: Some(expected[0].checksum.to_string()),
            },
            HistoryRow {
                version: Some(expected[0].version.to_string()),
                name: Some(expected[0].name.clone()),
                checksum: Some("not-a-checksum".to_string()),
            },
            HistoryRow {
                version: Some(expected[0].version.to_string()),
                name: Some(expected[0].name.clone()),
                checksum: Some(expected[0].checksum.wrapping_add(1).to_string()),
            },
        ];

        for corrupt in cases {
            let error = validate_history_rows(&[corrupt], &expected, HistoryRequirement::Prefix)
                .expect_err("corrupt history must fail");
            assert!(error.to_string().contains("destructively rebuilt or reset"));
        }
    }
}
