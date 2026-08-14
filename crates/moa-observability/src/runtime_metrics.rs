//! Shared Prometheus-backed runtime metrics helpers for MOA.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
use opentelemetry_otlp::{MetricExporter, WithExportConfig, WithHttpConfig, WithTonicConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{
    Aggregation, Instrument, MeterProviderBuilder, PeriodicReader, SdkMeterProvider, Stream,
};

use crate::telemetry::build_grpc_metadata;
use moa_config::{MetricsConfig, MetricsExporter, OtlpProtocol};
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionClass,
    types::action_policy::ActionPolicyEffect, types::action_policy::ActionReviewStatus,
    types::identifiers::ModelId, types::observability::genai_operation_name,
    types::observability::genai_provider_name, types::provider::ModelTier,
    types::sandbox_workspace::SandboxWorkspaceState,
    types::sandbox_workspace::WorkspaceCapacityDimension,
};

// Sub-10ms buckets exist because turn steps like snapshot_load and
// pipeline_compile sit in the 1-20ms range at baseline (docs/18-performance.md);
// without them loadtest percentile reports quantize to a useless 10ms floor.
const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];
const CACHE_HIT_RATE_BUCKETS: &[f64] = &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];
const GENAI_CLIENT_DURATION_BUCKETS: &[f64] = &[
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
const GENAI_CLIENT_TOKEN_BUCKETS: &[f64] = &[
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];
const APPROVAL_WAIT_DURATION_SECONDS_BUCKETS: &[f64] = &[
    0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
    14400.0, 28800.0, 86400.0,
];
const COORDINATION_ACK_DURATION_SECONDS_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
    120.0, 300.0,
];
const EXECUTION_TASK_COUNT_BUCKETS: &[f64] = &[
    0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0,
];
const GENAI_CLIENT_TOKEN_USAGE_METRIC: &str = "gen_ai.client.token.usage";
const GENAI_CLIENT_OPERATION_DURATION_METRIC: &str = "gen_ai.client.operation.duration";
const GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC: &str = "gen_ai.client.operation.time_to_first_chunk";
const OTEL_METRIC_EXPORT_INTERVAL_ENV: &str = "OTEL_METRIC_EXPORT_INTERVAL";
const DEFAULT_OTLP_METRIC_EXPORT_INTERVAL: Duration = Duration::from_secs(120);

/// Bounded outcomes for one worker terminal delivery to its owning session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerTerminalDeliveryResult {
    /// The session accepted and persisted the terminal delivery.
    Accepted,
    /// The session had already accepted the same terminal delivery.
    Duplicate,
}

impl WorkerTerminalDeliveryResult {
    /// Returns the stable low-cardinality result label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Duplicate => "duplicate",
        }
    }
}

/// Bounded terminal kinds that can settle a conversational worker fan-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerFanInSettledKind {
    /// The final outstanding child completed successfully.
    Completed,
    /// The final outstanding child settled through cancellation.
    Cancelled,
}

impl WorkerFanInSettledKind {
    /// Returns the stable low-cardinality terminal-kind label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Bounded nonterminal execution phases exported for fleet run counts.
///
/// The variants are exactly the thirteen nonterminal `ExecutionRunStatus`
/// values carried by the durable `execution_run_nonterminal_idx` predicate, in
/// that order. The mapping is total on purpose: a status with no phase would be
/// dropped from the census, and `sum(moa_execution_runs)` would quietly stop
/// equalling the live nonterminal fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRunMetricPhase {
    /// The displayed plan and estimate await owning-user confirmation.
    AwaitingConfirmation,
    /// Accepted work waiting for controller activation.
    Queued,
    /// Work currently advancing or running an attempt.
    Running,
    /// Storage-only wait for user or external input.
    WaitingInput,
    /// Storage-only wait for tenant review.
    WaitingReview,
    /// Storage-only wait for a named signal.
    WaitingSignal,
    /// Storage-only wait for an exact durable timer.
    WaitingTimer,
    /// Storage-only wait for an asynchronous external job.
    WaitingExternal,
    /// Storage-only wait for a compiler-validated plan amendment.
    WaitingReplan,
    /// A pause has been requested but active work is still settling.
    PauseRequested,
    /// The run is checkpointing and releasing resources before pausing.
    Pausing,
    /// The run is fully parked by an operator request.
    Paused,
    /// The run is reversing committed compensatable effects.
    Compensating,
}

impl ExecutionRunMetricPhase {
    /// Returns the stable low-cardinality phase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::WaitingReview => "waiting_review",
            Self::WaitingSignal => "waiting_signal",
            Self::WaitingTimer => "waiting_timer",
            Self::WaitingExternal => "waiting_external",
            Self::WaitingReplan => "waiting_replan",
            Self::PauseRequested => "pause_requested",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Compensating => "compensating",
        }
    }
}

/// Bounded long-horizon resources governed by execution admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionAdmissionResource {
    /// Nonterminal runs that are not fully parked.
    ActiveRuns,
    /// Forward and compensation attempts holding active-compute reservations.
    ActiveTasks,
    /// Runs retained in storage-only waiting or paused states.
    ParkedRuns,
    /// Pending durable trigger rows.
    ScheduledTriggers,
    /// Nonterminal asynchronous provider jobs.
    ExternalJobs,
}

impl ExecutionAdmissionResource {
    /// Returns the stable low-cardinality resource label.
    ///
    /// The labels are exactly the durable
    /// `moa.execution_capacity_bucket.resource_dimension` discriminators, so an
    /// operator reading an alert can query the originating bucket row without a
    /// translation table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveRuns => "active_runs",
            Self::ActiveTasks => "active_tasks",
            Self::ParkedRuns => "parked_runs",
            Self::ScheduledTriggers => "scheduled_triggers",
            Self::ExternalJobs => "external_jobs",
        }
    }
}

/// Bounded aggregation scopes for execution admission utilization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionAdmissionScope {
    /// Utilization of the shared fleet ceiling.
    Fleet,
    /// Highest utilization observed across tenant-scoped ceilings.
    TenantPeak,
}

impl ExecutionAdmissionScope {
    /// Returns the stable low-cardinality aggregation-scope label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fleet => "fleet",
            Self::TenantPeak => "tenant_peak",
        }
    }
}

/// Bounded sandbox-provider classes permitted on workspace metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceProviderKind {
    /// Local development provider.
    Local,
    /// Daytona cloud provider.
    Daytona,
    /// E2B cloud provider.
    E2b,
    /// Any provider outside the currently supported bounded set.
    Other,
}

impl SandboxWorkspaceProviderKind {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Daytona => "daytona",
            Self::E2b => "e2b",
            Self::Other => "other",
        }
    }
}

/// Bounded durable workspace lifecycle operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceLifecycleOperation {
    /// Create durable workspace state.
    Create,
    /// Attach compute to durable state.
    Attach,
    /// Commit the writable state.
    Commit,
    /// Create an immutable checkpoint.
    Checkpoint,
    /// Restore state into fresh compute.
    Restore,
    /// Delete durable workspace state.
    Delete,
    /// Reconcile an ambiguous provider outcome.
    Reconcile,
    /// Purge all state owned by a deleted tenant.
    Purge,
    /// Apply checkpoint retention and garbage collection.
    Retention,
    /// Release compute at a continuation boundary while keeping the filesystem.
    Suspend,
    /// Keep compute hot at a continuation boundary because suspend is unavailable.
    Retain,
}

impl SandboxWorkspaceLifecycleOperation {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Attach => "attach",
            Self::Commit => "commit",
            Self::Checkpoint => "checkpoint",
            Self::Restore => "restore",
            Self::Delete => "delete",
            Self::Reconcile => "reconcile",
            Self::Purge => "purge",
            Self::Retention => "retention",
            Self::Suspend => "suspend",
            Self::Retain => "retain",
        }
    }
}

/// Bounded terminal outcomes for sandbox workspace operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceMetricResult {
    /// The operation completed and its durable result was verified.
    Succeeded,
    /// The operation failed without an ambiguous provider result.
    Failed,
    /// Admission or policy rejected the operation before provider I/O.
    Rejected,
    /// Provider I/O may have happened and requires reconciliation.
    Ambiguous,
}

impl SandboxWorkspaceMetricResult {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// Bounded operations that transfer or remove portable checkpoint bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceCheckpointOperation {
    /// Publish a new portable checkpoint.
    Create,
    /// Restore a portable checkpoint into fresh compute.
    Restore,
    /// Delete checkpoint objects after a retention or purge claim.
    Delete,
}

impl SandboxWorkspaceCheckpointOperation {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Restore => "restore",
            Self::Delete => "delete",
        }
    }
}

/// Bounded durable storage-resource states for fleet metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStorageResourceMetricState {
    /// Creation intent exists but external creation is not verified.
    Creating,
    /// Durable storage exists without an attached writer.
    Ready,
    /// Storage is attached to exactly one fenced writer.
    Attached,
    /// Deletion is fenced and in progress.
    Deleting,
    /// External absence is verified.
    Deleted,
    /// Provider inventory cannot yet prove presence or absence.
    Unknown,
    /// The resource is durably failed and needs operator action.
    Failed,
}

impl SandboxStorageResourceMetricState {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Attached => "attached",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }
}

/// Bounded quota admission decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceQuotaDecision {
    /// Capacity was reserved and work may proceed.
    Admitted,
    /// Capacity was unavailable and the operation was rejected.
    Rejected,
}

impl SandboxWorkspaceQuotaDecision {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Rejected => "rejected",
        }
    }
}

/// Bounded provider-inventory drift classifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxWorkspaceInventoryDrift {
    /// A provider-owned resource has no matching durable MOA row.
    Unknown,
    /// More than one provider resource claims the same durable identity.
    Duplicate,
    /// The provider-account generation differs from the durable binding.
    WrongAccount,
    /// Verified ownership metadata names a different MOA workspace.
    WrongOwner,
    /// A durable resource row has no matching provider resource.
    Missing,
}

impl SandboxWorkspaceInventoryDrift {
    /// Returns the stable low-cardinality label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Duplicate => "duplicate",
            Self::WrongAccount => "wrong_account",
            Self::WrongOwner => "wrong_owner",
            Self::Missing => "missing",
        }
    }
}

/// Exact duration buckets for service latency histograms.
const SERVICE_DURATION_SECONDS_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0,
    1800.0, 3600.0,
];

/// Prometheus metric name for aggregate turn-step duration samples.
pub const TURN_STEP_DURATION_METRIC: &str = "moa_turn_step_duration_seconds";

/// Prometheus metric name for session event append transaction phase timings.
pub const SESSION_EVENT_APPEND_PHASE_METRIC: &str = "moa_session_event_append_phase_seconds";

/// Turn steps reported by the loadtest step-latency view.
pub const TURN_LATENCY_REPORT_STEPS: [TurnLatencyStep; 6] = [
    TurnLatencyStep::SnapshotLoad,
    TurnLatencyStep::SnapshotWrite,
    TurnLatencyStep::PipelineCompile,
    TurnLatencyStep::LlmCall,
    TurnLatencyStep::ToolDispatch,
    TurnLatencyStep::EventPersist,
];

/// Low-cardinality turn-latency step labels shared by metrics producers and loadtest consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLatencyStep {
    /// Time spent loading a cached turn snapshot.
    SnapshotLoad,
    /// Time spent writing a refreshed turn snapshot.
    SnapshotWrite,
    /// Time spent compiling the context pipeline.
    PipelineCompile,
    /// Time spent in the main LLM call.
    LlmCall,
    /// Time spent dispatching tools.
    ToolDispatch,
    /// Time spent persisting turn events.
    EventPersist,
    /// Time to first streamed LLM token.
    LlmTtft,
}

impl TurnLatencyStep {
    /// All turn-latency steps in a stable order, used to pre-build cached metric handles.
    const ALL: [TurnLatencyStep; 7] = [
        Self::SnapshotLoad,
        Self::SnapshotWrite,
        Self::PipelineCompile,
        Self::LlmCall,
        Self::ToolDispatch,
        Self::EventPersist,
        Self::LlmTtft,
    ];

    /// Returns the stable Prometheus label for this turn step.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotLoad => "snapshot_load",
            Self::SnapshotWrite => "snapshot_write",
            Self::PipelineCompile => "pipeline_compile",
            Self::LlmCall => "llm_call",
            Self::ToolDispatch => "tool_dispatch",
            Self::EventPersist => "event_persist",
            Self::LlmTtft => "llm_ttft",
        }
    }

    /// Returns the dense index of this step into [`TurnLatencyStep::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::SnapshotLoad => 0,
            Self::SnapshotWrite => 1,
            Self::PipelineCompile => 2,
            Self::LlmCall => 3,
            Self::ToolDispatch => 4,
            Self::EventPersist => 5,
            Self::LlmTtft => 6,
        }
    }
}

/// Low-cardinality phases inside one session event append operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventAppendPhase {
    /// Time from SessionStore handler entry through the completed handler action.
    HandlerTotal,
    /// Time spent inside the SessionStore named `ctx.run` action.
    HandlerAction,
    /// Time spent inside a TurnExecution named `ctx.run` direct append action.
    DirectAction,
    /// Pre-transaction payload encoding and claim-check preparation.
    Prepare,
    /// Wait for a pooled PostgreSQL connection.
    AcquireConnection,
    /// Start a transaction on an acquired connection.
    BeginTransaction,
    /// `sessions ... FOR UPDATE` lock acquisition and session metadata load.
    LockSession,
    /// Lookup of previously persisted idempotency keys.
    DedupeLookup,
    /// Fetch of original event rows for dedupe hits.
    DedupeFetchRecords,
    /// Local construction of multi-row insert arrays.
    BuildInsertPayloads,
    /// Multi-row insert into the append-only event table.
    InsertEvents,
    /// Multi-row insert into the dedupe table.
    InsertDedupeRows,
    /// Session aggregate counter update.
    UpdateSessionAggregates,
    /// Transaction commit, including Postgres commit wait.
    Commit,
}

impl SessionEventAppendPhase {
    /// All session event append phases in a stable order for cached metric handles.
    const ALL: [SessionEventAppendPhase; 14] = [
        Self::HandlerTotal,
        Self::HandlerAction,
        Self::DirectAction,
        Self::Prepare,
        Self::AcquireConnection,
        Self::BeginTransaction,
        Self::LockSession,
        Self::DedupeLookup,
        Self::DedupeFetchRecords,
        Self::BuildInsertPayloads,
        Self::InsertEvents,
        Self::InsertDedupeRows,
        Self::UpdateSessionAggregates,
        Self::Commit,
    ];

    /// Returns the stable Prometheus label for this append phase.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HandlerTotal => "handler_total",
            Self::HandlerAction => "handler_action",
            Self::DirectAction => "direct_action",
            Self::Prepare => "prepare",
            Self::AcquireConnection => "acquire_connection",
            Self::BeginTransaction => "begin_transaction",
            Self::LockSession => "lock_session",
            Self::DedupeLookup => "dedupe_lookup",
            Self::DedupeFetchRecords => "dedupe_fetch_records",
            Self::BuildInsertPayloads => "build_insert_payloads",
            Self::InsertEvents => "insert_events",
            Self::InsertDedupeRows => "insert_dedupe_rows",
            Self::UpdateSessionAggregates => "update_session_aggregates",
            Self::Commit => "commit",
        }
    }

    /// Returns the dense index of this phase into [`SessionEventAppendPhase::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::HandlerTotal => 0,
            Self::HandlerAction => 1,
            Self::DirectAction => 2,
            Self::Prepare => 3,
            Self::AcquireConnection => 4,
            Self::BeginTransaction => 5,
            Self::LockSession => 6,
            Self::DedupeLookup => 7,
            Self::DedupeFetchRecords => 8,
            Self::BuildInsertPayloads => 9,
            Self::InsertEvents => 10,
            Self::InsertDedupeRows => 11,
            Self::UpdateSessionAggregates => 12,
            Self::Commit => 13,
        }
    }
}

static PROMETHEUS_ENDPOINT: OnceLock<SocketAddr> = OnceLock::new();

/// Installs the configured metrics exporter, once per process.
///
/// Returns the OTLP meter provider when that exporter is selected, so the caller
/// owns it and can flush it at shutdown. The provider is deliberately NOT stored
/// in a global: a global could not be shut down in a defined order relative to
/// the tracer, and flushing metrics is the step this process used to skip.
pub fn init_metrics(
    config: &MetricsConfig,
    otlp_endpoint: Option<&str>,
    otlp_protocol: OtlpProtocol,
    otlp_headers: &std::collections::HashMap<String, String>,
    resource: Resource,
) -> Result<Option<SdkMeterProvider>> {
    match config.exporter {
        MetricsExporter::Disabled => Ok(None),
        MetricsExporter::Otlp => {
            install_otlp_metrics(otlp_endpoint, otlp_protocol, otlp_headers, resource).map(Some)
        }
        MetricsExporter::Prometheus => {
            install_prometheus_metrics(config)?;
            Ok(None)
        }
    }
}

/// Installs the OTLP push exporter and bridges the `metrics` facade onto it.
///
/// The bridge preserves the exact histogram boundaries the Prometheus exporter
/// used. Those buckets are not decoration: sub-10ms boundaries exist because
/// turn steps sit in the 1-20ms range, and exporting the same histograms with
/// default OTel boundaries would quantize every latency panel and SLO built on
/// them to a useless floor while still reporting a number.
fn install_otlp_metrics(
    endpoint: Option<&str>,
    protocol: OtlpProtocol,
    headers: &std::collections::HashMap<String, String>,
    resource: Resource,
) -> Result<SdkMeterProvider> {
    let export_interval = configured_otlp_metric_export_interval()?;
    // The transport is the one the operator configured, not a hardcoded default.
    // Reading `otlp_protocol` for traces and ignoring it here meant a fleet on
    // gRPC posted HTTP/1.1 at a gRPC port and exported no metric at all, with
    // nothing in-process saying so.
    let resolved = endpoint
        .map(|base| {
            moa_config::otlp_signal_endpoint(base, protocol, moa_config::OtlpSignal::Metrics)
        })
        .transpose()?;
    let exporter = match protocol {
        OtlpProtocol::Grpc => {
            let mut exporter = MetricExporter::builder().with_tonic();
            if let Some(resolved) = resolved.as_deref() {
                exporter = exporter.with_endpoint(resolved);
            }
            if !headers.is_empty() {
                exporter = exporter.with_metadata(build_grpc_metadata(headers)?);
            }
            exporter.build()
        }
        OtlpProtocol::Http => {
            let mut exporter = MetricExporter::builder().with_http();
            if let Some(resolved) = resolved.as_deref() {
                exporter = exporter.with_endpoint(resolved);
            }
            if !headers.is_empty() {
                exporter = exporter.with_headers(headers.clone());
            }
            exporter.build()
        }
    };
    let exporter = exporter.map_err(|error| MoaError::ProviderError(error.to_string()))?;
    let reader = PeriodicReader::builder(exporter)
        .with_interval(export_interval)
        .build();
    let builder = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource);
    let provider = apply_otlp_metric_views(builder).build();
    if OTEL_METRICS_INSTALLED.set(()).is_ok() {
        metrics::set_global_recorder(metrics_exporter_otel::OpenTelemetryRecorder::new(
            opentelemetry::metrics::MeterProvider::meter(&provider, "moa"),
        ))
        .map_err(|error| {
            MoaError::ProviderError(format!("metrics recorder already installed: {error}"))
        })?;
        register_metric_descriptions();
    }
    Ok(provider)
}

fn configured_otlp_metric_export_interval() -> Result<Duration> {
    match std::env::var(OTEL_METRIC_EXPORT_INTERVAL_ENV) {
        Ok(value) => parse_otlp_metric_export_interval(Some(&value)),
        Err(std::env::VarError::NotPresent) => parse_otlp_metric_export_interval(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(MoaError::ConfigError(format!(
            "{OTEL_METRIC_EXPORT_INTERVAL_ENV} must be a positive integer number of milliseconds"
        ))),
    }
}

fn parse_otlp_metric_export_interval(value: Option<&str>) -> Result<Duration> {
    let Some(value) = value else {
        return Ok(DEFAULT_OTLP_METRIC_EXPORT_INTERVAL);
    };
    let millis = value.trim().parse::<u64>().map_err(|_| {
        MoaError::ConfigError(format!(
            "{OTEL_METRIC_EXPORT_INTERVAL_ENV} must be a positive integer number of \
             milliseconds, got `{value}`"
        ))
    })?;
    if millis == 0 {
        return Err(MoaError::ConfigError(format!(
            "{OTEL_METRIC_EXPORT_INTERVAL_ENV} must be greater than zero milliseconds"
        )));
    }
    Ok(Duration::from_millis(millis))
}

/// Applies MOA's OTLP-only metric views to a meter-provider builder.
///
/// Append-phase latency is a local load-test diagnostic consumed through the
/// Prometheus exporter. A single drop view keeps it out of OTLP; its explicit
/// bucket view is deliberately skipped because SDK views are additive.
fn apply_otlp_metric_views(mut builder: MeterProviderBuilder) -> MeterProviderBuilder {
    for (metric, boundaries) in HISTOGRAM_BOUNDARIES {
        if *metric != SESSION_EVENT_APPEND_PHASE_METRIC {
            builder = builder.with_view(explicit_bucket_view(metric, boundaries));
        }
    }
    builder.with_view(drop_metric_view(SESSION_EVENT_APPEND_PHASE_METRIC))
}

/// Builds a view that pins one metric's histogram boundaries.
///
/// Without these views the OTLP exporter would use the SDK's default bucket
/// layout, and every latency percentile MOA reports would quantize to that
/// layout while still producing a number. The sub-10ms boundaries in particular
/// exist because turn steps sit in the 1-20ms range; losing them turns a working
/// latency panel into a flat line at the default floor.
fn explicit_bucket_view(
    metric: &'static str,
    boundaries: &'static [f64],
) -> impl Fn(&Instrument) -> Option<Stream> + Send + Sync + 'static {
    move |instrument: &Instrument| {
        if instrument.name() != metric {
            return None;
        }
        Stream::builder()
            .with_aggregation(Aggregation::ExplicitBucketHistogram {
                boundaries: boundaries.to_vec(),
                record_min_max: true,
            })
            .build()
            .ok()
    }
}

/// Builds a view that prevents one metric from being aggregated or exported.
fn drop_metric_view(
    metric: &'static str,
) -> impl Fn(&Instrument) -> Option<Stream> + Send + Sync + 'static {
    move |instrument: &Instrument| {
        if instrument.name() != metric {
            return None;
        }
        Stream::builder()
            .with_aggregation(Aggregation::Drop)
            .build()
            .ok()
    }
}

/// Explicit bucket layouts for retained histograms exported by MOA services.
///
/// Histogram emission remains distributed across the owning crates. Both MOA
/// exporters consume this inventory, except that OTLP deliberately drops the
/// Prometheus-only append-phase diagnostic. A newly retained histogram must be
/// added here instead of relying on either exporter's defaults.
const HISTOGRAM_BOUNDARIES: &[(&str, &[f64])] = &[
    ("moa_cache_hit_rate", CACHE_HIT_RATE_BUCKETS),
    (
        GENAI_CLIENT_OPERATION_DURATION_METRIC,
        GENAI_CLIENT_DURATION_BUCKETS,
    ),
    (
        GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC,
        GENAI_CLIENT_DURATION_BUCKETS,
    ),
    (GENAI_CLIENT_TOKEN_USAGE_METRIC, GENAI_CLIENT_TOKEN_BUCKETS),
    ("moa_turn_latency_seconds", SERVICE_DURATION_SECONDS_BUCKETS),
    (
        "moa_approval_wait_seconds",
        APPROVAL_WAIT_DURATION_SECONDS_BUCKETS,
    ),
    (
        "moa_worker_terminal_parent_ack_seconds",
        COORDINATION_ACK_DURATION_SECONDS_BUCKETS,
    ),
    (
        "moa_execution_dispatch_batch_size",
        EXECUTION_TASK_COUNT_BUCKETS,
    ),
    (
        "moa_sandbox_provision_seconds",
        SERVICE_DURATION_SECONDS_BUCKETS,
    ),
    (
        "moa_sandbox_workspace_lifecycle_duration_seconds",
        SERVICE_DURATION_SECONDS_BUCKETS,
    ),
    (
        "moa_sandbox_workspace_checkpoint_duration_seconds",
        SERVICE_DURATION_SECONDS_BUCKETS,
    ),
    (
        "moa_tool_call_duration_seconds",
        SERVICE_DURATION_SECONDS_BUCKETS,
    ),
    (TURN_STEP_DURATION_METRIC, LATENCY_BUCKETS),
    (SESSION_EVENT_APPEND_PHASE_METRIC, LATENCY_BUCKETS),
    (
        "moa_knowledge_ingestion_step_duration_seconds",
        SERVICE_DURATION_SECONDS_BUCKETS,
    ),
    ("moa_lineage_durable_append_seconds", LATENCY_BUCKETS),
    ("moa_retrieval_cache_hit_seconds", LATENCY_BUCKETS),
    ("moa_retrieval_leg_seconds", LATENCY_BUCKETS),
    ("moa_retrieval_rrf_rerank_seconds", LATENCY_BUCKETS),
];

/// Guards the one-time global recorder installation.
static OTEL_METRICS_INSTALLED: OnceLock<()> = OnceLock::new();

/// Installs the development-only Prometheus scrape exporter.
fn install_prometheus_metrics(config: &MetricsConfig) -> Result<()> {
    if PROMETHEUS_ENDPOINT.get().is_some() {
        return Ok(());
    }

    let addr = parse_metrics_listen_addr(config)?;
    let mut builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets(LATENCY_BUCKETS)
        .map_err(|error| MoaError::ConfigError(error.to_string()))?;
    // Prometheus consumes the complete retained inventory. OTLP consumes the
    // same boundaries except for the append-phase family it explicitly drops.
    for (metric, boundaries) in HISTOGRAM_BOUNDARIES {
        builder = builder
            .set_buckets_for_metric(Matcher::Full((*metric).to_string()), boundaries)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
    }

    builder
        .install()
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    register_metric_descriptions();
    let _ = PROMETHEUS_ENDPOINT.set(addr);
    Ok(())
}

/// Returns the scrape URL when the development Prometheus exporter is selected.
///
/// `None` under the OTLP and disabled exporters, because there is no endpoint to
/// name. Reporting a URL for a port nothing binds is what let manifests and
/// network policies grow scrape targets that never existed.
#[must_use]
pub fn metrics_endpoint_url(config: &MetricsConfig) -> Option<String> {
    if config.exporter != MetricsExporter::Prometheus {
        return None;
    }
    parse_metrics_listen_addr(config)
        .ok()
        .map(format_metrics_endpoint_url)
}

/// Sets the current number of active sessions.
pub fn record_sessions_active(count: u64) {
    gauge!("moa_sessions_active").set(count as f64);
}

/// Records one completed assistant turn.
pub fn record_turn_completed(model: &ModelId, model_tier: ModelTier) {
    counter!(
        "moa_turns_total",
        "model" => model.to_string(),
        "model_tier" => model_tier.as_str()
    )
    .increment(1);
}

/// Records GenAI client operation duration.
pub fn record_genai_client_operation_duration(
    provider: &str,
    request_model: &str,
    response_model: Option<&str>,
    error_type: Option<&str>,
    duration: Duration,
) {
    let provider = genai_provider_name(provider).to_string();
    let operation = genai_operation_name(&provider).to_string();
    match (response_model, error_type) {
        (Some(response_model), Some(error_type)) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string(),
                "gen_ai.response.model" => response_model.to_string(),
                "error.type" => error_type.to_string()
            )
            .record(duration.as_secs_f64());
        }
        (Some(response_model), None) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string(),
                "gen_ai.response.model" => response_model.to_string()
            )
            .record(duration.as_secs_f64());
        }
        (None, Some(error_type)) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string(),
                "error.type" => error_type.to_string()
            )
            .record(duration.as_secs_f64());
        }
        (None, None) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string()
            )
            .record(duration.as_secs_f64());
        }
    }
}

/// Records GenAI client token usage when provider-reported counts are available.
pub fn record_genai_client_token_usage(
    provider: &str,
    request_model: &str,
    response_model: &str,
    token_type: &str,
    tokens: u64,
) {
    if tokens == 0 {
        return;
    }

    histogram!(
        GENAI_CLIENT_TOKEN_USAGE_METRIC,
        "gen_ai.operation.name" => genai_operation_name(provider).to_string(),
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => request_model.to_string(),
        "gen_ai.response.model" => response_model.to_string(),
        "gen_ai.token.type" => token_type.to_string()
    )
    .record(tokens as f64);
}

/// Records time to first streamed GenAI response chunk.
pub fn record_genai_client_time_to_first_chunk(
    provider: &str,
    request_model: &str,
    response_model: &str,
    duration: Duration,
) {
    histogram!(
        GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC,
        "gen_ai.operation.name" => genai_operation_name(provider).to_string(),
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => request_model.to_string(),
        "gen_ai.response.model" => response_model.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records the ratio of input tokens that were served from cache for one request.
pub fn record_cache_hit_rate(provider: &str, model: &str, ratio: f64) {
    histogram!(
        "moa_cache_hit_rate",
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => model.to_string()
    )
    .record(ratio.clamp(0.0, 1.0));
}

/// Records one LLM completion cost sample in cents.
pub fn record_llm_cost_cents(provider: &str, model: &str, cost_cents: u64) {
    if cost_cents == 0 {
        return;
    }

    counter!(
        "moa_llm_cost_cents_total",
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => model.to_string()
    )
    .increment(cost_cents);
}

/// Records one session-level error that should appear on operational dashboards.
pub fn record_session_error(scope: &str) {
    counter!(
        "moa_session_errors_total",
        "scope" => scope.to_string()
    )
    .increment(1);
}

/// Records one tool call completion and its latency.
pub fn record_tool_call(tool_name: &str, status: &str, duration: Duration) {
    let tool_name = tool_name_label(tool_name);
    counter!(
        "moa_tool_calls_total",
        "tool_name" => tool_name,
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "moa_tool_call_duration_seconds",
        "tool_name" => tool_name
    )
    .record(duration.as_secs_f64());
}

/// Records one classified tool execution failure.
pub fn record_tool_failure(provider: &str, tool_name: &str, class: &str) {
    counter!(
        "moa_tool_failure_total",
        "class" => class.to_string(),
        "provider" => provider.to_string(),
        "tool" => tool_name_label(tool_name)
    )
    .increment(1);
}

/// Records one automatic sandbox re-provision.
pub fn record_tool_reprovision(provider: &str) {
    counter!(
        "moa_tool_reprovision_total",
        "provider" => provider.to_string()
    )
    .increment(1);
}

/// Records one end-to-end turn latency sample.
pub fn record_turn_latency(duration: Duration) {
    histogram!("moa_turn_latency_seconds").record(duration.as_secs_f64());
}

/// Records worker-terminal-to-parent acknowledgement latency.
///
/// The duration begins when the Worker starts its joined terminal handoff and
/// ends when the owning Session acknowledges the persisted delivery.
pub fn record_worker_terminal_parent_ack(duration: Duration) {
    histogram!("moa_worker_terminal_parent_ack_seconds").record(duration.as_secs_f64());
}

/// Records how many ready execution tasks the dispatcher admitted in one
/// bounded refill.
pub fn record_execution_dispatch_batch_size(size: usize) {
    histogram!("moa_execution_dispatch_batch_size").record(size as f64);
}

/// Sets the current fleet count for one bounded nonterminal execution phase.
///
/// Callers must record every phase on each fleet snapshot including the healthy
/// zero, so a phase that drains reports zero rather than keeping its last value
/// forever.
pub fn record_execution_run_phase(phase: ExecutionRunMetricPhase, count: u64) {
    gauge!("moa_execution_runs", "phase" => phase.as_str()).set(count as f64);
}

/// Sets the age of the oldest task currently ready for dispatch.
pub fn record_execution_oldest_ready_age(age: Duration) {
    gauge!("moa_execution_oldest_ready_age_seconds").set(age.as_secs_f64());
}

/// Sets the number of nonterminal runs whose absolute deadline has elapsed.
///
/// This is the exact-deadline invariant guard, so callers must record it on
/// every fleet snapshot including the healthy zero.
pub fn record_execution_overdue_deadlines(count: u64) {
    gauge!("moa_execution_overdue_deadlines").set(count as f64);
}

/// Sets trigger delivery lag, capped depth, and sample-completeness from one fleet snapshot.
///
/// Triggers carry no dead-letter state: claiming and retry exhaustion live entirely on
/// the dispatch outbox, so only the due sample is observed here.
pub fn record_execution_trigger_queue(
    lag: Duration,
    due_triggers: u64,
    due_sample_saturated: bool,
) {
    gauge!("moa_execution_trigger_lag_seconds").set(lag.as_secs_f64());
    gauge!("moa_execution_trigger_due").set(due_triggers as f64);
    gauge!(
        "moa_execution_queue_sample_saturated",
        "queue" => "trigger",
        "sample" => "due"
    )
    .set(if due_sample_saturated { 1.0 } else { 0.0 });
}

/// Sets outbox delivery lag, capped depth, and sample-completeness from one fleet snapshot.
pub fn record_execution_outbox_queue(
    lag: Duration,
    claimable_dispatches: u64,
    claimable_sample_saturated: bool,
    dead_letters: u64,
    dead_letter_sample_saturated: bool,
) {
    gauge!("moa_execution_outbox_lag_seconds").set(lag.as_secs_f64());
    gauge!("moa_execution_outbox_claimable").set(claimable_dispatches as f64);
    gauge!("moa_execution_outbox_dead_letters").set(dead_letters as f64);
    gauge!(
        "moa_execution_queue_sample_saturated",
        "queue" => "outbox",
        "sample" => "claimable"
    )
    .set(if claimable_sample_saturated { 1.0 } else { 0.0 });
    gauge!(
        "moa_execution_queue_sample_saturated",
        "queue" => "outbox",
        "sample" => "dead_letter"
    )
    .set(if dead_letter_sample_saturated {
        1.0
    } else {
        0.0
    });
}

/// Sets the age of the oldest active task-attempt lease.
///
/// Callers must record this on every fleet snapshot, using `Duration::ZERO`
/// when no attempt is active. A gauge only written while work exists keeps its
/// last value forever once the fleet drains, so its alert would page on a queue
/// that emptied hours earlier.
pub fn record_execution_active_attempt_oldest_age(age: Duration) {
    gauge!("moa_execution_active_attempt_oldest_age_seconds").set(age.as_secs_f64());
}

/// Sets the age of the oldest nonterminal asynchronous external job.
///
/// Callers must record this on every fleet snapshot, using `Duration::ZERO`
/// when no external job is outstanding, so the alert can tell a quiet fleet
/// from a stopped producer.
pub fn record_execution_external_job_oldest_age(age: Duration) {
    gauge!("moa_execution_external_job_oldest_age_seconds").set(age.as_secs_f64());
}

/// Sets admission utilization for one bounded resource and aggregation scope.
pub fn record_execution_admission_utilization(
    resource: ExecutionAdmissionResource,
    scope: ExecutionAdmissionScope,
    ratio: f64,
) {
    gauge!(
        "moa_execution_admission_utilization_ratio",
        "resource" => resource.as_str(),
        "scope" => scope.as_str()
    )
    .set(ratio.clamp(0.0, 1.0));
}

/// Sets the largest tenant share of one fleet execution resource.
///
/// This exposes fairness pressure without tenant identifiers or one series per
/// tenant. Callers calculate the maximum share from a complete fleet snapshot.
pub fn record_execution_tenant_max_share(resource: ExecutionAdmissionResource, ratio: f64) {
    gauge!(
        "moa_execution_tenant_max_share_ratio",
        "resource" => resource.as_str()
    )
    .set(ratio.clamp(0.0, 1.0));
}

/// Sets maintenance health from the durable last-success reconciliation receipt.
///
/// A missing receipt is exported as positive infinity so it is unambiguously
/// older than every finite staleness threshold.
pub fn record_execution_maintenance(ready: bool, last_success_age: Option<Duration>) {
    gauge!("moa_execution_maintenance_ready").set(if ready { 1.0 } else { 0.0 });
    gauge!("moa_execution_maintenance_last_success_age_seconds")
        .set(last_success_age.map_or(f64::INFINITY, |duration| duration.as_secs_f64()));
}

/// Sets execution-retention health from its independent durable success receipt.
///
/// A missing receipt is exported as positive infinity so a process restart cannot
/// make retention appear fresh before a bounded retention pass has succeeded.
pub fn record_execution_retention(ready: bool, last_success_age: Option<Duration>) {
    gauge!("moa_execution_retention_ready").set(if ready { 1.0 } else { 0.0 });
    gauge!("moa_execution_retention_last_success_age_seconds")
        .set(last_success_age.map_or(f64::INFINITY, |duration| duration.as_secs_f64()));
}

/// Sets the draining Restate deployment snapshot: how many superseded revisions
/// still hold work, how much work is left, and how long the oldest has drained.
///
/// `blocking_invocations` counts non-terminal invocations still pinned to, or
/// last attempted on, a draining deployment. That is the same predicate that
/// refuses deregistration in `bootstrap::active_invocations_query`, so the
/// gauge reads as work remaining before the old revision can be removed; zero
/// means the drain finished and the revision is merely un-deregistered.
///
/// `oldest_drain_age` is measured from supersession, not from registration. A
/// long-lived healthy deployment is registered far earlier than it is
/// superseded, so registration age would put nearly every revision instantly
/// past the alert threshold.
///
/// All three are unlabeled process-level gauges: deployment IDs stay in-process
/// for the supersession sort. Callers must record every snapshot including the
/// fully drained zero, or the `absent()`-guarded drain alert cannot tell a
/// finished drain from a stopped producer.
pub fn record_restate_draining_deployments(
    deployments: u64,
    blocking_invocations: u64,
    oldest_drain_age: Duration,
) {
    gauge!("moa_restate_draining_deployments").set(deployments as f64);
    gauge!("moa_restate_draining_deployment_blocking_invocations").set(blocking_invocations as f64);
    gauge!("moa_restate_draining_deployment_oldest_age_seconds")
        .set(oldest_drain_age.as_secs_f64());
}

/// Records whether one worker terminal delivery was accepted or deduplicated.
pub fn record_worker_terminal_delivery(result: WorkerTerminalDeliveryResult) {
    counter!(
        "moa_worker_terminal_deliveries_total",
        "result" => result.as_str()
    )
    .increment(1);
}

/// Records the terminal kind that settled one conversational worker fan-in.
pub fn record_worker_fan_in_settled(kind: WorkerFanInSettledKind) {
    counter!(
        "moa_worker_fan_in_settled_total",
        "kind" => kind.as_str()
    )
    .increment(1);
}

/// Records one aggregate turn-step duration sample.
///
/// The per-step histogram handles are resolved once (on first record after the recorder is
/// installed) and cached, so the hot per-turn path avoids re-resolving a metric handle on every
/// call. The `step` label is one of a fixed, bounded set.
pub fn record_turn_step_duration(step: TurnLatencyStep, duration: Duration) {
    static TURN_STEP_HISTOGRAMS: OnceLock<[metrics::Histogram; 7]> = OnceLock::new();
    let histograms = TURN_STEP_HISTOGRAMS.get_or_init(|| {
        TurnLatencyStep::ALL
            .map(|step| histogram!(TURN_STEP_DURATION_METRIC, "step" => step.as_str()))
    });
    histograms[step.index()].record(duration.as_secs_f64());
}

/// Records one terminal turn-workflow outcome.
///
/// End-to-end workflow latency is already covered by `moa_turn_latency_seconds`;
/// this counter only tracks terminal outcome counts by scope, result, and tier.
pub fn record_turn_workflow_outcome(scope: &str, result: &str, model_tier: ModelTier) {
    counter!(
        "moa_turn_outcomes_total",
        "scope" => scope.to_string(),
        "result" => result.to_string(),
        "model_tier" => model_tier.as_str()
    )
    .increment(1);
}

/// Records one sandbox provisioning duration sample.
pub fn record_sandbox_provision_duration(provider: &str, tier: &str, duration: Duration) {
    histogram!(
        "moa_sandbox_provision_seconds",
        "provider" => provider.to_string(),
        "tier" => tier.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one durable sandbox workspace lifecycle outcome and its latency.
pub fn record_sandbox_workspace_lifecycle(
    provider: SandboxWorkspaceProviderKind,
    operation: SandboxWorkspaceLifecycleOperation,
    result: SandboxWorkspaceMetricResult,
    duration: Duration,
) {
    counter!(
        "moa_sandbox_workspace_lifecycle_total",
        "provider_kind" => provider.as_str(),
        "operation" => operation.as_str(),
        "result" => result.as_str()
    )
    .increment(1);
    histogram!(
        "moa_sandbox_workspace_lifecycle_duration_seconds",
        "provider_kind" => provider.as_str(),
        "operation" => operation.as_str(),
        "result" => result.as_str()
    )
    .record(duration.as_secs_f64());
}

/// Sets the fleet count for one durable workspace state and provider class.
pub fn record_sandbox_workspace_state(
    provider: SandboxWorkspaceProviderKind,
    state: SandboxWorkspaceState,
    count: u64,
) {
    gauge!(
        "moa_sandbox_workspace_state",
        "provider_kind" => provider.as_str(),
        "state" => state.as_str()
    )
    .set(count as f64);
}

/// Sets the fleet count for one provider storage-resource state.
pub fn record_sandbox_storage_resource_state(
    provider: SandboxWorkspaceProviderKind,
    state: SandboxStorageResourceMetricState,
    count: u64,
) {
    gauge!(
        "moa_sandbox_workspace_storage_resource_state",
        "provider_kind" => provider.as_str(),
        "state" => state.as_str()
    )
    .set(count as f64);
}

/// Records one workspace capacity admission decision.
pub fn record_sandbox_workspace_quota_decision(
    dimension: WorkspaceCapacityDimension,
    decision: SandboxWorkspaceQuotaDecision,
) {
    counter!(
        "moa_sandbox_workspace_quota_decisions_total",
        "dimension" => dimension.as_str(),
        "decision" => decision.as_str()
    )
    .increment(1);
}

/// Sets the highest enforced-scope quota utilization for one capacity dimension.
///
/// Callers must aggregate the tenant and provider-account scopes before recording.
/// Per-scope values are intentionally not accepted because a shared gauge would
/// otherwise expose only the last scope observed while identity labels would be
/// unbounded.
pub fn record_sandbox_workspace_quota_utilization(
    dimension: WorkspaceCapacityDimension,
    ratio: f64,
) {
    gauge!(
        "moa_sandbox_workspace_quota_utilization_ratio",
        "dimension" => dimension.as_str()
    )
    .set(ratio.clamp(0.0, 1.0));
}

/// Sets the current supervised workspace-reaper health snapshot.
pub fn record_sandbox_workspace_reaper(
    ready: bool,
    heartbeat_age: Duration,
    backlog: u64,
    oldest_work_age: Duration,
) {
    gauge!("moa_sandbox_workspace_reaper_ready").set(if ready { 1.0 } else { 0.0 });
    gauge!("moa_sandbox_workspace_reaper_heartbeat_age_seconds").set(heartbeat_age.as_secs_f64());
    gauge!("moa_sandbox_workspace_reaper_backlog").set(backlog as f64);
    gauge!("moa_sandbox_workspace_reaper_oldest_work_age_seconds")
        .set(oldest_work_age.as_secs_f64());
}

/// Sets active sandbox-hand compute by bounded provider class.
pub fn record_sandbox_workspace_active_hands(provider: SandboxWorkspaceProviderKind, count: u64) {
    gauge!(
        "moa_sandbox_workspace_active_hands",
        "provider_kind" => provider.as_str()
    )
    .set(count as f64);
}

/// Sets the number of parked execution tasks that still own active compute.
///
/// This must remain zero: it is the only automated guard on the invariant that
/// a parked run owns no sandbox. The aggregate intentionally carries no run,
/// task, or tenant label so an invariant check cannot create an unbounded
/// series, and callers must record it on every fleet snapshot including the
/// healthy zero so its alert can tell "no violations" from "no producer".
pub fn record_sandbox_workspace_parked_tasks_with_active_hands(count: u64) {
    gauge!("moa_sandbox_workspace_parked_tasks_with_active_hands").set(count as f64);
}

/// Records one portable-checkpoint restore into fresh sandbox compute.
pub fn record_sandbox_workspace_restore(provider: SandboxWorkspaceProviderKind) {
    counter!(
        "moa_sandbox_workspace_restores_total",
        "provider_kind" => provider.as_str()
    )
    .increment(1);
}

/// Records one checkpoint-and-release result at an execution yield boundary.
pub fn record_sandbox_workspace_release(
    provider: SandboxWorkspaceProviderKind,
    result: SandboxWorkspaceMetricResult,
) {
    counter!(
        "moa_sandbox_workspace_releases_total",
        "provider_kind" => provider.as_str(),
        "result" => result.as_str()
    )
    .increment(1);
}

/// Records checkpoint bytes and latency for one bounded lifecycle outcome.
pub fn record_sandbox_workspace_checkpoint(
    provider: SandboxWorkspaceProviderKind,
    operation: SandboxWorkspaceCheckpointOperation,
    result: SandboxWorkspaceMetricResult,
    bytes: u64,
    duration: Duration,
) {
    if bytes > 0 {
        counter!(
            "moa_sandbox_workspace_checkpoint_bytes_total",
            "provider_kind" => provider.as_str(),
            "operation" => operation.as_str(),
            "result" => result.as_str()
        )
        .increment(bytes);
    }
    histogram!(
        "moa_sandbox_workspace_checkpoint_duration_seconds",
        "provider_kind" => provider.as_str(),
        "operation" => operation.as_str(),
        "result" => result.as_str()
    )
    .record(duration.as_secs_f64());
}

/// Sets unresolved provider-inventory findings for one bounded classification.
pub fn record_sandbox_workspace_inventory_drift(
    provider: SandboxWorkspaceProviderKind,
    classification: SandboxWorkspaceInventoryDrift,
    count: u64,
) {
    gauge!(
        "moa_sandbox_workspace_inventory_drift",
        "provider_kind" => provider.as_str(),
        "classification" => classification.as_str()
    )
    .set(count as f64);
}

/// Records one appended session event, labeled by event type.
pub fn record_session_event_append(event_type: &str) {
    counter!(
        "moa_session_events_appended_total",
        "event_type" => event_type.to_string()
    )
    .increment(1);
}

/// Records one duration sample for a bounded session event append phase.
pub fn record_session_event_append_phase_duration(
    phase: SessionEventAppendPhase,
    duration: Duration,
) {
    static APPEND_PHASE_HISTOGRAMS: OnceLock<[metrics::Histogram; 14]> = OnceLock::new();
    let histograms = APPEND_PHASE_HISTOGRAMS.get_or_init(|| {
        SessionEventAppendPhase::ALL
            .map(|phase| histogram!(SESSION_EVENT_APPEND_PHASE_METRIC, "phase" => phase.as_str()))
    });
    histograms[phase.index()].record(duration.as_secs_f64());
}

/// Records one memory service operation.
pub fn record_memory_operation(operation: &str, status: &str) {
    counter!(
        "moa_memory_operations_total",
        "operation" => operation.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
}

/// Records one tenant knowledge sync-run lifecycle observation.
pub fn record_knowledge_sync_run(provider: &str, status: &str) {
    counter!(
        "moa_knowledge_sync_runs_total",
        "provider" => knowledge_metric_label(provider),
        "status" => knowledge_metric_label(status)
    )
    .increment(1);
}

/// Records one experiment run lifecycle observation.
pub fn record_experiment_run(status: &str, target_kind: &str) {
    counter!(
        "moa_experiment_runs_total",
        "status" => status.to_string(),
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

/// Records one experiment trial lifecycle observation.
pub fn record_experiment_trial(status: &str, stop_reason: Option<&str>, target_kind: &str) {
    counter!(
        "moa_experiment_trials_total",
        "status" => status.to_string(),
        "stop_reason" => stop_reason.unwrap_or("none").to_string(),
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

/// Records one simulator turn submitted to a target.
pub fn record_simulation_turn(target_kind: &str) {
    counter!(
        "moa_simulation_turns_total",
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

/// Records simulation token usage for a bounded participant role.
pub fn record_simulation_tokens(role: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_simulation_tokens_total",
        "role" => role.to_string()
    )
    .increment(tokens);
}

/// Records simulation model cost for a bounded participant role.
pub fn record_simulation_cost_cents(role: &str, cost_cents: u64) {
    if cost_cents == 0 {
        return;
    }

    counter!(
        "moa_simulation_cost_cents_total",
        "role" => role.to_string()
    )
    .increment(cost_cents);
}

/// Records score rows read from an experiment scoring surface.
pub fn record_experiment_score_rows(source: &str, rows: u64) {
    if rows == 0 {
        return;
    }

    counter!(
        "moa_experiment_score_rows_total",
        "source" => source.to_string()
    )
    .increment(rows);
}

/// Records learning candidates proposed from experiment evidence.
pub fn record_experiment_learning_candidates(status: &str, count: u64) {
    if count == 0 {
        return;
    }

    counter!(
        "moa_experiment_learning_candidates_total",
        "status" => status.to_string()
    )
    .increment(count);
}

/// Records that action policy queued a tenant-admin review.
pub fn record_action_review_requested(effect: ActionPolicyEffect, action_class: ActionClass) {
    counter!(
        "moa_action_review_requests_total",
        "effect" => effect.as_str(),
        "action_class" => action_class.as_str()
    )
    .increment(1);
}

/// Records a tenant-admin action-review decision.
pub fn record_action_review_decision(status: ActionReviewStatus, action_class: ActionClass) {
    counter!(
        "moa_action_review_decisions_total",
        "status" => status.as_str(),
        "action_class" => action_class.as_str()
    )
    .increment(1);
}

/// Records how long a tenant action review waited before an admin decided it.
///
/// Observed at decision time as `decided_at - created_at`; labeled by action
/// class only so cardinality stays bounded. The tenant is intentionally not a
/// label.
pub fn record_approval_wait(action_class: ActionClass, wait: Duration) {
    histogram!(
        "moa_approval_wait_seconds",
        "action_class" => action_class.as_str()
    )
    .record(wait.as_secs_f64());
}

/// Canonical risk-level labels for the pending action-review depth gauge.
///
/// Every known label is reset each sample so a drained risk class reports zero
/// instead of holding its last non-zero value.
const ACTION_REVIEW_RISK_LEVELS: [&str; 3] = ["low", "medium", "high"];

/// Publishes the pending tenant action-review queue depth by bounded risk level.
///
/// `depth_by_risk` carries only the currently non-empty risk classes; the other
/// canonical labels are set to zero so the gauge never reports a stale backlog.
pub fn record_action_review_pending_depth(depth_by_risk: &[(String, i64)]) {
    for risk in ACTION_REVIEW_RISK_LEVELS {
        gauge!("moa_action_review_pending", "risk_level" => risk).set(0.0);
    }
    for (risk, depth) in depth_by_risk {
        gauge!("moa_action_review_pending", "risk_level" => risk.clone()).set(*depth as f64);
    }
}

/// Publishes the age in seconds of the oldest pending tenant action review.
pub fn record_action_review_oldest_pending_age(age_seconds: f64) {
    gauge!("moa_action_review_oldest_pending_age_seconds").set(age_seconds);
}

/// Publishes the pending builtin async-authorization approval queue depth.
pub fn record_builtin_approval_pending_depth(depth: u64) {
    gauge!("moa_builtin_approval_pending").set(depth as f64);
}

/// Publishes the age in seconds of the oldest pending builtin approval.
pub fn record_builtin_approval_oldest_pending_age(age_seconds: f64) {
    gauge!("moa_builtin_approval_oldest_pending_age_seconds").set(age_seconds);
}

/// Records a terminal builtin approval decision by bounded status.
///
/// `status` is one of `approved`, `denied`, or `timeout`.
pub fn record_builtin_approval_decision(status: &'static str) {
    counter!("moa_builtin_approval_decisions_total", "status" => status).increment(1);
}

fn parse_metrics_listen_addr(config: &MetricsConfig) -> Result<SocketAddr> {
    let listen = config.prometheus_listen.as_deref().ok_or_else(|| {
        MoaError::ConfigError(
            "metrics.prometheus_listen is required by the prometheus exporter".to_string(),
        )
    })?;
    listen.parse::<SocketAddr>().map_err(|error| {
        MoaError::ConfigError(format!(
            "invalid metrics.prometheus_listen `{listen}`: {error}"
        ))
    })
}

fn format_metrics_endpoint_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip == Ipv4Addr::UNSPECIFIED => "localhost".to_string(),
        IpAddr::V6(ip) if ip == Ipv6Addr::UNSPECIFIED => "localhost".to_string(),
        ip => ip.to_string(),
    };
    format!("http://{host}:{}/metrics", addr.port())
}

/// Built-in tool names that are safe to use verbatim as a metric label.
///
/// Every other tool name (tenant- or MCP-defined) is bucketed as `"other"` by
/// [`tool_name_label`] so tool metrics keep bounded cardinality.
const BUILTIN_TOOL_NAMES: &[&str] = &[
    "bash",
    "file_read",
    "file_write",
    "file_search",
    "file_outline",
    "grep",
    "str_replace",
    "memory_remember",
    "memory_forget",
    "memory_supersede",
    "memory_search",
    "session_search",
    "tool_result_read",
    "tool_result_search",
    "spawn_worker",
    "wait_worker",
    "message_worker",
    "list_workers",
    "cancel_worker",
    "provide_worker_input",
    "report_to_parent",
    "request_input",
];

/// Returns a bounded tool-name label: the name itself when it is a known built-in, else `"other"`.
fn tool_name_label(tool_name: &str) -> &'static str {
    BUILTIN_TOOL_NAMES
        .iter()
        .copied()
        .find(|builtin| *builtin == tool_name)
        .unwrap_or("other")
}

fn knowledge_metric_label(value: &str) -> String {
    let normalized = value
        .chars()
        .take(48)
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' | '_' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            '-' | '.' | '/' | ' ' => '_',
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
fn knowledge_metric_names() -> &'static [&'static str] {
    &[
        "moa_knowledge_sync_runs_total",
        "moa_knowledge_records_total",
        "moa_knowledge_ingestion_step_duration_seconds",
        "moa_knowledge_parse_jobs_total",
    ]
}

fn register_metric_descriptions() {
    describe_gauge!("moa_sessions_active", "Currently active MOA sessions.");
    describe_counter!(
        "moa_turns_total",
        "Total assistant turns completed, labeled by model and routing tier."
    );
    describe_histogram!(
        GENAI_CLIENT_OPERATION_DURATION_METRIC,
        "GenAI client operation duration in seconds."
    );
    describe_histogram!(
        GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC,
        "Time to first streamed GenAI response chunk in seconds."
    );
    describe_histogram!(
        GENAI_CLIENT_TOKEN_USAGE_METRIC,
        "Provider-reported GenAI client token usage."
    );
    describe_counter!(
        "moa_tool_calls_total",
        "Total tool calls completed, labeled by tool name and status."
    );
    describe_counter!(
        "moa_tool_failure_total",
        "Total classified tool execution failures, labeled by class, provider, and tool."
    );
    describe_counter!(
        "moa_tool_reprovision_total",
        "Total automatic sandbox re-provisions, labeled by provider."
    );
    describe_counter!(
        "moa_session_errors_total",
        "Total session-scoped error events surfaced by the orchestrator."
    );
    describe_counter!(
        "moa_llm_cost_cents_total",
        "Total LLM completion cost in cents."
    );
    describe_counter!(
        "moa_lineage_dropped_total",
        "Lineage events dropped because the hot-path channel was saturated."
    );
    describe_counter!(
        "moa_lineage_enqueued_total",
        "Lineage events accepted onto the hot-path channel toward the durable journal."
    );
    describe_counter!(
        "moa_lineage_written_total",
        "Lineage rows durably written to Postgres."
    );
    describe_gauge!(
        "moa_lineage_journal_depth",
        "Approximate lineage events pending in the durable journal."
    );
    describe_histogram!(
        "moa_turn_latency_seconds",
        "End-to-end turn latency in seconds."
    );
    describe_histogram!(
        "moa_worker_terminal_parent_ack_seconds",
        "Worker-terminal-to-parent acknowledgement latency in seconds."
    );
    describe_histogram!(
        "moa_execution_dispatch_batch_size",
        "Ready execution tasks admitted by one bounded execution dispatcher refill."
    );
    describe_gauge!(
        "moa_execution_runs",
        "Current nonterminal execution runs by bounded product phase."
    );
    describe_gauge!(
        "moa_execution_oldest_ready_age_seconds",
        "Age in seconds of the oldest execution task ready for dispatch."
    );
    describe_gauge!(
        "moa_execution_overdue_deadlines",
        "Nonterminal execution runs whose absolute deadline has elapsed."
    );
    describe_gauge!(
        "moa_execution_trigger_lag_seconds",
        "Age in seconds of the oldest due undelivered execution trigger."
    );
    describe_gauge!(
        "moa_execution_trigger_due",
        "Due execution triggers observed in the bounded fleet queue-health sample."
    );
    describe_gauge!(
        "moa_execution_outbox_lag_seconds",
        "Age in seconds of the oldest undispatched execution outbox row."
    );
    describe_gauge!(
        "moa_execution_outbox_claimable",
        "Claimable dispatch-outbox rows observed in the bounded fleet queue-health sample."
    );
    describe_gauge!(
        "moa_execution_outbox_dead_letters",
        "Execution dispatch-outbox rows currently held in dead-letter state."
    );
    describe_gauge!(
        "moa_execution_queue_sample_saturated",
        "Whether a bounded execution queue-health sample reached its observation cap, by fixed queue and sample kind."
    );
    describe_gauge!(
        "moa_execution_active_attempt_oldest_age_seconds",
        "Age in seconds of the oldest active execution task-attempt lease."
    );
    describe_gauge!(
        "moa_execution_external_job_oldest_age_seconds",
        "Age in seconds of the oldest nonterminal asynchronous execution job."
    );
    describe_gauge!(
        "moa_execution_admission_utilization_ratio",
        "Execution admission utilization by bounded resource and aggregation scope."
    );
    describe_gauge!(
        "moa_execution_tenant_max_share_ratio",
        "Largest tenant share of one bounded fleet execution resource."
    );
    describe_gauge!(
        "moa_execution_maintenance_ready",
        "Whether the singleton execution-maintenance owner is healthy."
    );
    describe_gauge!(
        "moa_execution_maintenance_last_success_age_seconds",
        "Age in seconds of the durable last successful bounded execution reconciliation receipt."
    );
    describe_gauge!(
        "moa_execution_retention_ready",
        "Whether execution retention has a healthy durable success receipt."
    );
    describe_gauge!(
        "moa_execution_retention_last_success_age_seconds",
        "Age in seconds of the durable last successful bounded execution-retention receipt."
    );
    describe_gauge!(
        "moa_restate_draining_deployments",
        "Restate service deployment revisions still draining active invocations."
    );
    describe_gauge!(
        "moa_restate_draining_deployment_blocking_invocations",
        "Non-terminal invocations still blocking retirement of a draining Restate revision."
    );
    describe_gauge!(
        "moa_restate_draining_deployment_oldest_age_seconds",
        "Seconds since the oldest still-draining Restate deployment revision was superseded."
    );
    describe_counter!(
        "moa_worker_terminal_deliveries_total",
        "Worker terminal deliveries by bounded acceptance result."
    );
    describe_counter!(
        "moa_worker_fan_in_settled_total",
        "Conversational worker fan-in settlements by bounded terminal kind."
    );
    describe_histogram!(
        TURN_STEP_DURATION_METRIC,
        "Aggregate per-turn step duration in seconds, labeled by documented turn step."
    );
    describe_counter!(
        "moa_turn_outcomes_total",
        "Terminal turn workflow outcomes, labeled by scope, result, and model tier."
    );
    describe_histogram!(
        "moa_tool_call_duration_seconds",
        "Tool execution duration in seconds."
    );
    describe_histogram!(
        "moa_sandbox_provision_seconds",
        "Sandbox provisioning duration in seconds."
    );
    describe_counter!(
        "moa_sandbox_workspace_lifecycle_total",
        "Durable sandbox workspace lifecycle outcomes by bounded provider class, operation, and result."
    );
    describe_histogram!(
        "moa_sandbox_workspace_lifecycle_duration_seconds",
        "Durable sandbox workspace lifecycle latency in seconds by bounded provider class, operation, and result."
    );
    describe_gauge!(
        "moa_sandbox_workspace_state",
        "Current durable sandbox workspaces by bounded provider class and lifecycle state."
    );
    describe_gauge!(
        "moa_sandbox_workspace_storage_resource_state",
        "Current provider storage resources by bounded provider class and durable state."
    );
    describe_counter!(
        "moa_sandbox_workspace_quota_decisions_total",
        "Workspace capacity admission decisions by bounded dimension and result."
    );
    describe_gauge!(
        "moa_sandbox_workspace_quota_utilization_ratio",
        "Fleet workspace capacity utilization ratio by bounded dimension."
    );
    describe_gauge!(
        "moa_sandbox_workspace_reaper_ready",
        "Whether the supervised workspace reaper is healthy on this service replica."
    );
    describe_gauge!(
        "moa_sandbox_workspace_reaper_heartbeat_age_seconds",
        "Age in seconds of the supervised workspace reaper heartbeat."
    );
    describe_gauge!(
        "moa_sandbox_workspace_reaper_backlog",
        "Workspace reaper rows awaiting maintenance on this service replica."
    );
    describe_gauge!(
        "moa_sandbox_workspace_reaper_oldest_work_age_seconds",
        "Age in seconds of the oldest workspace reaper item."
    );
    describe_gauge!(
        "moa_sandbox_workspace_active_hands",
        "Active sandbox-hand compute by bounded provider class."
    );
    describe_gauge!(
        "moa_sandbox_workspace_parked_tasks_with_active_hands",
        "Parked execution tasks that incorrectly retain active sandbox compute."
    );
    describe_counter!(
        "moa_sandbox_workspace_restores_total",
        "Portable-checkpoint restores into fresh compute by bounded provider class."
    );
    describe_counter!(
        "moa_sandbox_workspace_releases_total",
        "Checkpoint-and-release outcomes at execution yield boundaries."
    );
    describe_counter!(
        "moa_sandbox_workspace_checkpoint_bytes_total",
        "Portable checkpoint bytes processed by bounded provider class, operation, and result."
    );
    describe_histogram!(
        "moa_sandbox_workspace_checkpoint_duration_seconds",
        "Portable checkpoint operation latency in seconds by bounded provider class, operation, and result."
    );
    describe_gauge!(
        "moa_sandbox_workspace_inventory_drift",
        "Unresolved provider inventory findings by bounded provider class and classification."
    );
    describe_histogram!(
        "moa_cache_hit_rate",
        "Ratio of cached input tokens to total input tokens for one request."
    );
    describe_counter!(
        "moa_session_events_appended_total",
        "Session events appended to the durable event log, labeled by event type."
    );
    describe_histogram!(
        SESSION_EVENT_APPEND_PHASE_METRIC,
        "Session event append duration in seconds, labeled by bounded handler/action/transaction phase."
    );
    describe_counter!(
        "moa_turn_admission_decisions_total",
        "Coordinator-turn admission decisions, labeled by bounded scope and outcome."
    );
    describe_gauge!(
        "moa_turn_admission_live",
        "Live coordinator-turn admission leases, labeled by bounded scope."
    );
    describe_counter!(
        "moa_memory_operations_total",
        "Memory service operations, labeled by operation and status."
    );
    describe_counter!(
        "moa_knowledge_sync_runs_total",
        "Tenant knowledge sync-run lifecycle outcomes, labeled by provider and status."
    );
    describe_counter!(
        "moa_knowledge_records_total",
        "Tenant knowledge provider records observed, labeled by provider and action."
    );
    describe_histogram!(
        "moa_knowledge_ingestion_step_duration_seconds",
        "Tenant knowledge ingestion step duration in seconds, labeled by provider, parser, stage, and status."
    );
    describe_counter!(
        "moa_knowledge_parse_jobs_total",
        "Tenant knowledge parse job outcomes, labeled by parser and status."
    );
    describe_counter!(
        "moa_experiment_runs_total",
        "Experiment run lifecycle observations, labeled by terminal status and bounded target kind."
    );
    describe_counter!(
        "moa_experiment_trials_total",
        "Experiment trial lifecycle observations, labeled by status, bounded stop reason, and bounded target kind."
    );
    describe_counter!(
        "moa_simulation_turns_total",
        "Simulator turns submitted to experiment targets, labeled by bounded target kind."
    );
    describe_counter!(
        "moa_simulation_tokens_total",
        "Simulation token usage, labeled by bounded participant role."
    );
    describe_counter!(
        "moa_simulation_cost_cents_total",
        "Simulation model cost in cents, labeled by bounded participant role."
    );
    describe_counter!(
        "moa_experiment_score_rows_total",
        "Experiment score rows read by service surfaces, labeled by bounded source."
    );
    describe_counter!(
        "moa_experiment_learning_candidates_total",
        "Experiment learning candidates proposed, labeled by candidate status."
    );
    describe_counter!(
        "moa_action_review_requests_total",
        "Action reviews requested by policy evaluation, labeled by effect and action class."
    );
    describe_counter!(
        "moa_action_review_decisions_total",
        "Action review decisions, labeled by status and action class."
    );
    describe_histogram!(
        "moa_approval_wait_seconds",
        "Tenant action-review wait from creation to admin decision in seconds, labeled by action class."
    );
    describe_gauge!(
        "moa_action_review_pending",
        "Pending tenant action reviews awaiting an admin decision, labeled by risk level."
    );
    describe_gauge!(
        "moa_action_review_oldest_pending_age_seconds",
        "Age in seconds of the oldest pending tenant action review."
    );
    describe_gauge!(
        "moa_builtin_approval_pending",
        "Pending builtin async-authorization approvals awaiting a decision."
    );
    describe_gauge!(
        "moa_builtin_approval_oldest_pending_age_seconds",
        "Age in seconds of the oldest pending builtin async-authorization approval."
    );
    describe_counter!(
        "moa_builtin_approval_decisions_total",
        "Terminal builtin approval decisions, labeled by status (approved/denied/timeout)."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_service_histograms_have_explicit_boundary_inventory() {
        // Pins: every histogram retained on a MOA service exporter has an explicit
        // unit-appropriate layout shared by OTLP and Prometheus. Test-only perf-gate
        // histograms are owned by the loadtest's separate Prometheus recorder.
        let observed = HISTOGRAM_BOUNDARIES
            .iter()
            .map(|(metric, _)| *metric)
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "gen_ai.client.operation.duration",
            "gen_ai.client.operation.time_to_first_chunk",
            "gen_ai.client.token.usage",
            "moa_approval_wait_seconds",
            "moa_cache_hit_rate",
            "moa_execution_dispatch_batch_size",
            "moa_knowledge_ingestion_step_duration_seconds",
            "moa_lineage_durable_append_seconds",
            "moa_retrieval_cache_hit_seconds",
            "moa_retrieval_leg_seconds",
            "moa_retrieval_rrf_rerank_seconds",
            "moa_sandbox_provision_seconds",
            "moa_sandbox_workspace_checkpoint_duration_seconds",
            "moa_sandbox_workspace_lifecycle_duration_seconds",
            "moa_session_event_append_phase_seconds",
            "moa_tool_call_duration_seconds",
            "moa_turn_latency_seconds",
            "moa_turn_step_duration_seconds",
            "moa_worker_terminal_parent_ack_seconds",
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(observed, expected);
        for (metric, boundaries) in HISTOGRAM_BOUNDARIES {
            assert!(
                !boundaries.is_empty(),
                "{metric} has no explicit boundaries"
            );
            assert!(
                boundaries.iter().all(|boundary| boundary.is_finite()),
                "{metric} has a non-finite boundary"
            );
            assert!(
                boundaries.windows(2).all(|pair| pair[0] < pair[1]),
                "{metric} boundaries are not strictly increasing"
            );
        }
    }

    #[test]
    fn otlp_drops_append_phase_but_exports_retained_coordination_histograms() {
        // Pins: append-phase timing remains a Prometheus-only load-test
        // diagnostic while ordinary retained service histograms still flow to
        // OTLP through the same view configuration used in production.
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let provider =
            apply_otlp_metric_views(SdkMeterProvider::builder().with_reader(reader)).build();
        let recorder = metrics_exporter_otel::OpenTelemetryRecorder::new(
            provider.meter("moa-observability-view-test"),
        );
        let _recorder_guard = metrics::set_default_local_recorder(&recorder);

        histogram!(SESSION_EVENT_APPEND_PHASE_METRIC, "phase" => "commit").record(0.002);
        histogram!("moa_turn_latency_seconds").record(0.025);
        record_worker_terminal_parent_ack(Duration::from_millis(4));
        record_execution_dispatch_batch_size(32);
        provider
            .force_flush()
            .expect("in-memory metric exporter should flush");

        let exported = exporter
            .get_finished_metrics()
            .expect("in-memory metric exporter should retain metrics");
        let names = exported
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .map(|metric| metric.name())
            .collect::<std::collections::BTreeSet<_>>();

        for metric in [
            "moa_turn_latency_seconds",
            "moa_worker_terminal_parent_ack_seconds",
            "moa_execution_dispatch_batch_size",
        ] {
            assert!(
                names.contains(metric),
                "retained histogram {metric} must export through OTLP: {names:?}"
            );
        }
        assert!(
            !names.contains(SESSION_EVENT_APPEND_PHASE_METRIC),
            "append-phase timing must not export through OTLP: {names:?}"
        );
    }

    #[test]
    fn coordination_metrics_export_descriptions_and_bounded_labels() {
        // Pins: coordination handoffs and bounded dispatch observations reach
        // Prometheus with descriptions, while counter labels come only from
        // their closed-vocabulary enums and no runtime identifiers are exposed.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_worker_terminal_parent_ack(Duration::from_millis(4));
            record_execution_dispatch_batch_size(32);
            record_worker_terminal_delivery(WorkerTerminalDeliveryResult::Accepted);
            record_worker_terminal_delivery(WorkerTerminalDeliveryResult::Duplicate);
            record_worker_fan_in_settled(WorkerFanInSettledKind::Completed);
            record_worker_fan_in_settled(WorkerFanInSettledKind::Cancelled);
        });
        let rendered = handle.render();

        let coordination_metrics = [
            "moa_worker_terminal_parent_ack_seconds",
            "moa_execution_dispatch_batch_size",
            "moa_worker_terminal_deliveries_total",
            "moa_worker_fan_in_settled_total",
        ];
        for metric in coordination_metrics {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "coordination metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        let coordination_series = rendered
            .lines()
            .filter(|line| {
                coordination_metrics
                    .iter()
                    .any(|metric| line.starts_with(metric))
            })
            .collect::<Vec<_>>()
            .join("\n");
        for label in [
            "result=\"accepted\"",
            "result=\"duplicate\"",
            "kind=\"completed\"",
            "kind=\"cancelled\"",
        ] {
            assert!(
                coordination_series.contains(label),
                "coordination series should include bounded label `{label}`:\n{coordination_series}"
            );
        }
        for forbidden in [
            "session_id",
            "worker_id",
            "run_id",
            "run_uid",
            "task_id",
            "task_uid",
        ] {
            assert!(
                !coordination_series.contains(forbidden),
                "coordination series must not carry high-cardinality label `{forbidden}`:\n{coordination_series}"
            );
        }
    }

    #[test]
    fn long_horizon_metrics_export_descriptions_and_only_bounded_labels() {
        // Pins: execution fleet health, drain cost, and the sandbox-yield and
        // parked-task hand invariants reach production exporters without tenant,
        // run, task, deployment-version, or provider-account IDs. That each
        // recorder also has a caller outside this crate is pinned separately by
        // `validate-observability.sh`, not by this test.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            for phase in [
                ExecutionRunMetricPhase::AwaitingConfirmation,
                ExecutionRunMetricPhase::Queued,
                ExecutionRunMetricPhase::Running,
                ExecutionRunMetricPhase::WaitingInput,
                ExecutionRunMetricPhase::WaitingReview,
                ExecutionRunMetricPhase::WaitingSignal,
                ExecutionRunMetricPhase::WaitingTimer,
                ExecutionRunMetricPhase::WaitingExternal,
                ExecutionRunMetricPhase::WaitingReplan,
                ExecutionRunMetricPhase::PauseRequested,
                ExecutionRunMetricPhase::Pausing,
                ExecutionRunMetricPhase::Paused,
                ExecutionRunMetricPhase::Compensating,
            ] {
                record_execution_run_phase(phase, 1);
            }
            record_execution_oldest_ready_age(Duration::from_secs(31));
            record_execution_overdue_deadlines(2);
            record_execution_trigger_queue(Duration::from_secs(7), 11, true);
            record_execution_outbox_queue(Duration::from_secs(8), 12, false, 1, true);
            record_execution_active_attempt_oldest_age(Duration::from_secs(61));
            record_execution_external_job_oldest_age(Duration::from_secs(62));
            record_execution_admission_utilization(
                ExecutionAdmissionResource::ActiveTasks,
                ExecutionAdmissionScope::Fleet,
                0.75,
            );
            record_execution_admission_utilization(
                ExecutionAdmissionResource::ParkedRuns,
                ExecutionAdmissionScope::TenantPeak,
                0.5,
            );
            record_execution_tenant_max_share(ExecutionAdmissionResource::ActiveRuns, 0.4);
            record_execution_maintenance(true, Some(Duration::from_secs(3)));
            record_execution_maintenance(false, None);
            record_execution_retention(true, Some(Duration::from_secs(3_600)));
            record_execution_retention(false, None);
            record_restate_draining_deployments(2, 17, Duration::from_secs(3_600));
            record_sandbox_workspace_active_hands(SandboxWorkspaceProviderKind::E2b, 2);
            record_sandbox_workspace_parked_tasks_with_active_hands(0);
            record_sandbox_workspace_restore(SandboxWorkspaceProviderKind::E2b);
            record_sandbox_workspace_release(
                SandboxWorkspaceProviderKind::E2b,
                SandboxWorkspaceMetricResult::Succeeded,
            );
        });
        let rendered = handle.render();

        let metrics = [
            "moa_execution_runs",
            "moa_execution_oldest_ready_age_seconds",
            "moa_execution_overdue_deadlines",
            "moa_execution_trigger_lag_seconds",
            "moa_execution_trigger_due",
            "moa_execution_outbox_lag_seconds",
            "moa_execution_outbox_claimable",
            "moa_execution_outbox_dead_letters",
            "moa_execution_queue_sample_saturated",
            "moa_execution_active_attempt_oldest_age_seconds",
            "moa_execution_external_job_oldest_age_seconds",
            "moa_execution_admission_utilization_ratio",
            "moa_execution_tenant_max_share_ratio",
            "moa_execution_maintenance_ready",
            "moa_execution_maintenance_last_success_age_seconds",
            "moa_execution_retention_ready",
            "moa_execution_retention_last_success_age_seconds",
            "moa_restate_draining_deployments",
            "moa_restate_draining_deployment_blocking_invocations",
            "moa_restate_draining_deployment_oldest_age_seconds",
            "moa_sandbox_workspace_active_hands",
            "moa_sandbox_workspace_parked_tasks_with_active_hands",
            "moa_sandbox_workspace_restores_total",
            "moa_sandbox_workspace_releases_total",
        ];
        for metric in metrics {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "long-horizon metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        assert!(
            rendered.contains("moa_execution_maintenance_ready 0"),
            "a missing durable success receipt must make maintenance unready:\n{rendered}"
        );
        assert!(
            rendered.contains("moa_execution_maintenance_last_success_age_seconds inf"),
            "a missing durable success receipt must be older than every finite SLO:\n{rendered}"
        );
        assert!(
            rendered.contains("moa_execution_retention_ready 0"),
            "a missing durable retention receipt must make retention unready:\n{rendered}"
        );
        assert!(
            rendered.contains("moa_execution_retention_last_success_age_seconds inf"),
            "a missing durable retention receipt must be older than every finite SLO:\n{rendered}"
        );

        for label in [
            // The census is only total if the two statuses that carry no active
            // compute still get a phase; dropping either desyncs
            // `sum(moa_execution_runs)` from the durable nonterminal predicate.
            "phase=\"awaiting_confirmation\"",
            "phase=\"waiting_replan\"",
            "phase=\"waiting_timer\"",
            "resource=\"active_tasks\"",
            "scope=\"fleet\"",
            "scope=\"tenant_peak\"",
            "provider_kind=\"e2b\"",
            "result=\"succeeded\"",
            "queue=\"trigger\"",
            "sample=\"due\"",
            "queue=\"outbox\"",
            "sample=\"dead_letter\"",
        ] {
            assert!(
                rendered.contains(label),
                "long-horizon metrics should include bounded label `{label}`:\n{rendered}"
            );
        }
        for forbidden in [
            "tenant_id",
            "run_id",
            "run_uid",
            "task_id",
            "task_uid",
            "deployment_id",
            "deployment_version",
            "provider_account_id",
            "external_job_id",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "long-horizon metrics must not carry high-cardinality label `{forbidden}`:\n{rendered}"
            );
        }
    }

    #[test]
    fn tool_name_label_buckets_unknown_tools_as_other() {
        // Pins: built-in tool names pass through as metric labels; tenant/MCP-defined names bucket
        // to "other" so tool metric cardinality stays bounded.
        assert_eq!(tool_name_label("bash"), "bash");
        assert_eq!(tool_name_label("spawn_worker"), "spawn_worker");
        assert_eq!(tool_name_label("memory_search"), "memory_search");
        assert_eq!(tool_name_label("acme_customer_lookup"), "other");
        assert_eq!(tool_name_label(""), "other");
    }

    #[test]
    fn tool_metrics_use_bounded_labels() {
        // Pins: tool metrics bucket unknown tool names to "other".
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_tool_call("bash", "ok", Duration::from_millis(1));
            record_tool_call("acme_customer_lookup", "ok", Duration::from_millis(1));
            record_tool_failure("mock", "acme_customer_lookup", "transient");
        });
        let rendered = handle.render();

        assert!(
            rendered.contains("tool_name=\"bash\""),
            "built-in tool name should pass through:\n{rendered}"
        );
        assert!(
            rendered.contains("tool_name=\"other\""),
            "unknown tool name should bucket to other:\n{rendered}"
        );
        assert!(
            !rendered.contains("acme_customer_lookup"),
            "raw tenant/MCP tool name must never appear as a label:\n{rendered}"
        );
    }

    #[test]
    fn metrics_endpoint_url_uses_localhost_for_unspecified_listener() {
        let url = metrics_endpoint_url(&MetricsConfig {
            exporter: MetricsExporter::Prometheus,
            prometheus_listen: Some("0.0.0.0:9090".to_string()),
        });

        assert_eq!(url.as_deref(), Some("http://localhost:9090/metrics"));
    }

    #[test]
    fn no_scrape_url_is_reported_under_the_push_exporter() {
        // Pins: the OTLP default advertises no scrape endpoint. Reporting one
        // would put a URL in startup logs, manifests and network policies for a
        // port nothing binds - which is exactly the fake 9090 surface production
        // grew before this.
        //
        // The listen address is deliberately PRESENT. With it absent the address
        // parse fails and returns None on its own, so the exporter check would
        // never run and this test would pass with that check deleted - which is
        // exactly what it did when first written. `validate()` refuses this
        // combination, so it can only reach the function from a caller that
        // skipped validation; the exporter check is the guard for precisely that
        // caller, and this is the only shape that exercises it.
        for exporter in [MetricsExporter::Otlp, MetricsExporter::Disabled] {
            let config = MetricsConfig {
                exporter,
                prometheus_listen: Some("0.0.0.0:9090".to_string()),
            };
            assert!(
                config.validate().is_err(),
                "precondition: this combination must be one `validate()` refuses, or the \
                 guard under test is unreachable in production and should be deleted instead"
            );

            let url = metrics_endpoint_url(&config);

            assert_eq!(
                url,
                None,
                "exporter {} must advertise no scrape URL even when a listen address is \
                 present, got {url:?}",
                exporter.as_str()
            );
        }
    }

    #[test]
    fn a_missing_listen_address_also_yields_no_scrape_url() {
        // Negative control for the neighbour the test above deliberately avoids:
        // an unparseable (here absent) address must also produce no URL, so the
        // two guards are pinned separately rather than one standing in for both.
        assert_eq!(
            metrics_endpoint_url(&MetricsConfig {
                exporter: MetricsExporter::Prometheus,
                prometheus_listen: None,
            }),
            None
        );
    }

    #[test]
    fn otlp_metrics_validate_configured_grpc_headers() {
        // Pins: metrics consume the same configured OTLP auth/routing headers
        // as traces. If the metric exporter ignores this map, an invalid header
        // would incorrectly initialize instead of failing configuration.
        let error = install_otlp_metrics(
            None,
            OtlpProtocol::Grpc,
            &std::collections::HashMap::from([(
                "invalid header name".to_string(),
                "value".to_string(),
            )]),
            Resource::builder().build(),
        )
        .expect_err("metrics must validate configured OTLP gRPC headers");

        assert!(
            matches!(error, MoaError::ConfigError(_)),
            "invalid metric-export headers must be a configuration error, got {error:?}"
        );
    }

    #[test]
    fn otlp_metric_export_interval_defaults_to_120_seconds_and_accepts_milliseconds() {
        // Pins: workloads need no interval env for the cost-controlled 120s
        // default, while short-lived fixtures can still request a faster cycle.
        assert_eq!(
            parse_otlp_metric_export_interval(None)
                .expect("the in-code metric interval default must be valid"),
            Duration::from_secs(120)
        );
        assert_eq!(
            parse_otlp_metric_export_interval(Some("2000"))
                .expect("a positive millisecond override must be accepted"),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn otlp_metric_export_interval_rejects_zero_and_invalid_values() {
        // Pins: a malformed standard OTel interval fails startup clearly rather
        // than silently reverting to a different export cadence.
        for value in ["0", "", "later", "-1"] {
            let error = parse_otlp_metric_export_interval(Some(value))
                .expect_err("zero or non-integer intervals must be rejected");
            assert!(
                matches!(error, MoaError::ConfigError(_)),
                "invalid interval `{value}` must be a configuration error, got {error:?}"
            );
            assert!(
                error.to_string().contains(OTEL_METRIC_EXPORT_INTERVAL_ENV),
                "the refusal must identify {OTEL_METRIC_EXPORT_INTERVAL_ENV}, got {error}"
            );
        }
    }

    #[test]
    fn experiment_metrics_export_descriptions_and_bounded_labels() {
        // Pins: every experiment/simulation metric exports a HELP description and only
        // bounded dashboard labels. Asserted against rendered Prometheus output (the real
        // exported descriptors), not the crate's own source text.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_experiment_run("accepted", "agent_loop");
            record_experiment_trial("completed", Some("max_turns"), "agent_loop");
            record_simulation_turn("agent_loop");
            record_simulation_tokens("simulator", 16);
            record_simulation_cost_cents("simulator", 1);
            record_experiment_score_rows("scores", 3);
            record_experiment_learning_candidates("proposed", 1);
        });
        let rendered = handle.render();

        let experiment_metrics = [
            "moa_experiment_runs_total",
            "moa_experiment_trials_total",
            "moa_simulation_turns_total",
            "moa_simulation_tokens_total",
            "moa_simulation_cost_cents_total",
            "moa_experiment_score_rows_total",
            "moa_experiment_learning_candidates_total",
        ];
        for metric in experiment_metrics {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        // Bounded labels that SHOULD appear on the exported series.
        for label in [
            "status=",
            "target_kind=",
            "stop_reason=",
            "source=",
            "role=",
        ] {
            assert!(
                rendered.contains(label),
                "expected bounded experiment label `{label}` in rendered output:\n{rendered}"
            );
        }

        let experiment_series = rendered
            .lines()
            .filter(|line| {
                experiment_metrics
                    .iter()
                    .any(|metric| line.starts_with(metric))
            })
            .collect::<Vec<_>>();
        assert!(
            !experiment_series.is_empty(),
            "experiment metric series should be exported:\n{rendered}"
        );
        for forbidden in [
            "run_uid",
            "trial_uid",
            "session_id",
            "execution_run_uid",
            "score_run_id",
            "trial_key",
            "artifact_revision",
            "prompt",
            "profile",
            "persona",
            "scenario",
            "transcript",
            "connector",
            "model_output",
        ] {
            for line in &experiment_series {
                assert!(
                    !line.contains(forbidden),
                    "experiment series `{line}` must not carry high-cardinality label `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn knowledge_metrics_export_descriptions_and_low_cardinality_labels() {
        // Pins: tenant knowledge metrics export HELP descriptions and only the bounded
        // Task-13 label set (no tenant, source, object, contact, or error-message labels).
        // Asserted against rendered Prometheus output, not the crate's own source text.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_knowledge_sync_run("github", "succeeded");
            // Ingestion families are emitted by `moa-knowledge`'s production
            // step-observability helper; seed the same family and label shapes here.
            counter!(
                "moa_knowledge_records_total",
                "provider" => "github",
                "action" => "ingested"
            )
            .increment(1);
            histogram!(
                "moa_knowledge_ingestion_step_duration_seconds",
                "provider" => "github",
                "parser" => "pdf",
                "stage" => "parse_completed",
                "status" => "completed"
            )
            .record(Duration::from_millis(3).as_secs_f64());
            counter!(
                "moa_knowledge_parse_jobs_total",
                "parser" => "pdf",
                "status" => "completed"
            )
            .increment(1);
        });
        let rendered = handle.render();

        for metric in knowledge_metric_names() {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "knowledge metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        // Only the metric series lines carry labels; `# HELP`/`# TYPE` lines start with `#`.
        let knowledge_series = rendered
            .lines()
            .filter(|line| line.starts_with("moa_knowledge_"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !knowledge_series.is_empty(),
            "knowledge metric series should be exported:\n{rendered}"
        );
        for required_label in ["provider=", "status=", "action=", "parser=", "stage="] {
            assert!(
                knowledge_series.contains(required_label),
                "knowledge series should include bounded label `{required_label}`:\n{knowledge_series}"
            );
        }
        for forbidden in [
            "tenant_id",
            "source_uri",
            "object_id",
            "object_uid",
            "contact_id",
            "contact_uid",
            "error_message",
            "error_code",
            "provider_event_id",
            "parser_job_id",
            "access_token",
            "api_key",
        ] {
            assert!(
                !knowledge_series.contains(forbidden),
                "knowledge series must not carry high-cardinality label `{forbidden}`:\n{knowledge_series}"
            );
        }
    }
}
