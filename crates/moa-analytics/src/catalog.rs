//! Static catalog for the generic analytics query surface.

use moa_core::wire::analytics::{
    AnalyticsAggregation, AnalyticsCatalogResponse, AnalyticsDataset, AnalyticsField,
    AnalyticsFieldKind, AnalyticsFieldRole, AnalyticsFilterOperator,
};

/// Private SQL-backed dataset metadata used by the compiler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetSpec {
    /// Stable dataset identifier used by API requests.
    pub id: &'static str,
    /// Human-readable dataset label.
    pub label: &'static str,
    /// Dataset description exposed to clients.
    pub description: &'static str,
    /// Allowlisted relation or subquery used in the SQL `FROM` clause.
    pub relation_sql: &'static str,
    /// Default timestamp field used for bounded time-window validation.
    pub default_time_field: Option<&'static str>,
    /// Queryable fields.
    pub fields: Vec<FieldSpec>,
}

/// Private field metadata used by the compiler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    /// Stable field identifier used by requests.
    pub id: &'static str,
    /// Backing column exposed by the dataset relation.
    pub column: &'static str,
    /// Human-readable field label.
    pub label: &'static str,
    /// Human-readable field description.
    pub description: &'static str,
    /// Field data kind.
    pub kind: AnalyticsFieldKind,
    /// Field role.
    pub role: AnalyticsFieldRole,
    /// Aggregations supported for this field.
    pub aggregations: Vec<AnalyticsAggregation>,
    /// Filter operators supported for this field.
    pub filter_operators: Vec<AnalyticsFilterOperator>,
}

/// Builds the generic analytics catalog exposed to API clients.
pub fn analytics_catalog() -> AnalyticsCatalogResponse {
    AnalyticsCatalogResponse {
        datasets: dataset_specs().into_iter().map(dataset_from_spec).collect(),
    }
}

/// Finds a dataset in the public catalog by stable identifier.
pub fn find_dataset<'a>(
    catalog: &'a AnalyticsCatalogResponse,
    dataset_id: &str,
) -> Option<&'a AnalyticsDataset> {
    catalog
        .datasets
        .iter()
        .find(|dataset| dataset.id == dataset_id)
}

/// Finds a field in a public dataset by stable identifier.
pub fn find_field<'a>(dataset: &'a AnalyticsDataset, field_id: &str) -> Option<&'a AnalyticsField> {
    dataset.fields.iter().find(|field| field.id == field_id)
}

/// Finds a private dataset spec by stable identifier.
pub fn find_dataset_spec(dataset_id: &str) -> Option<DatasetSpec> {
    dataset_specs()
        .into_iter()
        .find(|dataset| dataset.id == dataset_id)
}

/// Finds a private field spec by stable identifier.
pub fn find_field_spec<'a>(dataset: &'a DatasetSpec, field_id: &str) -> Option<&'a FieldSpec> {
    dataset.fields.iter().find(|field| field.id == field_id)
}

fn dataset_from_spec(spec: DatasetSpec) -> AnalyticsDataset {
    AnalyticsDataset {
        id: spec.id.to_string(),
        label: spec.label.to_string(),
        description: spec.description.to_string(),
        default_time_field: spec.default_time_field.map(str::to_string),
        fields: spec.fields.into_iter().map(field_from_spec).collect(),
    }
}

fn field_from_spec(spec: FieldSpec) -> AnalyticsField {
    AnalyticsField {
        id: spec.id.to_string(),
        label: spec.label.to_string(),
        description: spec.description.to_string(),
        kind: spec.kind,
        role: spec.role,
        aggregations: spec.aggregations,
        filter_operators: spec.filter_operators,
    }
}

fn dataset_specs() -> Vec<DatasetSpec> {
    vec![
        dataset(
            "sessions",
            "Sessions",
            "Tenant-scoped session lifecycle, cost, cache, and outcome metrics.",
            "analytics.session_fact",
            Some("created_at"),
            vec![
                tenant_filter(),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "contact_id",
                    "contact_id",
                    "Contact",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension(
                    "agent_revision_uid",
                    "agent_revision_uid",
                    "Agent Revision",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "agent_display_name",
                    "agent_display_name",
                    "Agent Name",
                    AnalyticsFieldKind::String,
                ),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                timestamp("created_at", "created_at", "Created At"),
                timestamp("updated_at", "updated_at", "Updated At"),
                timestamp("completed_at", "completed_at", "Completed At"),
                measure(
                    "turn_count",
                    "turn_count",
                    "Turns",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "event_count",
                    "event_count",
                    "Events",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "total_input_tokens",
                    "total_input_tokens",
                    "Input Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "total_cache_read_tokens",
                    "total_cache_read_tokens",
                    "Cache Read Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "total_output_tokens",
                    "total_output_tokens",
                    "Output Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "total_cost_cents",
                    "total_cost_cents",
                    "Cost Cents",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "main_cost_cents",
                    "main_cost_cents",
                    "Main Cost Cents",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "auxiliary_cost_cents",
                    "auxiliary_cost_cents",
                    "Auxiliary Cost Cents",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "cache_hit_rate",
                    "cache_hit_rate",
                    "Cache Hit Rate",
                    AnalyticsFieldKind::Float,
                ),
                measure(
                    "duration_seconds",
                    "duration_seconds",
                    "Duration Seconds",
                    AnalyticsFieldKind::Float,
                ),
                measure(
                    "tool_call_count",
                    "tool_call_count",
                    "Tool Calls",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "error_count",
                    "error_count",
                    "Errors",
                    AnalyticsFieldKind::Integer,
                ),
            ],
        ),
        dataset(
            "turns",
            "Turns",
            "Per-turn latency, token, model, and cost metrics.",
            "analytics.turn_fact",
            Some("finished_at"),
            vec![
                tenant_filter(),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "contact_id",
                    "contact_id",
                    "Contact",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension(
                    "agent_revision_uid",
                    "agent_revision_uid",
                    "Agent Revision",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension("model", "model", "Model", AnalyticsFieldKind::String),
                dimension(
                    "turn_number",
                    "turn_number",
                    "Turn Number",
                    AnalyticsFieldKind::Integer,
                ),
                timestamp("finished_at", "finished_at", "Finished At"),
                measure(
                    "pipeline_ms",
                    "pipeline_ms",
                    "Pipeline Ms",
                    AnalyticsFieldKind::Float,
                ),
                measure("llm_ms", "llm_ms", "LLM Ms", AnalyticsFieldKind::Float),
                measure(
                    "llm_ttft_ms",
                    "llm_ttft_ms",
                    "TTFT Ms",
                    AnalyticsFieldKind::Float,
                ),
                measure("tool_ms", "tool_ms", "Tool Ms", AnalyticsFieldKind::Float),
                measure(
                    "tool_call_count",
                    "tool_call_count",
                    "Tool Calls",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "input_tokens_uncached",
                    "input_tokens_uncached",
                    "Uncached Input Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "input_tokens_cache_write",
                    "input_tokens_cache_write",
                    "Cache Write Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "input_tokens_cache_read",
                    "input_tokens_cache_read",
                    "Cache Read Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "total_input_tokens",
                    "total_input_tokens",
                    "Input Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "output_tokens",
                    "output_tokens",
                    "Output Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "cost_cents",
                    "cost_cents",
                    "Cost Cents",
                    AnalyticsFieldKind::Integer,
                ),
            ],
        ),
        dataset(
            "tool_calls",
            "Tool Calls",
            "Tenant-scoped tool usage, latency, and success metrics.",
            "analytics.tool_call_fact",
            Some("called_at"),
            vec![
                tenant_filter(),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension("tool_id", "tool_id", "Tool ID", AnalyticsFieldKind::Uuid),
                dimension(
                    "tool_name",
                    "tool_name",
                    "Tool Name",
                    AnalyticsFieldKind::String,
                ),
                dimension("success", "success", "Success", AnalyticsFieldKind::Boolean),
                dimension(
                    "model_tier",
                    "model_tier",
                    "Model Tier",
                    AnalyticsFieldKind::String,
                ),
                timestamp("called_at", "called_at", "Called At"),
                timestamp("finished_at", "finished_at", "Finished At"),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "task_segments",
            "Task Segments",
            "End-user task segments, outcomes, skills, tools, and cost.",
            "analytics.task_segment_fact",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension(
                    "segment_id",
                    "segment_id",
                    "Segment",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension("outcome", "outcome", "Outcome", AnalyticsFieldKind::String),
                dimension(
                    "task_summary",
                    "task_summary",
                    "Task Summary",
                    AnalyticsFieldKind::String,
                ),
                timestamp("started_at", "started_at", "Started At"),
                timestamp("ended_at", "ended_at", "Ended At"),
                measure(
                    "outcome_confidence",
                    "outcome_confidence",
                    "Outcome Confidence",
                    AnalyticsFieldKind::Float,
                ),
                measure(
                    "turn_count",
                    "turn_count",
                    "Turns",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "token_cost",
                    "token_cost",
                    "Token Cost",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "skills",
            "Skills",
            "Skill activations extracted from task segments.",
            "(SELECT tenant_id, session_id, agent_id, channel, started_at, outcome, token_cost, duration_ms, unnest(skills_activated) AS skill_name FROM analytics.task_segment_fact WHERE array_length(skills_activated, 1) IS NOT NULL)",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension(
                    "skill_name",
                    "skill_name",
                    "Skill",
                    AnalyticsFieldKind::String,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension("outcome", "outcome", "Outcome", AnalyticsFieldKind::String),
                timestamp("started_at", "started_at", "Started At"),
                measure(
                    "token_cost",
                    "token_cost",
                    "Token Cost",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        // Honest engaged-skill signal: `skills_used` records the skills the
        // model actually engaged during a task segment, in contrast to the
        // `skills` dataset above which reports `skills_activated` (skills merely
        // offered in the turn manifest). Compare the two to measure how often an
        // injected skill was really used.
        //
        // Postgres-only: the ClickHouse exporter carries `skills_activated` on
        // `dim_task_segments` but not `skills_used`, so this dataset has no
        // ClickHouse source (see `clickhouse_from_sql`).
        dataset(
            "skill_usage",
            "Skill Usage",
            "Skills the model actually engaged, extracted from task segments.",
            "(SELECT tenant_id, session_id, agent_id, channel, started_at, outcome, token_cost, duration_ms, unnest(skills_used) AS skill_name FROM analytics.task_segment_fact WHERE array_length(skills_used, 1) IS NOT NULL)",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension(
                    "skill_name",
                    "skill_name",
                    "Skill",
                    AnalyticsFieldKind::String,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension("outcome", "outcome", "Outcome", AnalyticsFieldKind::String),
                timestamp("started_at", "started_at", "Started At"),
                measure(
                    "token_cost",
                    "token_cost",
                    "Token Cost",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "execution_runs",
            "Execution Runs",
            "Normalized durable execution-run routing, plan, usage, and latency facts.",
            "analytics.execution_run_fact",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension("run_uid", "run_uid", "Run", AnalyticsFieldKind::Uuid),
                dimension(
                    "contact_id",
                    "contact_id",
                    "Contact",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "initial_plan_hash",
                    "initial_plan_hash",
                    "Initial Plan Hash",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "active_plan_hash",
                    "active_plan_hash",
                    "Active Plan Hash",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "plan_revision",
                    "plan_revision",
                    "Plan Revision",
                    AnalyticsFieldKind::Integer,
                ),
                dimension(
                    "source_kind",
                    "source_kind",
                    "Source Kind",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "skill_template_ref",
                    "skill_template_ref",
                    "Skill Template",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "skill_template_revision_uid",
                    "skill_template_revision_uid",
                    "Skill Template Revision",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                dimension(
                    "terminal_reason",
                    "terminal_reason",
                    "Terminal Reason",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "logical_task_count",
                    "logical_task_count",
                    "Logical Task Count",
                    AnalyticsFieldKind::Integer,
                ),
                timestamp("queued_at", "queued_at", "Queued At"),
                timestamp("started_at", "started_at", "Started At"),
                timestamp("completed_at", "completed_at", "Completed At"),
                timestamp("created_at", "created_at", "Created At"),
                timestamp("updated_at", "updated_at", "Updated At"),
                measure(
                    "requirement_count",
                    "requirement_count",
                    "Requirement Count",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "satisfied_requirement_count",
                    "satisfied_requirement_count",
                    "Satisfied Requirement Count",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "completion_check_count",
                    "completion_check_count",
                    "Completion Check Count",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_cost_microusd",
                    "reserved_cost_microusd",
                    "Reserved Cost Microusd",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_cost_microusd",
                    "actual_cost_microusd",
                    "Actual Cost Microusd",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_tokens",
                    "reserved_tokens",
                    "Reserved Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_tokens",
                    "actual_tokens",
                    "Actual Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_tasks",
                    "reserved_tasks",
                    "Reserved Tasks",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_tasks",
                    "actual_tasks",
                    "Actual Tasks",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_tool_calls",
                    "reserved_tool_calls",
                    "Reserved Tool Calls",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_tool_calls",
                    "actual_tool_calls",
                    "Actual Tool Calls",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_retrieved_bytes",
                    "reserved_retrieved_bytes",
                    "Reserved Retrieved Bytes",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_retrieved_bytes",
                    "actual_retrieved_bytes",
                    "Actual Retrieved Bytes",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "queue_to_start_ms",
                    "queue_to_start_ms",
                    "Queue To Start Ms",
                    AnalyticsFieldKind::Float,
                ),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "execution_tasks",
            "Execution Tasks",
            "Normalized durable execution-task capability, usage, and latency facts.",
            "analytics.execution_task_fact",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension("task_id", "task_id", "Task", AnalyticsFieldKind::Uuid),
                dimension("run_uid", "run_uid", "Run", AnalyticsFieldKind::Uuid),
                dimension("node_id", "node_id", "Node", AnalyticsFieldKind::String),
                dimension(
                    "item_key",
                    "item_key",
                    "Item Key",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "task_kind",
                    "task_kind",
                    "Task Kind",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "capability_name",
                    "capability_name",
                    "Capability Name",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "capability_version",
                    "capability_version",
                    "Capability Version",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "plan_revision",
                    "plan_revision",
                    "Plan Revision",
                    AnalyticsFieldKind::Integer,
                ),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                dimension("attempt", "attempt", "Attempt", AnalyticsFieldKind::Integer),
                dimension(
                    "generation",
                    "generation",
                    "Generation",
                    AnalyticsFieldKind::Integer,
                ),
                dimension(
                    "failure_class",
                    "failure_class",
                    "Failure Class",
                    AnalyticsFieldKind::String,
                ),
                timestamp("started_at", "started_at", "Started At"),
                timestamp("completed_at", "completed_at", "Completed At"),
                timestamp("created_at", "created_at", "Created At"),
                timestamp("updated_at", "updated_at", "Updated At"),
                measure(
                    "reserved_cost_microusd",
                    "reserved_cost_microusd",
                    "Reserved Cost Microusd",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_cost_microusd",
                    "actual_cost_microusd",
                    "Actual Cost Microusd",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_tokens",
                    "reserved_tokens",
                    "Reserved Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_tokens",
                    "actual_tokens",
                    "Actual Tokens",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_tasks",
                    "reserved_tasks",
                    "Reserved Tasks",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_tasks",
                    "actual_tasks",
                    "Actual Tasks",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_tool_calls",
                    "reserved_tool_calls",
                    "Reserved Tool Calls",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_tool_calls",
                    "actual_tool_calls",
                    "Actual Tool Calls",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "reserved_retrieved_bytes",
                    "reserved_retrieved_bytes",
                    "Reserved Retrieved Bytes",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "actual_retrieved_bytes",
                    "actual_retrieved_bytes",
                    "Actual Retrieved Bytes",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "citation_count",
                    "citation_count",
                    "Citation Count",
                    AnalyticsFieldKind::Integer,
                ),
                measure(
                    "queue_latency_ms",
                    "queue_latency_ms",
                    "Queue Latency Ms",
                    AnalyticsFieldKind::Float,
                ),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "learning_candidates",
            "Learning Candidates",
            "Skill, execution, and memory improvement candidates.",
            "analytics.learning_candidate_fact",
            Some("updated_at"),
            vec![
                tenant_filter(),
                dimension("id", "id", "Candidate", AnalyticsFieldKind::Uuid),
                dimension(
                    "contact_id",
                    "contact_id",
                    "Contact",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "candidate_type",
                    "candidate_type",
                    "Candidate Type",
                    AnalyticsFieldKind::String,
                ),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                dimension(
                    "target_id",
                    "target_id",
                    "Target",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "target_label",
                    "target_label",
                    "Target Label",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "risk_class",
                    "risk_class",
                    "Risk Class",
                    AnalyticsFieldKind::String,
                ),
                timestamp("created_at", "created_at", "Created At"),
                timestamp("updated_at", "updated_at", "Updated At"),
                measure(
                    "confidence",
                    "confidence",
                    "Confidence",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "experiment_runs",
            "Experiment Runs",
            "Tenant-scoped behavior-lab experiment run summaries.",
            "analytics.experiment_run_fact",
            Some("updated_at"),
            vec![
                tenant_filter(),
                dimension("run_uid", "run_uid", "Run", AnalyticsFieldKind::Uuid),
                dimension("name", "name", "Name", AnalyticsFieldKind::String),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                dimension(
                    "score_run_id",
                    "score_run_id",
                    "Score Run",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "error_present",
                    "error_present",
                    "Error Present",
                    AnalyticsFieldKind::Boolean,
                ),
                timestamp("created_at", "created_at", "Created At"),
                timestamp("updated_at", "updated_at", "Updated At"),
                timestamp("completed_at", "completed_at", "Completed At"),
                measure(
                    "duration_ms",
                    "duration_ms",
                    "Duration Ms",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
        dataset(
            "events",
            "Events",
            "Tenant-scoped session event volume and timing metrics.",
            "analytics.event_fact",
            Some("occurred_at"),
            vec![
                tenant_filter(),
                dimension("event_id", "event_id", "Event", AnalyticsFieldKind::Uuid),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension(
                    "contact_id",
                    "contact_id",
                    "Contact",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension("channel", "channel", "Channel", AnalyticsFieldKind::String),
                dimension(
                    "event_type",
                    "event_type",
                    "Event Type",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "sequence_num",
                    "sequence_num",
                    "Sequence",
                    AnalyticsFieldKind::Integer,
                ),
                timestamp("occurred_at", "occurred_at", "Occurred At"),
                measure(
                    "token_count",
                    "token_count",
                    "Token Count",
                    AnalyticsFieldKind::Integer,
                ),
            ],
        ),
        // Live retrieval precision proxy. Each row is one injected retrieval hit
        // (a `moa.retrieval_lineage` row inside the rank <= 3 rendered evidence
        // window) joined against the turn's durable citation lineage
        // (`analytics.turn_lineage`, record_kind 4 = Citation). A hit counts as
        // cited when any citation's `source_chunk_id` matches the hit's knowledge
        // chunk uid or graph node uid, or its `source_node_uid` matches the graph
        // node uid — the exact key mapping `emit_context_lineage` uses when it
        // fans evidence refs into `ChunkRef`s. Aggregate `count()` for
        // injected_hits, `sum(cited_hit)` for cited_hits, and `avg(cited_hit)`
        // for the citation rate; group by `retrieved_day` for per-day series.
        //
        // Postgres-only: `moa.retrieval_lineage` is not exported to ClickHouse,
        // so this dataset has no ClickHouse source and deployments running the
        // ClickHouse analytics backend (which also move `turn_lineage` rows out
        // of Postgres) cannot serve it.
        dataset(
            "citation_precision",
            "Citation Precision",
            "Per-turn precision proxy: injected retrieval hits and whether the final answer cited them.",
            "(SELECT rl.tenant_id AS tenant_id, \
             rl.session_id AS session_id, \
             rl.turn_id AS turn_id, \
             rl.rank AS rank, \
             rl.retrieved_at AS retrieved_at, \
             date_trunc('day', rl.retrieved_at) AS retrieved_day, \
             COALESCE(cit.cited, FALSE) AS cited, \
             COALESCE(cit.cited_verified, FALSE) AS cited_verified, \
             (CASE WHEN COALESCE(cit.cited, FALSE) THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS cited_hit, \
             (CASE WHEN COALESCE(cit.cited_verified, FALSE) THEN 1.0 ELSE 0.0 END)::DOUBLE PRECISION AS cited_verified_hit \
             FROM moa.retrieval_lineage AS rl \
             LEFT JOIN LATERAL ( \
             SELECT bool_or( \
             citation.value ->> 'source_chunk_id' = rl.uid::TEXT \
             OR citation.value ->> 'source_chunk_id' = rl.chunk_uid::TEXT \
             OR citation.value ->> 'source_node_uid' = rl.uid::TEXT \
             ) AS cited, \
             bool_or( \
             (citation.value ->> 'source_chunk_id' = rl.uid::TEXT \
             OR citation.value ->> 'source_chunk_id' = rl.chunk_uid::TEXT \
             OR citation.value ->> 'source_node_uid' = rl.uid::TEXT) \
             AND COALESCE((citation.value -> 'verifier' ->> 'verified')::BOOLEAN, FALSE) \
             ) AS cited_verified \
             FROM analytics.turn_lineage AS tl \
             CROSS JOIN LATERAL jsonb_array_elements( \
             CASE WHEN jsonb_typeof(tl.payload -> 'record' -> 'citations') = 'array' \
             THEN tl.payload -> 'record' -> 'citations' ELSE '[]'::JSONB END \
             ) AS citation(value) \
             WHERE tl.turn_id = rl.turn_id AND tl.record_kind = 4 \
             ) AS cit ON TRUE \
             WHERE rl.turn_id IS NOT NULL AND rl.rank <= 3)",
            Some("retrieved_at"),
            vec![
                tenant_filter(),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("turn_id", "turn_id", "Turn", AnalyticsFieldKind::Uuid),
                dimension("rank", "rank", "Rank", AnalyticsFieldKind::Integer),
                dimension("cited", "cited", "Cited", AnalyticsFieldKind::Boolean),
                dimension(
                    "cited_verified",
                    "cited_verified",
                    "Cited Verified",
                    AnalyticsFieldKind::Boolean,
                ),
                timestamp("retrieved_at", "retrieved_at", "Retrieved At"),
                timestamp("retrieved_day", "retrieved_day", "Retrieved Day"),
                measure(
                    "cited_hit",
                    "cited_hit",
                    "Cited Hit",
                    AnalyticsFieldKind::Float,
                ),
                measure(
                    "cited_verified_hit",
                    "cited_verified_hit",
                    "Cited Verified Hit",
                    AnalyticsFieldKind::Float,
                ),
            ],
        ),
    ]
}

fn dataset(
    id: &'static str,
    label: &'static str,
    description: &'static str,
    relation_sql: &'static str,
    default_time_field: Option<&'static str>,
    fields: Vec<FieldSpec>,
) -> DatasetSpec {
    DatasetSpec {
        id,
        label,
        description,
        relation_sql,
        default_time_field,
        fields,
    }
}

fn tenant_filter() -> FieldSpec {
    field(
        "tenant_id",
        "tenant_id",
        "Tenant",
        AnalyticsFieldKind::Uuid,
        AnalyticsFieldRole::FilterOnly,
        Vec::new(),
        vec![AnalyticsFilterOperator::Eq],
    )
}

fn dimension(
    id: &'static str,
    column: &'static str,
    label: &'static str,
    kind: AnalyticsFieldKind,
) -> FieldSpec {
    field(
        id,
        column,
        label,
        kind,
        AnalyticsFieldRole::Dimension,
        vec![
            AnalyticsAggregation::Count,
            AnalyticsAggregation::CountDistinct,
        ],
        default_filter_operators(kind),
    )
}

fn timestamp(id: &'static str, column: &'static str, label: &'static str) -> FieldSpec {
    field(
        id,
        column,
        label,
        AnalyticsFieldKind::Timestamp,
        AnalyticsFieldRole::Dimension,
        vec![
            AnalyticsAggregation::Count,
            AnalyticsAggregation::Min,
            AnalyticsAggregation::Max,
        ],
        vec![
            AnalyticsFilterOperator::Eq,
            AnalyticsFilterOperator::Lt,
            AnalyticsFilterOperator::Lte,
            AnalyticsFilterOperator::Gt,
            AnalyticsFilterOperator::Gte,
            AnalyticsFilterOperator::Between,
            AnalyticsFilterOperator::IsNull,
            AnalyticsFilterOperator::IsNotNull,
        ],
    )
}

fn measure(
    id: &'static str,
    column: &'static str,
    label: &'static str,
    kind: AnalyticsFieldKind,
) -> FieldSpec {
    field(
        id,
        column,
        label,
        kind,
        AnalyticsFieldRole::Measure,
        vec![
            AnalyticsAggregation::Count,
            AnalyticsAggregation::Sum,
            AnalyticsAggregation::Avg,
            AnalyticsAggregation::Min,
            AnalyticsAggregation::Max,
            AnalyticsAggregation::P50,
            AnalyticsAggregation::P95,
            AnalyticsAggregation::P99,
        ],
        default_filter_operators(kind),
    )
}

fn field(
    id: &'static str,
    column: &'static str,
    label: &'static str,
    kind: AnalyticsFieldKind,
    role: AnalyticsFieldRole,
    aggregations: Vec<AnalyticsAggregation>,
    filter_operators: Vec<AnalyticsFilterOperator>,
) -> FieldSpec {
    FieldSpec {
        id,
        column,
        label,
        description: label,
        kind,
        role,
        aggregations,
        filter_operators,
    }
}

fn default_filter_operators(kind: AnalyticsFieldKind) -> Vec<AnalyticsFilterOperator> {
    match kind {
        AnalyticsFieldKind::String | AnalyticsFieldKind::Uuid => vec![
            AnalyticsFilterOperator::Eq,
            AnalyticsFilterOperator::NotEq,
            AnalyticsFilterOperator::In,
            AnalyticsFilterOperator::NotIn,
            AnalyticsFilterOperator::Contains,
            AnalyticsFilterOperator::IsNull,
            AnalyticsFilterOperator::IsNotNull,
        ],
        AnalyticsFieldKind::Integer | AnalyticsFieldKind::Float => vec![
            AnalyticsFilterOperator::Eq,
            AnalyticsFilterOperator::NotEq,
            AnalyticsFilterOperator::Lt,
            AnalyticsFilterOperator::Lte,
            AnalyticsFilterOperator::Gt,
            AnalyticsFilterOperator::Gte,
            AnalyticsFilterOperator::Between,
            AnalyticsFilterOperator::IsNull,
            AnalyticsFilterOperator::IsNotNull,
        ],
        AnalyticsFieldKind::Boolean => vec![
            AnalyticsFilterOperator::Eq,
            AnalyticsFilterOperator::NotEq,
            AnalyticsFilterOperator::IsNull,
            AnalyticsFilterOperator::IsNotNull,
        ],
        AnalyticsFieldKind::Timestamp => vec![
            AnalyticsFilterOperator::Eq,
            AnalyticsFilterOperator::Lt,
            AnalyticsFilterOperator::Lte,
            AnalyticsFilterOperator::Gt,
            AnalyticsFilterOperator::Gte,
            AnalyticsFilterOperator::Between,
            AnalyticsFilterOperator::IsNull,
            AnalyticsFilterOperator::IsNotNull,
        ],
        AnalyticsFieldKind::Json => vec![
            AnalyticsFilterOperator::Eq,
            AnalyticsFilterOperator::Contains,
            AnalyticsFilterOperator::IsNull,
            AnalyticsFilterOperator::IsNotNull,
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_all_operator_datasets_offline() {
        let catalog = analytics_catalog();
        let ids: Vec<_> = catalog
            .datasets
            .iter()
            .map(|dataset| dataset.id.as_str())
            .collect();

        for expected in [
            "sessions",
            "turns",
            "tool_calls",
            "task_segments",
            "skills",
            "skill_usage",
            "execution_runs",
            "execution_tasks",
            "learning_candidates",
            "experiment_runs",
            "events",
            "citation_precision",
        ] {
            assert!(ids.contains(&expected), "missing dataset {expected}");
        }
        assert_eq!(ids.len(), 12, "catalog dataset membership must stay exact");
    }

    #[test]
    fn public_catalog_does_not_expose_sql_relations_offline() {
        let catalog = analytics_catalog();

        assert!(
            catalog
                .datasets
                .iter()
                .all(|dataset| !dataset.description.contains("analytics.")),
            "public descriptions must not leak backing SQL relation names"
        );
    }

    #[test]
    fn execution_run_catalog_preserves_bounded_dimensions_without_route_prose_offline() {
        // Pins: durable runs expose typed source provenance, terminal evidence,
        // coverage, cost, and latency without route prose or a constant mode.
        let catalog = analytics_catalog();
        let execution_runs = find_dataset(&catalog, "execution_runs")
            .expect("execution_runs must remain in the analytics catalog");
        let field_ids: Vec<_> = execution_runs
            .fields
            .iter()
            .map(|field| field.id.as_str())
            .collect();

        assert_eq!(
            field_ids,
            vec![
                "tenant_id",
                "run_uid",
                "contact_id",
                "session_id",
                "initial_plan_hash",
                "active_plan_hash",
                "plan_revision",
                "source_kind",
                "skill_template_ref",
                "skill_template_revision_uid",
                "status",
                "terminal_reason",
                "logical_task_count",
                "queued_at",
                "started_at",
                "completed_at",
                "created_at",
                "updated_at",
                "requirement_count",
                "satisfied_requirement_count",
                "completion_check_count",
                "reserved_cost_microusd",
                "actual_cost_microusd",
                "reserved_tokens",
                "actual_tokens",
                "reserved_tasks",
                "actual_tasks",
                "reserved_tool_calls",
                "actual_tool_calls",
                "reserved_retrieved_bytes",
                "actual_retrieved_bytes",
                "queue_to_start_ms",
                "duration_ms",
            ]
        );
    }
}
