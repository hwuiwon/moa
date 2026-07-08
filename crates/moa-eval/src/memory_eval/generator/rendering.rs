//! Transcript rendering helpers for synthetic memory eval sessions.

use std::collections::BTreeMap;

use crate::memory_eval::corpus::{LedgerFact, TranscriptStyle};
use moa_memory_types::ScopeTier;

use super::{FactCategory, mix_u64};

pub(super) fn render_fact_transcript(
    transcript_style: TranscriptStyle,
    category: FactCategory,
    fact: &LedgerFact,
    facts_by_id: &BTreeMap<&str, &LedgerFact>,
) -> String {
    if let Some(canonical) = fact
        .restates
        .as_deref()
        .and_then(|canonical_id| facts_by_id.get(canonical_id))
    {
        return match transcript_style {
            // Marked restatements stay verbatim so exact fact-hash collapse
            // remains pinned by the deterministic heuristic lane.
            TranscriptStyle::Marked => {
                render_fact_transcript(transcript_style, category, canonical, facts_by_id)
            }
            // Natural restatements paraphrase: real users rephrase, so the
            // recorded lane exercises the write-time duplicate detector and
            // reinforcement instead of byte-identical text dedup.
            TranscriptStyle::Natural => {
                natural_frames::render_restatement(&fact.fact_id, canonical)
            }
        };
    }

    match transcript_style {
        TranscriptStyle::Marked => render_marked_fact_transcript(category, fact),
        TranscriptStyle::Natural => {
            natural_frames::render_fact(category, fact, superseded_object(fact, facts_by_id))
        }
    }
}

pub(super) fn distractor_transcript(session_key: &str) -> String {
    natural_frames::distractor(session_key)
}

pub(super) fn should_restate_dependency(fact_id: &str) -> bool {
    matches!(natural_frames::tenant_frame_index(fact_id), 1 | 3)
}

fn render_marked_fact_transcript(category: FactCategory, fact: &LedgerFact) -> String {
    let scope_marker = match fact.scope {
        ScopeTier::Tenant => "tenant shared ",
        ScopeTier::Contact => "contact private ",
    };
    match category {
        FactCategory::Supersession => format!(
            "Fact: {scope_marker}{} {} is {}. Supersedes: {}.",
            fact.subject,
            fact.predicate,
            fact.object,
            list_or_none(&fact.supersedes)
        ),
        FactCategory::Contradiction => format!(
            "Fact: {scope_marker}{} {} is {}. This is an unresolved contradictory claim.",
            fact.subject, fact.predicate, fact.object
        ),
        FactCategory::TenantShared => format!(
            "Fact: tenant shared {} {} is {}.",
            fact.subject, fact.predicate, fact.object
        ),
        FactCategory::UserPrivate => format!(
            "Fact: contact private {} {} is {}.",
            fact.subject, fact.predicate, fact.object
        ),
        FactCategory::Temporal => format!(
            "Fact: tenant shared {} {} is {} from {} until {}. Supersedes: {}.",
            fact.subject,
            fact.predicate,
            fact.object,
            fact.valid_from.to_rfc3339(),
            fact.valid_to
                .map(|valid_to| valid_to.to_rfc3339())
                .unwrap_or_else(|| "open-ended".to_string()),
            list_or_none(&fact.supersedes)
        ),
        FactCategory::Preference => format!(
            "Fact: preference {} {} is {}.",
            fact.subject, fact.predicate, fact.object
        ),
        FactCategory::Pii => format!(
            "Fact: pii {} {} is {}. Expected answer must be redacted.",
            fact.subject, fact.predicate, fact.object
        ),
    }
}

fn superseded_object<'a>(
    fact: &LedgerFact,
    facts_by_id: &'a BTreeMap<&str, &LedgerFact>,
) -> Option<&'a str> {
    fact.supersedes
        .first()
        .and_then(|fact_id| facts_by_id.get(fact_id.as_str()))
        .map(|superseded| superseded.object.as_str())
}

mod natural_frames {
    //! Natural-language transcript frame selection.

    use super::{FactCategory, LedgerFact, ScopeTier, mix_u64};

    const USER_FRAMES: &[&str] = &[
        "Just so you know, I prefer {object} when it comes to {subject}.",
        "For my work, {subject} should use {object}.",
        "I switched my {subject} to {object} recently.",
        "My {subject} {predicate_phrase} {object} these days.",
    ];
    const TENANT_FRAMES: &[&str] = &[
        "The team agreed that {subject} {predicate_phrase} {object}.",
        "Heads up everyone: {subject} now {predicate_phrase} {object}.",
        "We standardized {subject} on {object} last sprint.",
        "{subject} {predicate_phrase} {object} per the platform decision.",
    ];
    const UPDATE_FRAMES: &[&str] = &[
        "Quick update: {subject} {predicate_phrase} {object} now, not {old_object} anymore.",
        "Correction to earlier: {subject} moved to {object}.",
        // A stated change date lets model extraction emit `event_time`, so the
        // fact's `valid_from` reflects when the change actually happened.
        "Actually, {subject} moved to {object} back on {event_date}; {old_object} is out of date.",
    ];
    const RESTATEMENT_FRAMES: &[&str] = &[
        "Still true that {subject} {predicate_phrase} {object}.",
        "As before, {subject} {predicate_phrase} {object}.",
        "Reminder for the record: {subject} {predicate_phrase} {object}.",
    ];
    const DISTRACTORS: &[&str] = &[
        "Thanks, that all sounds reasonable to me.",
        "Busy week here, lots of meetings about nothing in particular.",
    ];

    pub(super) fn render_fact(
        category: FactCategory,
        fact: &LedgerFact,
        old_object: Option<&str>,
    ) -> String {
        if matches!(
            category,
            FactCategory::Supersession | FactCategory::Temporal
        ) && !fact.supersedes.is_empty()
        {
            let frame = select(&fact.fact_id, UPDATE_FRAMES);
            return apply_frame(
                frame,
                fact,
                old_object.unwrap_or("the previous value"),
                predicate_phrase(&fact.predicate),
            );
        }

        let frames = if fact.scope == ScopeTier::Contact {
            USER_FRAMES
        } else {
            TENANT_FRAMES
        };
        apply_frame(
            select(&fact.fact_id, frames),
            fact,
            "the previous value",
            predicate_phrase(&fact.predicate),
        )
    }

    /// Renders a paraphrased restatement of the canonical fact, keyed by the
    /// restating fact id so repeated restatements vary their phrasing.
    pub(super) fn render_restatement(key: &str, canonical: &LedgerFact) -> String {
        apply_frame(
            select(key, RESTATEMENT_FRAMES),
            canonical,
            "the previous value",
            predicate_phrase(&canonical.predicate),
        )
    }

    pub(super) fn distractor(session_key: &str) -> String {
        let index = stable_index(session_key, DISTRACTORS.len());
        DISTRACTORS[index].to_string()
    }

    pub(super) fn predicate_phrase(predicate: &str) -> &'static str {
        match predicate {
            "cache_backend_conflict" => "has cache backend",
            "contact_email" => "uses contact email",
            "depends_on" => "depends on",
            "deploy_target" => "deploys to",
            "on_call_primary" => "has primary on-call",
            "owned_by" => "is owned by",
            "private_repository" => "keeps private repository",
            "require_runbook" => "requires",
            "response_style" => "uses response style",
            _ => "is",
        }
    }

    pub(super) fn tenant_frame_index(key: &str) -> usize {
        stable_index(key, TENANT_FRAMES.len())
    }

    fn select<'a>(key: &str, frames: &'a [&str]) -> &'a str {
        let index = stable_index(key, frames.len());
        frames[index]
    }

    fn stable_index(key: &str, len: usize) -> usize {
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        for byte in key.bytes() {
            state ^= u64::from(byte);
            state = mix_u64(state);
        }
        (state as usize) % len
    }

    fn apply_frame(
        frame: &str,
        fact: &LedgerFact,
        old_object: &str,
        predicate_phrase: &str,
    ) -> String {
        frame
            .replace("{subject}", &fact.subject)
            .replace("{predicate_phrase}", predicate_phrase)
            .replace("{object}", &fact.object)
            .replace("{old_object}", old_object)
            .replace(
                "{event_date}",
                &fact.valid_from.format("%Y-%m-%d").to_string(),
            )
    }
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(",")
    }
}
