//! Stage 8: emits task-specific coordinator delegation planning hints.

use std::collections::HashMap;

use async_trait::async_trait;
use moa_core::{
    ContextMessage, ContextProcessor, Event, MessageRole, ProcessorOutput, Result, WorkingContext,
    estimate_text_tokens,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const MAX_PLAN_NODES: usize = 5;
const MIN_LIST_NODES: usize = 3;
/// Minimum ready nodes for a two-sided comparison plan (gated on an explicit two-sided signal).
const MIN_COMPARISON_NODES: usize = 2;

/// Context metadata key containing the deterministic delegation DAG candidate.
pub const DELEGATION_PLAN_METADATA_KEY: &str = "delegation_plan";

/// One conservative coordinator delegation plan candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationPlan {
    /// Stable reason label explaining why this task looks delegable.
    pub reason: String,
    /// Ready DAG nodes with no dependencies in the initial slice.
    pub nodes: Vec<DelegationPlanNode>,
}

/// One ready node in a coordinator-owned subtask DAG candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationPlanNode {
    /// Stable node identifier within this plan.
    pub id: String,
    /// Concise subtask title derived from the user request.
    pub title: String,
    /// Node ids this node depends on.
    pub depends_on: Vec<String>,
}

/// Detects high-confidence delegation opportunities and records a DAG candidate.
#[derive(Debug, Clone, Default)]
pub struct DelegationPlanningProcessor;

impl DelegationPlanningProcessor {
    /// Creates a delegation-planning processor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ContextProcessor for DelegationPlanningProcessor {
    fn name(&self) -> &str {
        "delegation_planning"
    }

    fn stage(&self) -> u8 {
        8
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        // On a synthesis turn (worker results already bundled for the active user message), the
        // coordinator is producing the final answer from `<worker_result_bundle>`. Re-emitting a
        // "call spawn_worker" hint here contradicts that guidance and wastes tokens every turn.
        if synthesis_turn_in_progress(ctx) {
            return Ok(ProcessorOutput::default());
        }
        let Some(request_text) = latest_user_request_text(ctx) else {
            return Ok(ProcessorOutput::default());
        };
        let Some(plan) = plan_delegation_for_request(request_text) else {
            return Ok(ProcessorOutput::default());
        };

        let rendered = render_plan_hint(&plan);
        ctx.append_message(ContextMessage::user(rendered.clone()));
        ctx.insert_metadata(DELEGATION_PLAN_METADATA_KEY, json!(&plan));

        Ok(ProcessorOutput {
            tokens_added: estimate_text_tokens(&rendered),
            items_included: plan.nodes.iter().map(|node| node.id.clone()).collect(),
            metadata: HashMap::from([(
                DELEGATION_PLAN_METADATA_KEY.to_string(),
                serde_json::to_value(&plan)?,
            )]),
            ..ProcessorOutput::default()
        })
    }
}

/// Builds a conservative ready-node delegation plan from one user request.
#[must_use]
pub fn plan_delegation_for_request(request: &str) -> Option<DelegationPlan> {
    let normalized = normalize_whitespace(request);
    let lower = normalized.to_ascii_lowercase();
    if normalized.is_empty() || is_non_execution_request(&lower) || !has_work_signal(&lower) {
        return None;
    }

    // Only a GENERIC, request-derived workstream list produces a plan — every node title comes
    // from the user's own words, never a hardcoded phrasing or canned title. The coordinator's
    // operating contract (see `pipeline::identity`) still instructs the model to decompose work
    // and decide delegation from the task itself; this deterministic hint is a conservative nudge
    // for the clearly-decomposable cases, not a substitute for that reasoning.
    let (reason, items) = {
        let list = explicit_work_items(&normalized, &lower, MIN_LIST_NODES);
        if list.len() >= MIN_LIST_NODES {
            ("explicit_multi_workstream_list", list)
        } else if is_two_sided_request(&lower) {
            // A two-sided task ("compare/summarize/categorize X and Y") is delegable at exactly
            // two ready nodes. Gate the lower threshold on an explicit two-sided signal so an
            // ordinary two-item phrase is not mistaken for parallel workstreams.
            let pair = explicit_work_items(&normalized, &lower, MIN_COMPARISON_NODES);
            if pair.len() >= MIN_COMPARISON_NODES {
                ("explicit_comparison", pair)
            } else {
                return None;
            }
        } else {
            return None;
        }
    };
    let nodes = items
        .into_iter()
        .take(MAX_PLAN_NODES)
        .enumerate()
        .map(|(index, title)| DelegationPlanNode {
            id: format!("node-{}", index + 1),
            title,
            depends_on: Vec::new(),
        })
        .collect::<Vec<_>>();

    Some(DelegationPlan {
        reason: reason.to_string(),
        nodes,
    })
}

/// Returns whether worker results have already been bundled (or a synthesis requested) after the
/// latest user message — i.e. this turn is synthesizing auto-delegated results, not starting
/// fresh work, so the spawn hint must not re-fire.
fn synthesis_turn_in_progress(ctx: &WorkingContext) -> bool {
    let mut saw_results = false;
    for record in ctx.recent_events().iter().rev() {
        match &record.event {
            Event::UserMessage { .. } | Event::QueuedMessage { .. } => return saw_results,
            Event::WorkerResultBundle { .. } | Event::WorkerResultSynthesisRequested { .. } => {
                saw_results = true;
            }
            _ => {}
        }
    }
    saw_results
}

fn latest_user_request_text(ctx: &WorkingContext) -> Option<&str> {
    if let Some(text) = ctx
        .recent_events()
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
    {
        return Some(text);
    }

    ctx.messages
        .iter()
        .rev()
        .find(|message| {
            message.role == MessageRole::User && !message.content.starts_with("<available_skills>")
        })
        .map(|message| message.content.as_str())
}

fn has_work_signal(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "a/b test",
            "audit",
            "board update",
            "categorize",
            "compare",
            "investigation",
            "launch readiness",
            "ops report",
            "plan",
            "readiness",
            "reconcile",
            "report",
            "review",
            "rollout",
            "summarize",
            "synthesize",
            "triage",
        ],
    )
}

fn is_non_execution_request(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "what do you think",
            "why do we need",
            "should we",
            "what's the status",
            "what is the status",
            "explain how",
            "how does delegation",
        ],
    )
}

fn explicit_work_items(normalized: &str, lower: &str, min_items: usize) -> Vec<String> {
    // Distinct extractors only. Redundant anchors were dropped after a mutation check: " for
    // storing " is subsumed by " for ", and the standalone verb anchors " compare "/" review "
    // fire for no covered request (the leading-"compare " prefix and colon/leading-list
    // extractors already recover them). " summarize " and " categorize " are KEPT: they are
    // the only extractors that isolate a two-sided "verb X and Y" tail, so removing them regresses
    // `plans_generic_two_sided_summary_and_comparison`. " reconcile " is KEPT: it is the only
    // extractor for a mid-sentence "need to reconcile X, Y, and Z" item list (live sweep S008
    // regressed to no delegation when it was dropped — pinned by
    // `plans_mid_sentence_reconcile_item_list`).
    let candidates = [
        leading_list_segment(normalized),
        segment_after_colon(normalized),
        segment_after_anchor(normalized, lower, " from "),
        segment_after_anchor(normalized, lower, " across "),
        segment_after_anchor(normalized, lower, " using "),
        segment_after_anchor(normalized, lower, " between "),
        segment_after_anchor(normalized, lower, " for "),
        segment_after_prefix(normalized, lower, "compare "),
        segment_after_anchor(normalized, lower, " summarize "),
        segment_after_anchor(normalized, lower, " categorize "),
        segment_after_anchor(normalized, lower, " reconcile "),
        segment_after_semicolon(normalized),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(|segment| split_work_items(&segment))
        .find(|items| items.len() >= min_items)
        .unwrap_or_default()
}

/// Whether the request explicitly frames two sides to compare/summarize/categorize.
///
/// Used to gate the lower two-node threshold so an ordinary two-item phrase (e.g. "email Alice
/// and Bob") is not treated as parallel delegation.
fn is_two_sided_request(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "compare",
            "comparison",
            "reconcile",
            "versus",
            " vs ",
            " vs.",
        ],
    ) || (contains_any(lower, &["summarize", "categorize"]) && lower.contains(" and "))
}

fn segment_after_colon(text: &str) -> Option<String> {
    text.split_once(':')
        .map(|(_, tail)| trim_segment_boundary(tail).to_string())
}

fn segment_after_semicolon(text: &str) -> Option<String> {
    text.split_once(';')
        .map(|(_, tail)| trim_segment_boundary(tail).to_string())
}

fn segment_after_anchor(text: &str, lower: &str, anchor: &str) -> Option<String> {
    let index = lower.find(anchor)?;
    let start = index + anchor.len();
    Some(trim_segment_boundary(&text[start..]).to_string())
}

fn segment_after_prefix(text: &str, lower: &str, prefix: &str) -> Option<String> {
    lower
        .starts_with(prefix)
        .then(|| trim_segment_boundary(&text[prefix.len()..]).to_string())
}

fn leading_list_segment(text: &str) -> Option<String> {
    let first_sentence = text
        .split(['.', '?', '!'])
        .next()
        .map(str::trim)
        .unwrap_or_default();
    if !first_sentence.contains(',') {
        return None;
    }
    let lower = first_sentence.to_ascii_lowercase();
    for marker in [" are ", " were ", " is ", " was "] {
        if let Some(index) = lower.find(marker) {
            let head = first_sentence[..index].trim();
            if head.contains(',') {
                return Some(head.to_string());
            }
        }
    }
    None
}

fn split_work_items(segment: &str) -> Vec<String> {
    let bounded = trim_trailing_context(segment);
    // Accept comma-, semicolon-, or conjunction-delimited lists: a two-sided "X and Y" has no
    // comma, and incident symptom lists often separate clauses with ";".
    if !bounded.contains(',')
        && !bounded.contains(';')
        && !bounded.contains(" and ")
        && !bounded.contains(" or ")
    {
        return Vec::new();
    }

    let list_text = bounded
        .replace(';', ",")
        .replace(" and ", ", ")
        .replace(" or ", ", ");
    let mut items = Vec::new();
    for raw in list_text.split(',') {
        let item = clean_item(raw);
        if !is_useful_work_item(&item) {
            continue;
        }
        if !items
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&item))
        {
            items.push(item);
        }
    }
    items
}

fn is_useful_work_item(item: &str) -> bool {
    if item.is_empty() || item.split_whitespace().count() > 7 {
        return false;
    }
    let lower = item.to_ascii_lowercase();
    if lower == "there"
        || lower.starts_with("there ")
        || lower.starts_with("no ")
        || lower.starts_with("no-")
    {
        return false;
    }
    // Imperative clauses ("give me neutral questions", "tell me …") are task instructions, not
    // parallel workstream titles — drop them so a "compare the assumptions and give me X" tail
    // does not become a bogus node.
    for imperative in [
        "give ", "get ", "tell ", "show ", "make ", "let ", "help ", "send ",
    ] {
        if lower.starts_with(imperative) {
            return false;
        }
    }
    true
}

fn trim_trailing_context(segment: &str) -> &str {
    let mut end = segment.len();
    for marker in [
        ".",
        "?",
        "!",
        " for ",
        " by ",
        " before ",
        " this month",
        " next week",
        " tomorrow",
        " today",
    ] {
        if let Some(index) = segment.to_ascii_lowercase().find(marker) {
            end = end.min(index);
        }
    }
    segment[..end].trim()
}

fn trim_segment_boundary(segment: &str) -> &str {
    segment
        .trim()
        .trim_matches(|character: char| matches!(character, '.' | '?' | '!' | ';'))
        .trim()
}

fn clean_item(raw: &str) -> String {
    let mut item = raw
        .trim()
        .trim_matches(|character: char| {
            matches!(
                character,
                '.' | '?' | '!' | ';' | ':' | '"' | '\'' | '(' | ')'
            )
        })
        .trim();
    for prefix in ["and ", "or ", "the ", "a ", "an "] {
        if let Some(stripped) = item.strip_prefix(prefix) {
            item = stripped.trim();
        }
    }
    item.to_string()
}

fn render_plan_hint(plan: &DelegationPlan) -> String {
    let mut rendered = String::from(
        "<delegation_plan_candidate>\n\
The current task appears decomposable. Candidate ready DAG nodes:\n",
    );
    for node in &plan.nodes {
        rendered.push_str(&format!(
            "- {} depends_on=[]: {}\n",
            node.id,
            escape_xml_text(&node.title)
        ));
    }
    rendered.push_str(
        "If this still matches the active user request and there is enough context, \
call spawn_worker for the ready nodes before final synthesis. Keep dependency \
context and any relevant skill steps inside each spawn_worker.task. If the \
ready nodes were already spawned in history, do not spawn duplicates; wait for \
or synthesize from the worker results.\n\
</delegation_plan_candidate>",
    );
    rendered
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn escape_xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        Channel, EventRecord, EventType, ModelCapabilities, ModelId, SessionId, SessionMeta,
        TenantId, TokenPricing, ToolCallFormat,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn plans_ready_nodes_for_realistic_board_update() {
        // Pins: users do not need to ask for delegation when a synthesis task names
        // independent workstreams.
        let plan = plan_delegation_for_request(
            "I need a board update from revenue, churn, cash runway, and hiring-plan notes. \
             Turn it into a concise exec narrative with risks and asks.",
        )
        .expect("board update should produce a delegation plan");

        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["revenue", "churn", "cash runway", "hiring-plan notes"]
        );
        assert!(plan.nodes.iter().all(|node| node.depends_on.is_empty()));
    }

    #[test]
    fn plans_ready_nodes_for_option_comparison() {
        // Pins: compare prompts with explicit options are high-confidence parallel work.
        let plan = plan_delegation_for_request(
            "Compare LISTEN/NOTIFY, polling, and SSE for progress replay in our app.",
        )
        .expect("explicit option comparison should produce a delegation plan");

        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["LISTEN/NOTIFY", "polling", "SSE"]
        );
    }

    #[test]
    fn plans_ready_nodes_for_leading_source_list() {
        // Pins: realistic synthesis prompts often name scattered inputs before the
        // actual work verb rather than after "from" or "across".
        let plan = plan_delegation_for_request(
            "Logs, deploy diff, and customer-impact notes are scattered. \
             Synthesize likely cause and the next checks.",
        )
        .expect("leading source list should produce a delegation plan");

        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Logs", "deploy diff", "customer-impact notes"]
        );
    }

    #[test]
    fn plans_generic_two_sided_summary_and_comparison() {
        // Pins (S4): a two-sided "verb X and Y" is delegable at two nodes whose titles are the
        // user's OWN words — no hardcoded phrasing or canned title.
        let summary = plan_delegation_for_request(
            "I'm a support lead. Three customers reported damaged orders in the same zip code. \
             Summarize the customer response and the internal action plan.",
        )
        .expect("two-sided summary should produce a plan");
        assert_eq!(summary.reason, "explicit_comparison");
        assert_eq!(
            summary
                .nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["customer response", "internal action plan"]
        );

        let buckets = plan_delegation_for_request(
            "We have 40 refund tickets from the same promo code. \
             Categorize likely buckets and next ops checks.",
        )
        .expect("two-sided categorization should produce a plan");
        assert_eq!(
            buckets
                .nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["likely buckets", "next ops checks"]
        );
    }

    #[test]
    fn plans_generic_incident_symptom_list_from_request_words() {
        // Pins (S4): an incident stated as multiple symptom clauses (";"/"," separated) yields a
        // plan whose nodes are the request's own symptom phrases, and drops the "no 5xxs" clause.
        let plan = plan_delegation_for_request(
            "Checkout is slow; promo rules changed, DB CPU is up, and there are no 5xxs. \
             Give me a triage plan.",
        )
        .expect("multi-symptom incident should produce a plan");
        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Checkout is slow", "promo rules changed", "DB CPU is up"]
        );
    }

    #[test]
    fn plans_mid_sentence_reconcile_item_list() {
        // Pins (live sweep S008): a mid-sentence "need to reconcile X, Y, and Z" item list must
        // fire the extractor. Dropping the " reconcile " anchor regressed this persona from
        // 3 workers + 1 bundle to no delegation in the 2026-07-01 post-simplification sweep.
        let plan = plan_delegation_for_request(
            "I need to reconcile Stripe payouts, bank deposits, and refunded orders. \
             Give me a practical reconciliation plan.",
        )
        .expect("mid-sentence reconcile item list should produce a plan");
        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Stripe payouts", "bank deposits", "refunded orders"]
        );
    }

    #[test]
    fn leaves_llm_to_decompose_when_workstreams_are_not_literal() {
        // Pins (S4): when the workstreams are not literally present in the request — an imperative
        // tail ("compare the assumptions and give me questions") or a single symptom whose
        // investigation angles must be invented — the planner emits NO hardcoded plan and defers
        // to the coordinator LLM (per the identity operating contract).
        assert!(
            plan_delegation_for_request(
                "Sales says Q3 is sandbagged, but the finance model says we miss by 8 percent. \
                 Compare the assumptions and give me neutral questions for both teams.",
            )
            .is_none()
        );
        assert!(
            plan_delegation_for_request(
                "Warehouse lead here: cold-chain complaints doubled in zone 4. \
                 Give me an ops investigation plan.",
            )
            .is_none()
        );
    }

    #[test]
    fn ignores_conceptual_or_status_prompts() {
        // Pins: the gate stays conservative and does not manufacture workers for
        // architecture discussion or status checks.
        assert!(
            plan_delegation_for_request(
                "What do you think, should we separate coordinator and worker?"
            )
            .is_none()
        );
        assert!(plan_delegation_for_request("What's the status of the active worker?").is_none());
    }

    #[tokio::test]
    async fn processor_reads_recent_user_event_not_skill_manifest() {
        // Pins: after skill injection, the last user-role context message can be the
        // skill manifest; delegation planning must use the durable event tail instead.
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        };
        let mut ctx = WorkingContext::new(&session, capabilities);
        ctx.append_message(ContextMessage::user(
            "<available_skills>finance</available_skills>",
        ));
        let event = Event::UserMessage {
            text: "I need launch readiness across docs, support training, billing, and analytics by Friday.".to_string(),
            attachments: Vec::new(),
        };
        ctx.set_recent_events(vec![EventRecord {
            id: Uuid::now_v7(),
            session_id: session.id,
            sequence_num: 1,
            event_type: EventType::from(&event),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }]);

        let output = DelegationPlanningProcessor::new()
            .process(&mut ctx)
            .await
            .expect("processor should run");

        assert_eq!(output.items_included.len(), 4);
        assert!(ctx.metadata().contains_key(DELEGATION_PLAN_METADATA_KEY));
        let hint = ctx
            .messages
            .last()
            .expect("processor should append a hint")
            .content
            .as_str();
        assert!(hint.contains("<delegation_plan_candidate>"));
        assert!(hint.contains("node-1"));
        assert!(hint.contains("spawn_worker"));
        assert!(hint.contains("support training"));
    }

    #[tokio::test]
    async fn processor_skips_spawn_hint_on_synthesis_turn() {
        // Pins (S5): once worker results are bundled for the active user message, the spawn hint
        // is suppressed so it does not contradict the <worker_result_bundle> synthesis guidance.
        let session = SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        };
        let mut ctx = WorkingContext::new(&session, capabilities);
        let record = |sequence_num: u64, event: Event| EventRecord {
            id: Uuid::now_v7(),
            session_id: session.id,
            sequence_num,
            event_type: EventType::from(&event),
            event,
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };
        // A decomposable request (would normally produce a plan) followed by a bundle for it.
        ctx.set_recent_events(vec![
            record(
                1,
                Event::UserMessage {
                    text: "I need launch readiness across docs, support training, billing, and analytics by Friday.".to_string(),
                    attachments: Vec::new(),
                },
            ),
            record(
                2,
                Event::WorkerResultBundle {
                    user_sequence_num: 1,
                    results: Vec::new(),
                },
            ),
        ]);

        let output = DelegationPlanningProcessor::new()
            .process(&mut ctx)
            .await
            .expect("processor should run");

        assert!(output.items_included.is_empty());
        assert!(!ctx.metadata().contains_key(DELEGATION_PLAN_METADATA_KEY));
        assert!(
            !ctx.messages
                .iter()
                .any(|message| message.content.contains("<delegation_plan_candidate>"))
        );
    }
}
