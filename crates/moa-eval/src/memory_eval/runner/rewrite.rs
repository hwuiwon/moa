//! Query rewrite fixture accounting for memory retrieval eval probes.

use std::collections::BTreeMap;

use super::QueryRewritePolicy;
use super::report::QueryRewriteClassMetrics;
use crate::memory_eval::{Probe, ProbeType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeRewriteDecision {
    Original,
    Rewritten,
}

#[derive(Debug, Clone)]
pub(super) struct QueryRewriteSummary {
    pub(super) policy: QueryRewritePolicy,
    pub(super) call_count: usize,
    pub(super) skip_count: usize,
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    pub(super) by_class: BTreeMap<String, QueryRewriteClassMetrics>,
}

impl QueryRewriteSummary {
    pub(super) fn empty(policy: QueryRewritePolicy) -> Self {
        Self {
            policy,
            call_count: 0,
            skip_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            by_class: BTreeMap::new(),
        }
    }

    fn total_count(&self) -> usize {
        self.call_count + self.skip_count
    }

    pub(super) fn call_rate(&self) -> f64 {
        let total = self.total_count();
        if total == 0 {
            0.0
        } else {
            self.call_count as f64 / total as f64
        }
    }
}

pub(super) struct QueryRewriteAccounting {
    summary: QueryRewriteSummary,
}

impl QueryRewriteAccounting {
    pub(super) fn new(policy: QueryRewritePolicy) -> Self {
        Self {
            summary: QueryRewriteSummary::empty(policy),
        }
    }

    pub(super) fn record(&mut self, probe: &Probe) -> ProbeRewriteDecision {
        let class = query_class_for_probe(probe);
        let rewritten = match self.summary.policy {
            QueryRewritePolicy::Off => false,
            QueryRewritePolicy::Always => true,
            QueryRewritePolicy::Gated => probe
                .expected_rewrite
                .unwrap_or_else(|| gated_rewrite_for_class(&class)),
        };
        let class_entry = self.summary.by_class.entry(class).or_default();
        class_entry.total_count += 1;
        if rewritten {
            self.summary.call_count += 1;
            class_entry.call_count += 1;
            self.summary.input_tokens += approximate_tokens(&probe.query) as u64;
            self.summary.output_tokens +=
                approximate_tokens(&deterministic_rewrite_query(probe)) as u64;
            ProbeRewriteDecision::Rewritten
        } else {
            self.summary.skip_count += 1;
            class_entry.skip_count += 1;
            ProbeRewriteDecision::Original
        }
    }

    pub(super) fn summary(mut self) -> QueryRewriteSummary {
        for metrics in self.summary.by_class.values_mut() {
            metrics.call_rate = if metrics.total_count == 0 {
                0.0
            } else {
                metrics.call_count as f64 / metrics.total_count as f64
            };
        }
        self.summary
    }
}

pub(super) fn probe_for_rewrite_policy(probe: &Probe, decision: ProbeRewriteDecision) -> Probe {
    if decision == ProbeRewriteDecision::Original {
        return probe.clone();
    }
    let mut rewritten = probe.clone();
    rewritten.query = deterministic_rewrite_query(probe);
    rewritten
}

fn query_class_for_probe(probe: &Probe) -> String {
    if let Some(query_class) = probe.query_class.as_deref() {
        return query_class.to_string();
    }
    if query_has_exact_anchor(&probe.query) {
        return "exact_identifier".to_string();
    }
    match probe.probe_type {
        ProbeType::MultiHop => "multi_hop",
        ProbeType::PreferenceApplication => "vector_first",
        ProbeType::TemporalAsOf => "explicit_temporal",
        ProbeType::LatestValueAfterUpdate => "vague_followup",
        _ => "explicit",
    }
    .to_string()
}

fn gated_rewrite_for_class(class: &str) -> bool {
    matches!(
        class,
        "coreference" | "vague_followup" | "vector_first" | "multi_hop"
    )
}

fn deterministic_rewrite_query(probe: &Probe) -> String {
    if let Some(rewrite_query) = probe.rewrite_query.as_ref() {
        return rewrite_query.clone();
    }
    match query_class_for_probe(probe).as_str() {
        "vague_followup" => format!("Latest active memory for: {}", probe.query),
        "vector_first" => format!(
            "Semantic memory search for user/tenant context: {}",
            probe.query
        ),
        "multi_hop" => format!("Graph relationship retrieval query: {}", probe.query),
        _ => probe.query.clone(),
    }
}

fn query_has_exact_anchor(query: &str) -> bool {
    query.contains("://")
        || query.contains('/')
        || query.contains('"')
        || query.split_whitespace().any(|token| {
            let token = token.trim_matches(|ch: char| ch.is_ascii_punctuation());
            token.contains('.')
                || token
                    .strip_prefix('#')
                    .is_some_and(|rest| rest.chars().all(|ch| ch.is_ascii_digit()))
        })
}

fn approximate_tokens(text: &str) -> usize {
    text.split_whitespace().count()
}
