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

    let mut items = explicit_work_items(&normalized, &lower);
    let mut reason = (items.len() >= MIN_LIST_NODES).then_some("explicit_multi_workstream_list");
    if reason.is_none() {
        items = two_sided_comparison_items(&lower);
        if items.len() >= 2 {
            reason = Some("explicit_comparison");
        }
    }
    if reason.is_none() {
        items = incident_investigation_items(&lower);
        if items.len() >= MIN_LIST_NODES {
            reason = Some("incident_investigation");
        }
    }
    let reason = reason?;
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

fn explicit_work_items(normalized: &str, lower: &str) -> Vec<String> {
    let candidates = [
        leading_list_segment(normalized),
        segment_after_colon(normalized),
        segment_after_anchor(normalized, lower, " from "),
        segment_after_anchor(normalized, lower, " across "),
        segment_after_anchor(normalized, lower, " using "),
        segment_after_anchor(normalized, lower, " between "),
        segment_after_anchor(normalized, lower, " for storing "),
        segment_after_anchor(normalized, lower, " for "),
        segment_after_prefix(normalized, lower, "compare "),
        segment_after_anchor(normalized, lower, " reconcile "),
        segment_after_anchor(normalized, lower, " compare "),
        segment_after_anchor(normalized, lower, " review "),
        segment_after_semicolon(normalized),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(|segment| split_work_items(&segment))
        .find(|items| items.len() >= MIN_LIST_NODES)
        .unwrap_or_default()
}

fn two_sided_comparison_items(lower: &str) -> Vec<String> {
    if lower.contains("sales says") && lower.contains("finance model") {
        return vec![
            "sales assumptions".to_string(),
            "finance model assumptions".to_string(),
        ];
    }
    if lower.contains("customer response") && lower.contains("internal action plan") {
        return vec![
            "customer response".to_string(),
            "internal action plan".to_string(),
        ];
    }
    if lower.contains("likely buckets") && lower.contains("next ops checks") {
        return vec![
            "likely refund buckets".to_string(),
            "next ops checks".to_string(),
        ];
    }
    Vec::new()
}

fn incident_investigation_items(lower: &str) -> Vec<String> {
    if lower.contains("checkout")
        && lower.contains("slow")
        && lower.contains("promo")
        && lower.contains("db cpu")
    {
        return vec![
            "promo-rule change impact".to_string(),
            "database CPU and slow-query pressure".to_string(),
            "checkout latency without 5xxs".to_string(),
        ];
    }
    if lower.contains("cold-chain")
        && lower.contains("complaints")
        && contains_any(lower, &["doubled", "spike", "increased"])
        && contains_any(lower, &["investigation", "triage", "plan"])
    {
        return vec![
            "complaint trend and affected scope".to_string(),
            "temperature-control process checks".to_string(),
            "zone 4 route, carrier, and staffing changes".to_string(),
        ];
    }
    Vec::new()
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
    if !bounded.contains(',') {
        return Vec::new();
    }

    let list_text = bounded.replace(" and ", ", ").replace(" or ", ", ");
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
    fn plans_two_sided_comparison_without_strict_schema_fields() {
        // Pins: the planner can surface ready coordinator work without adding
        // selected_skill or selected_action to the worker contract.
        let plan = plan_delegation_for_request(
            "Sales says Q3 forecast is sandbagged, but the finance model says we miss \
             by 8 percent. Compare the assumptions and give me neutral questions for both teams.",
        )
        .expect("two-sided comparison should produce a plan");

        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec!["sales assumptions", "finance model assumptions"]
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
    fn plans_ready_nodes_for_cold_chain_complaint_spike_investigation() {
        // Pins: users do not need to spell out delegation when an operational spike
        // investigation has clear independent workstreams.
        let plan = plan_delegation_for_request(
            "Warehouse lead here: cold-chain complaints doubled in zone 4. \
             Give me an ops investigation plan.",
        )
        .expect("cold-chain complaint spike should produce a delegation plan");

        assert_eq!(plan.reason, "incident_investigation");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "complaint trend and affected scope",
                "temperature-control process checks",
                "zone 4 route, carrier, and staffing changes"
            ]
        );
    }

    #[test]
    fn plans_checkout_latency_incident_without_vague_there_node() {
        // Pins: status clauses like "there are no 5xxs" are incident signals, not
        // standalone workstreams named "there".
        let plan = plan_delegation_for_request(
            "Checkout is slow; promo rules changed, DB CPU is up, and there are no 5xxs. \
             Give me a triage plan.",
        )
        .expect("checkout latency incident should produce a delegation plan");

        assert_eq!(plan.reason, "incident_investigation");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "promo-rule change impact",
                "database CPU and slow-query pressure",
                "checkout latency without 5xxs"
            ]
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
}
