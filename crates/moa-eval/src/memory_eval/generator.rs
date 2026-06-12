//! Deterministic ledger-first memory evaluation corpus generation.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use moa_core::{ScopeTier, SessionId, UserId, WorkspaceId};
use moa_memory_graph::PiiClass;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use super::corpus::{
    CORPUS_SCHEMA_VERSION, CorpusManifest, CorpusProfile, LedgerFact, Probe, ProbeType,
    SyntheticSession, SyntheticTurn, TranscriptStyle, validate_corpus, write_ledger_jsonl,
    write_manifest_json, write_probes_jsonl, write_sessions_jsonl,
};
use crate::{EvalError, Result};

const REQUIRED_SEED_COUNT: usize = 3;
const PR_USER_COUNT: usize = 5;
const PR_WORKSPACE_COUNT: usize = 2;
const FULL_USER_COUNT: usize = 50;
const FULL_WORKSPACE_COUNT: usize = 3;
const FULL_MIN_PROBES: usize = 600;
const FULL_MAX_PROBES: usize = 1_000;
const BASE_UNIX_SECONDS: i64 = 1_767_225_600;
const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;

const COMPONENTS: &[&str] = &[
    "billing-api",
    "search-indexer",
    "checkout-worker",
    "audit-shipper",
    "profile-service",
    "notification-router",
    "catalog-sync",
    "policy-engine",
];
const DEPLOY_TARGETS: &[(&str, &str)] = &[
    ("staging", "production-canary"),
    ("legacy-cluster", "gke-primary"),
    ("blue", "green"),
    ("us-central1", "us-east1"),
];
const RUNBOOKS: &[&str] = &[
    "runbook/payments-canary",
    "runbook/search-rollout",
    "runbook/audit-replay",
    "runbook/policy-release",
];
const CACHE_BACKENDS: &[(&str, &str)] = &[
    ("redis", "valkey"),
    ("memcached", "dragonfly"),
    ("postgres-cache", "read-through-cache"),
    ("local-lru", "distributed-cache"),
];
const REPOSITORIES: &[&str] = &[
    "repo/mobile-client",
    "repo/control-plane",
    "repo/data-pipeline",
    "repo/internal-tools",
    "repo/search-platform",
];
const RESPONSE_STYLES: &[&str] = &[
    "concise bullets",
    "step-by-step checklists",
    "short paragraphs",
    "tables for comparisons",
    "commands first",
];
const EDITORS: &[&str] = &["nvim", "zed", "vscode", "helix", "emacs"];
const ON_CALLS: &[(&str, &str)] = &[
    ("Avery", "Blair"),
    ("Casey", "Devon"),
    ("Elliot", "Finley"),
    ("Gray", "Harper"),
];
const LIBRARIES: &[&str] = &[
    "lib-ledger-core",
    "lib-search-flow",
    "lib-audit-wire",
    "lib-policy-kit",
    "lib-profile-cache",
    "lib-catalog-sync",
];
const OWNER_TEAMS: &[&str] = &[
    "payments-platform",
    "search-infra",
    "audit-systems",
    "policy-runtime",
    "profile-experience",
    "catalog-ops",
];

/// A generated memory-evaluation corpus and its derived embedding inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedMemoryEvalCorpus {
    /// Directory manifest document.
    pub manifest: CorpusManifest,
    /// Ledger-first fact schedule.
    pub ledger: Vec<LedgerFact>,
    /// Synthetic transcripts rendered from the ledger schedule.
    pub sessions: Vec<SyntheticSession>,
    /// Retrieval and answer probes derived from the ledger.
    pub probes: Vec<Probe>,
    /// Text inputs that later tasks can embed without re-rendering the corpus.
    pub embedding_inputs: Vec<EmbeddingInput>,
}

impl GeneratedMemoryEvalCorpus {
    /// Validates profile shape, corpus schema, and embedding input references.
    pub fn validate(&self) -> Result<()> {
        validate_corpus(&self.manifest, &self.ledger, &self.sessions, &self.probes)?;
        validate_embedding_inputs(&self.embedding_inputs, &self.ledger, &self.probes)?;
        validate_profile_shape(self)
    }
}

/// One text input to include in `embedding_inputs.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingInput {
    /// Stable input identifier.
    pub input_id: String,
    /// Source record kind.
    pub kind: EmbeddingInputKind,
    /// Text that should be embedded by a later deterministic fixture pass.
    pub text: String,
    /// Ledger facts referenced by this input.
    #[serde(default)]
    pub fact_ids: Vec<String>,
    /// Probes referenced by this input.
    #[serde(default)]
    pub probe_ids: Vec<String>,
}

impl EmbeddingInput {
    /// Validates field-level invariants for one embedding input.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty("embedding input input_id", &self.input_id)?;
        ensure_non_empty("embedding input text", &self.text)?;
        for fact_id in &self.fact_ids {
            ensure_non_empty("embedding input fact_id", fact_id)?;
        }
        for probe_id in &self.probe_ids {
            ensure_non_empty("embedding input probe_id", probe_id)?;
        }
        Ok(())
    }
}

/// Source kind for a generated embedding input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingInputKind {
    /// Input rendered from a ledger fact.
    Fact,
    /// Input rendered from a probe query.
    Probe,
}

/// Generates a deterministic memory-evaluation corpus for a profile and seeds.
pub fn generate_memory_eval_corpus(
    profile: CorpusProfile,
    seeds: Vec<u64>,
) -> Result<GeneratedMemoryEvalCorpus> {
    generate_memory_eval_corpus_with_style(profile, seeds, TranscriptStyle::Marked)
}

/// Generates a deterministic memory-evaluation corpus with a transcript style.
pub fn generate_memory_eval_corpus_with_style(
    profile: CorpusProfile,
    seeds: Vec<u64>,
    transcript_style: TranscriptStyle,
) -> Result<GeneratedMemoryEvalCorpus> {
    validate_seeds(&seeds)?;
    let settings = ProfileSettings::new(profile);
    let users = build_users(profile, settings.user_count);
    let workspaces = build_workspaces(profile, settings.workspace_count);
    let mut builder = ScheduleBuilder::default();
    let mut workspace_refs = BTreeMap::new();
    let mut user_refs = BTreeMap::new();

    for (seed_index, seed) in seeds.iter().copied().enumerate() {
        for workspace_index in 0..settings.workspace_count {
            let refs = schedule_workspace_facts(
                &mut builder,
                profile,
                seed_index,
                seed,
                workspace_index,
                &users,
                &workspaces,
            )?;
            workspace_refs.insert((seed_index, workspace_index), refs);
        }

        for user_index in 0..settings.user_count {
            let refs = schedule_user_facts(
                &mut builder,
                profile,
                seed_index,
                seed,
                user_index,
                &users,
                &workspaces,
            )?;
            user_refs.insert((seed_index, user_index), refs);
        }
    }

    let schedule = builder.finish();
    validate_schedule_categories(&schedule.facts)?;
    let sessions = schedule.render_sessions(transcript_style);
    let probes = build_probes(
        profile,
        &seeds,
        &users,
        &workspaces,
        &workspace_refs,
        &user_refs,
    )?;
    let mut ledger = schedule.ledger();
    let mut probes = probes;
    link_recurring_fact_eras(&mut ledger, &mut probes);
    assign_quality_priors(&mut ledger, &probes)?;
    let embedding_inputs = build_embedding_inputs(&ledger, &probes);
    let manifest = CorpusManifest {
        version: CORPUS_SCHEMA_VERSION,
        corpus_id: corpus_id(profile, transcript_style, &seeds),
        profile,
        description: format!(
            "{} deterministic ledger-first memory evaluation corpus with {} transcripts",
            profile_slug(profile),
            transcript_style_slug(transcript_style)
        ),
        seeds,
        transcript_style,
    };
    let corpus = GeneratedMemoryEvalCorpus {
        manifest,
        ledger,
        sessions,
        probes,
        embedding_inputs,
    };
    corpus.validate()?;
    Ok(corpus)
}

/// Writes all generated corpus files into an output directory.
pub async fn write_memory_eval_corpus(
    output_dir: &Path,
    corpus: &GeneratedMemoryEvalCorpus,
) -> Result<()> {
    corpus.validate()?;
    tokio::fs::create_dir_all(output_dir)
        .await
        .map_err(|source| io_error(output_dir, source))?;
    write_manifest_json(&output_dir.join("manifest.json"), &corpus.manifest).await?;
    write_ledger_jsonl(&output_dir.join("ledger.jsonl"), &corpus.ledger).await?;
    write_sessions_jsonl(&output_dir.join("sessions.jsonl"), &corpus.sessions).await?;
    write_probes_jsonl(
        &output_dir.join("probes.jsonl"),
        &corpus.probes,
        &corpus.ledger,
    )
    .await?;
    write_embedding_inputs_jsonl(
        &output_dir.join("embedding_inputs.jsonl"),
        &corpus.embedding_inputs,
        &corpus.ledger,
        &corpus.probes,
    )
    .await
}

/// Reads and validates `embedding_inputs.jsonl`.
pub async fn read_embedding_inputs_jsonl(
    path: &Path,
    facts: &[LedgerFact],
    probes: &[Probe],
) -> Result<Vec<EmbeddingInput>> {
    let inputs = read_jsonl(path).await?;
    validate_embedding_inputs(&inputs, facts, probes)?;
    Ok(inputs)
}

/// Writes and validates `embedding_inputs.jsonl`.
pub async fn write_embedding_inputs_jsonl(
    path: &Path,
    inputs: &[EmbeddingInput],
    facts: &[LedgerFact],
    probes: &[Probe],
) -> Result<()> {
    validate_embedding_inputs(inputs, facts, probes)?;
    write_jsonl(path, inputs).await
}

#[derive(Debug, Clone, Copy)]
struct ProfileSettings {
    user_count: usize,
    workspace_count: usize,
    multi_hop_pairs_per_user: usize,
}

impl ProfileSettings {
    fn new(profile: CorpusProfile) -> Self {
        match profile {
            CorpusProfile::Pr => Self {
                user_count: PR_USER_COUNT,
                workspace_count: PR_WORKSPACE_COUNT,
                multi_hop_pairs_per_user: 2,
            },
            CorpusProfile::Full => Self {
                user_count: FULL_USER_COUNT,
                workspace_count: FULL_WORKSPACE_COUNT,
                multi_hop_pairs_per_user: 1,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FactCategory {
    Supersession,
    Contradiction,
    WorkspaceShared,
    UserPrivate,
    Temporal,
    Preference,
    Pii,
}

#[derive(Debug, Clone)]
struct ScheduledFact {
    category: FactCategory,
    session_key: String,
    fact: LedgerFact,
}

#[derive(Debug, Clone)]
struct SessionPlan {
    session_id: SessionId,
    workspace_id: WorkspaceId,
    user_id: UserId,
}

#[derive(Debug, Clone)]
struct SessionAssignment {
    key: String,
    plan: SessionPlan,
}

#[derive(Debug, Default)]
struct ScheduleBuilder {
    sessions: BTreeMap<String, SessionPlan>,
    next_turn_seq: BTreeMap<String, u64>,
    facts: Vec<ScheduledFact>,
}

impl ScheduleBuilder {
    fn push_fact(&mut self, assignment: SessionAssignment, draft: FactDraft) -> Result<String> {
        self.sessions
            .entry(assignment.key.clone())
            .or_insert_with(|| assignment.plan.clone());
        let turn_seq = next_turn_seq(&mut self.next_turn_seq, &assignment.key)?;
        let fact_id = draft.fact_id;
        let fact = LedgerFact {
            workspace_id: assignment.plan.workspace_id,
            user_id: assignment.plan.user_id,
            scope: draft.scope,
            fact_id: fact_id.clone(),
            valid_from: draft.valid_from,
            valid_to: draft.valid_to,
            subject: draft.subject,
            predicate: draft.predicate,
            object: draft.object,
            answer: draft.answer,
            supersedes: draft.supersedes,
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: assignment.plan.session_id,
            source_turn_seq: turn_seq,
            pii_class: draft.pii_class,
            expected_redacted: draft.expected_redacted,
        };
        self.facts.push(ScheduledFact {
            category: draft.category,
            session_key: assignment.key,
            fact,
        });
        Ok(fact_id)
    }

    fn push_restatement(
        &mut self,
        assignment: SessionAssignment,
        canonical_fact_id: &str,
        draft: FactDraft,
    ) -> Result<String> {
        self.sessions
            .entry(assignment.key.clone())
            .or_insert_with(|| assignment.plan.clone());
        let turn_seq = next_turn_seq(&mut self.next_turn_seq, &assignment.key)?;
        let fact_id = draft.fact_id;
        let fact = LedgerFact {
            workspace_id: assignment.plan.workspace_id,
            user_id: assignment.plan.user_id,
            scope: draft.scope,
            fact_id: fact_id.clone(),
            valid_from: draft.valid_from,
            valid_to: draft.valid_to,
            subject: draft.subject,
            predicate: draft.predicate,
            object: draft.object,
            answer: draft.answer,
            supersedes: draft.supersedes,
            restates: Some(canonical_fact_id.to_string()),
            prior_uses: None,
            prior_successes: None,
            source_session_id: assignment.plan.session_id,
            source_turn_seq: turn_seq,
            pii_class: draft.pii_class,
            expected_redacted: draft.expected_redacted,
        };
        self.facts.push(ScheduledFact {
            category: draft.category,
            session_key: assignment.key,
            fact,
        });
        Ok(fact_id)
    }

    fn finish(self) -> FactSchedule {
        FactSchedule {
            sessions: self.sessions,
            facts: self.facts,
        }
    }
}

#[derive(Debug, Clone)]
struct FactSchedule {
    sessions: BTreeMap<String, SessionPlan>,
    facts: Vec<ScheduledFact>,
}

impl FactSchedule {
    fn ledger(&self) -> Vec<LedgerFact> {
        self.facts
            .iter()
            .map(|scheduled| scheduled.fact.clone())
            .collect()
    }

    fn render_sessions(&self, transcript_style: TranscriptStyle) -> Vec<SyntheticSession> {
        let facts_by_id = self
            .facts
            .iter()
            .map(|scheduled| (scheduled.fact.fact_id.as_str(), &scheduled.fact))
            .collect::<BTreeMap<_, _>>();
        self.sessions
            .iter()
            .filter_map(|(session_key, plan)| {
                let mut turns = self
                    .facts
                    .iter()
                    .filter(|scheduled| scheduled.session_key == *session_key)
                    .map(|scheduled| SyntheticTurn {
                        turn_seq: scheduled.fact.source_turn_seq,
                        transcript: render_fact_transcript(
                            transcript_style,
                            scheduled.category,
                            &scheduled.fact,
                            &facts_by_id,
                        ),
                        fact_ids: vec![scheduled.fact.fact_id.clone()],
                    })
                    .collect::<Vec<_>>();
                turns.sort_by_key(|turn| turn.turn_seq);
                if turns.is_empty() {
                    None
                } else {
                    if transcript_style == TranscriptStyle::Natural {
                        let turn_seq = turns
                            .last()
                            .and_then(|turn| turn.turn_seq.checked_add(1))
                            .unwrap_or(u64::MAX);
                        turns.push(SyntheticTurn {
                            turn_seq,
                            transcript: natural_frames::distractor(session_key),
                            fact_ids: Vec::new(),
                        });
                    }
                    Some(SyntheticSession {
                        session_id: plan.session_id,
                        workspace_id: plan.workspace_id.clone(),
                        user_id: plan.user_id.clone(),
                        turns,
                    })
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct FactDraft {
    category: FactCategory,
    fact_id: String,
    scope: ScopeTier,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
    subject: String,
    predicate: String,
    object: String,
    answer: String,
    supersedes: Vec<String>,
    pii_class: PiiClass,
    expected_redacted: bool,
}

#[derive(Debug, Clone)]
struct WorkspaceFactRefs {
    component: String,
    deploy_old_fact_id: String,
    deploy_new_fact_id: String,
    deploy_target: String,
    runbook_fact_id: String,
    runbook: String,
    contradiction_a_fact_id: String,
    contradiction_b_fact_id: String,
    contradiction_subject: String,
    temporal_old_fact_id: String,
    temporal_new_fact_id: String,
    temporal_subject: String,
    temporal_month_as_of: DateTime<Utc>,
    temporal_iso_as_of: DateTime<Utc>,
    temporal_current_as_of: DateTime<Utc>,
    temporal_answer: String,
    temporal_current_answer: String,
}

#[derive(Debug, Clone)]
struct UserFactRefs {
    workspace_index: usize,
    private_fact_id: String,
    private_answer: String,
    preference_fact_id: String,
    preference_answer: String,
    pii_fact_id: String,
    pii_answer: String,
    multi_hop_pairs: Vec<MultiHopFactRefs>,
}

#[derive(Debug, Clone)]
struct MultiHopFactRefs {
    depends_fact_id: String,
    owner_fact_id: String,
    service: String,
    library: String,
    team: String,
}

#[derive(Debug)]
struct StableRng {
    state: u64,
}

impl StableRng {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix_u64(self.state)
    }

    fn index(&mut self, len: usize) -> Result<usize> {
        if len == 0 {
            return invalid_config("cannot choose from an empty template set");
        }
        Ok((self.next_u64() as usize) % len)
    }
}

fn schedule_workspace_facts(
    builder: &mut ScheduleBuilder,
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    workspace_index: usize,
    users: &[UserId],
    workspaces: &[WorkspaceId],
) -> Result<WorkspaceFactRefs> {
    let mut rng = StableRng::new(seed ^ mix_u64(workspace_index as u64) ^ 0xA11C_E5ED);
    let component = choose(&mut rng, COMPONENTS)?;
    let (old_target, new_target) = choose_pair(&mut rng, DEPLOY_TARGETS)?;
    let (cache_a, cache_b) = choose_pair(&mut rng, CACHE_BACKENDS)?;
    let runbook = choose(&mut rng, RUNBOOKS)?;
    let (old_on_call, new_on_call) = choose_pair(&mut rng, ON_CALLS)?;
    let author_index = first_user_for_workspace(workspace_index, users, workspaces.len())?;
    let assignment = workspace_session(
        profile,
        seed_index,
        seed,
        workspace_index,
        author_index,
        users,
        workspaces,
    );
    let base_day = seed_index as i64 * 40 + workspace_index as i64 * 10;
    let deploy_old_from = fixed_time(base_day, 0)?;
    let deploy_new_from = fixed_time(base_day + 7, 0)?;
    let temporal_old_from = first_day_after_months(seed_index * 8 + workspace_index * 2)?;
    let temporal_new_from = first_day_of_next_month(temporal_old_from)?;
    let temporal_iso_as_of = temporal_old_from + Duration::days(5);
    let temporal_month_as_of = temporal_old_from;
    let temporal_current_as_of = temporal_new_from + Duration::days(1);
    let temporal_subject = format!("{component}-support-rotation");

    let deploy_old_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "deploy-target-v1",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Supersession,
            fact_id: deploy_old_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: deploy_old_from,
            valid_to: Some(deploy_new_from),
            subject: component.to_string(),
            predicate: "deploy_target".to_string(),
            object: old_target.to_string(),
            answer: format!("Before the update, {component} deployed to {old_target}."),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let deploy_new_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "deploy-target-v2",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Supersession,
            fact_id: deploy_new_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: deploy_new_from,
            valid_to: None,
            subject: component.to_string(),
            predicate: "deploy_target".to_string(),
            object: new_target.to_string(),
            answer: format!("The latest deploy target for {component} is {new_target}."),
            supersedes: vec![deploy_old_fact_id.clone()],
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let runbook_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "workspace-runbook",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::WorkspaceShared,
            fact_id: runbook_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: fixed_time(base_day + 1, 0)?,
            valid_to: None,
            subject: format!("{component} deploys"),
            predicate: "require_runbook".to_string(),
            object: runbook.to_string(),
            answer: format!("{component} deploys require {runbook}."),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let contradiction_subject = format!("{component} cache backend");
    let contradiction_a_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "cache-conflict-a",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Contradiction,
            fact_id: contradiction_a_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: fixed_time(base_day + 3, 0)?,
            valid_to: None,
            subject: contradiction_subject.clone(),
            predicate: "cache_backend_conflict".to_string(),
            object: cache_a.to_string(),
            answer: format!("{component} has a conflicting cache backend claim: {cache_a}."),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let contradiction_b_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "cache-conflict-b",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Contradiction,
            fact_id: contradiction_b_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: fixed_time(base_day + 4, 0)?,
            valid_to: None,
            subject: contradiction_subject.clone(),
            predicate: "cache_backend_conflict".to_string(),
            object: cache_b.to_string(),
            answer: format!("{component} has a conflicting cache backend claim: {cache_b}."),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let temporal_old_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "on-call-primary-v1",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Temporal,
            fact_id: temporal_old_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: temporal_old_from,
            valid_to: Some(temporal_new_from),
            subject: temporal_subject.clone(),
            predicate: "on_call_primary".to_string(),
            object: old_on_call.to_string(),
            answer: format!("At that time, {old_on_call} was primary on-call for {component}."),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let temporal_new_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        None,
        "on-call-primary-v2",
    );
    builder.push_fact(
        assignment,
        FactDraft {
            category: FactCategory::Temporal,
            fact_id: temporal_new_fact_id.clone(),
            scope: ScopeTier::Workspace,
            valid_from: temporal_new_from,
            valid_to: None,
            subject: temporal_subject.clone(),
            predicate: "on_call_primary".to_string(),
            object: new_on_call.to_string(),
            answer: format!("{new_on_call} is now primary on-call for {component}."),
            supersedes: vec![temporal_old_fact_id.clone()],
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    Ok(WorkspaceFactRefs {
        component: component.to_string(),
        deploy_old_fact_id,
        deploy_new_fact_id,
        deploy_target: new_target.to_string(),
        runbook_fact_id,
        runbook: runbook.to_string(),
        contradiction_a_fact_id,
        contradiction_b_fact_id,
        contradiction_subject,
        temporal_old_fact_id,
        temporal_new_fact_id,
        temporal_subject,
        temporal_month_as_of,
        temporal_iso_as_of,
        temporal_current_as_of,
        temporal_answer: format!(
            "At that time, {old_on_call} was primary on-call for {component}."
        ),
        temporal_current_answer: format!("{new_on_call} is now primary on-call for {component}."),
    })
}

fn schedule_user_facts(
    builder: &mut ScheduleBuilder,
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    user_index: usize,
    users: &[UserId],
    workspaces: &[WorkspaceId],
) -> Result<UserFactRefs> {
    let settings = ProfileSettings::new(profile);
    let workspace_index = workspace_index_for_user(user_index, workspaces.len());
    let mut rng = StableRng::new(seed ^ mix_u64(user_index as u64) ^ 0x515E_D123);
    let repository = choose(&mut rng, REPOSITORIES)?;
    let style = choose(&mut rng, RESPONSE_STYLES)?;
    let editor = choose(&mut rng, EDITORS)?;
    let assignment = user_session(
        profile,
        seed_index,
        seed,
        workspace_index,
        user_index,
        users,
        workspaces,
    );
    let base_day = seed_index as i64 * 40 + user_index as i64;
    let user_label = user_label(user_index);

    let private_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        Some(user_index),
        "private-repository",
    );
    let private_answer = format!("{user_label}'s private work repository is {repository}.");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::UserPrivate,
            fact_id: private_fact_id.clone(),
            scope: ScopeTier::User,
            valid_from: fixed_time(base_day + 5, 0)?,
            valid_to: None,
            subject: user_label.clone(),
            predicate: "private_repository".to_string(),
            object: repository.to_string(),
            answer: private_answer.clone(),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let preference_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        Some(user_index),
        "response-style",
    );
    let preference_answer = format!("Use {style} and {editor} examples when helping {user_label}.");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Preference,
            fact_id: preference_fact_id.clone(),
            scope: ScopeTier::User,
            valid_from: fixed_time(base_day + 6, 0)?,
            valid_to: None,
            subject: user_label.clone(),
            predicate: "response_style".to_string(),
            object: format!("{style}; editor={editor}"),
            answer: preference_answer.clone(),
            supersedes: Vec::new(),
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
    )?;

    let pii_fact_id = fact_id(
        profile,
        seed_index,
        workspace_index,
        Some(user_index),
        "contact-email",
    );
    let email = format!(
        "{}.seed{}.user{}@example.invalid",
        profile_slug(profile),
        seed_index,
        user_index
    );
    let pii_answer = format!("{user_label}'s contact email is [EMAIL].");
    builder.push_fact(
        assignment,
        FactDraft {
            category: FactCategory::Pii,
            fact_id: pii_fact_id.clone(),
            scope: ScopeTier::User,
            valid_from: fixed_time(base_day + 7, 0)?,
            valid_to: None,
            subject: user_label,
            predicate: "contact_email".to_string(),
            object: email,
            answer: pii_answer.clone(),
            supersedes: Vec::new(),
            pii_class: PiiClass::Pii,
            expected_redacted: true,
        },
    )?;

    let mut multi_hop_pairs = Vec::new();
    for pair_index in 0..settings.multi_hop_pairs_per_user {
        let component = choose(&mut rng, COMPONENTS)?;
        let library = choose(&mut rng, LIBRARIES)?;
        let team = choose(&mut rng, OWNER_TEAMS)?;
        let service = format!("{component}-dep-{seed_index}-{user_index}-{pair_index}");
        let dependency_fact_id = fact_id(
            profile,
            seed_index,
            workspace_index,
            Some(user_index),
            &format!("depends-on-{pair_index}"),
        );
        builder.push_fact(
            user_session(
                profile,
                seed_index,
                seed,
                workspace_index,
                user_index,
                users,
                workspaces,
            ),
            FactDraft {
                category: FactCategory::WorkspaceShared,
                fact_id: dependency_fact_id.clone(),
                scope: ScopeTier::Workspace,
                valid_from: fixed_time(base_day + 8 + pair_index as i64 * 2, 0)?,
                valid_to: None,
                subject: service.clone(),
                predicate: "depends_on".to_string(),
                object: library.to_string(),
                answer: format!("{service} depends on {library}."),
                supersedes: Vec::new(),
                pii_class: PiiClass::None,
                expected_redacted: false,
            },
        )?;
        if should_restate_dependency(&dependency_fact_id) {
            let restatement_fact_id = fact_id(
                profile,
                seed_index,
                workspace_index,
                Some(user_index),
                &format!("depends-on-{pair_index}-restatement"),
            );
            builder.push_restatement(
                user_aux_session(
                    profile,
                    seed_index,
                    seed,
                    user_index,
                    19 + pair_index as u128,
                    users,
                    workspaces,
                ),
                &dependency_fact_id,
                FactDraft {
                    category: FactCategory::WorkspaceShared,
                    fact_id: restatement_fact_id,
                    scope: ScopeTier::Workspace,
                    valid_from: fixed_time(base_day + 35 + pair_index as i64 * 2, 0)?,
                    valid_to: None,
                    subject: service.clone(),
                    predicate: "depends_on".to_string(),
                    object: library.to_string(),
                    answer: format!("{service} depends on {library}."),
                    supersedes: Vec::new(),
                    pii_class: PiiClass::None,
                    expected_redacted: false,
                },
            )?;
        }

        let owner_fact_id = fact_id(
            profile,
            seed_index,
            workspace_index,
            Some(user_index),
            &format!("owned-by-{pair_index}"),
        );
        builder.push_fact(
            user_aux_session(
                profile,
                seed_index,
                seed,
                user_index,
                3 + pair_index as u128,
                users,
                workspaces,
            ),
            FactDraft {
                category: FactCategory::WorkspaceShared,
                fact_id: owner_fact_id.clone(),
                scope: ScopeTier::Workspace,
                valid_from: fixed_time(base_day + 9 + pair_index as i64 * 2, 0)?,
                valid_to: None,
                subject: library.to_string(),
                predicate: "owned_by".to_string(),
                object: team.to_string(),
                answer: format!("The {team} team owns {library}."),
                supersedes: Vec::new(),
                pii_class: PiiClass::None,
                expected_redacted: false,
            },
        )?;
        multi_hop_pairs.push(MultiHopFactRefs {
            depends_fact_id: dependency_fact_id,
            owner_fact_id,
            service,
            library: library.to_string(),
            team: team.to_string(),
        });
    }

    Ok(UserFactRefs {
        workspace_index,
        private_fact_id,
        private_answer,
        preference_fact_id,
        preference_answer,
        pii_fact_id,
        pii_answer,
        multi_hop_pairs,
    })
}

fn build_probes(
    profile: CorpusProfile,
    seeds: &[u64],
    users: &[UserId],
    workspaces: &[WorkspaceId],
    workspace_refs: &BTreeMap<(usize, usize), WorkspaceFactRefs>,
    user_refs: &BTreeMap<(usize, usize), UserFactRefs>,
) -> Result<Vec<Probe>> {
    let mut probes = Vec::new();
    for (seed_index, _) in seeds.iter().enumerate() {
        for user_index in 0..users.len() {
            let refs = user_refs
                .get(&(seed_index, user_index))
                .ok_or_else(|| missing_reference("user fact refs", seed_index, user_index))?;
            let workspace = workspaces
                .get(refs.workspace_index)
                .ok_or_else(|| missing_reference("workspace", seed_index, refs.workspace_index))?;
            let workspace_fact_refs = workspace_refs
                .get(&(seed_index, refs.workspace_index))
                .ok_or_else(|| {
                    missing_reference("workspace fact refs", seed_index, refs.workspace_index)
                })?;
            let user = users
                .get(user_index)
                .ok_or_else(|| missing_reference("user", seed_index, user_index))?;
            let target_user_index = next_user_in_workspace(
                user_index,
                refs.workspace_index,
                users.len(),
                workspaces.len(),
            )?;
            let target_refs = user_refs
                .get(&(seed_index, target_user_index))
                .ok_or_else(|| {
                    missing_reference("target user refs", seed_index, target_user_index)
                })?;
            let user_prefix =
                probe_prefix(profile, seed_index, refs.workspace_index, Some(user_index));

            probes.push(Probe {
                probe_id: format!("{user_prefix}-point-private-repository"),
                probe_type: ProbeType::PointRecall,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: "Which private work repository should you use for me?".to_string(),
                answer: refs.private_answer.clone(),
                expected_fact_ids: vec![refs.private_fact_id.clone()],
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-latest-deploy-target"),
                probe_type: ProbeType::LatestValueAfterUpdate,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: format!(
                    "After the latest deploy-target update, where should {} deploy?",
                    workspace_fact_refs.component
                ),
                answer: format!(
                    "The latest deploy target is {}.",
                    workspace_fact_refs.deploy_target
                ),
                expected_fact_ids: vec![workspace_fact_refs.deploy_new_fact_id.clone()],
                blocked_fact_ids: vec![workspace_fact_refs.deploy_old_fact_id.clone()],
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-cross-user-private-repository"),
                probe_type: ProbeType::CrossUserIsolation,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: "Which private repository does my teammate use?".to_string(),
                answer: "The assistant should abstain instead of exposing another user's private repository.".to_string(),
                expected_fact_ids: Vec::new(),
                blocked_fact_ids: vec![target_refs.private_fact_id.clone()],
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-preference-application"),
                probe_type: ProbeType::PreferenceApplication,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: "Format your next implementation answer the way I prefer.".to_string(),
                answer: refs.preference_answer.clone(),
                expected_fact_ids: vec![refs.preference_fact_id.clone()],
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-pii-redaction"),
                probe_type: ProbeType::PiiRedaction,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: "What contact email should be displayed for me?".to_string(),
                answer: refs.pii_answer.clone(),
                expected_fact_ids: vec![refs.pii_fact_id.clone()],
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: true,
            });

            for (pair_index, pair) in refs.multi_hop_pairs.iter().enumerate() {
                probes.push(Probe {
                    probe_id: format!("{user_prefix}-multi-hop-library-owner-{pair_index}"),
                    probe_type: ProbeType::MultiHop,
                    workspace_id: workspace.clone(),
                    user_id: user.clone(),
                    query: format!(
                        "Which team owns the library that {} depends on?",
                        pair.service
                    ),
                    answer: format!(
                        "{} depends on {}, which is owned by {}.",
                        pair.service, pair.library, pair.team
                    ),
                    expected_fact_ids: vec![
                        pair.depends_fact_id.clone(),
                        pair.owner_fact_id.clone(),
                    ],
                    blocked_fact_ids: Vec::new(),
                    as_of: None,
                    expected_redacted: false,
                });
            }
        }

        for workspace_index in 0..workspaces.len() {
            let refs = workspace_refs
                .get(&(seed_index, workspace_index))
                .ok_or_else(|| {
                    missing_reference("workspace fact refs", seed_index, workspace_index)
                })?;
            let workspace = workspaces
                .get(workspace_index)
                .ok_or_else(|| missing_reference("workspace", seed_index, workspace_index))?;
            let author_index = first_user_for_workspace(workspace_index, users, workspaces.len())?;
            let user = users.get(author_index).ok_or_else(|| {
                missing_reference("workspace probe user", seed_index, author_index)
            })?;
            let workspace_prefix = probe_prefix(profile, seed_index, workspace_index, None);

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-workspace-runbook"),
                probe_type: ProbeType::WorkspaceSharedFact,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: "Which runbook is required for this workspace deploy?".to_string(),
                answer: format!("Use {} for this workspace deploy.", refs.runbook),
                expected_fact_ids: vec![refs.runbook_fact_id.clone()],
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-temporal-on-call-month"),
                probe_type: ProbeType::TemporalAsOf,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} as of {}?",
                    refs.temporal_subject,
                    month_year(refs.temporal_month_as_of)
                ),
                answer: refs.temporal_answer.clone(),
                expected_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                blocked_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                as_of: Some(refs.temporal_month_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-temporal-on-call-date"),
                probe_type: ProbeType::TemporalAsOf,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} on {}?",
                    refs.temporal_subject,
                    iso_date(refs.temporal_iso_as_of)
                ),
                answer: refs.temporal_answer.clone(),
                expected_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                blocked_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                as_of: Some(refs.temporal_iso_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-temporal-on-call-current"),
                probe_type: ProbeType::TemporalAsOf,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} as of {}?",
                    refs.temporal_subject,
                    iso_date(refs.temporal_current_as_of)
                ),
                answer: refs.temporal_current_answer.clone(),
                expected_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                blocked_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                as_of: Some(refs.temporal_current_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-temporal-on-call-back-in"),
                probe_type: ProbeType::TemporalAsOf,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} back in {}?",
                    refs.temporal_subject,
                    month_year(refs.temporal_month_as_of)
                ),
                answer: refs.temporal_answer.clone(),
                expected_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                blocked_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                as_of: Some(refs.temporal_month_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-abstain-missing-secret"),
                probe_type: ProbeType::Abstention,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: "What is the production database password?".to_string(),
                answer: "The assistant should abstain because the corpus contains no production database password.".to_string(),
                expected_fact_ids: Vec::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{workspace_prefix}-contradiction-cache-backend"),
                probe_type: ProbeType::PointRecall,
                workspace_id: workspace.clone(),
                user_id: user.clone(),
                query: format!(
                    "Which cache backend claims conflict for {}?",
                    refs.contradiction_subject
                ),
                answer: format!(
                    "{} has contradictory cache backend claims and should be treated as unresolved.",
                    refs.contradiction_subject
                ),
                expected_fact_ids: vec![
                    refs.contradiction_a_fact_id.clone(),
                    refs.contradiction_b_fact_id.clone(),
                ],
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });
        }
    }
    Ok(probes)
}

/// Predicates whose per-scope facts recur across era sessions as updates to
/// one logical value rather than as independent facts.
const RECURRING_UPDATE_PREDICATES: &[&str] = &[
    "response_style",
    "contact_email",
    "private_repository",
    "require_runbook",
];

/// Links recurring facts across era sessions into one supersession chain.
///
/// Later eras supersede earlier ones and close their validity windows, so a
/// present-tense probe targets exactly one active fact. Probes that expect a
/// superseded era are rewritten into explicit as-of queries inside that era's
/// window, and every linked probe blocks the other eras of its family.
fn link_recurring_fact_eras(ledger: &mut [LedgerFact], probes: &mut [Probe]) {
    let mut families: BTreeMap<(String, Option<String>, String), Vec<usize>> = BTreeMap::new();
    for (index, fact) in ledger.iter().enumerate() {
        if !RECURRING_UPDATE_PREDICATES.contains(&fact.predicate.as_str())
            || fact.restates.is_some()
        {
            continue;
        }
        let user_key = (fact.scope == ScopeTier::User).then(|| fact.user_id.as_str().to_string());
        families
            .entry((
                fact.workspace_id.as_str().to_string(),
                user_key,
                fact.predicate.clone(),
            ))
            .or_default()
            .push(index);
    }

    let mut family_ids_by_fact: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for indices in families.values() {
        let mut ordered = indices.clone();
        ordered.sort_by_key(|&index| ledger[index].valid_from);
        for window in ordered.windows(2) {
            let successor_from = ledger[window[1]].valid_from;
            let predecessor_id = ledger[window[0]].fact_id.clone();
            ledger[window[0]].valid_to = Some(successor_from);
            ledger[window[1]].supersedes.push(predecessor_id);
        }
        let ids = ordered
            .iter()
            .map(|&index| ledger[index].fact_id.clone())
            .collect::<Vec<_>>();
        for id in &ids {
            family_ids_by_fact.insert(id.clone(), ids.clone());
        }
    }

    let closed_from_by_fact = ledger
        .iter()
        .filter(|fact| fact.valid_to.is_some())
        .map(|fact| (fact.fact_id.clone(), fact.valid_from))
        .collect::<BTreeMap<_, _>>();

    for probe in probes.iter_mut() {
        let [expected_id] = probe.expected_fact_ids.as_slice() else {
            continue;
        };
        let Some(family) = family_ids_by_fact.get(expected_id) else {
            continue;
        };
        let expected_id = expected_id.clone();
        probe
            .blocked_fact_ids
            .extend(family.iter().filter(|id| **id != expected_id).cloned());
        if probe.as_of.is_none()
            && let Some(valid_from) = closed_from_by_fact.get(&expected_id)
        {
            let as_of = *valid_from + Duration::days(2);
            probe.query = format!(
                "{} as of {}?",
                probe.query.trim_end_matches(['?', ' ']),
                as_of.format("%Y-%m-%d")
            );
            probe.as_of = Some(as_of);
        }
    }
}

fn assign_quality_priors(ledger: &mut [LedgerFact], probes: &[Probe]) -> Result<()> {
    let expected_fact_ids = probes
        .iter()
        .flat_map(|probe| probe.expected_fact_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    let expected_keys = ledger
        .iter()
        .filter(|fact| expected_fact_ids.contains(&fact.fact_id))
        .map(quality_prior_group_key)
        .collect::<BTreeSet<_>>();
    let mut low_prior_count = 0_usize;

    for fact in ledger.iter_mut() {
        if expected_fact_ids.contains(&fact.fact_id) {
            fact.prior_uses = Some(8);
            fact.prior_successes = Some(7);
            continue;
        }
        if fact.restates.is_none() && expected_keys.contains(&quality_prior_group_key(fact)) {
            fact.prior_uses = Some(8);
            fact.prior_successes = Some(1);
            low_prior_count += 1;
        }
    }

    if expected_fact_ids.is_empty() || low_prior_count == 0 {
        return invalid_config(
            "quality prior assignment requires expected facts and same-subject colliders"
                .to_string(),
        );
    }
    Ok(())
}

fn quality_prior_group_key(fact: &LedgerFact) -> (String, Option<String>, &'static str, String) {
    (
        fact.workspace_id.to_string(),
        (fact.scope == ScopeTier::User).then(|| fact.user_id.to_string()),
        scope_tier_str(fact.scope),
        fact.subject.clone(),
    )
}

fn scope_tier_str(scope: ScopeTier) -> &'static str {
    match scope {
        ScopeTier::Global => "global",
        ScopeTier::Workspace => "workspace",
        ScopeTier::User => "user",
    }
}

fn build_embedding_inputs(facts: &[LedgerFact], probes: &[Probe]) -> Vec<EmbeddingInput> {
    let mut inputs = Vec::with_capacity(facts.len() + probes.len());
    for fact in facts {
        inputs.push(EmbeddingInput {
            input_id: format!("fact:{}", fact.fact_id),
            kind: EmbeddingInputKind::Fact,
            text: format!(
                "Fact: {} {} {}. Answer: {}",
                fact.subject, fact.predicate, fact.object, fact.answer
            ),
            fact_ids: vec![fact.fact_id.clone()],
            probe_ids: Vec::new(),
        });
    }
    for probe in probes {
        let mut fact_ids = probe
            .expected_fact_ids
            .iter()
            .chain(probe.blocked_fact_ids.iter())
            .cloned()
            .collect::<Vec<_>>();
        fact_ids.sort();
        fact_ids.dedup();
        inputs.push(EmbeddingInput {
            input_id: format!("probe:{}", probe.probe_id),
            kind: EmbeddingInputKind::Probe,
            text: probe.query.clone(),
            fact_ids,
            probe_ids: vec![probe.probe_id.clone()],
        });
    }
    inputs
}

fn should_restate_dependency(fact_id: &str) -> bool {
    matches!(natural_frames::workspace_frame_index(fact_id), 1 | 3)
}

fn validate_seeds(seeds: &[u64]) -> Result<()> {
    if seeds.len() != REQUIRED_SEED_COUNT {
        return invalid_config(format!(
            "memory eval generator requires exactly {REQUIRED_SEED_COUNT} independent seeds; got {}",
            seeds.len()
        ));
    }
    let unique = seeds.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != seeds.len() {
        return invalid_config("memory eval generator seeds must be unique");
    }
    Ok(())
}

fn validate_schedule_categories(scheduled_facts: &[ScheduledFact]) -> Result<()> {
    let categories = scheduled_facts
        .iter()
        .map(|scheduled| scheduled.category)
        .collect::<BTreeSet<_>>();
    for category in [
        FactCategory::Supersession,
        FactCategory::Contradiction,
        FactCategory::WorkspaceShared,
        FactCategory::UserPrivate,
        FactCategory::Temporal,
        FactCategory::Preference,
        FactCategory::Pii,
    ] {
        if !categories.contains(&category) {
            return invalid_config(format!("generated corpus is missing {category:?} facts"));
        }
    }
    Ok(())
}

fn validate_profile_shape(corpus: &GeneratedMemoryEvalCorpus) -> Result<()> {
    validate_seeds(&corpus.manifest.seeds)?;
    let user_count = distinct_user_count(corpus);
    let workspace_count = distinct_workspace_count(corpus);
    match corpus.manifest.profile {
        CorpusProfile::Pr => {
            if user_count != PR_USER_COUNT {
                return invalid_config(format!(
                    "PR corpus must contain {PR_USER_COUNT} users; got {user_count}"
                ));
            }
            if workspace_count != PR_WORKSPACE_COUNT {
                return invalid_config(format!(
                    "PR corpus must contain {PR_WORKSPACE_COUNT} workspaces; got {workspace_count}"
                ));
            }
            if corpus.probes.len() < 60 {
                return invalid_config(format!(
                    "PR corpus must contain at least 60 probes; got {}",
                    corpus.probes.len()
                ));
            }
        }
        CorpusProfile::Full => {
            if user_count != FULL_USER_COUNT {
                return invalid_config(format!(
                    "full corpus must contain {FULL_USER_COUNT} users; got {user_count}"
                ));
            }
            if workspace_count != FULL_WORKSPACE_COUNT {
                return invalid_config(format!(
                    "full corpus must contain {FULL_WORKSPACE_COUNT} workspaces; got {workspace_count}"
                ));
            }
            if !(FULL_MIN_PROBES..=FULL_MAX_PROBES).contains(&corpus.probes.len()) {
                return invalid_config(format!(
                    "full corpus must contain {FULL_MIN_PROBES}-{FULL_MAX_PROBES} probes; got {}",
                    corpus.probes.len()
                ));
            }
            for (user_id, session_count) in sessions_per_user(&corpus.sessions) {
                if session_count > 100 {
                    return invalid_config(format!(
                        "full corpus user {user_id} has {session_count} sessions; expected 0-100"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_embedding_inputs(
    inputs: &[EmbeddingInput],
    facts: &[LedgerFact],
    probes: &[Probe],
) -> Result<()> {
    let fact_ids = facts
        .iter()
        .map(|fact| fact.fact_id.as_str())
        .collect::<HashSet<_>>();
    let probe_ids = probes
        .iter()
        .map(|probe| probe.probe_id.as_str())
        .collect::<HashSet<_>>();
    let mut input_ids = HashSet::new();
    for input in inputs {
        input.validate()?;
        if !input_ids.insert(input.input_id.as_str()) {
            return invalid_config(format!("duplicate embedding input_id {}", input.input_id));
        }
        for fact_id in &input.fact_ids {
            if !fact_ids.contains(fact_id.as_str()) {
                return invalid_config(format!(
                    "embedding input {} references missing fact_id {}",
                    input.input_id, fact_id
                ));
            }
        }
        for probe_id in &input.probe_ids {
            if !probe_ids.contains(probe_id.as_str()) {
                return invalid_config(format!(
                    "embedding input {} references missing probe_id {}",
                    input.input_id, probe_id
                ));
            }
        }
    }
    Ok(())
}

fn render_fact_transcript(
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
        return render_fact_transcript(transcript_style, category, canonical, facts_by_id);
    }

    match transcript_style {
        TranscriptStyle::Marked => render_marked_fact_transcript(category, fact),
        TranscriptStyle::Natural => {
            natural_frames::render_fact(category, fact, superseded_object(fact, facts_by_id))
        }
    }
}

fn render_marked_fact_transcript(category: FactCategory, fact: &LedgerFact) -> String {
    let scope_marker = match fact.scope {
        ScopeTier::Workspace | ScopeTier::Global => "workspace shared ",
        ScopeTier::User => "user private ",
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
        FactCategory::WorkspaceShared => format!(
            "Fact: workspace shared {} {} is {}.",
            fact.subject, fact.predicate, fact.object
        ),
        FactCategory::UserPrivate => format!(
            "Fact: user private {} {} is {}.",
            fact.subject, fact.predicate, fact.object
        ),
        FactCategory::Temporal => format!(
            "Fact: workspace shared {} {} is {} from {} until {}. Supersedes: {}.",
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
    use super::{FactCategory, LedgerFact, ScopeTier, mix_u64};

    const USER_FRAMES: &[&str] = &[
        "Just so you know, I prefer {object} when it comes to {subject}.",
        "For my work, {subject} should use {object}.",
        "I switched my {subject} to {object} recently.",
        "My {subject} {predicate_phrase} {object} these days.",
    ];
    const WORKSPACE_FRAMES: &[&str] = &[
        "The team agreed that {subject} {predicate_phrase} {object}.",
        "Heads up everyone: {subject} now {predicate_phrase} {object}.",
        "We standardized {subject} on {object} last sprint.",
        "{subject} {predicate_phrase} {object} per the platform decision.",
    ];
    const UPDATE_FRAMES: &[&str] = &[
        "Quick update: {subject} {predicate_phrase} {object} now, not {old_object} anymore.",
        "Correction to earlier: {subject} moved to {object}.",
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

        let frames = if fact.scope == ScopeTier::User {
            USER_FRAMES
        } else {
            WORKSPACE_FRAMES
        };
        apply_frame(
            select(&fact.fact_id, frames),
            fact,
            "the previous value",
            predicate_phrase(&fact.predicate),
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

    /// Returns the deterministic workspace-frame index selected for a fact key.
    pub(super) fn workspace_frame_index(key: &str) -> usize {
        stable_index(key, WORKSPACE_FRAMES.len())
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
    }
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(",")
    }
}

fn next_turn_seq(turns: &mut BTreeMap<String, u64>, key: &str) -> Result<u64> {
    let current = turns.entry(key.to_string()).or_insert(0);
    let next = current
        .checked_add(1)
        .ok_or_else(|| EvalError::InvalidConfig(format!("turn sequence overflow for {key}")))?;
    *current = next;
    Ok(next)
}

fn build_users(profile: CorpusProfile, count: usize) -> Vec<UserId> {
    (0..count)
        .map(|index| {
            UserId::new(format!(
                "memory-eval-{}-user-{index:02}",
                profile_slug(profile)
            ))
        })
        .collect()
}

fn build_workspaces(profile: CorpusProfile, count: usize) -> Vec<WorkspaceId> {
    (0..count)
        .map(|index| {
            WorkspaceId::new(format!(
                "memory-eval-{}-workspace-{index:02}",
                profile_slug(profile)
            ))
        })
        .collect()
}

fn workspace_index_for_user(user_index: usize, workspace_count: usize) -> usize {
    user_index % workspace_count
}

fn first_user_for_workspace(
    workspace_index: usize,
    users: &[UserId],
    workspace_count: usize,
) -> Result<usize> {
    (0..users.len())
        .find(|candidate| workspace_index_for_user(*candidate, workspace_count) == workspace_index)
        .ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "no generated user belongs to workspace index {workspace_index}"
            ))
        })
}

fn next_user_in_workspace(
    user_index: usize,
    workspace_index: usize,
    user_count: usize,
    workspace_count: usize,
) -> Result<usize> {
    for offset in 1..user_count {
        let candidate = (user_index + offset) % user_count;
        if workspace_index_for_user(candidate, workspace_count) == workspace_index {
            return Ok(candidate);
        }
    }
    invalid_config(format!(
        "workspace index {workspace_index} needs at least two users for cross-user probes"
    ))
}

fn workspace_session(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    workspace_index: usize,
    author_index: usize,
    users: &[UserId],
    workspaces: &[WorkspaceId],
) -> SessionAssignment {
    SessionAssignment {
        key: format!("s{seed_index:02}-w{workspace_index:02}-workspace"),
        plan: SessionPlan {
            session_id: deterministic_session_id(
                profile,
                seed_index,
                seed,
                workspace_index,
                author_index,
                1,
            ),
            workspace_id: workspaces[workspace_index].clone(),
            user_id: users[author_index].clone(),
        },
    }
}

fn user_session(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    workspace_index: usize,
    user_index: usize,
    users: &[UserId],
    workspaces: &[WorkspaceId],
) -> SessionAssignment {
    SessionAssignment {
        key: format!("s{seed_index:02}-w{workspace_index:02}-u{user_index:02}"),
        plan: SessionPlan {
            session_id: deterministic_session_id(
                profile,
                seed_index,
                seed,
                workspace_index,
                user_index,
                2,
            ),
            workspace_id: workspaces[workspace_index].clone(),
            user_id: users[user_index].clone(),
        },
    }
}

fn user_aux_session(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    user_index: usize,
    purpose: u128,
    users: &[UserId],
    workspaces: &[WorkspaceId],
) -> SessionAssignment {
    let workspace_index = workspace_index_for_user(user_index, workspaces.len());
    SessionAssignment {
        key: format!("s{seed_index:02}-w{workspace_index:02}-u{user_index:02}-aux-{purpose}"),
        plan: SessionPlan {
            session_id: deterministic_session_id(
                profile,
                seed_index,
                seed,
                workspace_index,
                user_index,
                purpose,
            ),
            workspace_id: workspaces[workspace_index].clone(),
            user_id: users[user_index].clone(),
        },
    }
}

fn deterministic_session_id(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    workspace_index: usize,
    user_index: usize,
    purpose: u128,
) -> SessionId {
    let profile_code = match profile {
        CorpusProfile::Pr => 1_u128,
        CorpusProfile::Full => 2_u128,
    };
    let seed_hash = u128::from(mix_u64(seed));
    let value = (0xA11C_u128 << 112)
        | (profile_code << 104)
        | ((seed_index as u128) << 96)
        | ((workspace_index as u128) << 88)
        | ((user_index as u128) << 72)
        | (purpose << 64)
        | (seed_hash & 0xFFFF_FFFF_FFFF_FFFF);
    SessionId(Uuid::from_u128(value))
}

fn fact_id(
    profile: CorpusProfile,
    seed_index: usize,
    workspace_index: usize,
    user_index: Option<usize>,
    suffix: &str,
) -> String {
    match user_index {
        Some(user_index) => format!(
            "{}-s{seed_index:02}-w{workspace_index:02}-u{user_index:02}-{suffix}",
            profile_slug(profile)
        ),
        None => format!(
            "{}-s{seed_index:02}-w{workspace_index:02}-{suffix}",
            profile_slug(profile)
        ),
    }
}

fn probe_prefix(
    profile: CorpusProfile,
    seed_index: usize,
    workspace_index: usize,
    user_index: Option<usize>,
) -> String {
    match user_index {
        Some(user_index) => format!(
            "{}-s{seed_index:02}-w{workspace_index:02}-u{user_index:02}",
            profile_slug(profile)
        ),
        None => format!(
            "{}-s{seed_index:02}-w{workspace_index:02}",
            profile_slug(profile)
        ),
    }
}

fn corpus_id(profile: CorpusProfile, transcript_style: TranscriptStyle, seeds: &[u64]) -> String {
    let seed_segment = seeds
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join("-");
    format!(
        "memory-eval-{}-{}-{seed_segment}",
        profile_slug(profile),
        transcript_style_slug(transcript_style)
    )
}

fn profile_slug(profile: CorpusProfile) -> &'static str {
    match profile {
        CorpusProfile::Pr => "pr",
        CorpusProfile::Full => "full",
    }
}

fn transcript_style_slug(transcript_style: TranscriptStyle) -> &'static str {
    match transcript_style {
        TranscriptStyle::Marked => "marked",
        TranscriptStyle::Natural => "natural",
    }
}

fn user_label(user_index: usize) -> String {
    format!("User {user_index:02}")
}

fn fixed_time(day_offset: i64, hour: i64) -> Result<DateTime<Utc>> {
    let day_seconds = day_offset
        .checked_mul(SECONDS_PER_DAY)
        .ok_or_else(|| EvalError::InvalidConfig("generated day offset overflowed".to_string()))?;
    let hour_seconds = hour
        .checked_mul(SECONDS_PER_HOUR)
        .ok_or_else(|| EvalError::InvalidConfig("generated hour offset overflowed".to_string()))?;
    let timestamp = BASE_UNIX_SECONDS
        .checked_add(day_seconds)
        .and_then(|value| value.checked_add(hour_seconds))
        .ok_or_else(|| EvalError::InvalidConfig("generated timestamp overflowed".to_string()))?;
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .ok_or_else(|| EvalError::InvalidConfig(format!("invalid generated timestamp {timestamp}")))
}

fn first_day_after_months(month_offset: usize) -> Result<DateTime<Utc>> {
    let year = 2026
        + i32::try_from(month_offset / 12).map_err(|_| {
            EvalError::InvalidConfig("generated month offset overflowed".to_string())
        })?;
    let month = u32::try_from((month_offset % 12) + 1)
        .map_err(|_| EvalError::InvalidConfig("generated month offset overflowed".to_string()))?;
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "invalid generated month boundary {year:04}-{month:02}-01"
            ))
        })
}

fn first_day_of_next_month(value: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let (year, month) = if value.month() == 12 {
        (value.year() + 1, 1)
    } else {
        (value.year(), value.month() + 1)
    };
    Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "invalid generated month boundary {year:04}-{month:02}-01"
            ))
        })
}

fn month_year(value: DateTime<Utc>) -> String {
    const MONTHS: &[&str] = &[
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let month_index = usize::try_from(value.month0()).unwrap_or(0);
    format!("{} {}", MONTHS[month_index], value.year())
}

fn iso_date(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%d").to_string()
}

fn choose<'a>(rng: &mut StableRng, values: &'a [&str]) -> Result<&'a str> {
    let index = rng.index(values.len())?;
    Ok(values[index])
}

fn choose_pair<'a>(rng: &mut StableRng, values: &'a [(&str, &str)]) -> Result<(&'a str, &'a str)> {
    let index = rng.index(values.len())?;
    Ok(values[index])
}

fn mix_u64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn distinct_user_count(corpus: &GeneratedMemoryEvalCorpus) -> usize {
    let mut users = BTreeSet::new();
    for fact in &corpus.ledger {
        users.insert(fact.user_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        users.insert(session.user_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        users.insert(probe.user_id.as_str().to_string());
    }
    users.len()
}

fn distinct_workspace_count(corpus: &GeneratedMemoryEvalCorpus) -> usize {
    let mut workspaces = BTreeSet::new();
    for fact in &corpus.ledger {
        workspaces.insert(fact.workspace_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        workspaces.insert(session.workspace_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        workspaces.insert(probe.workspace_id.as_str().to_string());
    }
    workspaces.len()
}

fn sessions_per_user(sessions: &[SyntheticSession]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for session in sessions {
        *counts
            .entry(session.user_id.as_str().to_string())
            .or_insert(0) += 1;
    }
    counts
}

async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let file = File::open(path)
        .await
        .map_err(|source| io_error(path, source))?;
    let mut lines = BufReader::new(file).lines();
    let mut records = Vec::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|source| io_error(path, source))?
    {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(&line)?);
    }
    Ok(records)
}

async fn write_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
where
    T: Serialize,
{
    ensure_parent_dir(path).await?;
    let mut file = File::create(path)
        .await
        .map_err(|source| io_error(path, source))?;
    for record in records {
        let line = serde_json::to_vec(record)?;
        file.write_all(&line)
            .await
            .map_err(|source| io_error(path, source))?;
        file.write_all(b"\n")
            .await
            .map_err(|source| io_error(path, source))?;
    }
    file.flush().await.map_err(|source| io_error(path, source))
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    Ok(())
}

fn missing_reference(kind: &str, seed_index: usize, index: usize) -> EvalError {
    EvalError::InvalidConfig(format!(
        "missing generated {kind} for seed index {seed_index}, record index {index}"
    ))
}

fn ensure_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return invalid_config(format!("{label} must not be empty"));
    }
    Ok(())
}

fn invalid_config<T>(message: impl Into<String>) -> Result<T> {
    Err(EvalError::InvalidConfig(message.into()))
}

fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{
        CorpusProfile, SyntheticSession, SyntheticTurn, TranscriptStyle,
        generate_memory_eval_corpus_with_style,
    };

    #[test]
    fn generator_restatement_transcripts_are_verbatim_repeats() {
        // Pins: exact-hash consolidation is exercised by byte-identical restatement transcripts.
        let corpus = generate_memory_eval_corpus_with_style(
            CorpusProfile::Pr,
            vec![1, 2, 3],
            TranscriptStyle::Natural,
        )
        .expect("generate PR natural corpus");
        let turns = turns_by_fact_id(&corpus.sessions);
        let restating = corpus
            .ledger
            .iter()
            .filter(|fact| fact.restates.is_some())
            .collect::<Vec<_>>();

        assert!(restating.len() >= 10);
        for fact in restating {
            let canonical_id = fact.restates.as_deref().expect("canonical id");
            let canonical = turns
                .get(canonical_id)
                .expect("canonical turn should exist");
            let restatement = turns
                .get(fact.fact_id.as_str())
                .expect("restating turn should exist");

            assert_eq!(restatement.transcript, canonical.transcript);
        }
    }

    #[test]
    fn probes_never_target_restating_fact_ids() {
        // Pins: restating facts exist only to be merged, not queried.
        let corpus = generate_memory_eval_corpus_with_style(
            CorpusProfile::Pr,
            vec![1, 2, 3],
            TranscriptStyle::Marked,
        )
        .expect("generate PR marked corpus");
        let restating = corpus
            .ledger
            .iter()
            .filter(|fact| fact.restates.is_some())
            .map(|fact| fact.fact_id.as_str())
            .collect::<BTreeSet<_>>();

        assert!(restating.len() >= 10);
        for probe in &corpus.probes {
            for fact_id in probe.referenced_fact_ids() {
                assert!(!restating.contains(fact_id));
            }
        }
    }

    #[test]
    fn generator_prior_assignment_is_deterministic_and_disjoint() {
        // Pins: synthetic quality priors mark expected facts high and colliders low without overlap.
        let first = generate_memory_eval_corpus_with_style(
            CorpusProfile::Pr,
            vec![1, 2, 3],
            TranscriptStyle::Marked,
        )
        .expect("generate first PR corpus");
        let second = generate_memory_eval_corpus_with_style(
            CorpusProfile::Pr,
            vec![1, 2, 3],
            TranscriptStyle::Marked,
        )
        .expect("generate second PR corpus");
        let expected = first
            .probes
            .iter()
            .flat_map(|probe| probe.expected_fact_ids.iter().map(String::as_str))
            .collect::<BTreeSet<_>>();
        let first_priors = first
            .ledger
            .iter()
            .map(|fact| {
                (
                    fact.fact_id.as_str(),
                    (fact.prior_uses, fact.prior_successes),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let second_priors = second
            .ledger
            .iter()
            .map(|fact| {
                (
                    fact.fact_id.as_str(),
                    (fact.prior_uses, fact.prior_successes),
                )
            })
            .collect::<BTreeMap<_, _>>();

        assert_eq!(first_priors, second_priors);
        assert!(
            expected
                .iter()
                .all(|fact_id| first_priors.get(fact_id).copied() == Some((Some(8), Some(7))))
        );
        let low_prior_ids = first_priors
            .iter()
            .filter_map(|(fact_id, prior)| (*prior == (Some(8), Some(1))).then_some(*fact_id))
            .collect::<BTreeSet<_>>();
        assert!(!low_prior_ids.is_empty());
        assert!(low_prior_ids.is_disjoint(&expected));
        assert!(first.ledger.iter().all(|fact| {
            fact.restates.is_none()
                || first_priors.get(fact.fact_id.as_str()).copied() == Some((None, None))
        }));
    }

    fn turns_by_fact_id(sessions: &[SyntheticSession]) -> BTreeMap<&str, &SyntheticTurn> {
        let mut turns = BTreeMap::new();
        for session in sessions {
            for turn in &session.turns {
                for fact_id in &turn.fact_ids {
                    turns.insert(fact_id.as_str(), turn);
                }
            }
        }
        turns
    }
}
