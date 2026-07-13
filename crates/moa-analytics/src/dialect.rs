//! SQL dialect selection and ClickHouse source/field mappings.
//!
//! The analytics catalog is single-source: every dataset, dimension, and
//! measure is defined once in [`crate::catalog`]. The Postgres backend reads
//! from the `analytics.*_fact` materialized views (one relation per dataset,
//! columns named exactly like the catalog field's `column`). The ClickHouse
//! backend reads from the raw stream, dimension tables, and windowed fact
//! tables described in `docs/schemas/clickhouse-analytics.md`, whose column
//! names and join shapes differ from the Postgres views. This module holds the
//! ClickHouse `FROM`/`JOIN` clause and the per-field SQL expression for each
//! catalog field so the compiler can emit either dialect from one catalog.

/// Backend the compiler targets when emitting SQL for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyticsBackend {
    /// Emit PostgreSQL against the `analytics.*_fact` materialized views.
    Postgres,
    /// Emit ClickHouse against the exporter-maintained raw/dim/fact tables.
    ClickHouse,
}

impl AnalyticsBackend {
    /// Returns the stable name used in diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            AnalyticsBackend::Postgres => "postgres",
            AnalyticsBackend::ClickHouse => "clickhouse",
        }
    }
}

/// Returns the ClickHouse `FROM`/`JOIN` clause for a dataset.
///
/// The driving table is always aliased `d` and carries the `tenant_id` the
/// compiler injects. Dimension and windowed-fact tables are read with `FINAL`
/// to collapse `ReplacingMergeTree` duplicates; the append-only `events_raw`
/// stream is never read with `FINAL`. Table names are unqualified because the
/// executor pins the target database on the client.
///
/// A `None` return means the dataset is Postgres-only and the compiler rejects
/// it on the ClickHouse backend with a `BackendFieldUnavailable` error.
/// Currently only `citation_precision` is Postgres-only: it joins
/// `moa.retrieval_lineage`, which is never exported to ClickHouse.
pub fn clickhouse_from_sql(dataset_id: &str) -> Option<&'static str> {
    let sql = match dataset_id {
        "sessions" => {
            // events_raw is a ReplacingMergeTree read WITHOUT `FINAL` (the
            // exporter's overlap window re-inserts rows on purpose), so the
            // per-session counts must be duplicate-tolerant: uniqExactIf over the
            // event id, never countIf, which would inflate on un-merged duplicates.
            "dim_sessions AS d FINAL \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = d.session_id AND sac.tenant_id = d.tenant_id \
             LEFT JOIN ( \
             SELECT session_id, tenant_id, \
             uniqExactIf(event_id, event_type = 'ToolCall') AS tool_call_count, \
             uniqExactIf(event_id, event_type = 'Error') AS error_count \
             FROM events_raw GROUP BY session_id, tenant_id \
             ) AS ev ON ev.session_id = d.session_id AND ev.tenant_id = d.tenant_id"
        }
        "turns" => {
            "turn_fact AS d FINAL \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = d.session_id AND sac.tenant_id = d.tenant_id \
             LEFT JOIN dim_sessions AS s FINAL \
             ON s.session_id = d.session_id AND s.tenant_id = d.tenant_id"
        }
        "tool_calls" => {
            "tool_call_fact AS d FINAL \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = d.session_id AND sac.tenant_id = d.tenant_id \
             LEFT JOIN dim_sessions AS s FINAL \
             ON s.session_id = d.session_id AND s.tenant_id = d.tenant_id"
        }
        "task_segments" => {
            "dim_task_segments AS d FINAL \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = d.session_id AND sac.tenant_id = d.tenant_id \
             LEFT JOIN dim_sessions AS s FINAL \
             ON s.session_id = d.session_id AND s.tenant_id = d.tenant_id"
        }
        "skills" => {
            "( \
             SELECT ts.tenant_id AS tenant_id, ts.session_id AS session_id, \
             sac.agent_id AS agent_id, s.channel AS channel, \
             ts.started_at AS started_at, ts.outcome AS outcome, \
             ts.token_cost AS token_cost, \
             if(ts.ended_at IS NULL, NULL, \
             (toUnixTimestamp64Micro(ts.ended_at) - toUnixTimestamp64Micro(ts.started_at)) / 1000.0) \
             AS duration_ms, \
             arrayJoin(ts.skills_activated) AS skill_name \
             FROM dim_task_segments AS ts FINAL \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = ts.session_id AND sac.tenant_id = ts.tenant_id \
             LEFT JOIN dim_sessions AS s FINAL \
             ON s.session_id = ts.session_id AND s.tenant_id = ts.tenant_id \
             WHERE length(ts.skills_activated) > 0 \
             ) AS d"
        }
        "procedure_runs" => {
            "dim_artifact_run AS d FINAL \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = d.session_id AND sac.tenant_id = d.tenant_id"
        }
        "procedure_node_runs" => {
            "dim_artifact_node_run AS d FINAL \
             INNER JOIN dim_artifact_run AS ar FINAL \
             ON ar.run_uid = d.run_uid AND ar.tenant_id = d.tenant_id"
        }
        "learning_candidates" => "dim_learning_candidates AS d FINAL",
        "experiment_runs" => "dim_experiment_run AS d FINAL",
        "events" => {
            // events_raw is a ReplacingMergeTree read WITHOUT `FINAL`. Dedup the
            // stream to one row per (session_id, sequence_num) with `LIMIT 1 BY`
            // BEFORE aggregating so counts and token sums are duplicate-tolerant.
            // The dedup runs behind the tenant primary-key filter ($TENANT$),
            // which the compiler substitutes and binds first; the outer query adds
            // no tenant predicate for this dataset. (`$TENANT$` marks a
            // source-injected tenant filter — see the compiler.)
            "( \
             SELECT event_id, session_id, tenant_id, sequence_num, event_type, \
             token_count, ts \
             FROM events_raw \
             WHERE tenant_id = $TENANT$ \
             LIMIT 1 BY (session_id, sequence_num) \
             ) AS d \
             LEFT JOIN dim_session_agent_context AS sac FINAL \
             ON sac.session_id = d.session_id AND sac.tenant_id = d.tenant_id \
             LEFT JOIN dim_sessions AS s FINAL \
             ON s.session_id = d.session_id AND s.tenant_id = d.tenant_id"
        }
        _ => return None,
    };
    Some(sql)
}

/// Returns the ClickHouse SQL expression for a catalog field.
///
/// The expression is the raw value form used in filters, `GROUP BY`, and as the
/// argument to aggregates; the compiler wraps it for output (`toString` for
/// UUIDs, `toUnixTimestamp64Micro` for timestamps). Expressions reference the
/// aliases established by [`clickhouse_from_sql`] (`d`, `sac`, `s`, `ar`, `ev`).
///
/// A `None` return means the catalog exposes a field the ClickHouse sources do
/// not provide and the compiler cannot emit it; every catalog field of every
/// dataset that [`clickhouse_from_sql`] serves is covered here by construction
/// (Postgres-only datasets such as `citation_precision` have no entries).
pub fn clickhouse_field_expr(dataset_id: &str, field_id: &str) -> Option<&'static str> {
    let expr = match (dataset_id, field_id) {
        // sessions -> dim_sessions (d) + dim_session_agent_context (sac) + events_raw agg (ev)
        ("sessions", "tenant_id") => "d.tenant_id",
        ("sessions", "session_id") => "d.session_id",
        ("sessions", "contact_id") => "d.contact_id",
        ("sessions", "agent_id") => "sac.agent_id",
        ("sessions", "agent_revision_uid") => "sac.agent_revision_uid",
        ("sessions", "agent_display_name") => "sac.display_name",
        ("sessions", "channel") => "d.channel",
        ("sessions", "status") => "d.status",
        ("sessions", "created_at") => "d.created_at",
        ("sessions", "updated_at") => "d.updated_at",
        // Requires dim_sessions.completed_at (see module docs / exporter contract).
        ("sessions", "completed_at") => "d.completed_at",
        ("sessions", "turn_count") => "d.turn_count",
        ("sessions", "event_count") => "d.event_count",
        ("sessions", "total_input_tokens") => {
            "(d.total_input_tokens_uncached + d.total_input_tokens_cache_write \
             + d.total_input_tokens_cache_read)"
        }
        ("sessions", "total_cache_read_tokens") => "d.total_input_tokens_cache_read",
        ("sessions", "total_output_tokens") => "d.total_output_tokens",
        ("sessions", "total_cost_cents") => "d.total_cost_cents",
        // Requires dim_sessions.main_cost_cents / auxiliary_cost_cents.
        ("sessions", "main_cost_cents") => "d.main_cost_cents",
        ("sessions", "auxiliary_cost_cents") => "d.auxiliary_cost_cents",
        ("sessions", "cache_hit_rate") => {
            "(d.total_input_tokens_cache_read / nullIf(d.total_input_tokens_uncached \
             + d.total_input_tokens_cache_write + d.total_input_tokens_cache_read, 0))"
        }
        ("sessions", "duration_seconds") => {
            "((toUnixTimestamp64Micro(d.updated_at) - toUnixTimestamp64Micro(d.created_at)) \
             / 1000000.0)"
        }
        ("sessions", "tool_call_count") => "ifNull(ev.tool_call_count, 0)",
        ("sessions", "error_count") => "ifNull(ev.error_count, 0)",

        // turns -> turn_fact (d) + dim_session_agent_context (sac) + dim_sessions (s)
        ("turns", "tenant_id") => "d.tenant_id",
        ("turns", "session_id") => "d.session_id",
        ("turns", "contact_id") => "d.contact_id",
        ("turns", "agent_id") => "sac.agent_id",
        ("turns", "agent_revision_uid") => "sac.agent_revision_uid",
        ("turns", "channel") => "s.channel",
        ("turns", "model") => "d.model",
        ("turns", "turn_number") => "d.turn_number",
        ("turns", "finished_at") => "d.finished_at",
        ("turns", "pipeline_ms") => "d.pipeline_ms",
        ("turns", "llm_ms") => "d.llm_ms",
        // Postgres now populates llm_ttft_ms from the streamed BrainResponse
        // payload, but the ClickHouse exporter does not carry per-turn TTFT, so
        // this column is null on the ClickHouse backend only.
        ("turns", "llm_ttft_ms") => "CAST(NULL AS Nullable(Float64))",
        ("turns", "tool_ms") => "d.tool_ms",
        ("turns", "tool_call_count") => "d.tool_call_count",
        ("turns", "input_tokens_uncached") => "d.input_tokens_uncached",
        ("turns", "input_tokens_cache_write") => "d.input_tokens_cache_write",
        ("turns", "input_tokens_cache_read") => "d.input_tokens_cache_read",
        ("turns", "total_input_tokens") => "d.total_input_tokens",
        ("turns", "output_tokens") => "d.output_tokens",
        ("turns", "cost_cents") => "d.cost_cents",

        // tool_calls -> tool_call_fact (d) + dim_session_agent_context (sac) + dim_sessions (s)
        ("tool_calls", "tenant_id") => "d.tenant_id",
        ("tool_calls", "session_id") => "d.session_id",
        ("tool_calls", "agent_id") => "sac.agent_id",
        ("tool_calls", "channel") => "s.channel",
        // Requires tool_call_fact.tool_id (see module docs / exporter contract).
        ("tool_calls", "tool_id") => "d.tool_id",
        ("tool_calls", "tool_name") => "d.tool_name",
        ("tool_calls", "success") => "d.success",
        ("tool_calls", "model_tier") => "d.model_tier",
        ("tool_calls", "called_at") => "d.ts",
        ("tool_calls", "finished_at") => {
            "if(d.duration_ms IS NULL, NULL, d.ts + toIntervalMillisecond(toInt64(d.duration_ms)))"
        }
        ("tool_calls", "duration_ms") => "d.duration_ms",

        // task_segments -> dim_task_segments (d) + dim_session_agent_context (sac) + dim_sessions (s)
        ("task_segments", "tenant_id") => "d.tenant_id",
        ("task_segments", "segment_id") => "d.segment_id",
        ("task_segments", "session_id") => "d.session_id",
        ("task_segments", "agent_id") => "sac.agent_id",
        ("task_segments", "channel") => "s.channel",
        ("task_segments", "outcome") => "d.outcome",
        ("task_segments", "task_summary") => "d.task_summary",
        ("task_segments", "started_at") => "d.started_at",
        ("task_segments", "ended_at") => "d.ended_at",
        ("task_segments", "outcome_confidence") => "d.outcome_confidence",
        ("task_segments", "turn_count") => "d.turn_count",
        ("task_segments", "token_cost") => "d.token_cost",
        ("task_segments", "duration_ms") => {
            "if(d.ended_at IS NULL, NULL, \
             (toUnixTimestamp64Micro(d.ended_at) - toUnixTimestamp64Micro(d.started_at)) / 1000.0)"
        }

        // skills -> arrayJoin(dim_task_segments.skills_activated) subquery (d)
        ("skills", "tenant_id") => "d.tenant_id",
        ("skills", "skill_name") => "d.skill_name",
        ("skills", "agent_id") => "d.agent_id",
        ("skills", "channel") => "d.channel",
        ("skills", "outcome") => "d.outcome",
        ("skills", "started_at") => "d.started_at",
        ("skills", "token_cost") => "d.token_cost",
        ("skills", "duration_ms") => "d.duration_ms",

        // procedure_runs -> dim_artifact_run (d) + dim_session_agent_context (sac)
        ("procedure_runs", "tenant_id") => "d.tenant_id",
        ("procedure_runs", "run_uid") => "d.run_uid",
        ("procedure_runs", "session_id") => "d.session_id",
        ("procedure_runs", "agent_id") => "sac.agent_id",
        ("procedure_runs", "procedure_ref") => "d.procedure_ref",
        // Requires dim_artifact_run.revision_uid (see module docs / exporter contract).
        ("procedure_runs", "revision_uid") => "d.revision_uid",
        ("procedure_runs", "status") => "d.status",
        ("procedure_runs", "error_present") => "(d.error IS NOT NULL)",
        ("procedure_runs", "started_at") => "d.started_at",
        ("procedure_runs", "completed_at") => "d.completed_at",
        ("procedure_runs", "duration_ms") => {
            "if(d.completed_at IS NULL, NULL, \
             (toUnixTimestamp64Micro(d.completed_at) - toUnixTimestamp64Micro(d.started_at)) / 1000.0)"
        }

        // procedure_node_runs -> dim_artifact_node_run (d) + dim_artifact_run (ar)
        ("procedure_node_runs", "tenant_id") => "d.tenant_id",
        ("procedure_node_runs", "node_run_uid") => "d.node_run_uid",
        ("procedure_node_runs", "run_uid") => "d.run_uid",
        ("procedure_node_runs", "procedure_ref") => "ar.procedure_ref",
        ("procedure_node_runs", "node_id") => "d.node_id",
        ("procedure_node_runs", "status") => "d.status",
        ("procedure_node_runs", "error_present") => "(d.error IS NOT NULL)",
        ("procedure_node_runs", "started_at") => "d.started_at",
        ("procedure_node_runs", "completed_at") => "d.completed_at",
        ("procedure_node_runs", "duration_ms") => {
            "if(d.completed_at IS NULL, NULL, \
             (toUnixTimestamp64Micro(d.completed_at) - toUnixTimestamp64Micro(d.started_at)) / 1000.0)"
        }

        // learning_candidates -> dim_learning_candidates (d)
        ("learning_candidates", "tenant_id") => "d.tenant_id",
        ("learning_candidates", "id") => "d.candidate_id",
        // Postgres nulls contact_id in this view too.
        ("learning_candidates", "contact_id") => "CAST(NULL AS Nullable(UUID))",
        ("learning_candidates", "candidate_type") => "d.candidate_type",
        ("learning_candidates", "status") => "d.status",
        // Requires dim_learning_candidates.target_id (see module docs / exporter contract).
        ("learning_candidates", "target_id") => "d.target_id",
        ("learning_candidates", "target_label") => "d.target_label",
        ("learning_candidates", "risk_class") => "d.risk_class",
        ("learning_candidates", "created_at") => "d.created_at",
        ("learning_candidates", "updated_at") => "d.updated_at",
        ("learning_candidates", "confidence") => "d.confidence",

        // experiment_runs -> dim_experiment_run (d)
        ("experiment_runs", "tenant_id") => "d.tenant_id",
        ("experiment_runs", "run_uid") => "d.run_uid",
        ("experiment_runs", "name") => "d.name",
        ("experiment_runs", "status") => "d.status",
        ("experiment_runs", "score_run_id") => "d.score_run_id",
        ("experiment_runs", "error_present") => "(d.error IS NOT NULL)",
        ("experiment_runs", "created_at") => "d.created_at",
        ("experiment_runs", "updated_at") => "d.updated_at",
        ("experiment_runs", "completed_at") => "d.completed_at",
        ("experiment_runs", "duration_ms") => {
            "if(d.started_at IS NULL OR d.completed_at IS NULL, NULL, \
             (toUnixTimestamp64Micro(d.completed_at) - toUnixTimestamp64Micro(d.started_at)) / 1000.0)"
        }

        // events -> events_raw (d) + dim_session_agent_context (sac) + dim_sessions (s)
        ("events", "tenant_id") => "d.tenant_id",
        ("events", "event_id") => "d.event_id",
        ("events", "session_id") => "d.session_id",
        ("events", "contact_id") => "s.contact_id",
        ("events", "agent_id") => "sac.agent_id",
        ("events", "channel") => "s.channel",
        ("events", "event_type") => "d.event_type",
        ("events", "sequence_num") => "d.sequence_num",
        ("events", "occurred_at") => "d.ts",
        ("events", "token_count") => "d.token_count",

        _ => return None,
    };
    Some(expr)
}
