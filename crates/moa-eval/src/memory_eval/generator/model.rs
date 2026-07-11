//! Deterministic corpus schedules, fact models, probes, and identifiers.

use super::embeddings::build_embedding_inputs;
use super::*;

pub(super) fn generate_memory_eval_corpus(
    profile: CorpusProfile,
    seeds: Vec<u64>,
    transcript_style: TranscriptStyle,
) -> Result<GeneratedMemoryEvalCorpus> {
    validate_seeds(&seeds)?;
    let settings = ProfileSettings::new(profile);
    let users = build_users(profile, settings.user_count);
    let storage_partitions = build_storage_partitions(profile, settings.tenant_count);
    let mut builder = ScheduleBuilder::default();
    let mut tenant_refs = BTreeMap::new();
    let mut user_refs = BTreeMap::new();

    for (seed_index, seed) in seeds.iter().copied().enumerate() {
        for tenant_index in 0..settings.tenant_count {
            let refs = schedule_tenant_facts(
                &mut builder,
                profile,
                seed_index,
                seed,
                tenant_index,
                &users,
                &storage_partitions,
            )?;
            tenant_refs.insert((seed_index, tenant_index), refs);
        }

        for user_index in 0..settings.user_count {
            let refs = schedule_user_facts(
                &mut builder,
                profile,
                seed_index,
                seed,
                user_index,
                &users,
                &storage_partitions,
            )?;
            user_refs.insert((seed_index, user_index), refs);
        }
    }

    let schedule = builder.finish();
    validate_schedule_categories(&schedule.facts)?;
    let sessions = schedule.render_sessions(transcript_style);
    let mut probes = build_probes(
        profile,
        transcript_style,
        &seeds,
        &users,
        &storage_partitions,
        &tenant_refs,
        &user_refs,
    )?;
    let mut ledger = schedule.ledger();
    link_recurring_fact_eras(&mut ledger, &mut probes);
    attach_rewrite_fixtures(&mut probes);
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

#[derive(Debug, Clone, Copy)]
struct ProfileSettings {
    user_count: usize,
    tenant_count: usize,
    multi_hop_pairs_per_user: usize,
}

impl ProfileSettings {
    fn new(profile: CorpusProfile) -> Self {
        match profile {
            CorpusProfile::Pr => Self {
                user_count: PR_USER_COUNT,
                tenant_count: PR_TENANT_COUNT,
                multi_hop_pairs_per_user: 2,
            },
            CorpusProfile::Full => Self {
                user_count: FULL_USER_COUNT,
                tenant_count: FULL_TENANT_COUNT,
                multi_hop_pairs_per_user: 1,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum FactCategory {
    Supersession,
    Contradiction,
    TenantShared,
    UserPrivate,
    Temporal,
    Preference,
    Pii,
}

#[derive(Debug, Clone)]
pub(super) struct ScheduledFact {
    pub(super) category: FactCategory,
    session_key: String,
    fact: LedgerFact,
}

#[derive(Debug, Clone)]
struct SessionPlan {
    session_id: SessionId,
    storage_partition_id: StoragePartitionId,
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
            storage_partition_id: assignment.plan.storage_partition_id,
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
            storage_partition_id: assignment.plan.storage_partition_id,
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
                            transcript: distractor_transcript(session_key),
                            fact_ids: Vec::new(),
                        });
                    }
                    Some(SyntheticSession {
                        session_id: plan.session_id,
                        storage_partition_id: plan.storage_partition_id.clone(),
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
struct TenantFactRefs {
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
    tenant_index: usize,
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

fn schedule_tenant_facts(
    builder: &mut ScheduleBuilder,
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    tenant_index: usize,
    users: &[UserId],
    storage_partitions: &[StoragePartitionId],
) -> Result<TenantFactRefs> {
    let mut rng = StableRng::new(seed ^ mix_u64(tenant_index as u64) ^ 0xA11C_E5ED);
    let component = choose(&mut rng, COMPONENTS)?;
    let (old_target, new_target) = choose_pair(&mut rng, DEPLOY_TARGETS)?;
    let (cache_a, cache_b) = choose_pair(&mut rng, CACHE_BACKENDS)?;
    let runbook = choose(&mut rng, RUNBOOKS)?;
    let (old_on_call, new_on_call) = choose_pair(&mut rng, ON_CALLS)?;
    let author_index = first_user_for_tenant(tenant_index, users, storage_partitions.len())?;
    let assignment = tenant_session(
        profile,
        seed_index,
        seed,
        tenant_index,
        author_index,
        users,
        storage_partitions,
    );
    let base_day = seed_index as i64 * 40 + tenant_index as i64 * 10;
    let deploy_old_from = fixed_time(base_day, 0)?;
    let deploy_new_from = fixed_time(base_day + 7, 0)?;
    let temporal_old_from = first_day_after_months(seed_index * 8 + tenant_index * 2)?;
    let temporal_new_from = first_day_of_next_month(temporal_old_from)?;
    let temporal_iso_as_of = temporal_old_from + Duration::days(5);
    let temporal_month_as_of = temporal_old_from;
    let temporal_current_as_of = temporal_new_from + Duration::days(1);
    let temporal_subject = format!("{component}-support-rotation");

    let deploy_old_fact_id = fact_id(profile, seed_index, tenant_index, None, "deploy-target-v1");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Supersession,
            fact_id: deploy_old_fact_id.clone(),
            scope: ScopeTier::Tenant,
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

    let deploy_new_fact_id = fact_id(profile, seed_index, tenant_index, None, "deploy-target-v2");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Supersession,
            fact_id: deploy_new_fact_id.clone(),
            scope: ScopeTier::Tenant,
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

    let runbook_fact_id = fact_id(profile, seed_index, tenant_index, None, "tenant-runbook");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::TenantShared,
            fact_id: runbook_fact_id.clone(),
            scope: ScopeTier::Tenant,
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
    let contradiction_a_fact_id =
        fact_id(profile, seed_index, tenant_index, None, "cache-conflict-a");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Contradiction,
            fact_id: contradiction_a_fact_id.clone(),
            scope: ScopeTier::Tenant,
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

    let contradiction_b_fact_id =
        fact_id(profile, seed_index, tenant_index, None, "cache-conflict-b");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Contradiction,
            fact_id: contradiction_b_fact_id.clone(),
            scope: ScopeTier::Tenant,
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
        tenant_index,
        None,
        "on-call-primary-v1",
    );
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Temporal,
            fact_id: temporal_old_fact_id.clone(),
            scope: ScopeTier::Tenant,
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
        tenant_index,
        None,
        "on-call-primary-v2",
    );
    builder.push_fact(
        assignment,
        FactDraft {
            category: FactCategory::Temporal,
            fact_id: temporal_new_fact_id.clone(),
            scope: ScopeTier::Tenant,
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

    Ok(TenantFactRefs {
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
    storage_partitions: &[StoragePartitionId],
) -> Result<UserFactRefs> {
    let settings = ProfileSettings::new(profile);
    let tenant_index = tenant_index_for_user(user_index, storage_partitions.len());
    let mut rng = StableRng::new(seed ^ mix_u64(user_index as u64) ^ 0x515E_D123);
    let repository = choose(&mut rng, REPOSITORIES)?;
    let style = choose(&mut rng, RESPONSE_STYLES)?;
    let editor = choose(&mut rng, EDITORS)?;
    let assignment = user_session(
        profile,
        seed_index,
        seed,
        tenant_index,
        user_index,
        users,
        storage_partitions,
    );
    let base_day = seed_index as i64 * 40 + user_index as i64;
    let user_label = user_label(user_index);

    let private_fact_id = fact_id(
        profile,
        seed_index,
        tenant_index,
        Some(user_index),
        "private-repository",
    );
    let private_answer = format!("{user_label}'s private work repository is {repository}.");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::UserPrivate,
            fact_id: private_fact_id.clone(),
            scope: ScopeTier::Contact,
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
        tenant_index,
        Some(user_index),
        "response-style",
    );
    let preference_answer = format!("Use {style} and {editor} examples when helping {user_label}.");
    builder.push_fact(
        assignment.clone(),
        FactDraft {
            category: FactCategory::Preference,
            fact_id: preference_fact_id.clone(),
            scope: ScopeTier::Contact,
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
        tenant_index,
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
            scope: ScopeTier::Contact,
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
            tenant_index,
            Some(user_index),
            &format!("depends-on-{pair_index}"),
        );
        builder.push_fact(
            user_session(
                profile,
                seed_index,
                seed,
                tenant_index,
                user_index,
                users,
                storage_partitions,
            ),
            FactDraft {
                category: FactCategory::TenantShared,
                fact_id: dependency_fact_id.clone(),
                scope: ScopeTier::Tenant,
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
                tenant_index,
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
                    storage_partitions,
                ),
                &dependency_fact_id,
                FactDraft {
                    category: FactCategory::TenantShared,
                    fact_id: restatement_fact_id,
                    scope: ScopeTier::Tenant,
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
            tenant_index,
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
                storage_partitions,
            ),
            FactDraft {
                category: FactCategory::TenantShared,
                fact_id: owner_fact_id.clone(),
                scope: ScopeTier::Tenant,
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
        tenant_index,
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
    transcript_style: TranscriptStyle,
    seeds: &[u64],
    users: &[UserId],
    storage_partitions: &[StoragePartitionId],
    tenant_refs: &BTreeMap<(usize, usize), TenantFactRefs>,
    user_refs: &BTreeMap<(usize, usize), UserFactRefs>,
) -> Result<Vec<Probe>> {
    let mut probes = Vec::new();
    for (seed_index, _) in seeds.iter().enumerate() {
        for user_index in 0..users.len() {
            let refs = user_refs
                .get(&(seed_index, user_index))
                .ok_or_else(|| missing_reference("user fact refs", seed_index, user_index))?;
            let storage_partition = storage_partitions.get(refs.tenant_index).ok_or_else(|| {
                missing_reference("storage partition", seed_index, refs.tenant_index)
            })?;
            let tenant_fact_refs = tenant_refs
                .get(&(seed_index, refs.tenant_index))
                .ok_or_else(|| {
                    missing_reference("tenant fact refs", seed_index, refs.tenant_index)
                })?;
            let user = users
                .get(user_index)
                .ok_or_else(|| missing_reference("user", seed_index, user_index))?;
            let target_user_index = next_user_in_tenant(
                user_index,
                refs.tenant_index,
                users.len(),
                storage_partitions.len(),
            )?;
            let target_refs = user_refs
                .get(&(seed_index, target_user_index))
                .ok_or_else(|| {
                    missing_reference("target user refs", seed_index, target_user_index)
                })?;
            let user_prefix =
                probe_prefix(profile, seed_index, refs.tenant_index, Some(user_index));

            probes.push(Probe {
                probe_id: format!("{user_prefix}-point-private-repository"),
                probe_type: ProbeType::PointRecall,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: "Which private work repository should you use for me?".to_string(),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.private_answer.clone(),
                expected_fact_ids: vec![refs.private_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-latest-deploy-target"),
                probe_type: ProbeType::LatestValueAfterUpdate,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: format!(
                    "After the latest deploy-target update, where should {} deploy?",
                    tenant_fact_refs.component
                ),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: format!(
                    "The latest deploy target is {}.",
                    tenant_fact_refs.deploy_target
                ),
                expected_fact_ids: vec![tenant_fact_refs.deploy_new_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: vec![tenant_fact_refs.deploy_old_fact_id.clone()],
                as_of: None,
                expected_redacted: false,
            });

            // Conversational phrasing variant, natural style only so marked
            // (golden) corpora stay byte-identical. Real queries are colloquial
            // and underspecified; the `-conv` suffix lets reports split the
            // templated and conversational slices.
            if transcript_style == TranscriptStyle::Natural {
                probes.push(Probe {
                    probe_id: format!("{user_prefix}-latest-deploy-target-conv"),
                    probe_type: ProbeType::LatestValueAfterUpdate,
                    storage_partition_id: storage_partition.clone(),
                    user_id: user.clone(),
                    query: format!(
                        "Where does {} deploy these days?",
                        tenant_fact_refs.component
                    ),
                    rewrite_query: None,
                    expected_rewrite: None,
                    query_class: None,
                    answer: format!(
                        "The latest deploy target is {}.",
                        tenant_fact_refs.deploy_target
                    ),
                    expected_fact_ids: vec![tenant_fact_refs.deploy_new_fact_id.clone()],
                    expected_fact_grades: std::collections::BTreeMap::new(),
                    blocked_fact_ids: vec![tenant_fact_refs.deploy_old_fact_id.clone()],
                    as_of: None,
                    expected_redacted: false,
                });
            }

            probes.push(Probe {
                probe_id: format!("{user_prefix}-cross-user-private-repository"),
                probe_type: ProbeType::CrossUserIsolation,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: "Which private repository does my teammate use?".to_string(),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: "The assistant should abstain instead of exposing another user's private repository.".to_string(),
                expected_fact_ids: Vec::new(),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: vec![target_refs.private_fact_id.clone()],
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-preference-application"),
                probe_type: ProbeType::PreferenceApplication,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: "Format your next implementation answer the way I prefer.".to_string(),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.preference_answer.clone(),
                expected_fact_ids: vec![refs.preference_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{user_prefix}-pii-redaction"),
                probe_type: ProbeType::PiiRedaction,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: "What contact email should be displayed for me?".to_string(),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.pii_answer.clone(),
                expected_fact_ids: vec![refs.pii_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: true,
            });

            for (pair_index, pair) in refs.multi_hop_pairs.iter().enumerate() {
                probes.push(Probe {
                    probe_id: format!("{user_prefix}-multi-hop-library-owner-{pair_index}"),
                    probe_type: ProbeType::MultiHop,
                    storage_partition_id: storage_partition.clone(),
                    user_id: user.clone(),
                    query: format!(
                        "Which team owns the library that {} depends on?",
                        pair.service
                    ),
                    rewrite_query: None,
                    expected_rewrite: None,
                    query_class: None,
                    answer: format!(
                        "{} depends on {}, which is owned by {}.",
                        pair.service, pair.library, pair.team
                    ),
                    expected_fact_ids: vec![
                        pair.depends_fact_id.clone(),
                        pair.owner_fact_id.clone(),
                    ],
                    expected_fact_grades: std::collections::BTreeMap::new(),
                    blocked_fact_ids: Vec::new(),
                    as_of: None,
                    expected_redacted: false,
                });
            }
        }

        for tenant_index in 0..storage_partitions.len() {
            let refs = tenant_refs
                .get(&(seed_index, tenant_index))
                .ok_or_else(|| missing_reference("tenant fact refs", seed_index, tenant_index))?;
            let storage_partition = storage_partitions
                .get(tenant_index)
                .ok_or_else(|| missing_reference("storage partition", seed_index, tenant_index))?;
            let author_index =
                first_user_for_tenant(tenant_index, users, storage_partitions.len())?;
            let user = users
                .get(author_index)
                .ok_or_else(|| missing_reference("tenant probe user", seed_index, author_index))?;
            let tenant_prefix = probe_prefix(profile, seed_index, tenant_index, None);

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-tenant-runbook"),
                probe_type: ProbeType::TenantSharedFact,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: "Which runbook is required for this tenant deploy?".to_string(),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: format!("Use {} for this tenant deploy.", refs.runbook),
                expected_fact_ids: vec![refs.runbook_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-temporal-on-call-month"),
                probe_type: ProbeType::TemporalAsOf,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} as of {}?",
                    refs.temporal_subject,
                    month_year(refs.temporal_month_as_of)
                ),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.temporal_answer.clone(),
                expected_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                as_of: Some(refs.temporal_month_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-temporal-on-call-date"),
                probe_type: ProbeType::TemporalAsOf,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} on {}?",
                    refs.temporal_subject,
                    iso_date(refs.temporal_iso_as_of)
                ),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.temporal_answer.clone(),
                expected_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                as_of: Some(refs.temporal_iso_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-temporal-on-call-current"),
                probe_type: ProbeType::TemporalAsOf,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} as of {}?",
                    refs.temporal_subject,
                    iso_date(refs.temporal_current_as_of)
                ),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.temporal_current_answer.clone(),
                expected_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                as_of: Some(refs.temporal_current_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-temporal-on-call-back-in"),
                probe_type: ProbeType::TemporalAsOf,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: format!(
                    "What was the on_call_primary for {} back in {}?",
                    refs.temporal_subject,
                    month_year(refs.temporal_month_as_of)
                ),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: refs.temporal_answer.clone(),
                expected_fact_ids: vec![refs.temporal_old_fact_id.clone()],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: vec![refs.temporal_new_fact_id.clone()],
                as_of: Some(refs.temporal_month_as_of),
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-abstain-missing-secret"),
                probe_type: ProbeType::Abstention,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: "What is the production database password?".to_string(),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: "The assistant should abstain because the corpus contains no production database password.".to_string(),
                expected_fact_ids: Vec::new(),
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });

            probes.push(Probe {
                probe_id: format!("{tenant_prefix}-contradiction-cache-backend"),
                probe_type: ProbeType::PointRecall,
                storage_partition_id: storage_partition.clone(),
                user_id: user.clone(),
                query: format!(
                    "Which cache backend claims conflict for {}?",
                    refs.contradiction_subject
                ),
                rewrite_query: None,
                expected_rewrite: None,
                query_class: None,
                answer: format!(
                    "{} has contradictory cache backend claims and should be treated as unresolved.",
                    refs.contradiction_subject
                ),
                expected_fact_ids: vec![
                    refs.contradiction_a_fact_id.clone(),
                    refs.contradiction_b_fact_id.clone(),
                ],
                expected_fact_grades: std::collections::BTreeMap::new(),
                blocked_fact_ids: Vec::new(),
                as_of: None,
                expected_redacted: false,
            });
        }
    }
    Ok(probes)
}

fn attach_rewrite_fixtures(probes: &mut [Probe]) {
    for probe in probes {
        if should_use_exact_identifier_control(probe) {
            probe.query = exact_identifier_control_query(probe);
        }
        let query_class = query_class_for_probe(probe);
        let expected_rewrite = gated_rewrite_for_class(query_class);
        probe.query_class = Some(query_class.to_string());
        probe.expected_rewrite = Some(expected_rewrite);
        probe.rewrite_query = Some(rewrite_query_for_probe(probe, query_class));
    }
}

fn should_use_exact_identifier_control(probe: &Probe) -> bool {
    probe.probe_type == ProbeType::PointRecall && probe.expected_fact_ids.len() == 1
}

fn exact_identifier_control_query(probe: &Probe) -> String {
    let fact_id = &probe.expected_fact_ids[0];
    format!("Using exact memory id \"{fact_id}\", {}", probe.query)
}

fn query_class_for_probe(probe: &Probe) -> &'static str {
    if query_has_exact_anchor(&probe.query) {
        return "exact_identifier";
    }
    match probe.probe_type {
        ProbeType::MultiHop => "multi_hop",
        ProbeType::PreferenceApplication => "vector_first",
        ProbeType::TemporalAsOf => "explicit_temporal",
        ProbeType::LatestValueAfterUpdate => "vague_followup",
        _ => "explicit",
    }
}

fn gated_rewrite_for_class(query_class: &str) -> bool {
    matches!(
        query_class,
        "coreference" | "vague_followup" | "vector_first" | "multi_hop"
    )
}

fn rewrite_query_for_probe(probe: &Probe, query_class: &str) -> String {
    match query_class {
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
        let user_key =
            (fact.scope == ScopeTier::Contact).then(|| fact.user_id.as_str().to_string());
        families
            .entry((
                fact.storage_partition_id.as_str().to_string(),
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
        fact.storage_partition_id.to_string(),
        (fact.scope == ScopeTier::Contact).then(|| fact.user_id.to_string()),
        scope_tier_str(fact.scope),
        fact.subject.clone(),
    )
}

fn scope_tier_str(scope: ScopeTier) -> &'static str {
    match scope {
        ScopeTier::Tenant => "tenant",
        ScopeTier::Contact => "contact",
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

fn build_storage_partitions(profile: CorpusProfile, count: usize) -> Vec<StoragePartitionId> {
    (0..count)
        .map(|index| {
            StoragePartitionId::new(format!(
                "memory-eval-{}-tenant-{index:02}",
                profile_slug(profile)
            ))
        })
        .collect()
}

fn tenant_index_for_user(user_index: usize, tenant_count: usize) -> usize {
    user_index % tenant_count
}

fn first_user_for_tenant(
    tenant_index: usize,
    users: &[UserId],
    tenant_count: usize,
) -> Result<usize> {
    (0..users.len())
        .find(|candidate| tenant_index_for_user(*candidate, tenant_count) == tenant_index)
        .ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "no generated user belongs to tenant index {tenant_index}"
            ))
        })
}

fn next_user_in_tenant(
    user_index: usize,
    tenant_index: usize,
    user_count: usize,
    tenant_count: usize,
) -> Result<usize> {
    for offset in 1..user_count {
        let candidate = (user_index + offset) % user_count;
        if tenant_index_for_user(candidate, tenant_count) == tenant_index {
            return Ok(candidate);
        }
    }
    invalid_config(format!(
        "tenant index {tenant_index} needs at least two users for cross-user probes"
    ))
}

fn tenant_session(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    tenant_index: usize,
    author_index: usize,
    users: &[UserId],
    storage_partitions: &[StoragePartitionId],
) -> SessionAssignment {
    SessionAssignment {
        key: format!("s{seed_index:02}-t{tenant_index:02}-tenant"),
        plan: SessionPlan {
            session_id: deterministic_session_id(
                profile,
                seed_index,
                seed,
                tenant_index,
                author_index,
                1,
            ),
            storage_partition_id: storage_partitions[tenant_index].clone(),
            user_id: users[author_index].clone(),
        },
    }
}

fn user_session(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    tenant_index: usize,
    user_index: usize,
    users: &[UserId],
    storage_partitions: &[StoragePartitionId],
) -> SessionAssignment {
    SessionAssignment {
        key: format!("s{seed_index:02}-t{tenant_index:02}-u{user_index:02}"),
        plan: SessionPlan {
            session_id: deterministic_session_id(
                profile,
                seed_index,
                seed,
                tenant_index,
                user_index,
                2,
            ),
            storage_partition_id: storage_partitions[tenant_index].clone(),
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
    storage_partitions: &[StoragePartitionId],
) -> SessionAssignment {
    let tenant_index = tenant_index_for_user(user_index, storage_partitions.len());
    SessionAssignment {
        key: format!("s{seed_index:02}-t{tenant_index:02}-u{user_index:02}-aux-{purpose}"),
        plan: SessionPlan {
            session_id: deterministic_session_id(
                profile,
                seed_index,
                seed,
                tenant_index,
                user_index,
                purpose,
            ),
            storage_partition_id: storage_partitions[tenant_index].clone(),
            user_id: users[user_index].clone(),
        },
    }
}

fn deterministic_session_id(
    profile: CorpusProfile,
    seed_index: usize,
    seed: u64,
    tenant_index: usize,
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
        | ((tenant_index as u128) << 88)
        | ((user_index as u128) << 72)
        | (purpose << 64)
        | (seed_hash & 0xFFFF_FFFF_FFFF_FFFF);
    SessionId(Uuid::from_u128(value))
}

fn fact_id(
    profile: CorpusProfile,
    seed_index: usize,
    tenant_index: usize,
    user_index: Option<usize>,
    suffix: &str,
) -> String {
    match user_index {
        Some(user_index) => format!(
            "{}-s{seed_index:02}-t{tenant_index:02}-u{user_index:02}-{suffix}",
            profile_slug(profile)
        ),
        None => format!(
            "{}-s{seed_index:02}-t{tenant_index:02}-{suffix}",
            profile_slug(profile)
        ),
    }
}

fn probe_prefix(
    profile: CorpusProfile,
    seed_index: usize,
    tenant_index: usize,
    user_index: Option<usize>,
) -> String {
    match user_index {
        Some(user_index) => format!(
            "{}-s{seed_index:02}-t{tenant_index:02}-u{user_index:02}",
            profile_slug(profile)
        ),
        None => format!(
            "{}-s{seed_index:02}-t{tenant_index:02}",
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

pub(super) fn mix_u64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

pub(super) fn distinct_user_count(corpus: &GeneratedMemoryEvalCorpus) -> usize {
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

pub(super) fn distinct_storage_partition_count(corpus: &GeneratedMemoryEvalCorpus) -> usize {
    let mut storage_partitions = BTreeSet::new();
    for fact in &corpus.ledger {
        storage_partitions.insert(fact.storage_partition_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        storage_partitions.insert(session.storage_partition_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        storage_partitions.insert(probe.storage_partition_id.as_str().to_string());
    }
    storage_partitions.len()
}

pub(super) fn sessions_per_user(sessions: &[SyntheticSession]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for session in sessions {
        *counts
            .entry(session.user_id.as_str().to_string())
            .or_insert(0) += 1;
    }
    counts
}

fn missing_reference(kind: &str, seed_index: usize, index: usize) -> EvalError {
    EvalError::InvalidConfig(format!(
        "missing generated {kind} for seed index {seed_index}, record index {index}"
    ))
}
