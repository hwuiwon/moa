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
        dataset(
            "procedure_runs",
            "Procedure Runs",
            "Procedure execution status, latency, and error metrics.",
            "analytics.procedure_run_fact",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension("run_uid", "run_uid", "Run", AnalyticsFieldKind::Uuid),
                dimension(
                    "session_id",
                    "session_id",
                    "Session",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("agent_id", "agent_id", "Agent", AnalyticsFieldKind::Uuid),
                dimension(
                    "procedure_ref",
                    "procedure_ref",
                    "Procedure",
                    AnalyticsFieldKind::String,
                ),
                dimension(
                    "revision_uid",
                    "revision_uid",
                    "Revision",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                dimension(
                    "error_present",
                    "error_present",
                    "Error Present",
                    AnalyticsFieldKind::Boolean,
                ),
                timestamp("started_at", "started_at", "Started At"),
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
            "procedure_node_runs",
            "Procedure Node Runs",
            "Procedure node execution status, latency, and error metrics.",
            "analytics.procedure_node_run_fact",
            Some("started_at"),
            vec![
                tenant_filter(),
                dimension(
                    "node_run_uid",
                    "node_run_uid",
                    "Node Run",
                    AnalyticsFieldKind::Uuid,
                ),
                dimension("run_uid", "run_uid", "Run", AnalyticsFieldKind::Uuid),
                dimension(
                    "procedure_ref",
                    "procedure_ref",
                    "Procedure",
                    AnalyticsFieldKind::String,
                ),
                dimension("node_id", "node_id", "Node", AnalyticsFieldKind::String),
                dimension("status", "status", "Status", AnalyticsFieldKind::String),
                dimension(
                    "error_present",
                    "error_present",
                    "Error Present",
                    AnalyticsFieldKind::Boolean,
                ),
                timestamp("started_at", "started_at", "Started At"),
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
            "learning_candidates",
            "Learning Candidates",
            "Skill, procedure, and memory improvement candidates.",
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
            "procedure_runs",
            "procedure_node_runs",
            "learning_candidates",
            "experiment_runs",
            "events",
        ] {
            assert!(ids.contains(&expected), "missing dataset {expected}");
        }
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
}
