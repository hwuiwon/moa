//! Typed sanitized evidence required at every automatic learning boundary.
//!
//! Skill distillation, improvement, sibling re-synthesis, regression-suite
//! generation, and task-summary embedding all read session transcripts. None of
//! them may read a *raw* one: a raw transcript carries whatever the user, a tool,
//! or a remote document put in it, and these paths ship their input to a model
//! provider and into durable draft rows before any human sees it.
//!
//! [`SanitizedLearningEvidence`] is the only shape those paths accept. Its fields
//! are private, it has no raw-string or raw-`EventRecord` constructor, and it
//! does not implement `Deserialize`, so it cannot be forged or revived from a
//! wire payload. The one way to build it is [`sanitize_segment_evidence`], which
//! sanitizes every text carrier through [`moa_memory_pii::sanitized`] and refuses
//! the whole segment if any carrier cannot be released.
//!
//! This crate owns no classifier and no detection policy. The caller injects an
//! `Arc<dyn PiiClassifier>`: production passes the deterministic heuristic shared
//! with lineage capture, and workflow tests pass abstaining, failing, and
//! invalid-span classifiers to prove the refusals are load-bearing.

use std::collections::BTreeSet;

use moa_core::{
    events::Event,
    types::contact::ContactId,
    types::events_stream::EventRecord,
    types::experience::LearningCandidateSourceRef,
    types::identifiers::{SegmentId, SessionId, TenantId, ToolCallId},
    types::security::SensitivityClass,
};
use moa_memory_pii::sanitized::{SanitizationRejection, SanitizedText, sanitize_with};
use moa_memory_pii::{PiiCategory, PiiClassifier};
use uuid::Uuid;

/// Upper bound on distinct source-event provenance rows one candidate records.
///
/// The session, segment, and experience references are unconditional, so this
/// caps audit granularity, never the derivation chain itself.
const MAX_EVIDENCE_EVENT_SOURCES: usize = 32;

/// Privacy policy revision stamped on every piece of sanitized learning evidence.
///
/// One constant, not a config knob: a deployment that could weaken the learning
/// privacy contract per-tenant would make a stored draft's provenance
/// unreadable after the fact.
pub const LEARNING_PRIVACY_POLICY_REVISION: &str = "moa.learning-privacy.v1";

/// Which transcript carrier a sanitized entry came from.
///
/// A closed vocabulary. These labels ride into reviewer payloads, log lines, and
/// metric dimensions, so none of them may be derived from transcript bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EvidenceSource {
    /// A message the caller sent.
    UserMessage,
    /// A message queued while the agent was busy.
    QueuedMessage,
    /// Assistant-authored response text.
    AssistantMessage,
    /// Assistant-authored reasoning summary.
    AssistantThinking,
    /// Arguments the model passed to a tool.
    ToolInput,
    /// Output a tool returned.
    ToolResult,
    /// A durable tool failure.
    ToolError,
    /// A memory path read or written during the segment.
    MemoryPath,
    /// The source document of a memory ingestion.
    MemoryIngestSource,
    /// One page an ingestion affected.
    MemoryIngestPage,
    /// The task summary carried by the assessed segment or experience.
    TaskSummary,
    /// One segment-assessment evidence summary.
    AssessmentEvidence,
}

impl EvidenceSource {
    /// Returns the stable label used by logs, metrics, and reviewer payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::QueuedMessage => "queued_message",
            Self::AssistantMessage => "assistant_message",
            Self::AssistantThinking => "assistant_thinking",
            Self::ToolInput => "tool_input",
            Self::ToolResult => "tool_result",
            Self::ToolError => "tool_error",
            Self::MemoryPath => "memory_path",
            Self::MemoryIngestSource => "memory_ingest_source",
            Self::MemoryIngestPage => "memory_ingest_page",
            Self::TaskSummary => "task_summary",
            Self::AssessmentEvidence => "assessment_evidence",
        }
    }
}

/// Tenant, contact, and session provenance every sanitized entry inherits.
///
/// Identifiers only. Nothing here is derived from transcript content, so the
/// scope is safe to log and to key derived rows by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceScope {
    /// Tenant that owns the source session.
    pub tenant_id: TenantId,
    /// Contact the session belongs to, when the session has one.
    pub contact_id: Option<ContactId>,
    /// Session that produced the segment.
    pub session_id: SessionId,
    /// Assessed segment the evidence was drawn from.
    pub segment_id: SegmentId,
    /// Experience record the segment produced.
    pub experience_id: Uuid,
}

/// One sanitized transcript carrier with its exact event provenance.
///
/// `text` is already irreversibly redacted. The identifiers are not: an event id,
/// sequence number, tool name, and tool call id are routing identity, not
/// content, and derived rows need them to point a reviewer back at the exact
/// source event.
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedEntry {
    source: EvidenceSource,
    event_id: Option<Uuid>,
    sequence_num: Option<u64>,
    tool_name: Option<String>,
    tool_id: Option<ToolCallId>,
    success: Option<bool>,
    is_error: bool,
    text: String,
    structured: Option<serde_json::Value>,
    class: SensitivityClass,
    categories: Vec<PiiCategory>,
}

impl SanitizedEntry {
    /// Returns which carrier this entry came from.
    #[must_use]
    pub const fn source(&self) -> EvidenceSource {
        self.source
    }

    /// Returns the source event's identifier, when the entry came from an event.
    #[must_use]
    pub const fn event_id(&self) -> Option<Uuid> {
        self.event_id
    }

    /// Returns the source event's sequence number, when the entry came from an event.
    #[must_use]
    pub const fn sequence_num(&self) -> Option<u64> {
        self.sequence_num
    }

    /// Returns the tool this entry belongs to, for tool-derived entries.
    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    /// Returns the tool call this entry belongs to, for tool-derived entries.
    #[must_use]
    pub const fn tool_id(&self) -> Option<ToolCallId> {
        self.tool_id
    }

    /// Returns whether the tool call succeeded, for tool-result entries.
    #[must_use]
    pub const fn success(&self) -> Option<bool> {
        self.success
    }

    /// Returns whether a tool result carried an error envelope.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        self.is_error
    }

    /// Returns the irreversibly redacted text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the redacted argument tree, for tool-input entries.
    ///
    /// Downstream resource extraction and skill-engagement detection walk the
    /// argument structure rather than its rendering, so the redacted tree is
    /// carried alongside the text instead of being re-parsed out of it.
    #[must_use]
    pub const fn structured(&self) -> Option<&serde_json::Value> {
        self.structured.as_ref()
    }

    /// Returns the class the classifier assigned to this carrier before redaction.
    #[must_use]
    pub const fn class(&self) -> SensitivityClass {
        self.class
    }

    /// Returns the categories redacted out of this carrier.
    #[must_use]
    pub fn categories(&self) -> &[PiiCategory] {
        &self.categories
    }
}

/// Irreversibly sanitized learning evidence for one assessed segment.
///
/// Private fields and no public constructor by design: this type is the proof
/// that the content inside it passed the sanitization gate, and a value that
/// could be assembled from `String`s would be a proof of nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct SanitizedLearningEvidence {
    scope: EvidenceScope,
    entries: Vec<SanitizedEntry>,
    class: SensitivityClass,
    detector_version: String,
    redacted_categories: Vec<PiiCategory>,
}

impl SanitizedLearningEvidence {
    /// Returns the tenant, contact, session, segment, and experience scope.
    #[must_use]
    pub const fn scope(&self) -> &EvidenceScope {
        &self.scope
    }

    /// Returns every sanitized entry in source-event order.
    #[must_use]
    pub fn entries(&self) -> &[SanitizedEntry] {
        &self.entries
    }

    /// Returns entries from one carrier, in order.
    pub fn entries_from(
        &self,
        source: EvidenceSource,
    ) -> impl DoubleEndedIterator<Item = &SanitizedEntry> + '_ {
        self.entries
            .iter()
            .filter(move |entry| entry.source == source)
    }

    /// Returns the highest sensitivity class observed before redaction.
    #[must_use]
    pub const fn class(&self) -> SensitivityClass {
        self.class
    }

    /// Returns the classifier model and serving version that produced the result.
    #[must_use]
    pub fn detector_version(&self) -> &str {
        &self.detector_version
    }

    /// Returns every category redacted anywhere in this evidence, sorted.
    #[must_use]
    pub fn redacted_categories(&self) -> &[PiiCategory] {
        &self.redacted_categories
    }

    /// Returns the privacy policy revision this evidence was produced under.
    #[must_use]
    pub const fn policy_revision(&self) -> &'static str {
        LEARNING_PRIVACY_POLICY_REVISION
    }

    /// Returns the complete typed provenance for a candidate derived from this evidence.
    ///
    /// Every level of the closure the evidence actually saw is emitted: the
    /// contact when the session has one, the session, the assessed segment, the
    /// experience record, and each distinct source event, bounded so a long
    /// segment cannot write an unbounded number of provenance rows.
    ///
    /// The bound is the one place this can lose fidelity, and it is deliberate:
    /// the contact, session, segment, and experience references are always
    /// present and are what erasure and export actually traverse, so capping the
    /// event level narrows the audit detail without ever breaking the chain.
    #[must_use]
    pub fn candidate_sources(&self) -> Vec<LearningCandidateSourceRef> {
        let mut sources = Vec::new();
        if let Some(contact_id) = self.scope.contact_id {
            sources.push(LearningCandidateSourceRef::Contact { contact_id });
        }
        sources.push(LearningCandidateSourceRef::Session {
            session_id: self.scope.session_id,
        });
        sources.push(LearningCandidateSourceRef::TaskSegment {
            segment_id: self.scope.segment_id,
        });
        sources.push(LearningCandidateSourceRef::Experience {
            experience_id: self.scope.experience_id,
        });

        let mut seen = BTreeSet::new();
        for entry in &self.entries {
            let Some(event_id) = entry.event_id else {
                continue;
            };
            if !seen.insert(event_id) {
                continue;
            }
            if seen.len() > MAX_EVIDENCE_EVENT_SOURCES {
                break;
            }
            sources.push(LearningCandidateSourceRef::Event {
                event_id,
                session_id: self.scope.session_id,
            });
        }
        sources
    }

    /// Returns the number of tool calls observed in the segment.
    ///
    /// The dispatch and distillation gates both key off this depth, so it is
    /// derived from the sanitized entries rather than requiring the caller to
    /// keep the raw events alive alongside the evidence.
    #[must_use]
    pub fn tool_call_count(&self) -> usize {
        self.entries_from(EvidenceSource::ToolInput).count()
    }

    /// Returns the tool trajectory in call order.
    #[must_use]
    pub fn tool_trajectory(&self) -> Vec<String> {
        self.entries_from(EvidenceSource::ToolInput)
            .filter_map(|entry| entry.tool_name.clone())
            .collect()
    }

    /// Returns the reviewer-facing provenance block for a derived row.
    ///
    /// Derived rows must be able to name their source without embedding any of
    /// its content, so this is identifiers, counts, and the closed redaction
    /// vocabulary only.
    #[must_use]
    pub fn provenance_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "policy_revision": LEARNING_PRIVACY_POLICY_REVISION,
            "detector_version": self.detector_version,
            "sensitivity_class": self.class.as_str(),
            "redacted_categories": self
                .redacted_categories
                .iter()
                .map(|category| category.field_name())
                .collect::<Vec<_>>(),
            "tenant_id": self.scope.tenant_id.to_string(),
            "contact_id": self.scope.contact_id.map(|id| id.to_string()),
            "session_id": self.scope.session_id.to_string(),
            "segment_id": self.scope.segment_id.to_string(),
            "experience_id": self.scope.experience_id.to_string(),
            "source_event_ids": self
                .entries
                .iter()
                .filter_map(|entry| entry.event_id.map(|id| id.to_string()))
                .collect::<BTreeSet<_>>(),
            "entry_count": self.entries.len(),
        })
    }
}

/// Failure of one sanitized-evidence construction.
///
/// Carries the stable rejection code and the carrier that produced it, and
/// nothing else. A reviewer or an on-call engineer learns which kind of content
/// refused and why, without the refused bytes being copied into a durable error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "learning evidence rejected: carrier={} reason={}",
    .carrier.as_str(),
    .rejection.code()
)]
pub struct EvidenceRejection {
    /// Carrier whose content could not be released.
    pub carrier: EvidenceSource,
    /// Stable sanitization reason code.
    pub rejection: SanitizationRejection,
}

impl EvidenceRejection {
    /// Returns the stable sanitization reason code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.rejection.code()
    }
}

/// Extra non-event carriers that ride into the same sanitization gate.
///
/// The task summary and the assessment evidence summaries are model-written text
/// derived from the transcript, so they carry the same disclosure risk as the
/// transcript itself and must clear the same gate before reaching a provider.
#[derive(Debug, Clone, Default)]
pub struct SegmentNarrative<'a> {
    /// Assessed task summary, when the segment has one.
    pub task_summary: Option<&'a str>,
    /// Segment-assessment evidence summaries.
    pub assessment_summaries: &'a [String],
}

/// Sanitizes one segment's transcript into typed learning evidence.
///
/// Every text carrier in the segment is classified and redacted individually, so
/// one unreleasable carrier refuses the whole segment rather than being dropped
/// silently — a partial corpus would let a reviewer approve a draft built from
/// evidence they never saw was incomplete.
///
/// Tool arguments are walked as JSON and sanitized value-by-value, keeping the
/// object shape so downstream resource and skill-engagement extraction still
/// works on a redacted tree.
pub async fn sanitize_segment_evidence(
    classifier: &dyn PiiClassifier,
    scope: EvidenceScope,
    events: &[EventRecord],
    narrative: SegmentNarrative<'_>,
) -> Result<SanitizedLearningEvidence, EvidenceRejection> {
    let mut builder = EvidenceBuilder::new(scope);

    if let Some(summary) = narrative.task_summary {
        builder
            .push(classifier, EvidenceSource::TaskSummary, summary, None)
            .await?;
    }
    for summary in narrative.assessment_summaries {
        builder
            .push(
                classifier,
                EvidenceSource::AssessmentEvidence,
                summary,
                None,
            )
            .await?;
    }

    for record in events {
        builder.push_event(classifier, record).await?;
    }

    Ok(builder.finish())
}

/// Accumulates sanitized entries and the aggregate classification.
struct EvidenceBuilder {
    scope: EvidenceScope,
    entries: Vec<SanitizedEntry>,
    class: SensitivityClass,
    detector_version: Option<String>,
    categories: BTreeSet<&'static str>,
    category_values: Vec<PiiCategory>,
}

/// Per-entry event provenance and tool identity.
#[derive(Debug, Clone, Default)]
struct EntryContext {
    event_id: Option<Uuid>,
    sequence_num: Option<u64>,
    tool_id: Option<ToolCallId>,
    success: Option<bool>,
    is_error: bool,
    structured: Option<serde_json::Value>,
}

impl EvidenceBuilder {
    fn new(scope: EvidenceScope) -> Self {
        Self {
            scope,
            entries: Vec::new(),
            class: SensitivityClass::None,
            detector_version: None,
            categories: BTreeSet::new(),
            category_values: Vec::new(),
        }
    }

    /// Sanitizes one text carrier and records it with its provenance.
    async fn push(
        &mut self,
        classifier: &dyn PiiClassifier,
        source: EvidenceSource,
        text: &str,
        tool_name: Option<&str>,
    ) -> Result<(), EvidenceRejection> {
        self.push_with(classifier, source, text, tool_name, EntryContext::default())
            .await
    }

    async fn push_with(
        &mut self,
        classifier: &dyn PiiClassifier,
        source: EvidenceSource,
        text: &str,
        tool_name: Option<&str>,
        context: EntryContext,
    ) -> Result<(), EvidenceRejection> {
        let sanitized =
            sanitize_with(classifier, text)
                .await
                .map_err(|rejection| EvidenceRejection {
                    carrier: source,
                    rejection,
                })?;
        self.record(source, sanitized, tool_name, context);
        Ok(())
    }

    fn record(
        &mut self,
        source: EvidenceSource,
        sanitized: SanitizedText,
        tool_name: Option<&str>,
        context: EntryContext,
    ) {
        if sanitized.class().rank() > self.class.rank() {
            self.class = sanitized.class();
        }
        if self.detector_version.is_none() {
            self.detector_version = Some(sanitized.detector_version().to_string());
        }
        for category in sanitized.categories() {
            if self.categories.insert(category.field_name()) {
                self.category_values.push(*category);
            }
        }
        let class = sanitized.class();
        let categories = sanitized.categories().to_vec();
        self.entries.push(SanitizedEntry {
            source,
            event_id: context.event_id,
            sequence_num: context.sequence_num,
            tool_name: tool_name.map(str::to_string),
            tool_id: context.tool_id,
            success: context.success,
            is_error: context.is_error,
            text: sanitized.into_redacted(),
            structured: context.structured,
            class,
            categories,
        });
    }

    /// Extracts and sanitizes every text carrier in one event.
    async fn push_event(
        &mut self,
        classifier: &dyn PiiClassifier,
        record: &EventRecord,
    ) -> Result<(), EvidenceRejection> {
        let context = EntryContext {
            event_id: Some(record.id),
            sequence_num: Some(record.sequence_num),
            ..EntryContext::default()
        };
        match &record.event {
            Event::UserMessage { text, .. } => {
                self.push_with(classifier, EvidenceSource::UserMessage, text, None, context)
                    .await
            }
            Event::QueuedMessage { text, .. } => {
                self.push_with(
                    classifier,
                    EvidenceSource::QueuedMessage,
                    text,
                    None,
                    context,
                )
                .await
            }
            Event::BrainResponse { text, .. } => {
                self.push_with(
                    classifier,
                    EvidenceSource::AssistantMessage,
                    text,
                    None,
                    context,
                )
                .await
            }
            Event::BrainThinking { summary, .. } => {
                self.push_with(
                    classifier,
                    EvidenceSource::AssistantThinking,
                    summary,
                    None,
                    context,
                )
                .await
            }
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                let redacted =
                    sanitize_json_value(classifier, input)
                        .await
                        .map_err(|rejection| EvidenceRejection {
                            carrier: EvidenceSource::ToolInput,
                            rejection,
                        })?;
                let rendered = redacted.to_string();
                self.push_with(
                    classifier,
                    EvidenceSource::ToolInput,
                    &rendered,
                    Some(tool_name),
                    EntryContext {
                        tool_id: Some(*tool_id),
                        structured: Some(redacted),
                        ..context.clone()
                    },
                )
                .await
            }
            Event::ToolResult {
                tool_id,
                output,
                success,
                ..
            } => {
                self.push_with(
                    classifier,
                    EvidenceSource::ToolResult,
                    &output.to_text(),
                    None,
                    EntryContext {
                        tool_id: Some(*tool_id),
                        success: Some(*success),
                        is_error: output.is_error,
                        ..context.clone()
                    },
                )
                .await
            }
            Event::ToolError {
                tool_id,
                tool_name,
                error,
                ..
            } => {
                self.push_with(
                    classifier,
                    EvidenceSource::ToolError,
                    error,
                    Some(tool_name),
                    EntryContext {
                        tool_id: Some(*tool_id),
                        success: Some(false),
                        is_error: true,
                        ..context.clone()
                    },
                )
                .await
            }
            Event::MemoryRead { path, scope } | Event::MemoryWrite { path, scope, .. } => {
                self.push_with(
                    classifier,
                    EvidenceSource::MemoryPath,
                    path,
                    Some(scope),
                    context,
                )
                .await
            }
            Event::MemoryIngest {
                source_path,
                affected_pages,
                ..
            } => {
                self.push_with(
                    classifier,
                    EvidenceSource::MemoryIngestSource,
                    source_path,
                    Some("ingest_source"),
                    context.clone(),
                )
                .await?;
                for page in affected_pages {
                    self.push_with(
                        classifier,
                        EvidenceSource::MemoryIngestPage,
                        page,
                        None,
                        context.clone(),
                    )
                    .await?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn finish(self) -> SanitizedLearningEvidence {
        let mut redacted_categories = self.category_values;
        redacted_categories.sort_unstable_by_key(|category| category.field_name());
        SanitizedLearningEvidence {
            scope: self.scope,
            entries: self.entries,
            class: self.class,
            detector_version: self.detector_version.unwrap_or_else(|| "none".to_string()),
            redacted_categories,
        }
    }
}

/// Recursively sanitizes string leaves and object keys, preserving structure.
///
/// Keys are sanitized as well as values: a free-form argument map (environment
/// variables, header bags, MCP servers that accept arbitrary shapes) can put
/// caller-controlled text in key position. Two keys that redact to the same
/// placeholder collapse to one entry, which loses a duplicate placeholder but
/// cannot leak — the alternative, keeping keys raw, can.
fn sanitize_json_value<'a>(
    classifier: &'a dyn PiiClassifier,
    input: &'a serde_json::Value,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<serde_json::Value, SanitizationRejection>>
            + Send
            + 'a,
    >,
> {
    Box::pin(async move {
        match input {
            serde_json::Value::String(value) => {
                let sanitized = sanitize_with(classifier, value).await?;
                Ok(serde_json::Value::String(sanitized.into_redacted()))
            }
            serde_json::Value::Array(items) => {
                let mut redacted = Vec::with_capacity(items.len());
                for item in items {
                    redacted.push(sanitize_json_value(classifier, item).await?);
                }
                Ok(serde_json::Value::Array(redacted))
            }
            serde_json::Value::Object(map) => {
                let mut redacted = serde_json::Map::with_capacity(map.len());
                for (key, value) in map {
                    let sanitized_key = sanitize_with(classifier, key).await?.into_redacted();
                    redacted.insert(sanitized_key, sanitize_json_value(classifier, value).await?);
                }
                Ok(serde_json::Value::Object(redacted))
            }
            other => Ok(other.clone()),
        }
    })
}

/// Sanitizes raw events with the production heuristic, for this crate's unit tests.
///
/// Unit tests still have to go through the real gate — there is no back door that
/// builds evidence without classifying — so this is a scope wrapper, not a bypass.
#[cfg(test)]
pub(crate) async fn sanitize_for_tests(events: &[EventRecord]) -> SanitizedLearningEvidence {
    sanitize_segment_evidence(
        &moa_memory_pii::HeuristicPiiClassifier,
        EvidenceScope {
            tenant_id: TenantId::new(),
            contact_id: None,
            session_id: SessionId::new(),
            segment_id: SegmentId::new(),
            experience_id: Uuid::now_v7(),
        },
        events,
        SegmentNarrative::default(),
    )
    .await
    .expect("unit-test transcripts sanitize cleanly")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use moa_core::types::channel::Attachment;
    use moa_core::types::identifiers::ModelId;
    use moa_core::types::provider::ModelTier;
    use moa_core::types::tools::ToolOutput;
    use moa_memory_pii::{
        HeuristicPiiClassifier, PiiCategory, PiiResult, PiiSpan, Result as PiiCrateResult,
    };

    use super::*;

    /// Every identifier planted in the fixture transcript, one per carrier.
    const USER_EMAIL: &str = "alice@example.com";
    const QUEUED_EMAIL: &str = "queued-carol@example.com";
    const TOOL_INPUT_EMAIL: &str = "input-dave@example.com";
    const TOOL_RESULT_EMAIL: &str = "result-erin@example.com";
    const TOOL_ERROR_EMAIL: &str = "error-frank@example.com";
    const ASSISTANT_EMAIL: &str = "assistant-grace@example.com";
    const SUMMARY_EMAIL: &str = "summary-heidi@example.com";
    const ASSESSMENT_EMAIL: &str = "assessment-ivan@example.com";

    fn scope() -> EvidenceScope {
        EvidenceScope {
            tenant_id: TenantId::new(),
            contact_id: None,
            session_id: SessionId::new(),
            segment_id: SegmentId::new(),
            experience_id: Uuid::now_v7(),
        }
    }

    fn record(sequence_num: u64, event: Event) -> EventRecord {
        EventRecord {
            id: Uuid::now_v7(),
            session_id: SessionId::new(),
            sequence_num,
            event_type: event.event_type(),
            event,
            timestamp: chrono::Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        }
    }

    /// One segment with a distinct planted identifier in every text carrier.
    fn segment_with_pii_everywhere() -> Vec<EventRecord> {
        let tool_id = moa_core::types::identifiers::ToolCallId::new();
        vec![
            record(
                1,
                Event::UserMessage {
                    text: format!("please email {USER_EMAIL} about the migration"),
                    attachments: Vec::<Attachment>::new(),
                },
            ),
            record(
                2,
                Event::QueuedMessage {
                    text: format!("also cc {QUEUED_EMAIL}"),
                    attachments: Vec::<Attachment>::new(),
                    queued_at: chrono::Utc::now(),
                },
            ),
            record(
                3,
                Event::ToolCall {
                    tool_id,
                    provider_tool_use_id: None,
                    provider_thought_signature: None,
                    tool_name: "send_mail".to_string(),
                    input: serde_json::json!({ "to": TOOL_INPUT_EMAIL, "retries": 2 }),
                    hand_id: None,
                },
            ),
            record(
                4,
                Event::ToolResult {
                    tool_id,
                    provider_tool_use_id: None,
                    output: ToolOutput::text(
                        format!("delivered to {TOOL_RESULT_EMAIL}"),
                        Duration::from_millis(1),
                    ),
                    original_output_tokens: None,
                    success: true,
                    duration_ms: 1,
                    assessment: moa_core::types::security::ToolOutputAssessment::safe(),
                    capability: moa_core::types::security::ToolCapabilityId::builtin("send_mail"),
                },
            ),
            record(
                5,
                Event::ToolError {
                    tool_id,
                    provider_tool_use_id: None,
                    tool_name: "send_mail".to_string(),
                    error: format!("bounce for {TOOL_ERROR_EMAIL}"),
                    retryable: false,
                },
            ),
            record(
                6,
                Event::BrainResponse {
                    text: format!("notified {ASSISTANT_EMAIL} as requested"),
                    thought_signature: None,
                    model: ModelId::new("scripted-model"),
                    model_tier: ModelTier::Auxiliary,
                    input_tokens_uncached: 8,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 8,
                    cost_cents: 0,
                    duration_ms: 1,
                    llm_ttft_ms: None,
                },
            ),
        ]
    }

    async fn evidence_with_pii_everywhere() -> SanitizedLearningEvidence {
        let summary = format!("email {SUMMARY_EMAIL} about the migration");
        let assessment = vec![format!("verified delivery for {ASSESSMENT_EMAIL}")];
        sanitize_segment_evidence(
            &HeuristicPiiClassifier,
            scope(),
            &segment_with_pii_everywhere(),
            SegmentNarrative {
                task_summary: Some(&summary),
                assessment_summaries: &assessment,
            },
        )
        .await
        .expect("a segment whose only sensitivity is redactable PII sanitizes")
    }

    #[tokio::test]
    async fn candidate_sources_emit_every_closure_level_the_evidence_saw() {
        // Pins: the derivation chain a privacy erasure walks in reverse. Each of
        // contact, session, segment, and experience must appear, because each is a
        // level at which a subject can be reached — dropping any one silently
        // orphans every candidate built from this evidence for erasures that enter
        // through that level, and no count-based check would notice.
        //
        // Source events are asserted separately: they are the audit-detail level,
        // deliberately bounded, and their absence would not break the chain.
        let mut scope = scope();
        let contact_id = ContactId::new();
        scope.contact_id = Some(contact_id);
        let summary = "task summary".to_string();
        let evidence = sanitize_segment_evidence(
            &HeuristicPiiClassifier,
            scope.clone(),
            &segment_with_pii_everywhere(),
            SegmentNarrative {
                task_summary: Some(&summary),
                assessment_summaries: &[],
            },
        )
        .await
        .expect("fixture segment sanitizes");

        let sources = evidence.candidate_sources();
        for required in [
            LearningCandidateSourceRef::Contact { contact_id },
            LearningCandidateSourceRef::Session {
                session_id: scope.session_id,
            },
            LearningCandidateSourceRef::TaskSegment {
                segment_id: scope.segment_id,
            },
            LearningCandidateSourceRef::Experience {
                experience_id: scope.experience_id,
            },
        ] {
            assert!(
                sources.contains(&required),
                "closure level missing from candidate provenance: {required:?}"
            );
        }
        assert!(
            sources
                .iter()
                .any(|source| matches!(source, LearningCandidateSourceRef::Event { .. })),
            "event-level provenance must be recorded for a segment that carried events"
        );
    }

    #[tokio::test]
    async fn a_contactless_session_still_records_the_rest_of_the_closure() {
        // Pins: the contact level is the only optional one. A session with no
        // contact must still produce session, segment, and experience provenance
        // rather than an empty list — a candidate with no sources cannot be
        // committed at all, so silently dropping them would break filing outright.
        let evidence = evidence_with_pii_everywhere().await;
        let sources = evidence.candidate_sources();

        assert!(
            !sources
                .iter()
                .any(|source| matches!(source, LearningCandidateSourceRef::Contact { .. }))
        );
        assert!(
            sources
                .iter()
                .any(|source| matches!(source, LearningCandidateSourceRef::Session { .. }))
        );
        assert!(
            sources
                .iter()
                .any(|source| matches!(source, LearningCandidateSourceRef::TaskSegment { .. }))
        );
        assert!(
            sources
                .iter()
                .any(|source| matches!(source, LearningCandidateSourceRef::Experience { .. }))
        );
    }

    #[tokio::test]
    async fn pii_from_every_carrier_is_absent_from_the_sanitized_evidence() {
        // Pins: the acceptance list of carriers — user, queued, tool input, tool
        // result, tool error, assistant, summary, and assessment — is redacted, and
        // each identifier is checked individually so a gap in one carrier cannot
        // hide behind the others passing.
        let evidence = evidence_with_pii_everywhere().await;

        let rendered = evidence
            .entries()
            .iter()
            .map(|entry| entry.text())
            .collect::<Vec<_>>()
            .join("\n");

        for planted in [
            USER_EMAIL,
            QUEUED_EMAIL,
            TOOL_INPUT_EMAIL,
            TOOL_RESULT_EMAIL,
            TOOL_ERROR_EMAIL,
            ASSISTANT_EMAIL,
            SUMMARY_EMAIL,
            ASSESSMENT_EMAIL,
        ] {
            assert!(
                !rendered.contains(planted),
                "{planted} survived sanitization: {rendered}"
            );
        }
        assert_eq!(
            rendered.matches("[EMAIL_REDACTED]").count(),
            8,
            "every carrier should contribute exactly one redaction: {rendered}"
        );
    }

    #[tokio::test]
    async fn provenance_survives_sanitization() {
        // Pins: tenant/contact scope and exact session/segment/experience/event
        // provenance ride the evidence, so a derived row can name its source
        // without embedding any of its content.
        let expected = scope();
        let events = segment_with_pii_everywhere();
        let evidence = sanitize_segment_evidence(
            &HeuristicPiiClassifier,
            expected.clone(),
            &events,
            SegmentNarrative::default(),
        )
        .await
        .expect("fixture sanitizes");

        assert_eq!(evidence.scope(), &expected);
        assert_eq!(evidence.detector_version(), "moa-heuristic:v1");
        assert_eq!(evidence.class(), SensitivityClass::Pii);
        assert_eq!(evidence.redacted_categories(), &[PiiCategory::Email]);
        assert_eq!(evidence.policy_revision(), LEARNING_PRIVACY_POLICY_REVISION);

        let event_ids = events.iter().map(|record| record.id).collect::<Vec<_>>();
        for entry in evidence.entries() {
            let id = entry
                .event_id()
                .expect("event-derived entry carries its id");
            assert!(event_ids.contains(&id), "unknown event id {id}");
            assert!(entry.sequence_num().is_some());
        }

        let payload = evidence.provenance_payload();
        assert_eq!(
            payload["session_id"],
            expected.session_id.to_string().as_str()
        );
        assert_eq!(
            payload["segment_id"],
            expected.segment_id.to_string().as_str()
        );
        assert_eq!(
            payload["experience_id"],
            expected.experience_id.to_string().as_str()
        );
        assert_eq!(payload["policy_revision"], LEARNING_PRIVACY_POLICY_REVISION);
    }

    #[tokio::test]
    async fn tool_arguments_are_redacted_in_key_and_value_position() {
        // Pins: a free-form argument map can put caller text in key position, so
        // keys are sanitized too. The non-string leaves keep their type, because
        // downstream extraction walks this tree.
        let tool_id = moa_core::types::identifiers::ToolCallId::new();
        let events = vec![record(
            1,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "set_env".to_string(),
                input: serde_json::json!({ USER_EMAIL: TOOL_INPUT_EMAIL, "retries": 3 }),
                hand_id: None,
            },
        )];

        let evidence = sanitize_segment_evidence(
            &HeuristicPiiClassifier,
            scope(),
            &events,
            SegmentNarrative::default(),
        )
        .await
        .expect("tool arguments sanitize");

        let entry = evidence
            .entries_from(EvidenceSource::ToolInput)
            .next()
            .expect("one tool-input entry");
        let structured = entry.structured().expect("tool input carries its tree");
        assert!(!entry.text().contains(USER_EMAIL), "{}", entry.text());
        assert!(!entry.text().contains(TOOL_INPUT_EMAIL), "{}", entry.text());
        assert_eq!(structured["[EMAIL_REDACTED]"], "[EMAIL_REDACTED]");
        assert_eq!(structured["retries"], 3);
        assert_eq!(entry.tool_name(), Some("set_env"));
        assert_eq!(entry.tool_id(), Some(tool_id));
    }

    /// A classifier that counts its calls and always returns the same result.
    struct CountingClassifier {
        result: PiiResult,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PiiClassifier for CountingClassifier {
        async fn classify(&self, _text: &str) -> PiiCrateResult<PiiResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.result.clone())
        }
    }

    /// A classifier that always fails.
    struct FailingClassifier;

    #[async_trait::async_trait]
    impl PiiClassifier for FailingClassifier {
        async fn classify(&self, _text: &str) -> PiiCrateResult<PiiResult> {
            Err(moa_memory_pii::Error::Inference(
                "detector down".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn restricted_abstained_error_and_invalid_span_all_refuse_the_whole_segment() {
        // Pins: each unreleasable condition refuses the entire segment with its own
        // reason code rather than silently dropping the offending carrier, which
        // would hand a reviewer a corpus they cannot tell is incomplete.
        let events = segment_with_pii_everywhere();

        let restricted = CountingClassifier {
            result: PiiResult {
                class: SensitivityClass::Restricted,
                spans: Vec::new(),
                model_version: "test:v1".to_string(),
                abstained: false,
            },
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let rejection =
            sanitize_segment_evidence(&restricted, scope(), &events, SegmentNarrative::default())
                .await
                .expect_err("restricted content refuses");
        assert_eq!(rejection.code(), "restricted_class");
        assert_eq!(rejection.carrier, EvidenceSource::UserMessage);

        let abstaining = CountingClassifier {
            result: PiiResult::fail_closed("test:v1"),
            calls: Arc::new(AtomicUsize::new(0)),
        };
        assert_eq!(
            sanitize_segment_evidence(&abstaining, scope(), &events, SegmentNarrative::default())
                .await
                .expect_err("abstention refuses")
                .code(),
            "classifier_abstained"
        );

        assert_eq!(
            sanitize_segment_evidence(
                &FailingClassifier,
                scope(),
                &events,
                SegmentNarrative::default()
            )
            .await
            .expect_err("classifier failure refuses")
            .code(),
            "classifier_error"
        );

        let invalid_span = CountingClassifier {
            result: PiiResult {
                class: SensitivityClass::Pii,
                spans: vec![PiiSpan::new(0, 100_000, PiiCategory::Email, 0.9)],
                model_version: "test:v1".to_string(),
                abstained: false,
            },
            calls: Arc::new(AtomicUsize::new(0)),
        };
        assert_eq!(
            sanitize_segment_evidence(&invalid_span, scope(), &events, SegmentNarrative::default())
                .await
                .expect_err("an unapplicable span refuses")
                .code(),
            "span_out_of_range"
        );
    }

    #[tokio::test]
    async fn a_reserved_dlp_token_refuses_before_the_classifier_sees_it() {
        // Pins: a reversible DLP token in the transcript refuses the segment, and
        // does so without the classifier being consulted — the reversible and
        // irreversible mechanisms must never be mixed in one artifact.
        let calls = Arc::new(AtomicUsize::new(0));
        let classifier = CountingClassifier {
            result: PiiResult {
                class: SensitivityClass::None,
                spans: Vec::new(),
                model_version: "test:v1".to_string(),
                abstained: false,
            },
            calls: calls.clone(),
        };
        let events = vec![record(
            1,
            Event::UserMessage {
                text: format!(
                    "value {}MOA_DLP_1_2_3{}",
                    moa_memory_pii::sanitized::RESERVED_DLP_TOKEN_OPEN,
                    moa_memory_pii::sanitized::RESERVED_DLP_TOKEN_CLOSE
                ),
                attachments: Vec::<Attachment>::new(),
            },
        )];

        let rejection =
            sanitize_segment_evidence(&classifier, scope(), &events, SegmentNarrative::default())
                .await
                .expect_err("a reversible token refuses");

        assert_eq!(rejection.code(), "reserved_dlp_token");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejection_errors_carry_reason_codes_and_never_the_refused_content() {
        // Pins: durable errors and log lines carry the stable carrier + reason code
        // only. A rejection that echoed the input would re-leak exactly what the
        // gate refused to release.
        let events = segment_with_pii_everywhere();
        let rendered = sanitize_segment_evidence(
            &FailingClassifier,
            scope(),
            &events,
            SegmentNarrative::default(),
        )
        .await
        .expect_err("classifier failure refuses")
        .to_string();

        assert!(rendered.contains("reason=classifier_error"), "{rendered}");
        assert!(!rendered.contains(USER_EMAIL), "{rendered}");
        assert!(!rendered.contains("detector down"), "{rendered}");
    }

    #[tokio::test]
    async fn tool_call_count_and_trajectory_come_from_sanitized_entries() {
        // Pins: the dispatch and distillation gates read call depth and trajectory
        // off the evidence, so removing the raw-event parameter did not change what
        // those gates measure.
        let evidence = evidence_with_pii_everywhere().await;

        assert_eq!(evidence.tool_call_count(), 1);
        assert_eq!(evidence.tool_trajectory(), vec!["send_mail".to_string()]);
    }
}
