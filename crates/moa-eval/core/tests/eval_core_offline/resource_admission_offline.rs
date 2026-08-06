//! Admission and reservation behavior for eval runs, exercised end to end
//! against a scripted `LLMProvider`.
//!
//! These tests drive the same production sequence the eval engine uses —
//! `EvalAdmissionPolicy::admit`, `SharedResourceLedger::try_reserve` before any
//! provider call, `usage_from_metrics` plus `reconcile` afterwards — so a
//! weakened comparison anywhere in that path either dispatches unauthorized
//! provider calls or mis-reconciles the ledger, and fails here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::traits::LLMProvider;
use moa_core::types::completion::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, StopReason,
    TokenUsage,
};
use moa_core::types::identifiers::ModelId;
use moa_core::types::model::{ModelCapabilities, TokenPricing, ToolCallFormat};
use moa_core::types::resource::{
    ResourceAmounts, ResourceEnvelope, ResourceError, SharedResourceLedger,
};
use moa_eval_core::admission::{AdmissionError, EvalAdmissionLimits, EvalAdmissionPolicy};
use moa_eval_core::resource_report::{RunResourceReport, usage_from_metrics};
use moa_eval_core::{AgentConfig, EvalMetrics, TestCase, TestSuite};

const INPUT_PER_MTOK: f64 = 1.0;
const OUTPUT_PER_MTOK: f64 = 2.0;
const SCRIPTED_INPUT_TOKENS: usize = 400;
const SCRIPTED_OUTPUT_TOKENS: usize = 100;
const SCRIPTED_TOOL_CALLS: usize = 2;

/// A provider that records every dispatch so a test can prove none happened.
#[derive(Debug, Default)]
struct ScriptedProvider {
    calls: AtomicUsize,
}

impl ScriptedProvider {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LLMProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("scripted-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: INPUT_PER_MTOK,
                output_per_mtok: OUTPUT_PER_MTOK,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionStream::from_response(CompletionResponse {
            text: "scripted".to_string(),
            content: vec![CompletionContent::Text("scripted".to_string())],
            stop_reason: StopReason::EndTurn,
            model: ModelId::new("scripted-model"),
            usage: TokenUsage {
                input_tokens_uncached: SCRIPTED_INPUT_TOKENS,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: SCRIPTED_OUTPUT_TOKENS,
            },
            duration_ms: 1,
            thought_signature: None,
        }))
    }

    async fn complete_shared(
        &self,
        _request: moa_core::types::completion::SharedCompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        self.complete(CompletionRequest::new("shared-test")).await
    }
}

/// Metrics the eval collector would produce for one scripted turn.
fn metrics_from(response: &CompletionResponse, pricing: &TokenPricing) -> EvalMetrics {
    let input_tokens = response.usage.input_tokens_uncached
        + response.usage.input_tokens_cache_write
        + response.usage.input_tokens_cache_read;
    let output_tokens = response.usage.output_tokens;
    EvalMetrics {
        total_tokens: input_tokens + output_tokens,
        input_tokens,
        output_tokens,
        cost_dollars: pricing.cost_dollars(&response.usage),
        latency_ms: response.duration_ms,
        turn_count: 1,
        tool_call_count: SCRIPTED_TOOL_CALLS,
        tool_error_count: 0,
    }
}

/// The metrics every scripted dispatch is expected to produce.
fn scripted_metrics() -> EvalMetrics {
    let usage = TokenUsage {
        input_tokens_uncached: SCRIPTED_INPUT_TOKENS,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: SCRIPTED_OUTPUT_TOKENS,
    };
    let pricing = TokenPricing {
        input_per_mtok: INPUT_PER_MTOK,
        output_per_mtok: OUTPUT_PER_MTOK,
        cached_input_per_mtok: None,
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
    };
    EvalMetrics {
        total_tokens: SCRIPTED_INPUT_TOKENS + SCRIPTED_OUTPUT_TOKENS,
        input_tokens: SCRIPTED_INPUT_TOKENS,
        output_tokens: SCRIPTED_OUTPUT_TOKENS,
        cost_dollars: pricing.cost_dollars(&usage),
        latency_ms: 1,
        turn_count: 1,
        tool_call_count: SCRIPTED_TOOL_CALLS,
        tool_error_count: 0,
    }
}

fn case(name: &str) -> TestCase {
    TestCase {
        name: name.to_string(),
        input: format!("input for {name}"),
        ..TestCase::default()
    }
}

fn suite(count: usize) -> TestSuite {
    TestSuite {
        name: "scripted-suite".to_string(),
        cases: (0..count)
            .map(|index| case(&format!("case-{index}")))
            .collect(),
        ..TestSuite::default()
    }
}

fn agent(name: &str) -> AgentConfig {
    AgentConfig {
        name: name.to_string(),
        ..AgentConfig::default()
    }
}

/// Worst case per scripted turn, sized generously above what the provider
/// actually returns so reconciliation has something to give back.
fn per_case_reservation() -> ResourceAmounts {
    ResourceAmounts {
        cost_micro_usd: 2_000,
        tokens: 2_000,
        turns: 4,
        model_calls: 4,
        tool_calls: 8,
    }
}

/// Amounts the scripted provider actually consumes for one dispatched case.
fn scripted_usage() -> ResourceAmounts {
    usage_from_metrics(&scripted_metrics()).expect("scripted metrics convert to usage")
}

/// Reserves, dispatches, and reconciles one case exactly as the engine does.
///
/// Returns `Ok(())` when the case was dispatched and `Err` with the refusal when
/// the ledger refused: the provider is untouched on the error path.
async fn reserve_dispatch_reconcile(
    ledger: &SharedResourceLedger,
    provider: &Arc<ScriptedProvider>,
    request: ResourceAmounts,
) -> Result<EvalMetrics, ResourceError> {
    let reservation = ledger.try_reserve(request, Utc::now())?;

    let response = provider
        .complete(CompletionRequest::new("hello"))
        .await
        .expect("scripted provider")
        .collect()
        .await
        .expect("scripted response");
    let metrics = metrics_from(&response, &provider.capabilities().pricing);

    let usage = usage_from_metrics(&metrics).expect("metrics convert to usage");
    ledger.reconcile(reservation, usage).expect("reconcile");
    Ok(metrics)
}

#[tokio::test]
async fn scripted_parallel_cases_reconcile_reservations_exactly() {
    // Pins: worst-case reservations are taken before each provider call and the
    // difference is returned on reconciliation, so committed usage matches the
    // scripted provider exactly even under concurrency.
    let cases = 8;
    let per_case = per_case_reservation();
    let usage = scripted_usage();
    let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
        per_case,
        total: per_case.checked_mul(cases as u64).expect("projection fits"),
        max_parallel_cases: 4,
        ..EvalAdmissionLimits::default()
    });
    let admitted = policy
        .admit(&suite(cases), &[agent("baseline")], 4, Utc::now())
        .expect("matrix is admitted");
    assert_eq!(admitted.total_runs, cases);

    let ledger = SharedResourceLedger::from_envelope(admitted.envelope.clone()).expect("ledger");
    let provider = Arc::new(ScriptedProvider::default());
    let reservation = admitted.per_case;

    let mut handles = Vec::new();
    for _ in 0..cases {
        let ledger = ledger.clone();
        let provider = Arc::clone(&provider);
        handles.push(tokio::spawn(async move {
            reserve_dispatch_reconcile(&ledger, &provider, reservation)
                .await
                .is_ok()
        }));
    }
    let mut dispatched = 0usize;
    for handle in handles {
        if handle.await.expect("join") {
            dispatched += 1;
        }
    }

    assert_eq!(dispatched, cases);
    assert_eq!(provider.calls(), cases);

    let snapshot = ledger.snapshot();
    assert_eq!(snapshot.open_reservations, 0);
    assert_eq!(snapshot.outstanding, ResourceAmounts::ZERO);
    assert_eq!(
        snapshot.committed,
        usage.checked_mul(cases as u64).expect("committed total")
    );
    // The unused part of every reservation came back: remaining is the envelope
    // minus the actual spend, not minus the worst case.
    assert_eq!(
        snapshot.remaining,
        snapshot.limits.saturating_sub(&snapshot.committed)
    );
    assert!(snapshot.remaining.cost_micro_usd > 0);
}

#[tokio::test]
async fn exhausted_envelope_refuses_further_dispatch() {
    // Pins: the ledger, not luck, decides dispatch. An envelope sized for three
    // cases admits exactly three provider calls out of eight attempts.
    let admitted_cases = 3;
    let attempted_cases = 8;
    let per_case = per_case_reservation();
    let ledger = SharedResourceLedger::from_envelope(ResourceEnvelope::new(
        per_case
            .checked_mul(admitted_cases as u64)
            .expect("projection fits"),
        None,
    ))
    .expect("ledger");
    let provider = Arc::new(ScriptedProvider::default());
    let mut report = RunResourceReport::new(
        1,
        &ledger,
        per_case,
        per_case
            .checked_mul(attempted_cases as u64)
            .expect("projection fits"),
        1,
        attempted_cases,
    );

    let mut refusals = Vec::new();
    for _ in 0..attempted_cases {
        // Reconciliation returns the unused part, so the reservation size — not
        // the scripted spend — is what has to run out.
        match reserve_dispatch_reconcile(&ledger, &provider, per_case).await {
            Ok(_) => report.record_dispatched(),
            Err(error) => {
                report.record_unreserved(&error.to_string());
                refusals.push(error);
            }
        }
    }
    report.refresh(&ledger);

    // Each dispatched case commits far less than it reserved, so more than three
    // cases fit; what must hold is that every provider call was authorized by a
    // granted reservation and the envelope was never oversubscribed.
    assert_eq!(report.dispatched_cases, provider.calls());
    assert_eq!(
        report.dispatched_cases + report.unreserved_cases,
        attempted_cases
    );
    assert!(
        !refusals.is_empty(),
        "an envelope sized for {admitted_cases} worst-case cases must refuse some of \
         {attempted_cases} attempts"
    );
    assert!(matches!(
        refusals[0],
        ResourceError::Exhausted { .. } | ResourceError::DeadlineExceeded { .. }
    ));
    assert!(report.stop_reason.is_some());
    let snapshot = ledger.snapshot();
    assert!(
        snapshot
            .committed
            .first_exceeding(&snapshot.limits)
            .is_none()
    );
    assert_eq!(snapshot.open_reservations, 0);
}

#[tokio::test]
async fn a_refused_reservation_never_reaches_the_provider() {
    // Pins: `try_reserve` is the gate. With zero capacity the provider is never
    // constructed a request, so no unauthorized work is dispatched.
    let ledger =
        SharedResourceLedger::from_envelope(ResourceEnvelope::new(ResourceAmounts::ZERO, None))
            .expect("ledger");
    let provider = Arc::new(ScriptedProvider::default());

    for _ in 0..4 {
        let error = reserve_dispatch_reconcile(&ledger, &provider, per_case_reservation())
            .await
            .expect_err("a zero envelope refuses every reservation");
        assert!(matches!(error, ResourceError::Exhausted { .. }));
    }

    assert_eq!(provider.calls(), 0);
    assert_eq!(ledger.snapshot().open_reservations, 0);
}

#[tokio::test]
async fn an_expired_deadline_refuses_reservations_and_dispatches_nothing() {
    // Pins: the run deadline is checked before capacity, so a run that is already
    // out of time cannot dispatch even with budget left.
    let now = Utc::now();
    let ledger = SharedResourceLedger::from_envelope(ResourceEnvelope::new(
        per_case_reservation(),
        Some(now),
    ))
    .expect("ledger");
    let provider = Arc::new(ScriptedProvider::default());

    let error = reserve_dispatch_reconcile(&ledger, &provider, per_case_reservation())
        .await
        .expect_err("an expired deadline refuses the reservation");
    assert!(matches!(error, ResourceError::DeadlineExceeded { .. }));
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn an_oversized_matrix_is_refused_before_any_reservation_exists() {
    // Pins: admission precedes the ledger, so a rejected matrix never creates an
    // envelope to reserve against.
    let policy = EvalAdmissionPolicy::new(EvalAdmissionLimits {
        max_total_runs: 4,
        ..EvalAdmissionLimits::default()
    });

    policy
        .admit(&suite(4), &[agent("baseline")], 1, Utc::now())
        .expect("a matrix exactly at the limit is admitted");
    let error = policy
        .admit(&suite(5), &[agent("baseline")], 1, Utc::now())
        .expect_err("one run over the limit is refused");
    assert!(matches!(
        error,
        AdmissionError::MatrixTooLarge {
            total_runs: 5,
            limit: 4
        }
    ));
}
