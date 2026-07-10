//! Model-aware per-stage token and cost accounting.

use serde::{Deserialize, Serialize};

use super::answer::ExternalMemoryMode;
use super::{ExternalMemoryError, Result};

/// Paid or measured stage in an external-memory run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageName {
    /// Fact extraction during formation.
    FormationExtraction,
    /// Entity merge verification during formation.
    FormationMerge,
    /// Formation or query embedding.
    Embedding,
    /// Memory retrieval and rendering.
    Retrieval,
    /// Generated answer reader.
    Reader,
    /// Dataset-independent absolute answer judge.
    Judge,
}

/// Whether normalized token usage is forecast or provider-reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProvenance {
    /// Estimated before a paid stage begins.
    Estimated,
    /// Normalized from the provider response.
    Actual,
}

/// Provider-neutral normalized token counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedUsage {
    /// Standard uncached input tokens.
    pub input_tokens_uncached: usize,
    /// Input tokens written to prompt cache.
    pub input_tokens_cache_write: usize,
    /// Input tokens read from prompt cache.
    pub input_tokens_cache_read: usize,
    /// Output tokens.
    pub output_tokens: usize,
    /// Estimate or actual provenance.
    pub provenance: UsageProvenance,
}

/// Immutable model pricing used to calculate one stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingSnapshotV1 {
    /// Exact provider/model selector.
    pub model: String,
    /// Effective pricing date.
    pub effective_date: String,
    /// Uncached input price per million tokens.
    pub input_per_million_usd: f64,
    /// Output price per million tokens.
    pub output_per_million_usd: f64,
    /// Cache-read price per million tokens.
    pub cache_read_per_million_usd: f64,
    /// Cache-write price per million tokens.
    pub cache_write_per_million_usd: f64,
}

impl PricingSnapshotV1 {
    fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty() || self.effective_date.trim().is_empty() {
            return Err(ExternalMemoryError::InvalidConfig(
                "pricing model and effective date are required".to_string(),
            ));
        }
        for rate in [
            self.input_per_million_usd,
            self.output_per_million_usd,
            self.cache_read_per_million_usd,
            self.cache_write_per_million_usd,
        ] {
            if !rate.is_finite() || rate < 0.0 {
                return Err(ExternalMemoryError::InvalidConfig(
                    "pricing rates must be finite and non-negative".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Calculates USD cost for normalized usage.
    #[must_use]
    pub fn cost_usd(&self, usage: &NormalizedUsage) -> f64 {
        let million = 1_000_000.0;
        (usage.input_tokens_uncached as f64 * self.input_per_million_usd
            + usage.input_tokens_cache_write as f64 * self.cache_write_per_million_usd
            + usage.input_tokens_cache_read as f64 * self.cache_read_per_million_usd
            + usage.output_tokens as f64 * self.output_per_million_usd)
            / million
    }
}

/// Forecast and actual accounting for one model-backed stage call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageCostRecord {
    /// Attributed stage.
    pub stage: StageName,
    /// Benchmark mode, or null for formation and embedding.
    pub mode: Option<ExternalMemoryMode>,
    /// Model/date-specific pricing.
    pub pricing: PricingSnapshotV1,
    /// Pre-call forecast usage.
    pub estimated_usage: NormalizedUsage,
    /// Forecast cost.
    pub estimated_cost_usd: f64,
    /// Provider-reported usage after the response.
    pub actual_usage: Option<NormalizedUsage>,
    /// Provider-reported cost after the response.
    pub actual_cost_usd: Option<f64>,
}

/// Cumulative paid-run budget ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetLedger {
    budget_usd: f64,
    records: Vec<StageCostRecord>,
}

impl BudgetLedger {
    /// Creates a ledger with a positive finite cumulative ceiling.
    pub fn new(budget_usd: f64) -> Result<Self> {
        if !budget_usd.is_finite() || budget_usd <= 0.0 {
            return Err(ExternalMemoryError::InvalidConfig(
                "budget-usd must be positive and finite".to_string(),
            ));
        }
        Ok(Self {
            budget_usd,
            records: Vec::new(),
        })
    }

    /// Forecasts a paid call before provider construction or execution.
    pub fn forecast(
        &mut self,
        stage: StageName,
        mode: Option<ExternalMemoryMode>,
        pricing: PricingSnapshotV1,
        usage: NormalizedUsage,
    ) -> Result<usize> {
        pricing.validate()?;
        let mode_required = matches!(
            stage,
            StageName::Retrieval | StageName::Reader | StageName::Judge
        );
        if mode_required != mode.is_some() {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "stage {stage:?} has invalid benchmark mode attribution"
            )));
        }
        if usage.provenance != UsageProvenance::Estimated {
            return Err(ExternalMemoryError::InvalidConfig(
                "stage forecast usage must be estimated".to_string(),
            ));
        }
        let estimated_cost_usd = pricing.cost_usd(&usage);
        let projected = self.committed_cost_usd() + estimated_cost_usd;
        if projected > self.budget_usd {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "stage {stage:?} forecast ${projected:.6} exceeds budget ${:.6}",
                self.budget_usd
            )));
        }
        let id = self.records.len();
        self.records.push(StageCostRecord {
            stage,
            mode,
            pricing,
            estimated_usage: usage,
            estimated_cost_usd,
            actual_usage: None,
            actual_cost_usd: None,
        });
        Ok(id)
    }

    /// Records provider-normalized usage and rechecks the cumulative budget.
    pub fn record_actual(&mut self, record_id: usize, usage: NormalizedUsage) -> Result<()> {
        if usage.provenance != UsageProvenance::Actual {
            return Err(ExternalMemoryError::InvalidConfig(
                "completed stage usage must be actual".to_string(),
            ));
        }
        let Some(record) = self.records.get_mut(record_id) else {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "unknown stage accounting record {record_id}"
            )));
        };
        let actual_cost = record.pricing.cost_usd(&usage);
        record.actual_usage = Some(usage);
        record.actual_cost_usd = Some(actual_cost);
        if self.committed_cost_usd() > self.budget_usd {
            return Err(ExternalMemoryError::InvalidConfig(format!(
                "actual provider usage exceeds budget ${:.6}",
                self.budget_usd
            )));
        }
        Ok(())
    }

    /// Returns all per-stage accounting records.
    #[must_use]
    pub fn records(&self) -> &[StageCostRecord] {
        &self.records
    }

    /// Returns actual cost where available and forecast cost otherwise.
    #[must_use]
    pub fn committed_cost_usd(&self) -> f64 {
        self.records
            .iter()
            .map(|record| record.actual_cost_usd.unwrap_or(record.estimated_cost_usd))
            .sum()
    }

    /// Returns the cumulative configured ceiling.
    #[must_use]
    pub fn ceiling_usd(&self) -> f64 {
        self.budget_usd
    }

    /// Returns the sum of all pre-call forecasts.
    #[must_use]
    pub fn estimated_committed_cost_usd(&self) -> f64 {
        self.records
            .iter()
            .map(|record| record.estimated_cost_usd)
            .sum()
    }
}
