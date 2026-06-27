// Memory eval corpus fixture support.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;

use chrono::{DateTime, Utc};
use moa_brain::planning::parse_temporal;
use moa_core::{MoaError, SessionId, StoragePartitionId, UserId, traits::EmbeddingProvider};
use moa_eval::memory_eval::{
    CORPUS_SCHEMA_VERSION, CachedEmbeddingProvider, CorpusManifest, CorpusProfile,
    EmbeddingInputKind, GeneratedMemoryEvalCorpus, LedgerFact, Probe, ProbeType,
    SyntheticSession, SyntheticTurn, TranscriptStyle, build_cached_embedding_fixtures,
    embedding_text_hash, generate_memory_eval_corpus, generate_memory_eval_corpus_with_style,
    read_embedding_inputs_jsonl, read_embeddings_jsonl, read_ledger_jsonl, read_manifest_json,
    read_probes_jsonl, read_sessions_jsonl, validate_corpus, write_embeddings_jsonl,
    write_ledger_jsonl, write_manifest_json, write_memory_eval_corpus, write_probes_jsonl,
    write_sessions_jsonl,
};
use moa_eval_core::EvalError;
use moa_memory_graph::PiiClass;
use moa_memory_types::ScopeTier;
use moa_memory_vector::VECTOR_DIMENSION;
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

fn assert_profile_shape(
    corpus: &GeneratedMemoryEvalCorpus,
    expected_users: usize,
    expected_tenants: usize,
    probe_range: std::ops::RangeInclusive<usize>,
) {
    assert_eq!(corpus.manifest.seeds.len(), 3);
    assert_eq!(
        corpus
            .manifest
            .seeds
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        3,
        "generator seeds should be independent"
    );
    assert_eq!(distinct_users(corpus).len(), expected_users);
    assert_eq!(distinct_tenants(corpus).len(), expected_tenants);
    assert!(
        probe_range.contains(&corpus.probes.len()),
        "probe count {} should be in expected range",
        corpus.probes.len()
    );

    for probe_type in [
        ProbeType::PointRecall,
        ProbeType::LatestValueAfterUpdate,
        ProbeType::Abstention,
        ProbeType::CrossUserIsolation,
        ProbeType::TenantSharedFact,
        ProbeType::MultiHop,
        ProbeType::TemporalAsOf,
        ProbeType::PreferenceApplication,
        ProbeType::PiiRedaction,
    ] {
        assert!(
            corpus
                .probes
                .iter()
                .any(|probe| probe.probe_type == probe_type),
            "generated corpus should include {probe_type:?}"
        );
    }
}

fn assert_ledger_first_fact_classes(ledger: &[LedgerFact]) {
    assert!(
        ledger.iter().any(|fact| !fact.supersedes.is_empty()),
        "ledger should include supersession facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.scope == ScopeTier::Tenant && fact.predicate == "require_runbook"),
        "ledger should include tenant-shared facts"
    );
    assert!(
        ledger.iter().any(|fact| fact.scope == ScopeTier::Contact
            && fact.predicate == "private_repository"
            && fact.pii_class == PiiClass::None),
        "ledger should include user-private facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.predicate == "on_call_primary" && fact.valid_to.is_some()),
        "ledger should include temporal facts"
    );
    assert!(
        ledger.iter().any(|fact| fact.predicate == "response_style"),
        "ledger should include preference facts"
    );
    assert!(
        ledger
            .iter()
            .any(|fact| fact.pii_class == PiiClass::Pii && fact.expected_redacted),
        "ledger should include PII facts"
    );

    let mut contradiction_objects = BTreeMap::<(String, String, String), BTreeSet<String>>::new();
    for fact in ledger
        .iter()
        .filter(|fact| fact.predicate == "cache_backend_conflict")
    {
        contradiction_objects
            .entry((
                fact.storage_partition_id.as_str().to_string(),
                fact.subject.clone(),
                fact.predicate.clone(),
            ))
            .or_default()
            .insert(fact.object.clone());
    }
    assert!(
        contradiction_objects
            .values()
            .any(|objects| objects.len() >= 2),
        "ledger should include unresolved contradiction facts"
    );
}

fn distinct_users(corpus: &GeneratedMemoryEvalCorpus) -> BTreeSet<String> {
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
    users
}

fn distinct_tenants(corpus: &GeneratedMemoryEvalCorpus) -> BTreeSet<String> {
    let mut tenants = BTreeSet::new();
    for fact in &corpus.ledger {
        tenants.insert(fact.storage_partition_id.as_str().to_string());
    }
    for session in &corpus.sessions {
        tenants.insert(session.storage_partition_id.as_str().to_string());
    }
    for probe in &corpus.probes {
        tenants.insert(probe.storage_partition_id.as_str().to_string());
    }
    tenants
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

fn fixture_vector(
    fixtures: &[moa_eval::memory_eval::CachedEmbeddingFixture],
    text: &str,
) -> Vec<f32> {
    let text_hash = embedding_text_hash(text);
    fixtures
        .iter()
        .find(|fixture| fixture.text_hash == text_hash)
        .expect("fixture vector exists for text")
        .vector
        .clone()
}

fn realistic_corpus() -> (
    CorpusManifest,
    Vec<LedgerFact>,
    Vec<SyntheticSession>,
    Vec<Probe>,
) {
    let alice_session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08c01");
    let bob_session = session_id("018f0d64-7bf4-7a25-b57a-f87fd6b08c02");
    let payments_tenant = storage_partition("tenant-payments");
    let alice = user("user-alice");
    let bob = user("user-bob");

    let manifest = CorpusManifest {
        version: CORPUS_SCHEMA_VERSION,
        corpus_id: "memory-eval-pr-realistic".to_string(),
        profile: CorpusProfile::Pr,
        description:
            "PR memory retrieval corpus with updates, isolation, temporal, and PII probes."
                .to_string(),
        seeds: vec![1, 2, 3],
        transcript_style: TranscriptStyle::Marked,
    };

    let facts = vec![
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-deploy-target-v1".to_string(),
            valid_from: utc("2026-01-01T00:00:00Z"),
            valid_to: Some(utc("2026-01-08T00:00:00Z")),
            subject: "payments-api".to_string(),
            predicate: "deploy_target".to_string(),
            object: "staging".to_string(),
            answer: "As of January 5, payments-api deployed to staging.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-deploy-target-v2".to_string(),
            valid_from: utc("2026-01-08T00:00:00Z"),
            valid_to: None,
            subject: "payments-api".to_string(),
            predicate: "deploy_target".to_string(),
            object: "production-canary".to_string(),
            answer: "The latest payments-api deploy target is production-canary.".to_string(),
            supersedes: vec!["fact-deploy-target-v1".to_string()],
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 2,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Tenant,
            fact_id: "fact-runbook".to_string(),
            valid_from: utc("2026-01-02T00:00:00Z"),
            valid_to: None,
            subject: "payments-api deploys".to_string(),
            predicate: "require_runbook".to_string(),
            object: "runbook/payments-canary".to_string(),
            answer: "Payments deploys require runbook/payments-canary.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 3,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-bob-editor".to_string(),
            valid_from: utc("2026-01-03T00:00:00Z"),
            valid_to: None,
            subject: "Bob".to_string(),
            predicate: "preferred_editor".to_string(),
            object: "nvim".to_string(),
            answer: "Bob prefers nvim for config edits.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: bob_session,
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        },
        LedgerFact {
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            scope: ScopeTier::Contact,
            fact_id: "fact-alice-phone".to_string(),
            valid_from: utc("2026-01-04T00:00:00Z"),
            valid_to: None,
            subject: "Alice".to_string(),
            predicate: "contact_phone".to_string(),
            object: "+1-555-0100".to_string(),
            answer: "Alice's contact phone is [PHONE].".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: alice_session,
            source_turn_seq: 4,
            pii_class: PiiClass::Pii,
            expected_redacted: true,
        },
    ];

    let sessions = vec![
        SyntheticSession {
            session_id: alice_session,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            turns: vec![
                SyntheticTurn {
                    turn_seq: 1,
                    transcript: "Fact: payments-api deploy target is staging.".to_string(),
                    fact_ids: vec!["fact-deploy-target-v1".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 2,
                    transcript: "Fact: payments-api deploy target is now production-canary."
                        .to_string(),
                    fact_ids: vec!["fact-deploy-target-v2".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 3,
                    transcript: "Fact: payments deploys require runbook/payments-canary."
                        .to_string(),
                    fact_ids: vec!["fact-runbook".to_string()],
                },
                SyntheticTurn {
                    turn_seq: 4,
                    transcript: "Fact: Alice contact phone is +1-555-0100.".to_string(),
                    fact_ids: vec!["fact-alice-phone".to_string()],
                },
            ],
        },
        SyntheticSession {
            session_id: bob_session,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            turns: vec![SyntheticTurn {
                turn_seq: 1,
                transcript: "Fact: Bob prefers nvim for config edits.".to_string(),
                fact_ids: vec!["fact-bob-editor".to_string()],
            }],
        },
    ];

    let probes = vec![
        probe(ProbeSpec {
            probe_id: "probe-point-recall",
            probe_type: ProbeType::PointRecall,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What is the latest payments-api deploy target?",
            answer: "The latest payments-api deploy target is production-canary.",
            expected_fact_ids: &["fact-deploy-target-v2"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-latest-value",
            probe_type: ProbeType::LatestValueAfterUpdate,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "After the deploy target update, where should payments-api deploy?",
            answer: "The latest payments-api deploy target is production-canary.",
            expected_fact_ids: &["fact-deploy-target-v2"],
            blocked_fact_ids: &["fact-deploy-target-v1"],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-abstention",
            probe_type: ProbeType::Abstention,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What is the database password for payments production?",
            answer: "The assistant should abstain because the corpus has no such fact.",
            expected_fact_ids: &[],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-cross-user-isolation",
            probe_type: ProbeType::CrossUserIsolation,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "What editor does Bob prefer?",
            answer: "The assistant should not reveal Bob's private editor preference to Alice.",
            expected_fact_ids: &[],
            blocked_fact_ids: &["fact-bob-editor"],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-tenant-shared",
            probe_type: ProbeType::TenantSharedFact,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "Which runbook is required for payments deploys?",
            answer: "Payments deploys require runbook/payments-canary.",
            expected_fact_ids: &["fact-runbook"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-multi-hop",
            probe_type: ProbeType::MultiHop,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice.clone(),
            query: "Where should payments-api deploy and which runbook applies?",
            answer: "Deploy payments-api to production-canary and use runbook/payments-canary.",
            expected_fact_ids: &["fact-deploy-target-v2", "fact-runbook"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-temporal-as-of",
            probe_type: ProbeType::TemporalAsOf,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "Where did payments-api deploy on January 5, 2026?",
            answer: "As of January 5, payments-api deployed to staging.",
            expected_fact_ids: &["fact-deploy-target-v1"],
            blocked_fact_ids: &["fact-deploy-target-v2"],
            as_of: Some(utc("2026-01-05T12:00:00Z")),
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-preference-application",
            probe_type: ProbeType::PreferenceApplication,
            storage_partition_id: payments_tenant.clone(),
            user_id: bob.clone(),
            query: "I need to edit a config file; which editor should you use for me?",
            answer: "Use nvim for Bob's config edit.",
            expected_fact_ids: &["fact-bob-editor"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: false,
        }),
        probe(ProbeSpec {
            probe_id: "probe-pii-redaction",
            probe_type: ProbeType::PiiRedaction,
            storage_partition_id: payments_tenant.clone(),
            user_id: alice,
            query: "What is Alice's contact phone?",
            answer: "Alice's contact phone is [PHONE].",
            expected_fact_ids: &["fact-alice-phone"],
            blocked_fact_ids: &[],
            as_of: None,
            expected_redacted: true,
        }),
    ];

    (manifest, facts, sessions, probes)
}

struct ProbeSpec<'a> {
    probe_id: &'a str,
    probe_type: ProbeType,
    storage_partition_id: StoragePartitionId,
    user_id: UserId,
    query: &'a str,
    answer: &'a str,
    expected_fact_ids: &'a [&'a str],
    blocked_fact_ids: &'a [&'a str],
    as_of: Option<DateTime<Utc>>,
    expected_redacted: bool,
}

fn probe(spec: ProbeSpec<'_>) -> Probe {
    Probe {
        probe_id: spec.probe_id.to_string(),
        probe_type: spec.probe_type,
        storage_partition_id: spec.storage_partition_id,
        user_id: spec.user_id,
        query: spec.query.to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: spec.answer.to_string(),
        expected_fact_ids: spec
            .expected_fact_ids
            .iter()
            .map(|fact_id| (*fact_id).to_string())
            .collect(),
        blocked_fact_ids: spec
            .blocked_fact_ids
            .iter()
            .map(|fact_id| (*fact_id).to_string())
            .collect(),
        as_of: spec.as_of,
        expected_redacted: spec.expected_redacted,
    }
}

fn session_id(value: &str) -> SessionId {
    SessionId(Uuid::parse_str(value).expect("stable fixture session UUID"))
}

fn user(value: &str) -> UserId {
    UserId::new(value)
}

fn storage_partition(value: &str) -> StoragePartitionId {
    StoragePartitionId::new(value)
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp parses")
        .with_timezone(&Utc)
}
