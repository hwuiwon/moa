//! Deterministic extraction of experience records from assessed task segments.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use moa_core::{
    types::context::WorkingContext, types::experience::ExperienceRecord,
    types::experience::ExperienceResource, types::experience::TaskFacetSet,
    types::experience::TaskFingerprint, types::identifiers::SegmentId, types::identifiers::UserId,
    types::segment_assessment::SegmentAssessment, types::segments::TaskSegment,
    types::session::SessionMeta,
};
use moa_skills::evidence::{EvidenceSource, SanitizedEntry, SanitizedLearningEvidence};
use serde_json::Value;
use uuid::Uuid;

use crate::pipeline::memory::extract_search_keywords;
use crate::query_rewrite::QueryRewriteResult;

/// Current deterministic experience extraction policy.
pub const EXPERIENCE_EXTRACTION_POLICY_VERSION: &str = "experience_v1";

/// Returns a task fingerprint for the current working context when a query exists.
#[must_use]
pub fn task_fingerprint_for_context(ctx: &WorkingContext) -> Option<TaskFingerprint> {
    let rewrite = ctx
        .metadata()
        .get("query_rewrite")
        .and_then(|value| serde_json::from_value::<QueryRewriteResult>(value.clone()).ok());
    let summary = rewrite
        .as_ref()
        .and_then(|rewrite| rewrite.task_summary.as_deref())
        .or_else(|| ctx.last_user_message())?;
    let facets = facets_for_task(
        summary,
        rewrite
            .as_ref()
            .and_then(|rewrite| rewrite.task_facets.as_ref()),
        &[],
        &[],
        &[],
    );
    Some(fingerprint_for_task(summary, &facets))
}

/// Builds an experience record from a task segment and an explicit assessment.
///
/// The record's summary, facets, actions, and resources are all derived from
/// transcript content, and the row they land in is read back by distillation,
/// embedding, and reviewer surfaces. It is therefore built from sanitized
/// evidence: there is no signature here that accepts raw events.
///
/// `task_summary` in particular is stored REDACTED AT REST. Every candidate for
/// it comes from `evidence`, which has already been through the classifier. The
/// raw `rewrite.task_summary` is deliberately not a fallback: it is the exact
/// text that was handed to sanitization, so reaching for it when sanitization
/// produced nothing would reinstate the value the classifier rejected or
/// abstained on — turning the one case where redaction mattered most into the
/// one case where it did not happen. `rewrite` is still consulted for facets,
/// which are categorical slots rather than transcript text.
///
/// This also keeps the stored summary and the embedded summary the same bytes.
/// The embedding backfill reads sanitized text; a raw stored summary would fork
/// the two, so a row's vector would describe text the row does not contain.
#[must_use]
pub fn experience_from_assessment(
    session: &SessionMeta,
    segment: &TaskSegment,
    assessment: &SegmentAssessment,
    evidence: &SanitizedLearningEvidence,
    rewrite: Option<&QueryRewriteResult>,
    duration_ms: Option<u64>,
    now: DateTime<Utc>,
) -> ExperienceRecord {
    let sanitized_summary = first_text(evidence, EvidenceSource::TaskSummary);
    let summary = sanitized_summary
        .as_deref()
        .or_else(|| first_user_message(evidence))
        .unwrap_or("unspecified task");
    let facets = facets_for_task(
        summary,
        rewrite.and_then(|rewrite| rewrite.task_facets.as_ref()),
        &segment.tools_used,
        &segment.skills_activated,
        evidence.entries(),
    );
    let fingerprint = fingerprint_for_task(summary, &facets);
    ExperienceRecord {
        id: deterministic_experience_id(segment.id),
        segment_id: segment.id,
        session_id: segment.session_id,
        tenant_id: session.tenant_id,
        user_id: experience_user_id(session),
        task_summary: Some(summary.to_string()),
        task_fingerprint: fingerprint,
        task_facets: facets,
        actions: actions_for_task(summary, evidence.entries()),
        resources: resources_for_evidence(evidence),
        outcome: assessment.outcome,
        confidence: assessment.confidence.clamp(0.0, 1.0),
        evidence: assessment.evidence.clone(),
        tools_used: normalized_list(segment.tools_used.clone()),
        skills_activated: normalized_list(segment.skills_activated.clone()),
        skills_used: normalized_list(segment.skills_used.clone()),
        turn_count: segment.turn_count,
        token_cost: segment.token_cost,
        duration_ms,
        assessment_policy_version: assessment.policy_version.clone(),
        extraction_policy_version: EXPERIENCE_EXTRACTION_POLICY_VERSION.to_string(),
        created_at: now,
    }
}

fn experience_user_id(session: &SessionMeta) -> UserId {
    let id = session
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.to_string())
        .unwrap_or_else(|| format!("tenant:{}", session.tenant_id));
    UserId::new(id)
}

/// Returns the deterministic experience id one assessed segment produces.
///
/// Public because the evidence scope must name the experience before the record
/// exists: the caller that sanitizes a segment's transcript stamps this id as
/// provenance, and [`experience_from_assessment`] then produces exactly that row.
#[must_use]
pub fn deterministic_experience_id(segment_id: SegmentId) -> Uuid {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"moa:experience-record:v1");
    hasher.update(segment_id.0.as_bytes());
    hasher.update(EXPERIENCE_EXTRACTION_POLICY_VERSION.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Computes a stable fingerprint for a task summary and deterministic facets.
#[must_use]
pub fn fingerprint_for_task(summary: &str, facets: &TaskFacetSet) -> TaskFingerprint {
    let normalized_summary = normalized_summary(summary);
    let canonical = canonical_fingerprint_input(&normalized_summary, facets);
    let hash = blake3::hash(canonical.as_bytes()).to_hex().to_string();
    TaskFingerprint {
        hash,
        normalized_summary,
        policy_version: EXPERIENCE_EXTRACTION_POLICY_VERSION.to_string(),
    }
}

/// Computes deterministic task facets from rewrite hints, observed tools, and event text.
#[must_use]
pub fn facets_for_task(
    summary: &str,
    rewrite_facets: Option<&TaskFacetSet>,
    tools: &[String],
    skills: &[String],
    entries: &[SanitizedEntry],
) -> TaskFacetSet {
    let text = task_text(summary, entries);
    let mut facets = rewrite_facets.cloned().unwrap_or_default();
    facets.domain = facets
        .domain
        .or_else(|| first_matching(&text, DOMAIN_PATTERNS));
    facets.action = facets
        .action
        .or_else(|| first_matching(&text, ACTION_PATTERNS));
    facets.artifact_kind = facets
        .artifact_kind
        .or_else(|| first_matching(&text, ARTIFACT_PATTERNS));
    facets.language_or_framework = facets
        .language_or_framework
        .or_else(|| first_matching(&text, LANGUAGE_PATTERNS));
    facets.verification_style = facets
        .verification_style
        .or_else(|| verification_style(entries, &text));
    facets.risk_class = facets
        .risk_class
        .or_else(|| first_matching(&text, RISK_PATTERNS))
        .or_else(|| Some("low".to_string()));
    facets.tool_pattern = normalized_list(
        facets
            .tool_pattern
            .into_iter()
            .chain(tools.iter().cloned())
            .collect(),
    );
    facets.skill_pattern = normalized_list(
        facets
            .skill_pattern
            .into_iter()
            .chain(skills.iter().cloned())
            .collect(),
    );
    facets
}

const DOMAIN_PATTERNS: &[(&str, &[&str])] = &[
    ("auth", &["auth", "oauth", "oidc", "login", "token"]),
    ("memory", &["memory", "retrieval", "graph", "embedding"]),
    ("skills", &["skill", "distill", "learning"]),
    ("database", &["postgres", "sql", "migration", "schema"]),
    (
        "runtime",
        &["orchestrator", "restate", "workflow", "session"],
    ),
    ("docs", &["doc", "docs", "readme"]),
    ("frontend", &["react", "next", "css", "ui"]),
];

const ACTION_PATTERNS: &[(&str, &[&str])] = &[
    (
        "debug",
        &["debug", "fix", "failure", "error", "bug", "regression"],
    ),
    (
        "implement",
        &["implement", "add", "create", "build", "wire"],
    ),
    ("review", &["review", "audit", "inspect"]),
    ("document", &["document", "docs", "readme"]),
    ("test", &["test", "verify", "validate"]),
    ("deploy", &["deploy", "release", "ship"]),
    ("research", &["research", "investigate", "analyze"]),
];

const ARTIFACT_PATTERNS: &[(&str, &[&str])] = &[
    ("migration", &["migration", "schema", "table", "view"]),
    ("test", &["test", "spec", "scenario"]),
    ("documentation", &["doc", "docs", "readme", "plan"]),
    ("code", &["code", "crate", "module", "function"]),
    ("configuration", &["config", "toml", "yaml", "json"]),
];

const LANGUAGE_PATTERNS: &[(&str, &[&str])] = &[
    ("rust", &["rust", "cargo", "crate", "clippy"]),
    ("sql", &["sql", "postgres", "pgvector"]),
    ("typescript", &["typescript", "javascript", "react", "next"]),
    ("python", &["python", "py"]),
    ("shell", &["bash", "shell", "zsh"]),
];

const RISK_PATTERNS: &[(&str, &[&str])] = &[
    (
        "high",
        &[
            "auth",
            "security",
            "delete",
            "erase",
            "credential",
            "secret",
        ],
    ),
    ("medium", &["migration", "policy", "approval", "deploy"]),
];

fn normalized_summary(summary: &str) -> String {
    let mut keywords = extract_search_keywords(summary);
    keywords.sort();
    keywords.dedup();
    if keywords.is_empty() {
        summary.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        keywords.join(" ")
    }
}

fn canonical_fingerprint_input(summary: &str, facets: &TaskFacetSet) -> String {
    let mut parts = vec![format!("summary={summary}")];
    for (name, value) in [
        ("domain", facets.domain.as_deref()),
        ("action", facets.action.as_deref()),
        ("artifact", facets.artifact_kind.as_deref()),
        ("language", facets.language_or_framework.as_deref()),
        ("verification", facets.verification_style.as_deref()),
        ("risk", facets.risk_class.as_deref()),
    ] {
        if let Some(value) = value {
            parts.push(format!("{name}={value}"));
        }
    }
    parts.join("|")
}

/// Builds the lowercased corpus the deterministic facet patterns match against.
///
/// Tool results are deliberately excluded, as they were before: a facet must
/// describe the task, not whatever a tool happened to print.
fn task_text(summary: &str, entries: &[SanitizedEntry]) -> String {
    let mut text = summary.to_ascii_lowercase();
    for entry in entries {
        match entry.source() {
            EvidenceSource::UserMessage
            | EvidenceSource::QueuedMessage
            | EvidenceSource::AssistantThinking
            | EvidenceSource::AssistantMessage => {
                text.push(' ');
                text.push_str(&entry.text().to_ascii_lowercase());
            }
            EvidenceSource::ToolInput => {
                if let Some(tool_name) = entry.tool_name() {
                    text.push(' ');
                    text.push_str(&tool_name.to_ascii_lowercase());
                }
                if let Some(input) = entry.structured() {
                    collect_json_strings(input, &mut text);
                }
            }
            EvidenceSource::ToolError => {
                if let Some(tool_name) = entry.tool_name() {
                    text.push(' ');
                    text.push_str(&tool_name.to_ascii_lowercase());
                }
                text.push(' ');
                text.push_str(&entry.text().to_ascii_lowercase());
            }
            EvidenceSource::ToolResult
            | EvidenceSource::MemoryPath
            | EvidenceSource::MemoryIngestSource
            | EvidenceSource::MemoryIngestPage
            | EvidenceSource::TaskSummary
            | EvidenceSource::AssessmentEvidence => {}
        }
    }
    text
}

fn first_matching(text: &str, patterns: &[(&str, &[&str])]) -> Option<String> {
    patterns.iter().find_map(|(label, needles)| {
        needles
            .iter()
            .any(|needle| text.contains(needle))
            .then(|| (*label).to_string())
    })
}

fn verification_style(entries: &[SanitizedEntry], text: &str) -> Option<String> {
    if text.contains("cargo test")
        || text.contains("cargo clippy")
        || text.contains("validator")
        || text.contains("git diff --check")
    {
        return Some("command".to_string());
    }
    if entries.iter().any(|entry| match entry.source() {
        EvidenceSource::ToolResult => entry.success() == Some(true),
        EvidenceSource::ToolError => true,
        _ => false,
    }) {
        return Some("tool_result".to_string());
    }
    None
}

fn actions_for_task(summary: &str, entries: &[SanitizedEntry]) -> Vec<String> {
    let text = task_text(summary, entries);
    let mut actions = ACTION_PATTERNS
        .iter()
        .filter(|(_, needles)| needles.iter().any(|needle| text.contains(needle)))
        .map(|(label, _)| (*label).to_string())
        .collect::<Vec<_>>();
    actions.sort();
    actions.dedup();
    actions
}

fn resources_for_evidence(evidence: &SanitizedLearningEvidence) -> Vec<ExperienceResource> {
    let mut resources = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in evidence.entries() {
        match entry.source() {
            EvidenceSource::MemoryPath | EvidenceSource::MemoryIngestSource => {
                push_resource(
                    &mut resources,
                    &mut seen,
                    "memory",
                    entry.text().to_string(),
                    entry.tool_name().map(str::to_string),
                );
            }
            EvidenceSource::MemoryIngestPage => {
                push_resource(
                    &mut resources,
                    &mut seen,
                    "memory",
                    entry.text().to_string(),
                    None,
                );
            }
            EvidenceSource::ToolInput => {
                if let Some(tool_name) = entry.tool_name() {
                    push_resource(
                        &mut resources,
                        &mut seen,
                        "tool",
                        tool_name.to_string(),
                        None,
                    );
                }
                if let Some(input) = entry.structured() {
                    for value in json_resource_strings(input) {
                        push_resource(&mut resources, &mut seen, "file", value, None);
                    }
                }
            }
            _ => {}
        }
    }
    resources
}

fn push_resource(
    resources: &mut Vec<ExperienceResource>,
    seen: &mut BTreeSet<(String, String)>,
    resource_type: &str,
    id: String,
    label: Option<String>,
) {
    let key = (resource_type.to_string(), id.clone());
    if seen.insert(key) {
        resources.push(ExperienceResource {
            resource_type: resource_type.to_string(),
            id,
            label,
        });
    }
}

fn json_resource_strings(input: &Value) -> Vec<String> {
    let mut values = Vec::new();
    collect_json_resource_strings(input, &mut values);
    values.sort();
    values.dedup();
    values
}

fn collect_json_resource_strings(input: &Value, values: &mut Vec<String>) {
    match input {
        Value::String(value) if looks_like_resource(value) => values.push(value.clone()),
        Value::Array(items) => {
            for item in items {
                collect_json_resource_strings(item, values);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_json_resource_strings(value, values);
            }
        }
        _ => {}
    }
}

fn collect_json_strings(input: &Value, output: &mut String) {
    match input {
        Value::String(value) => {
            output.push(' ');
            output.push_str(&value.to_ascii_lowercase());
        }
        Value::Array(items) => {
            for item in items {
                collect_json_strings(item, output);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_json_strings(value, output);
            }
        }
        _ => {}
    }
}

fn looks_like_resource(value: &str) -> bool {
    value.contains('/')
        || value.ends_with(".rs")
        || value.ends_with(".md")
        || value.ends_with(".sql")
        || value.ends_with(".toml")
        || value.starts_with("http://")
        || value.starts_with("https://")
}

fn first_user_message(evidence: &SanitizedLearningEvidence) -> Option<&str> {
    evidence
        .entries_from(EvidenceSource::UserMessage)
        .next()
        .map(SanitizedEntry::text)
}

/// Returns the first entry's text for one carrier, owned so it can outlive the borrow.
fn first_text(evidence: &SanitizedLearningEvidence, source: EvidenceSource) -> Option<String> {
    evidence
        .entries_from(source)
        .next()
        .map(|entry| entry.text().to_string())
}

fn normalized_list(values: Vec<String>) -> Vec<String> {
    let mut values = values
        .into_iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_core::{
        events::Event, types::events_stream::EventRecord, types::identifiers::SegmentId,
        types::identifiers::SessionId, types::identifiers::TenantId,
    };
    use moa_skills::evidence::{EvidenceScope, SegmentNarrative, sanitize_segment_evidence};
    use uuid::Uuid;

    use super::*;

    /// Sanitizes raw fixture events into the evidence extraction consumes.
    ///
    /// Experience extraction reads only sanitized evidence now, so the unit tests
    /// run their fixtures through the same production gate.
    async fn evidence(
        session_id: SessionId,
        segment_id: SegmentId,
        task_summary: Option<&str>,
        events: &[EventRecord],
    ) -> SanitizedLearningEvidence {
        sanitize_segment_evidence(
            &moa_memory_pii::HeuristicPiiClassifier,
            EvidenceScope {
                tenant_id: TenantId::new(),
                contact_id: None,
                session_id,
                segment_id,
                experience_id: deterministic_experience_id(segment_id),
            },
            events,
            SegmentNarrative {
                task_summary,
                assessment_summaries: &[],
            },
        )
        .await
        .expect("unit-test fixtures sanitize cleanly")
    }

    #[test]
    fn fingerprint_is_stable_for_irrelevant_wording() {
        // Pins: deterministic task fingerprints group wording variants with the same facets.
        let facets = TaskFacetSet {
            domain: Some("auth".to_string()),
            action: Some("debug".to_string()),
            artifact_kind: Some("code".to_string()),
            language_or_framework: Some("rust".to_string()),
            verification_style: Some("command".to_string()),
            risk_class: Some("high".to_string()),
            tool_pattern: vec!["bash".to_string()],
            skill_pattern: Vec::new(),
        };

        let left = fingerprint_for_task("Please fix the Rust auth failure", &facets);
        let right = fingerprint_for_task("Fix Rust auth failure", &facets);

        assert_eq!(left.hash, right.hash);
        assert_eq!(left.normalized_summary, "auth failure fix rust");
    }

    #[tokio::test]
    async fn experience_from_assessment_extracts_facets_and_resources() {
        // Pins: an assessed segment becomes a bounded experience record with
        // deterministic task facets and deduplicated resources.
        let session_id = SessionId::new();
        let segment_id = SegmentId::new();
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        let session = SessionMeta {
            id: session_id,
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        };
        let assessment = SegmentAssessment {
            outcome: moa_core::types::segment_assessment::SegmentOutcome::Resolved,
            confidence: 0.9,
            phase: moa_core::types::segment_assessment::AssessmentPhase::Immediate,
            evidence: Vec::new(),
            assessed_at: now,
            policy_version: "assessment_v1".to_string(),
        };
        let segment = TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: "tenant".to_string(),
            segment_index: 0,
            task_summary: Some("Fix Rust auth migration".to_string()),
            started_at: now,
            ended_at: Some(now),
            turn_count: 2,
            tools_used: vec!["bash".to_string()],
            skills_activated: vec!["rust".to_string()],
            skills_used: Vec::new(),
            token_cost: 42,
            previous_segment_id: None,
            outcome: Some("resolved".to_string()),
            assessment: Some(assessment.clone()),
            outcome_confidence: Some(0.9),
        };
        let events = vec![EventRecord {
            id: Uuid::now_v7(),
            session_id,
            sequence_num: 0,
            event_type: moa_core::events::EventType::ToolCall,
            event: Event::ToolCall {
                tool_id: moa_core::types::identifiers::ToolCallId::new(),
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({"cmd": "cargo test crates/moa-core/src/lib.rs"}),
                hand_id: None,
            },
            timestamp: now,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }];

        let evidence = evidence(
            session_id,
            segment_id,
            segment.task_summary.as_deref(),
            &events,
        )
        .await;
        let experience = experience_from_assessment(
            &session,
            &segment,
            &assessment,
            &evidence,
            None,
            Some(100),
            now,
        );

        assert_eq!(
            experience.outcome,
            moa_core::types::segment_assessment::SegmentOutcome::Resolved
        );
        assert_eq!(experience.task_facets.domain.as_deref(), Some("auth"));
        assert_eq!(
            experience.task_facets.language_or_framework.as_deref(),
            Some("rust")
        );
        assert_eq!(experience.task_facets.tool_pattern, vec!["bash"]);
        assert_eq!(experience.resources.len(), 2);
    }

    #[tokio::test]
    async fn stored_task_summary_never_falls_back_to_the_raw_query_rewrite() {
        // Pins: `task_summary` is stored redacted at rest. When sanitization yields no
        // summary — which is exactly what happens when the classifier rejects or abstains
        // on it — extraction must fall through to other SANITIZED text, never to the raw
        // rewrite summary it was handed.
        //
        // The raw value is the same text sanitization was asked to clean. Reaching for it
        // on the sanitizer's failure would turn the one case where redaction mattered most
        // into the one case where it did not happen, and nothing downstream would notice:
        // the row would look populated and the value would be wrong.
        let session_id = SessionId::new();
        let segment_id = SegmentId::new();
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        let session = SessionMeta {
            id: session_id,
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        };
        let assessment = SegmentAssessment {
            outcome: moa_core::types::segment_assessment::SegmentOutcome::Resolved,
            confidence: 0.9,
            phase: moa_core::types::segment_assessment::AssessmentPhase::Immediate,
            evidence: Vec::new(),
            assessed_at: now,
            policy_version: "assessment_v1".to_string(),
        };
        let segment = TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: "tenant".to_string(),
            segment_index: 0,
            task_summary: None,
            started_at: now,
            ended_at: Some(now),
            turn_count: 1,
            tools_used: Vec::new(),
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 1,
            previous_segment_id: None,
            outcome: Some("resolved".to_string()),
            assessment: Some(assessment.clone()),
            outcome_confidence: Some(0.9),
        };
        // No narrative summary reaches sanitization, so the evidence carries no
        // `TaskSummary` entry and the fallback chain is what decides the stored value.
        let evidence = evidence(session_id, segment_id, None, &[]).await;
        let raw = "escalate the outage for priya.raman@example.com";
        let rewrite = QueryRewriteResult {
            task_summary: Some(raw.to_string()),
            ..QueryRewriteResult::original("escalate the outage")
        };

        let experience = experience_from_assessment(
            &session,
            &segment,
            &assessment,
            &evidence,
            Some(&rewrite),
            Some(10),
            now,
        );

        let stored = experience.task_summary.unwrap_or_default();
        assert_ne!(
            stored, raw,
            "the raw rewrite summary must never become the stored summary"
        );
        assert!(
            !stored.contains("priya.raman@example.com"),
            "unclassified transcript text reached the stored summary: {stored}"
        );
    }

    #[tokio::test]
    async fn experience_id_is_stable_for_reassessed_segment() {
        // Pins: repeated active-segment assessments upsert the same experience parent row.
        let session_id = SessionId::new();
        let segment_id = SegmentId::new();
        let now = Utc
            .with_ymd_and_hms(2026, 6, 15, 12, 0, 0)
            .single()
            .expect("fixed test timestamp should be valid");
        let session = SessionMeta {
            id: session_id,
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        };
        let assessment = SegmentAssessment {
            outcome: moa_core::types::segment_assessment::SegmentOutcome::Partial,
            confidence: 0.7,
            phase: moa_core::types::segment_assessment::AssessmentPhase::Immediate,
            evidence: Vec::new(),
            assessed_at: now,
            policy_version: "assessment_v1".to_string(),
        };
        let segment = TaskSegment {
            id: segment_id,
            session_id,
            tenant_id: "tenant".to_string(),
            segment_index: 0,
            task_summary: Some("Reassess the same work".to_string()),
            started_at: now,
            ended_at: None,
            turn_count: 1,
            tools_used: vec!["session_search".to_string()],
            skills_activated: Vec::new(),
            skills_used: Vec::new(),
            token_cost: 10,
            previous_segment_id: None,
            outcome: None,
            assessment: Some(assessment.clone()),
            outcome_confidence: Some(0.7),
        };

        let evidence = evidence(session_id, segment_id, segment.task_summary.as_deref(), &[]).await;
        let first =
            experience_from_assessment(&session, &segment, &assessment, &evidence, None, None, now);
        let second = experience_from_assessment(
            &session,
            &segment,
            &assessment,
            &evidence,
            None,
            Some(100),
            now + chrono::Duration::seconds(1),
        );

        assert_eq!(first.id, second.id);
        assert_eq!(first.id, deterministic_experience_id(segment_id));
    }

    #[test]
    fn task_fingerprint_ignores_observed_strategy_patterns() {
        // Pins: task-conditioned ranking can retrieve prior outcomes before selecting tools/skills.
        let mut first = TaskFacetSet {
            domain: Some("auth".to_string()),
            action: Some("debug".to_string()),
            artifact_kind: Some("code".to_string()),
            language_or_framework: Some("rust".to_string()),
            verification_style: Some("command".to_string()),
            risk_class: Some("high".to_string()),
            tool_pattern: vec!["bash".to_string()],
            skill_pattern: vec!["api-contract-repair".to_string()],
        };
        let mut second = first.clone();
        second.tool_pattern = vec!["file_read".to_string(), "grep".to_string()];
        second.skill_pattern = vec!["generic-debugger".to_string()];

        let first_fingerprint = fingerprint_for_task("Fix Rust auth API contract", &first);
        let second_fingerprint = fingerprint_for_task("Fix Rust auth API contract", &second);

        assert_eq!(first_fingerprint.hash, second_fingerprint.hash);
        assert_ne!(first.tool_pattern, second.tool_pattern);
        assert_ne!(first.skill_pattern, second.skill_pattern);
        first.tool_pattern.clear();
        first.skill_pattern.clear();
        assert_eq!(
            first_fingerprint.hash,
            fingerprint_for_task("Fix Rust auth API contract", &first).hash
        );
    }
}
