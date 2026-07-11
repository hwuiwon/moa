//! Report output and cached-fixture validation.

use super::*;

pub(super) async fn write_report(path: &Path, report: &MemoryRetrievalEvalReport) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    let json = serde_json::to_vec_pretty(report)?;
    tokio::fs::write(path, json)
        .await
        .map_err(|source| io_error(path, source))
}

pub(crate) async fn cached_embedding_provider_for_corpus(
    corpus: &LoadedMemoryEvalCorpus,
    extractor: &dyn FactExtractor,
) -> Result<CachedEmbeddingProvider> {
    let mut fixtures_by_hash = BTreeMap::<String, CachedEmbeddingFixture>::new();
    for fixture in corpus.embeddings.clone() {
        insert_fixture(&mut fixtures_by_hash, fixture)?;
    }
    ensure_embedding_input_coverage(&corpus.embedding_inputs, &fixtures_by_hash)?;

    for text in extracted_embedding_texts(&corpus.sessions, extractor).await? {
        insert_fixture(
            &mut fixtures_by_hash,
            CachedEmbeddingFixture::for_text(&text),
        )?;
    }

    CachedEmbeddingProvider::from_fixtures(fixtures_by_hash.into_values().collect())
}

pub(super) fn insert_fixture(
    fixtures_by_hash: &mut BTreeMap<String, CachedEmbeddingFixture>,
    fixture: CachedEmbeddingFixture,
) -> Result<()> {
    match fixtures_by_hash.get(&fixture.text_hash) {
        Some(existing) if existing == &fixture => Ok(()),
        Some(_) => Err(EvalError::InvalidConfig(format!(
            "cached embedding text_hash {} has conflicting fixture values",
            fixture.text_hash
        ))),
        None => {
            fixtures_by_hash.insert(fixture.text_hash.clone(), fixture);
            Ok(())
        }
    }
}

pub(super) fn ensure_embedding_input_coverage(
    inputs: &[EmbeddingInput],
    fixtures_by_hash: &BTreeMap<String, CachedEmbeddingFixture>,
) -> Result<()> {
    for input in inputs {
        let text_hash = embedding_text_hash(&input.text);
        if !fixtures_by_hash.contains_key(&text_hash) {
            return Err(EvalError::InvalidConfig(format!(
                "embeddings.jsonl is missing text_hash {text_hash} for embedding input {}",
                input.input_id
            )));
        }
    }
    Ok(())
}

pub(super) async fn extracted_embedding_texts(
    sessions: &[SyntheticSession],
    extractor: &dyn FactExtractor,
) -> Result<Vec<String>> {
    let finalized_at = DateTime::<Utc>::from_timestamp(0, 0).ok_or_else(|| {
        EvalError::InvalidConfig("failed to construct deterministic eval timestamp".to_string())
    })?;
    let mut texts = BTreeMap::<String, ()>::new();
    for session in sessions {
        for turn in &session.turns {
            let session_turn = SessionTurn {
                tenant_id: tenant_id_from_storage_partition_id(&session.storage_partition_id),
                contact_id: Some(contact_id_from_user_id(&session.user_id)),
                session_id: session.session_id,
                turn_seq: turn.turn_seq,
                transcript: turn.transcript.clone(),
                dominant_pii_class: "none".to_string(),
                finalized_at,
            };
            let chunks = chunk_turn(&session_turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS)
                .map_err(|error| {
                    EvalError::InvalidConfig(format!(
                        "failed to chunk synthetic session {} turn {}: {error}",
                        session.session_id, turn.turn_seq
                    ))
                })?;
            for fact in extractor.extract(&chunks).await.map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "failed to extract embedding texts for synthetic session {} turn {}: {error}",
                    session.session_id, turn.turn_seq
                ))
            })? {
                texts.insert(fact.summary.clone(), ());
                insert_entity_embedding_texts(&mut texts, &fact.subject);
                insert_entity_embedding_texts(&mut texts, &fact.object);
                let pii = deterministic_pii_result(&fact.summary);
                let redacted = redact_text(&fact.summary, &pii.spans);
                if redacted != fact.summary {
                    texts.insert(redacted, ());
                }
            }
        }
    }
    Ok(texts.into_keys().collect())
}

pub(super) fn insert_entity_embedding_texts(texts: &mut BTreeMap<String, ()>, mention: &str) {
    let normalized = normalize_entity_name(mention);
    if !normalized.trim().is_empty() {
        texts.insert(normalized, ());
    }

    let pii = deterministic_pii_result(mention);
    let redacted = redact_text(mention, &pii.spans);
    if redacted != mention {
        let normalized_redacted = normalize_entity_name(&redacted);
        if !normalized_redacted.trim().is_empty() {
            texts.insert(normalized_redacted, ());
        }
    }
}
