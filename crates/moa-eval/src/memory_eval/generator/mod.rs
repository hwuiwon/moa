//! Deterministic ledger-first memory evaluation corpus generation.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::UserId,
};
use moa_memory_graph::PiiClass;
use moa_memory_types::ScopeTier;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::corpus::{
    CORPUS_SCHEMA_VERSION, CorpusManifest, CorpusProfile, LedgerFact, Probe, ProbeType,
    SyntheticSession, SyntheticTurn, TranscriptStyle, validate_corpus, write_ledger_jsonl,
    write_manifest_json, write_probes_jsonl, write_sessions_jsonl,
};
use moa_eval_core::{EvalError, Result};

mod embeddings;
mod model;
mod rendering;
mod validation;

use super::io::{ensure_non_empty, invalid_config, io_error, read_jsonl, write_jsonl};
use rendering::{distractor_transcript, render_fact_transcript, should_restate_dependency};
use validation::*;

const REQUIRED_SEED_COUNT: usize = 3;
const PR_USER_COUNT: usize = 5;
const PR_TENANT_COUNT: usize = 2;
const FULL_USER_COUNT: usize = 50;
const FULL_TENANT_COUNT: usize = 3;
const FULL_MIN_PROBES: usize = 600;
const FULL_MAX_PROBES: usize = 1_000;
const BASE_UNIX_SECONDS: i64 = 1_767_225_600;
const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;

const COMPONENTS: &[&str] = &[
    "billing-api",
    "search-indexer",
    "checkout-worker",
    "audit-shipper",
    "profile-service",
    "notification-router",
    "catalog-sync",
    "policy-engine",
];
const DEPLOY_TARGETS: &[(&str, &str)] = &[
    ("staging", "production-canary"),
    ("retired-cluster", "gke-primary"),
    ("blue", "green"),
    ("us-central1", "us-east1"),
];
const RUNBOOKS: &[&str] = &[
    "runbook/payments-canary",
    "runbook/search-rollout",
    "runbook/audit-replay",
    "runbook/policy-release",
];
const CACHE_BACKENDS: &[(&str, &str)] = &[
    ("redis", "valkey"),
    ("memcached", "dragonfly"),
    ("postgres-cache", "read-through-cache"),
    ("local-lru", "distributed-cache"),
];
const REPOSITORIES: &[&str] = &[
    "repo/mobile-client",
    "repo/control-plane",
    "repo/data-pipeline",
    "repo/internal-tools",
    "repo/search-platform",
];
const RESPONSE_STYLES: &[&str] = &[
    "concise bullets",
    "step-by-step checklists",
    "short paragraphs",
    "tables for comparisons",
    "commands first",
];
const EDITORS: &[&str] = &["nvim", "zed", "vscode", "helix", "emacs"];
const ON_CALLS: &[(&str, &str)] = &[
    ("Avery", "Blair"),
    ("Casey", "Devon"),
    ("Elliot", "Finley"),
    ("Gray", "Harper"),
];
const LIBRARIES: &[&str] = &[
    "lib-ledger-core",
    "lib-search-flow",
    "lib-audit-wire",
    "lib-policy-kit",
    "lib-profile-cache",
    "lib-catalog-sync",
];
const OWNER_TEAMS: &[&str] = &[
    "payments-platform",
    "search-infra",
    "audit-systems",
    "policy-runtime",
    "profile-experience",
    "catalog-ops",
];

/// A generated memory-evaluation corpus and its derived embedding inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedMemoryEvalCorpus {
    /// Directory manifest document.
    pub manifest: CorpusManifest,
    /// Ledger-first fact schedule.
    pub ledger: Vec<LedgerFact>,
    /// Synthetic transcripts rendered from the ledger schedule.
    pub sessions: Vec<SyntheticSession>,
    /// Retrieval and answer probes derived from the ledger.
    pub probes: Vec<Probe>,
    /// Text inputs that later tasks can embed without re-rendering the corpus.
    pub embedding_inputs: Vec<EmbeddingInput>,
}

impl GeneratedMemoryEvalCorpus {
    /// Validates profile shape, corpus schema, and embedding input references.
    pub fn validate(&self) -> Result<()> {
        validate_corpus(&self.manifest, &self.ledger, &self.sessions, &self.probes)?;
        validate_embedding_inputs(&self.embedding_inputs, &self.ledger, &self.probes)?;
        validate_profile_shape(self)
    }
}

/// One text input to include in `embedding_inputs.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingInput {
    /// Stable input identifier.
    pub input_id: String,
    /// Source record kind.
    pub kind: EmbeddingInputKind,
    /// Text that should be embedded by a later deterministic fixture pass.
    pub text: String,
    /// Ledger facts referenced by this input.
    #[serde(default)]
    pub fact_ids: Vec<String>,
    /// Probes referenced by this input.
    #[serde(default)]
    pub probe_ids: Vec<String>,
}

impl EmbeddingInput {
    /// Validates field-level invariants for one embedding input.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty("embedding input input_id", &self.input_id)?;
        ensure_non_empty("embedding input text", &self.text)?;
        for fact_id in &self.fact_ids {
            ensure_non_empty("embedding input fact_id", fact_id)?;
        }
        for probe_id in &self.probe_ids {
            ensure_non_empty("embedding input probe_id", probe_id)?;
        }
        Ok(())
    }
}

/// Source kind for a generated embedding input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingInputKind {
    /// Input rendered from a ledger fact.
    Fact,
    /// Input rendered from a probe query.
    Probe,
}

/// Generates a deterministic memory-evaluation corpus with a transcript style.
pub fn generate_memory_eval_corpus(
    profile: CorpusProfile,
    seeds: Vec<u64>,
    transcript_style: TranscriptStyle,
) -> Result<GeneratedMemoryEvalCorpus> {
    model::generate_memory_eval_corpus(profile, seeds, transcript_style)
}
pub async fn write_memory_eval_corpus(
    output_dir: &Path,
    corpus: &GeneratedMemoryEvalCorpus,
) -> Result<()> {
    corpus.validate()?;
    tokio::fs::create_dir_all(output_dir)
        .await
        .map_err(|source| io_error(output_dir, source))?;
    write_manifest_json(&output_dir.join("manifest.json"), &corpus.manifest).await?;
    write_ledger_jsonl(&output_dir.join("ledger.jsonl"), &corpus.ledger).await?;
    write_sessions_jsonl(&output_dir.join("sessions.jsonl"), &corpus.sessions).await?;
    write_probes_jsonl(
        &output_dir.join("probes.jsonl"),
        &corpus.probes,
        &corpus.ledger,
    )
    .await?;
    write_embedding_inputs_jsonl(
        &output_dir.join("embedding_inputs.jsonl"),
        &corpus.embedding_inputs,
        &corpus.ledger,
        &corpus.probes,
    )
    .await
}

/// Reads and validates `embedding_inputs.jsonl`.
pub async fn read_embedding_inputs_jsonl(
    path: &Path,
    facts: &[LedgerFact],
    probes: &[Probe],
) -> Result<Vec<EmbeddingInput>> {
    let inputs = read_jsonl(path).await?;
    validate_embedding_inputs(&inputs, facts, probes)?;
    Ok(inputs)
}

/// Writes and validates `embedding_inputs.jsonl`.
pub async fn write_embedding_inputs_jsonl(
    path: &Path,
    inputs: &[EmbeddingInput],
    facts: &[LedgerFact],
    probes: &[Probe],
) -> Result<()> {
    validate_embedding_inputs(inputs, facts, probes)?;
    write_jsonl(path, inputs).await
}

#[cfg(test)]
mod tests;
