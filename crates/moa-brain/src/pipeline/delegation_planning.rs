//! Stage 8: emits task-specific coordinator delegation planning hints.

use std::collections::HashMap;

use async_trait::async_trait;
use moa_core::{
    error::Result, events::Event, traits::ContextProcessor, types::context::ContextMessage,
    types::context::MessageRole, types::context::ProcessorOutput, types::context::WorkingContext,
    types::context::estimate_text_tokens,
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
    // An explicit parallel-delegation frame ("delegate ... parallel workers", "run these in
    // parallel as separate workers") is itself a high-confidence work signal even when the request
    // uses no verb from `has_work_signal` (e.g. S006 "prepares a variance analysis ..." and S030
    // "one drafts ..., one prepares ..., one lists ..." carry no listed verb). The frame is
    // conservative by construction (see `has_parallel_delegation_frame`), so OR-ing it in here does
    // not widen `has_work_signal` for ordinary prose.
    if normalized.is_empty()
        || is_non_execution_request(&lower)
        || !(has_work_signal(&lower) || has_parallel_delegation_frame(&lower))
    {
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
            // Explicit parallel tasks conjoined by "and separately" / "; separately" /
            // "and in parallel" under a parallel-delegation frame are delegable at two ready nodes.
            // The explicit conjunction (not merely the frame) is the signal, so ordinary two-item
            // prose without it does not qualify.
            let parallel = separately_conjoined_items(&normalized, &lower);
            if parallel.len() >= MIN_COMPARISON_NODES {
                ("explicit_parallel_tasks", parallel)
            } else {
                return None;
            }
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
    // Structured parallel-delegation enumerations take priority over the generic segment
    // extractors below: an explicit "(1)/(2)/(3)" list or an ordinal-worker list
    // ("one worker ..., another ...") is a higher-confidence signal, and its item titles are whole
    // clauses that the 7-word `is_useful_work_item` filter used by `split_work_items` would reject.
    let numbered = numbered_enumeration_items(normalized, lower);
    if numbered.len() >= min_items {
        return numbered;
    }
    let ordinal = ordinal_worker_items(normalized, lower);
    if ordinal.len() >= min_items {
        return ordinal;
    }

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

/// Whether the request explicitly frames work to fan out to parallel workers.
///
/// Requires a strong, unambiguous cue so ordinary prose that merely says "one ... another ..." or
/// delegates a single task ("delegate this to Sam") does NOT qualify. This gates both the
/// ordinal-worker enumeration extractor and the bare "1)/2)" numbered style, and — via
/// `plan_delegation_for_request` — serves as a work signal for delegation prompts whose verbs are
/// not in `has_work_signal`.
fn has_parallel_delegation_frame(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "in parallel",
            "parallel worker",
            "parallel analys",
            "parallel review",
            "separate worker",
        ],
    ) || (lower.contains("delegate") && lower.contains("parallel"))
}

/// Extracts item titles from an explicit "(1) ... (2) ... (3) ..." (or, under a parallel-delegation
/// frame, "1) ... 2) ...") enumeration. Each item spans from its marker to the next marker, or —
/// for the final item — to the first sentence boundary, so a trailing "then synthesize ..."
/// instruction is not captured as work.
fn numbered_enumeration_items(normalized: &str, lower: &str) -> Vec<String> {
    let parenthesized = normalized.contains("(1)");
    // The bare "1)" style is weaker (it collides with ordinary prose), so only trust it under an
    // explicit parallel-delegation frame and only when at least two markers are present.
    let bare = !parenthesized
        && has_parallel_delegation_frame(lower)
        && normalized.contains("1)")
        && normalized.contains("2)");
    if !parenthesized && !bare {
        return Vec::new();
    }
    let marker = |n: usize| {
        if parenthesized {
            format!("({n})")
        } else {
            format!("{n})")
        }
    };

    let mut marker_starts = Vec::new();
    let mut content_starts = Vec::new();
    let mut search_from = 0usize;
    for n in 1..=MAX_PLAN_NODES + 1 {
        let m = marker(n);
        let Some(rel) = normalized[search_from..].find(&m) else {
            break;
        };
        let abs = search_from + rel;
        marker_starts.push(abs);
        content_starts.push(abs + m.len());
        search_from = abs + m.len();
    }

    let mut items = Vec::new();
    for (index, &start) in content_starts.iter().enumerate() {
        let end = marker_starts
            .get(index + 1)
            .copied()
            .unwrap_or(normalized.len());
        if let Some(item) = clean_enumeration_item(&normalized[start..end]) {
            items.push(item);
        }
    }
    items
}

/// Extracts item titles from an ordinal-worker enumeration
/// ("one worker ..., another ...", "one drafts ..., one prepares ..., one lists ...") that appears
/// ONLY under an explicit parallel-delegation frame. The frame guard is what keeps ordinary prose
/// ("one team owns X, another owns Y") from seeding a plan. Items are the contiguous run of
/// comma-separated clauses, starting at the first clause, that each begin with an ordinal marker.
fn ordinal_worker_items(normalized: &str, lower: &str) -> Vec<String> {
    if !has_parallel_delegation_frame(lower) {
        return Vec::new();
    }
    // The worker list is introduced by the frame's colon in the covered prompts; fall back to the
    // whole request otherwise. Cut at the first sentence boundary so the trailing
    // "combine/merge/synthesize into one ..." instruction is not treated as an item.
    let segment = normalized
        .split_once(':')
        .map(|(_, tail)| tail)
        .unwrap_or(normalized);
    let segment = match segment.find(['.', '?', '!']) {
        Some(index) => &segment[..index],
        None => segment,
    };

    let mut items = Vec::new();
    for clause in segment.split(',') {
        let Some(rest) = strip_ordinal_marker(clause) else {
            break;
        };
        if let Some(item) = clean_enumeration_item(rest) {
            items.push(item);
        }
    }
    items
}

/// Splits top-level tasks that an explicit parallel conjunction joins under a parallel-delegation
/// frame: "draft X, and separately review Y", "prepare X; separately audit Y", "do X, and in
/// parallel do Y". The conjunction — not merely the frame — is the signal, so ordinary two-item
/// prose without it returns empty. Requires the frame AND at least one conjunction split (yielding
/// two or more items) to fire.
fn separately_conjoined_items(normalized: &str, lower: &str) -> Vec<String> {
    if !has_parallel_delegation_frame(lower) {
        return Vec::new();
    }
    // Isolate the task list after the frame's colon (fall back to the whole request), then cut at
    // the first sentence boundary so a trailing "Return one combined recommendation." is excluded.
    let segment = normalized
        .split_once(':')
        .map(|(_, tail)| tail)
        .unwrap_or(normalized);
    let segment = match segment.find(['.', '?', '!']) {
        Some(index) => &segment[..index],
        None => segment,
    };
    let segment_lower = segment.to_ascii_lowercase();

    // Comma/semicolon-prefixed forms are listed so the earliest-start match consumes the separator
    // cleanly; the bare forms cover a task list without a separator before the conjunction.
    const CONJUNCTIONS: [&str; 7] = [
        ", and separately ",
        ", and in parallel ",
        "; separately ",
        "; in parallel ",
        ", separately ",
        " and separately ",
        " and in parallel ",
    ];

    let mut items = Vec::new();
    let mut start = 0usize;
    let mut cursor = 0usize;
    loop {
        let next = CONJUNCTIONS
            .iter()
            .filter_map(|conjunction| {
                segment_lower[cursor..]
                    .find(conjunction)
                    .map(|rel| (cursor + rel, cursor + rel + conjunction.len()))
            })
            .min_by_key(|(conjunction_start, _)| *conjunction_start);
        match next {
            Some((conjunction_start, conjunction_end)) => {
                if let Some(item) = clean_enumeration_item(&segment[start..conjunction_start]) {
                    items.push(item);
                }
                start = conjunction_end;
                cursor = conjunction_end;
            }
            None => {
                if let Some(item) = clean_enumeration_item(&segment[start..]) {
                    items.push(item);
                }
                break;
            }
        }
    }

    // A single item means no conjunction fired — not a parallel-task list.
    if items.len() < MIN_COMPARISON_NODES {
        return Vec::new();
    }
    items
}

/// Returns the text after a leading ordinal-worker marker ("one worker ", "a second ", "one ", …),
/// or `None` when the clause does not begin with one. Markers are ordered longest-first so
/// "one worker " wins over "one ".
fn strip_ordinal_marker(clause: &str) -> Option<&str> {
    let trimmed = clause.trim();
    let lower = trimmed.to_ascii_lowercase();
    const MARKERS: [&str; 8] = [
        "one worker ",
        "another worker ",
        "a second ",
        "a third ",
        "a fourth ",
        "a fifth ",
        "another ",
        "one ",
    ];
    for marker in MARKERS {
        if lower.starts_with(marker) {
            return Some(trimmed[marker.len()..].trim());
        }
    }
    None
}

/// Normalizes one enumeration item: cut at the first sentence boundary, strip surrounding
/// punctuation, and drop a leading article so titles read as the user's own words.
fn clean_enumeration_item(raw: &str) -> Option<String> {
    let cut = match raw.find(['.', '?', '!']) {
        Some(index) => &raw[..index],
        None => raw,
    };
    let mut item = cut
        .trim()
        .trim_matches(|character: char| {
            matches!(character, ',' | ';' | ':' | '"' | '\'' | '(' | ')')
        })
        .trim();
    for prefix in ["a ", "an ", "the "] {
        if let Some(stripped) = item.strip_prefix(prefix) {
            item = stripped.trim();
            break;
        }
    }
    if item.is_empty() {
        return None;
    }
    Some(item.to_string())
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
call spawn_worker for the ready nodes before final synthesis. Put the full \
task envelope inside each spawn_worker.task: purpose, relevant context, \
expected output, evidence needs, constraints, and any relevant skill steps. \
If the ready nodes were already spawned in history, do not spawn duplicates; \
wait for or synthesize from the worker results.\n\
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
        events::EventType, types::channel::Channel, types::events_stream::EventRecord,
        types::identifiers::ModelId, types::identifiers::SessionId, types::identifiers::TenantId,
        types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
        types::session::SessionMeta,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn delegation_plan_node_stays_minimal() {
        // Pins: planner metadata remains a minimal ready-node hint, not a
        // durable worker contract.
        let node = DelegationPlanNode {
            id: "node-1".to_string(),
            title: "support readiness".to_string(),
            depends_on: Vec::new(),
        };
        let value = serde_json::to_value(node).expect("delegation plan node should serialize");
        let object = value
            .as_object()
            .expect("delegation plan node should serialize to an object");
        let actual = object
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let expected = ["depends_on", "id", "title"]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn rendered_hint_uses_task_envelope_not_contract_fields() {
        // Pins: rich worker guidance belongs in spawn_worker.task rather than
        // new planner metadata or worker DTO fields.
        let plan = DelegationPlan {
            reason: "explicit_multi_workstream_list".to_string(),
            nodes: vec![DelegationPlanNode {
                id: "node-1".to_string(),
                title: "billing readiness".to_string(),
                depends_on: Vec::new(),
            }],
        };

        let hint = render_plan_hint(&plan);

        assert!(hint.contains("spawn_worker.task"));
        assert!(hint.contains("task envelope"));
        assert!(hint.contains("expected output"));
        assert!(hint.contains("evidence needs"));
        assert!(hint.contains("constraints"));
        assert!(hint.contains("relevant skill steps"));
        assert!(!hint.contains("task_name"));
        assert!(!hint.contains("capability_mode"));
        assert!(!hint.contains("output_contract"));
    }

    #[test]
    fn does_not_plan_for_single_generic_question() {
        // Pins: a generic one-workstream request is left to the coordinator,
        // even when it uses a work-like verb.
        assert!(
            plan_delegation_for_request(
                "Can you review the onboarding flow and tell me what matters?"
            )
            .is_none()
        );
    }

    #[test]
    fn plans_ready_nodes_for_explicit_general_purpose_workstreams() {
        // Pins: explicit, general-purpose workstream lists still produce ready
        // nodes without adding classification metadata.
        let plan = plan_delegation_for_request(
            "Audit launch readiness across customer comms, billing readiness, \
             support staffing, and incident response.",
        )
        .expect("explicit general-purpose workstreams should produce a plan");

        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            vec!["node-1", "node-2", "node-3", "node-4"]
        );
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "customer comms",
                "billing readiness",
                "support staffing",
                "incident response"
            ]
        );
        assert!(plan.nodes.iter().all(|node| node.depends_on.is_empty()));
    }

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
    fn plans_ordinal_worker_pair_live_s006() {
        // Pins (live sweep S006): "Delegate two parallel analyses: one worker ..., another ..."
        // yields a two-node plan. Before the ordinal-worker extractor this prompt got no plan, so
        // the model fell back to manual spawn_worker orchestration (workers=2, bundles=0) and
        // partialed with F-QUALITY.
        let plan = plan_delegation_for_request(
            "Delegate two parallel analyses: one worker prepares a variance analysis of \
             marketing spend vs budget, another prepares a headcount cost forecast through \
             year end. Merge both into a single summary for the CFO.",
        )
        .expect("ordinal-worker pair under a parallel frame should produce a plan");
        assert_eq!(plan.reason, "explicit_comparison");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "prepares a variance analysis of marketing spend vs budget",
                "prepares a headcount cost forecast through year end"
            ]
        );
        assert!(plan.nodes.iter().all(|node| node.depends_on.is_empty()));
    }

    #[test]
    fn plans_numbered_enumeration_live_s012() {
        // Pins (live sweep S012): "(1) ..., (2) ..., (3) ..." under a parallel frame yields a
        // three-node plan. This prompt previously got no plan (F-DELEGATE, workers=0).
        let plan = plan_delegation_for_request(
            "Run these in parallel as separate workers: (1) a cohort revenue retention table \
             description for 2025 signups, (2) a vendor spend categorization by department, \
             (3) a runway sensitivity analysis at ±15% burn. Then synthesize the three into \
             one memo.",
        )
        .expect("parenthesized enumeration should produce a plan");
        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "cohort revenue retention table description for 2025 signups",
                "vendor spend categorization by department",
                "runway sensitivity analysis at ±15% burn"
            ]
        );
    }

    #[test]
    fn plans_ordinal_worker_enumeration_live_s019() {
        // Pins (live sweep S019): "one worker ..., a second ..., a third ..." under "Delegate in
        // parallel:" yields a three-node plan. Previously fail with F-QUALITY,F-RAW-LEAK.
        let plan = plan_delegation_for_request(
            "Delegate in parallel: one worker drafts a refund-decision tree for agents, \
             a second drafts macros for the top four refund scenarios, a third summarizes \
             last month's refund reasons from the notes I paste next. Combine into one \
             enablement doc.",
        )
        .expect("mixed ordinal-worker enumeration should produce a plan");
        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "drafts a refund-decision tree for agents",
                "drafts macros for the top four refund scenarios",
                "summarizes last month's refund reasons from the notes I paste next"
            ]
        );
    }

    #[test]
    fn plans_ordinal_worker_enumeration_live_s030() {
        // Pins (live sweep S030): a bare repeated "one <verb> ..." list under "Delegate three
        // parallel workers:" yields a three-node plan even though the request carries no verb from
        // `has_work_signal` (the parallel frame is the work signal). Previously fail
        // F-QUALITY,F-RAW-LEAK with manual workers=3, bundles=0.
        let plan = plan_delegation_for_request(
            "Delegate three parallel workers: one drafts the customer status-page update for the \
             ongoing API latency incident, one prepares the internal timeline from the notes I \
             provide, one lists probable root causes for elevated p99 after a deploy. Combine \
             into an incident packet.",
        )
        .expect("bare repeated one-verb enumeration should produce a plan");
        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "drafts the customer status-page update for the ongoing API latency incident",
                "prepares the internal timeline from the notes I provide",
                "lists probable root causes for elevated p99 after a deploy"
            ]
        );
    }

    #[test]
    fn plans_ordinal_worker_enumeration_live_s044() {
        // Pins (live sweep S044): "one worker ..., one ..., one ..." under "Delegate three parallel
        // reviews:" yields a three-node plan. Previously F-DELEGATE, workers=0.
        let plan = plan_delegation_for_request(
            "Delegate three parallel reviews: one worker assesses the SOC 2 evidence gaps from \
             the list I share, one drafts the vendor security questionnaire for a new payroll \
             provider, one summarizes our data-retention exceptions. Combine into a quarterly \
             security memo.",
        )
        .expect("ordinal-worker review enumeration should produce a plan");
        assert_eq!(plan.reason, "explicit_multi_workstream_list");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "assesses the SOC 2 evidence gaps from the list I share",
                "drafts the vendor security questionnaire for a new payroll provider",
                "summarizes our data-retention exceptions"
            ]
        );
    }

    #[test]
    fn plans_separately_conjoined_tasks_live_s052() {
        // Pins (live sweep S052): two parallel tasks conjoined by ", and separately " under a
        // "parallel worker" frame yield a two-node plan. Neither the numbered nor the ordinal
        // extractor parses this phrasing (no markers, no numbers), so before the
        // separately-conjoined extractor this prompt got no plan and partialed with F-QUALITY.
        let plan = plan_delegation_for_request(
            "Two parallel worker tasks: draft the DPIA outline for our new location-tracking \
             feature, and separately review the marketing team's plan to upload customer emails \
             to an ad platform. Return one combined recommendation.",
        )
        .expect("separately-conjoined parallel tasks under a frame should produce a plan");
        assert_eq!(plan.reason, "explicit_parallel_tasks");
        assert_eq!(
            plan.nodes
                .iter()
                .map(|node| node.title.as_str())
                .collect::<Vec<_>>(),
            vec![
                "draft the DPIA outline for our new location-tracking feature",
                "review the marketing team's plan to upload customer emails to an ad platform"
            ]
        );
        assert!(plan.nodes.iter().all(|node| node.depends_on.is_empty()));
    }

    #[test]
    fn does_not_plan_separately_prose_without_delegation_frame() {
        // Pins: the separately-conjoined extractor fires ONLY under an explicit parallel-delegation
        // frame. Ordinary "X, and separately Y" prose (with work verbs but no delegate/parallel
        // cue) must not seed a plan. Breaking `has_parallel_delegation_frame` (always true) flips
        // this to a spurious two-node plan.
        assert!(
            plan_delegation_for_request(
                "I refreshed the billing report, and separately reviewed the churn plan.",
            )
            .is_none()
        );
    }

    #[test]
    fn does_not_plan_ordinal_prose_without_delegation_frame() {
        // Pins: the ordinal-worker extractor fires ONLY under an explicit parallel-delegation
        // frame. Ordinary "one ... another ... a third ..." prose (with work verbs but no
        // delegate/parallel cue) must not seed a plan. Breaking `has_parallel_delegation_frame`
        // (making it always true) flips this to a spurious three-node plan.
        assert!(
            plan_delegation_for_request(
                "One teammate handles the billing report, another handles the support review, \
                 a third handles the runway plan.",
            )
            .is_none()
        );
    }

    #[test]
    fn does_not_plan_numbered_list_in_non_execution_request() {
        // Pins: a numbered enumeration inside a non-execution question does not delegate — the
        // `is_non_execution_request` gate wins over the extractor.
        assert!(
            plan_delegation_for_request(
                "Should we (1) rebuild search relevance, (2) rewrite ingestion, and \
                 (3) add a rerank stage? What do you think?",
            )
            .is_none()
        );
    }

    #[test]
    fn does_not_plan_two_item_ordinary_phrase() {
        // Pins: an ordinary two-item phrase (no two-sided compare/reconcile cue and no delegation
        // frame) stays below threshold and produces no plan.
        assert!(
            plan_delegation_for_request("Email the finance report to Alice and Bob.").is_none()
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
