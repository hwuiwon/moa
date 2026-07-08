#[path = "memory_eval_support/common.rs"]
mod common;
use common::*;

include!("memory_eval_support/corpus.rs");

#[tokio::test]
async fn memory_eval_corpus_round_trips_versioned_jsonl() {
    // Pins: memory eval corpus files preserve scoped, temporal, PII, and probe metadata.
    let (manifest, facts, sessions, probes) = realistic_corpus();
    let temp = tempfile::tempdir().expect("create temp corpus directory");
    let manifest_path = temp.path().join("manifest.json");
    let ledger_path = temp.path().join("ledger.jsonl");
    let sessions_path = temp.path().join("sessions.jsonl");
    let probes_path = temp.path().join("probes.jsonl");

    write_manifest_json(&manifest_path, &manifest)
        .await
        .expect("write manifest");
    write_ledger_jsonl(&ledger_path, &facts)
        .await
        .expect("write ledger jsonl");
    write_sessions_jsonl(&sessions_path, &sessions)
        .await
        .expect("write sessions jsonl");
    write_probes_jsonl(&probes_path, &probes, &facts)
        .await
        .expect("write probes jsonl");

    let round_tripped_manifest = read_manifest_json(&manifest_path)
        .await
        .expect("read manifest");
    let round_tripped_facts = read_ledger_jsonl(&ledger_path)
        .await
        .expect("read ledger jsonl");
    let round_tripped_sessions = read_sessions_jsonl(&sessions_path)
        .await
        .expect("read sessions jsonl");
    let round_tripped_probes = read_probes_jsonl(&probes_path, &round_tripped_facts)
        .await
        .expect("read probes jsonl");

    validate_corpus(
        &round_tripped_manifest,
        &round_tripped_facts,
        &round_tripped_sessions,
        &round_tripped_probes,
    )
    .expect("round-tripped corpus validates");

    assert_eq!(round_tripped_manifest, manifest);
    assert_eq!(round_tripped_facts, facts);
    assert_eq!(round_tripped_sessions, sessions);
    assert_eq!(round_tripped_probes, probes);

    let probes_jsonl = tokio::fs::read_to_string(&probes_path)
        .await
        .expect("read probes jsonl text");
    for probe_type in [
        "point_recall",
        "latest_value_after_update",
        "abstention",
        "cross_user_isolation",
        "tenant_shared_fact",
        "multi_hop",
        "temporal_as_of",
        "preference_application",
        "pii_redaction",
    ] {
        assert!(
            probes_jsonl.contains(probe_type),
            "serialized probes should include {probe_type}"
        );
    }
}

#[tokio::test]
async fn memory_eval_corpus_rejects_cross_user_probe_owned_by_asking_user() {
    // Pins: cross-user isolation probes must reference another user's private fact.
    let (_, facts, _, _) = realistic_corpus();
    let temp = tempfile::tempdir().expect("create temp corpus directory");
    let probes_path = temp.path().join("probes.jsonl");
    let bad_probe = Probe {
        probe_id: "probe-cross-user-bad-owner".to_string(),
        probe_type: ProbeType::CrossUserIsolation,
        storage_partition_id: storage_partition("tenant-payments"),
        user_id: user("user-bob"),
        query: "What editor does Bob prefer?".to_string(),
        rewrite_query: None,
        expected_rewrite: None,
        query_class: None,
        answer: "The assistant should abstain instead of exposing Bob's private preference."
            .to_string(),
        expected_fact_ids: Vec::new(),
        expected_fact_grades: std::collections::BTreeMap::new(),
        blocked_fact_ids: vec!["fact-bob-editor".to_string()],
        as_of: None,
        expected_redacted: false,
    };

    let error = write_probes_jsonl(&probes_path, &[bad_probe], &facts)
        .await
        .expect_err("cross-user probe owned by asking user should fail validation");

    match error {
        EvalError::InvalidConfig(message) => {
            assert!(
                message.contains("cross-user isolation")
                    && message.contains("probe-cross-user-bad-owner")
                    && message.contains("fact-bob-editor"),
                "error should identify the invalid cross-user probe: {message}"
            );
        }
        other => panic!("expected EvalError::InvalidConfig, got {other:?}"),
    }
}

#[tokio::test]
async fn memory_eval_pr_generator_writes_byte_stable_ledger_first_corpus() {
    // Pins: PR corpus generation is deterministic and includes every ledger-first fact class.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    assert_profile_shape(&corpus, 5, 2, 60..=usize::MAX);
    assert_ledger_first_fact_classes(&corpus.ledger);

    let temp = tempfile::tempdir().expect("create temp corpus root");
    let first_dir = temp.path().join("pr-a");
    let second_dir = temp.path().join("pr-b");
    write_memory_eval_corpus(&first_dir, &corpus)
        .await
        .expect("write first generated corpus");
    write_memory_eval_corpus(
        &second_dir,
        &generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
            .expect("regenerate PR memory eval corpus"),
    )
    .await
    .expect("write second generated corpus");

    for file_name in [
        "manifest.json",
        "ledger.jsonl",
        "sessions.jsonl",
        "probes.jsonl",
        "embedding_inputs.jsonl",
    ] {
        let first = tokio::fs::read(first_dir.join(file_name))
            .await
            .expect("read first generated file");
        let second = tokio::fs::read(second_dir.join(file_name))
            .await
            .expect("read second generated file");
        assert_eq!(first, second, "{file_name} should be byte-stable");
    }

    let manifest = read_manifest_json(&first_dir.join("manifest.json"))
        .await
        .expect("read generated manifest");
    let ledger = read_ledger_jsonl(&first_dir.join("ledger.jsonl"))
        .await
        .expect("read generated ledger");
    let sessions = read_sessions_jsonl(&first_dir.join("sessions.jsonl"))
        .await
        .expect("read generated sessions");
    let probes = read_probes_jsonl(&first_dir.join("probes.jsonl"), &ledger)
        .await
        .expect("read generated probes");
    let embedding_inputs =
        read_embedding_inputs_jsonl(&first_dir.join("embedding_inputs.jsonl"), &ledger, &probes)
            .await
            .expect("read generated embedding inputs");

    validate_corpus(&manifest, &ledger, &sessions, &probes).expect("generated corpus validates");
    assert!(
        probes.iter().all(|probe| probe.query_class.is_some()
            && probe.expected_rewrite.is_some()
            && probe.rewrite_query.is_some()),
        "each generated probe should carry deterministic query-rewrite fixtures"
    );
    assert!(
        probes.iter().any(|probe| probe
            .rewrite_query
            .as_ref()
            .is_some_and(|rewrite| rewrite != &probe.query)),
        "at least one generated rewrite fixture should differ from the original query"
    );
    assert!(
        embedding_inputs.len() > ledger.len() + probes.len(),
        "embedding inputs should include original probes plus rewrite fixtures"
    );
    assert_eq!(
        embedding_inputs
            .iter()
            .filter(|input| input.kind == EmbeddingInputKind::Fact)
            .count(),
        ledger.len()
    );
    assert!(
        embedding_inputs
            .iter()
            .filter(|input| input.kind == EmbeddingInputKind::Probe)
            .count()
            > probes.len(),
        "probe embedding inputs should include rewrite fixture variants"
    );
}

#[test]
fn generator_emits_four_temporal_variants_per_supersession_chain() {
    // Pins: each PR tenant supersession chain emits four absolute-date temporal probes.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    let temporal = corpus
        .probes
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::TemporalAsOf)
        .collect::<Vec<_>>();

    assert_eq!(temporal.len(), 24);
    for suffix in ["month", "date", "current", "back-in"] {
        assert_eq!(
            temporal
                .iter()
                .filter(|probe| probe.probe_id.ends_with(suffix))
                .count(),
            6,
            "expected one `{suffix}` temporal probe per seed/tenant chain"
        );
    }
    assert!(
        temporal.iter().all(|probe| probe.as_of.is_some()),
        "each temporal probe should carry the instant encoded in query text"
    );
    for probe in temporal {
        assert_eq!(
            parse_temporal(&probe.query),
            probe.as_of,
            "temporal parser should recover generator query date for {}",
            probe.probe_id
        );
    }
}

#[test]
fn manifest_round_trips_transcript_style_and_defaults_to_marked() -> TestResult {
    // Pins: prompt-02-era manifests remain readable and new manifests preserve transcript style.
    let old_manifest = serde_json::json!({
        "version": CORPUS_SCHEMA_VERSION,
        "corpus_id": "memory-eval-pr-minimal",
        "profile": "pr",
        "description": "manifest without transcript style",
        "seeds": [1, 2, 3]
    });
    let parsed_old: CorpusManifest = serde_json::from_value(old_manifest)?;
    assert_eq!(parsed_old.transcript_style, TranscriptStyle::Marked);

    let natural_manifest = serde_json::json!({
        "version": CORPUS_SCHEMA_VERSION,
        "corpus_id": "memory-eval-pr-natural-1-2-3",
        "profile": "pr",
        "description": "natural manifest",
        "seeds": [1, 2, 3],
        "transcript_style": "natural"
    });
    let parsed_natural: CorpusManifest = serde_json::from_value(natural_manifest)?;
    assert_eq!(parsed_natural.transcript_style, TranscriptStyle::Natural);
    Ok(())
}

#[test]
fn natural_transcripts_contain_no_fact_markers() {
    // Pins: natural transcripts do not use marker tokens the heuristic extractor was tuned for.
    let corpus = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");

    for turn in corpus.sessions.iter().flat_map(|session| &session.turns) {
        for forbidden in ["Fact:", "tenant shared", "contact private"] {
            assert!(
                !turn.transcript.contains(forbidden),
                "natural transcript should not contain marker `{forbidden}`: {}",
                turn.transcript
            );
        }
    }
    assert!(
        corpus
            .sessions
            .iter()
            .all(|session| session.turns.iter().any(|turn| turn.fact_ids.is_empty())),
        "each natural session should include at least one distractor turn"
    );
}

#[test]
fn natural_pr_corpus_chunks_are_covered_by_recorded_fixtures() {
    use moa_eval::memory_eval::missing_extraction_chunk_hashes;
    // Pins: every chunk the recorded extraction lane will hash has a committed
    // fixture, so a natural-renderer or chunking change fails in the PR that
    // causes the drift instead of at the next recorded eval run.
    let corpus = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");
    let fixtures_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/memory");
    let fixture_path = ["v3", "v2"]
        .iter()
        .map(|version| {
            fixtures_dir.join(format!(
                "extractions-{}-{version}.jsonl",
                corpus.manifest.corpus_id
            ))
        })
        .find(|candidate| candidate.exists())
        .expect("a recorded extraction fixture file should exist for the natural PR corpus");

    let missing = missing_extraction_chunk_hashes(&corpus.sessions, &corpus.ledger, &fixture_path)
        .expect("compute fixture coverage");

    assert!(
        missing.is_empty(),
        "{} corpus chunk(s) have no recorded extraction fixture — the natural renderer          or chunking drifted. Re-record with: cargo run -p xtask --          record-memory-extractions --corpus target/memory-eval/pr-natural\nmissing: {:?}",
        missing.len(),
        &missing[..missing.len().min(5)]
    );
}

#[test]
fn natural_generation_is_deterministic_for_same_seed() {
    // Pins: natural corpus generation is byte-stable for the same profile and seeds.
    let first = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate first natural corpus");
    let second = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate second natural corpus");

    assert_eq!(first, second);
}

#[test]
fn corpus_id_encodes_transcript_style() {
    // Pins: marked and natural corpora have distinct identities for paired comparison.
    let marked = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate marked PR corpus");
    let natural = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");

    assert_eq!(marked.manifest.corpus_id, "memory-eval-pr-marked-1-2-3");
    assert_eq!(natural.manifest.corpus_id, "memory-eval-pr-natural-1-2-3");
    assert_ne!(marked.manifest.corpus_id, natural.manifest.corpus_id);
}

#[test]
fn natural_frames_cover_every_generated_predicate() {
    // Pins: generated predicates stay inside the deterministic natural phrase table contract.
    let corpus = generate_memory_eval_corpus_with_style(
        CorpusProfile::Pr,
        vec![1, 2, 3],
        TranscriptStyle::Natural,
    )
    .expect("generate natural PR corpus");
    let predicates = corpus
        .ledger
        .iter()
        .map(|fact| fact.predicate.as_str())
        .collect::<BTreeSet<_>>();
    let expected = BTreeSet::from([
        "cache_backend_conflict",
        "contact_email",
        "depends_on",
        "deploy_target",
        "on_call_primary",
        "owned_by",
        "private_repository",
        "require_runbook",
        "response_style",
    ]);

    assert_eq!(predicates, expected);
}

#[test]
fn multi_hop_templates_emit_two_expected_fact_ids_sharing_entity() {
    // Pins: multi-hop probes require a dependency fact and an owner fact linked by library.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate marked PR corpus");
    let facts = corpus
        .ledger
        .iter()
        .map(|fact| (fact.fact_id.as_str(), fact))
        .collect::<HashMap<_, _>>();

    for probe in corpus
        .probes
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::MultiHop)
    {
        assert_eq!(probe.expected_fact_ids.len(), 2);
        let dependency = facts
            .get(probe.expected_fact_ids[0].as_str())
            .expect("dependency fact exists");
        let owner = facts
            .get(probe.expected_fact_ids[1].as_str())
            .expect("owner fact exists");
        assert_eq!(dependency.predicate, "depends_on");
        assert_eq!(owner.predicate, "owned_by");
        assert_eq!(dependency.object, owner.subject);
        assert_ne!(dependency.source_session_id, owner.source_session_id);
    }
}

#[test]
fn pr_profile_emits_at_least_thirty_multi_hop_probes() {
    // Pins: prompt 04 has enough multi-hop probes for a statistical graph-leg gate.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate marked PR corpus");
    let multi_hop_count = corpus
        .probes
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::MultiHop)
        .count();

    assert!(
        multi_hop_count >= 30,
        "PR profile should emit at least 30 multi-hop probes, got {multi_hop_count}"
    );
}

#[tokio::test]
async fn cached_embedding_provider_returns_fixture_vectors_and_missing_hash_errors() {
    // Pins: cached embeddings are hermetic, dimension-checked, order-preserving, and fail closed.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
        .expect("generate PR memory eval corpus");
    let fixtures = build_cached_embedding_fixtures(&corpus.embedding_inputs)
        .expect("build deterministic cached embedding fixtures");
    assert!(
        fixtures
            .iter()
            .all(|fixture| fixture.dimension == VECTOR_DIMENSION
                && fixture.vector.len() == VECTOR_DIMENSION),
        "every cached fixture should match moa_memory_vector::VECTOR_DIMENSION"
    );

    let temp = tempfile::tempdir().expect("create temp embedding fixture directory");
    let embeddings_path = temp.path().join("embeddings.jsonl");
    write_embeddings_jsonl(&embeddings_path, &fixtures)
        .await
        .expect("write cached embeddings jsonl");

    let serialized = tokio::fs::read_to_string(&embeddings_path)
        .await
        .expect("read embeddings jsonl text");
    assert!(
        serialized.contains("\"text_hash\"")
            && serialized.contains("\"model\"")
            && serialized.contains("\"dimension\"")
            && serialized.contains("\"vector\""),
        "embeddings.jsonl should preserve the frozen fixture fields"
    );

    let loaded_fixtures = read_embeddings_jsonl(&embeddings_path)
        .await
        .expect("read cached embeddings jsonl");
    let provider = CachedEmbeddingProvider::from_jsonl(&embeddings_path)
        .await
        .expect("load cached embedding provider");
    assert_eq!(provider.dimensions(), VECTOR_DIMENSION);

    let first_input = corpus
        .embedding_inputs
        .first()
        .expect("generated corpus has embedding inputs");
    let last_input = corpus
        .embedding_inputs
        .last()
        .expect("generated corpus has embedding inputs");
    let request = vec![last_input.text.clone(), first_input.text.clone()];
    let embeddings = provider
        .embed(&request)
        .await
        .expect("embed from cached fixtures");
    assert_eq!(embeddings.len(), 2);
    assert_eq!(
        embeddings[0],
        fixture_vector(&loaded_fixtures, &last_input.text)
    );
    assert_eq!(
        embeddings[1],
        fixture_vector(&loaded_fixtures, &first_input.text)
    );

    let missing_text = "this text intentionally has no cached fixture".to_string();
    let missing_hash = embedding_text_hash(&missing_text);
    let error = provider
        .embed(&[missing_text])
        .await
        .expect_err("missing cached fixture should fail closed");
    match error {
        MoaError::ProviderError(message) => assert!(
            message.contains(&missing_hash),
            "missing fixture error should name text_hash {missing_hash}: {message}"
        ),
        other => panic!("expected MoaError::ProviderError, got {other:?}"),
    }
}

#[test]
fn memory_eval_full_generator_respects_profile_bounds() {
    // Pins: full corpus generation stays within the promised user, tenant, session, and probe bounds.
    let corpus = generate_memory_eval_corpus(CorpusProfile::Full, vec![11, 12, 13])
        .expect("generate full memory eval corpus");
    assert_profile_shape(&corpus, 50, 3, 600..=1_000);

    let session_counts = sessions_per_user(&corpus.sessions);
    assert_eq!(distinct_users(&corpus).len(), 50);
    for (user_id, session_count) in session_counts {
        assert!(
            session_count <= 100,
            "{user_id} should have at most 100 sessions, got {session_count}"
        );
    }
}
