//! Strict manifest, byte hashing, and JSONL loading for execution evaluation corpora.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use moa_eval_core::{Error, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use super::{
    contract::{ExecutionContractCase, validate_contract_case},
    live::{ExecutionTaskQualityCase, validate_task_quality_corpus},
    routing::{ExecutionRoutingCase, ExecutionRoutingLabel, validate_routing_case},
};
use moa_core::types::execution_planning::{ExecutionRouteClassifierOutcome, ExecutionStrategy};

/// Current schema version for the execution corpus manifest.
pub const EXECUTION_CORPUS_MANIFEST_SCHEMA_VERSION: u8 = 1;
const ROUTING_CASE_COUNT: usize = 328;
const CONTRACT_CASE_COUNT_MIN: usize = 80;

/// One checked-in corpus file declared by the execution manifest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCorpusFile {
    /// Relative path from the manifest directory.
    pub path: PathBuf,
    /// Exact lowercase SHA-256 of the file bytes.
    pub sha256: String,
    /// Exact expected record count.
    pub count: u64,
}

/// Strict manifest for the routing and recorded-contract corpora.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCorpusManifest {
    /// Manifest schema version, fixed at `1`.
    pub schema_version: u8,
    /// Routing corpus file contract.
    pub routing: ExecutionCorpusFile,
    /// Recorded contract corpus file contract.
    pub contract: ExecutionCorpusFile,
    /// Sampled live task-quality corpus file contract.
    pub task_quality: ExecutionCorpusFile,
}

/// Fully loaded and validated execution corpus package.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionCorpus {
    /// Strict checked-in manifest.
    pub manifest: ExecutionCorpusManifest,
    /// Exactly 328 labeled routing cases.
    pub routing_cases: Vec<ExecutionRoutingCase>,
    /// At least 80 strict recorded contract cases.
    pub contract_cases: Vec<ExecutionContractCase>,
    /// Exactly 20 sampled live task-quality cases.
    pub task_quality_cases: Vec<ExecutionTaskQualityCase>,
}

/// Loads, byte-verifies, parses, and validates one execution corpus package.
pub async fn load_execution_corpus(manifest_path: &Path) -> Result<ExecutionCorpus> {
    let manifest_bytes = tokio::fs::read(manifest_path)
        .await
        .map_err(|source| Error::Io {
            path: manifest_path.to_path_buf(),
            source,
        })?;
    let manifest_text = std::str::from_utf8(&manifest_bytes).map_err(|error| {
        invalid_config(format!(
            "execution corpus manifest {} is not UTF-8: {error}",
            manifest_path.display()
        ))
    })?;
    let manifest = toml::from_str::<ExecutionCorpusManifest>(manifest_text).map_err(|source| {
        Error::ParseToml {
            path: manifest_path.to_path_buf(),
            source,
        }
    })?;
    validate_manifest(&manifest)?;
    let root = manifest_path.parent().ok_or_else(|| {
        invalid_config("execution corpus manifest has no parent directory".to_string())
    })?;
    let routing_path = root.join(&manifest.routing.path);
    let contract_path = root.join(&manifest.contract.path);
    let task_quality_path = root.join(&manifest.task_quality.path);
    let routing_bytes = read_verified(&routing_path, &manifest.routing).await?;
    let contract_bytes = read_verified(&contract_path, &manifest.contract).await?;
    let task_quality_bytes = read_verified(&task_quality_path, &manifest.task_quality).await?;
    let routing_cases = parse_jsonl::<ExecutionRoutingCase>(&routing_path, &routing_bytes)?;
    let contract_cases = parse_jsonl::<ExecutionContractCase>(&contract_path, &contract_bytes)?;
    let task_quality_cases =
        parse_jsonl::<ExecutionTaskQualityCase>(&task_quality_path, &task_quality_bytes)?;
    validate_routing_corpus(&routing_cases, &manifest.routing)?;
    validate_contract_corpus(&contract_cases, &manifest.contract)?;
    validate_count(
        "task-quality",
        task_quality_cases.len(),
        manifest.task_quality.count,
    )?;
    validate_task_quality_corpus(&task_quality_cases)?;
    Ok(ExecutionCorpus {
        manifest,
        routing_cases,
        contract_cases,
        task_quality_cases,
    })
}

fn validate_manifest(manifest: &ExecutionCorpusManifest) -> Result<()> {
    if manifest.schema_version != EXECUTION_CORPUS_MANIFEST_SCHEMA_VERSION {
        return Err(invalid_config(format!(
            "execution corpus manifest version {} is unsupported",
            manifest.schema_version
        )));
    }
    for (name, file) in [
        ("routing", &manifest.routing),
        ("contract", &manifest.contract),
        ("task-quality", &manifest.task_quality),
    ] {
        if !safe_relative_path(&file.path) {
            return Err(invalid_config(format!(
                "execution {name} corpus path must be a safe relative path"
            )));
        }
        validate_sha256(&format!("execution {name} corpus"), &file.sha256)?;
    }
    Ok(())
}

async fn read_verified(path: &Path, file: &ExecutionCorpusFile) -> Result<Vec<u8>> {
    let bytes = tokio::fs::read(path).await.map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let observed = format!("{:x}", Sha256::digest(&bytes));
    if observed != file.sha256 {
        return Err(invalid_config(format!(
            "execution corpus {} SHA-256 mismatch: expected {}, got {observed}",
            path.display(),
            file.sha256
        )));
    }
    Ok(bytes)
}

fn parse_jsonl<T: DeserializeOwned>(path: &Path, bytes: &[u8]) -> Result<Vec<T>> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        invalid_config(format!(
            "execution corpus {} is not UTF-8: {error}",
            path.display()
        ))
    })?;
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            return Err(invalid_config(format!(
                "execution corpus {} contains a blank JSONL record at line {}",
                path.display(),
                index + 1
            )));
        }
        let record = serde_json::from_str::<T>(line).map_err(|source| Error::ParseJson {
            path: path.to_path_buf(),
            source,
        })?;
        records.push(record);
    }
    Ok(records)
}

fn validate_routing_corpus(
    cases: &[ExecutionRoutingCase],
    file: &ExecutionCorpusFile,
) -> Result<()> {
    validate_count("routing", cases.len(), file.count)?;
    if cases.len() != ROUTING_CASE_COUNT {
        return Err(invalid_config(format!(
            "execution routing corpus must contain exactly {ROUTING_CASE_COUNT} cases"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut labels = BTreeMap::<&str, usize>::new();
    let mut strategies = BTreeMap::<&str, usize>::new();
    let mut near_boundary = 0_usize;
    let mut durable_upgrade = 0_usize;
    let mut motivating_case = false;
    let mut classifier_fallback = 0_usize;
    for case in cases {
        validate_routing_case(case)?;
        if !ids.insert(case.case_id.as_str()) {
            return Err(invalid_config(format!(
                "duplicate execution routing case ID `{}`",
                case.case_id
            )));
        }
        let label = match case.expected_label {
            ExecutionRoutingLabel::Respond => "respond",
            ExecutionRoutingLabel::Execute => "execute",
            ExecutionRoutingLabel::NeedsInput => "needs_input",
        };
        *labels.entry(label).or_default() += 1;
        if let Some(strategy) = case.expected_strategy {
            let label = match strategy {
                ExecutionStrategy::Inline => "inline",
                ExecutionStrategy::Durable => "durable",
            };
            *strategies.entry(label).or_default() += 1;
        }
        near_boundary += usize::from(case.near_boundary);
        durable_upgrade += usize::from(case.durable_upgrade.is_some());
        classifier_fallback += usize::from(!matches!(
            case.expected_classifier_outcome,
            ExecutionRouteClassifierOutcome::Accepted | ExecutionRouteClassifierOutcome::NotCalled
        ));
        motivating_case |= case.expected_label == ExecutionRoutingLabel::Execute
            && case.expected_strategy == Some(ExecutionStrategy::Durable)
            && case
                .tags
                .iter()
                .any(|tag| tag == "sp500-ai-five-year-screen");
    }
    let required = [("respond", 60_usize), ("execute", 248), ("needs_input", 20)];
    for (label, minimum) in required {
        if labels.get(label).copied().unwrap_or_default() != minimum {
            return Err(invalid_config(format!(
                "execution routing corpus requires exactly {minimum} `{label}` cases"
            )));
        }
    }
    for (strategy, expected) in [("inline", 144_usize), ("durable", 104_usize)] {
        if strategies.get(strategy).copied().unwrap_or_default() != expected {
            return Err(invalid_config(format!(
                "execution routing corpus requires exactly {expected} `{strategy}` strategy cases"
            )));
        }
    }
    if near_boundary < 80 || durable_upgrade < 40 || classifier_fallback < 24 || !motivating_case {
        return Err(invalid_config(
            "execution routing corpus is missing near-boundary, Durable-upgrade, fallback, or motivating coverage".to_string(),
        ));
    }
    Ok(())
}

fn validate_contract_corpus(
    cases: &[ExecutionContractCase],
    file: &ExecutionCorpusFile,
) -> Result<()> {
    validate_count("contract", cases.len(), file.count)?;
    if cases.len() < CONTRACT_CASE_COUNT_MIN {
        return Err(invalid_config(format!(
            "execution contract corpus must contain at least {CONTRACT_CASE_COUNT_MIN} cases"
        )));
    }
    let mut ids = BTreeSet::new();
    let mut tags = BTreeSet::new();
    for case in cases {
        validate_contract_case(case)?;
        if !ids.insert(case.case_id.as_str()) {
            return Err(invalid_config(format!(
                "duplicate execution contract case ID `{}`",
                case.case_id
            )));
        }
        tags.extend(case.tags.iter().map(String::as_str));
    }
    for required in [
        "bulk-universe",
        "time-range",
        "evidence-citations",
        "exclusions",
        "deliverables",
        "definitions",
        "multi-constraint",
    ] {
        if !tags.contains(required) {
            return Err(invalid_config(format!(
                "execution contract corpus is missing required `{required}` coverage"
            )));
        }
    }
    Ok(())
}

fn validate_count(name: &str, observed: usize, expected: u64) -> Result<()> {
    let observed = u64::try_from(observed)
        .map_err(|_| invalid_config(format!("execution {name} corpus count exceeds u64")))?;
    if observed != expected {
        return Err(invalid_config(format!(
            "execution {name} corpus count mismatch: manifest says {expected}, loaded {observed}"
        )));
    }
    Ok(())
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn validate_sha256(name: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || value
            .bytes()
            .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid_config(format!(
            "{name} hash must be 64 lowercase hexadecimal characters"
        )));
    }
    Ok(())
}

fn invalid_config(message: String) -> Error {
    Error::InvalidConfig(message)
}
