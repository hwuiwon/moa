//! Deterministic embedding inputs derived from generated facts and probes.

use super::*;

pub(super) fn build_embedding_inputs(
    facts: &[LedgerFact],
    probes: &[Probe],
) -> Vec<EmbeddingInput> {
    let rewrite_input_count = probes
        .iter()
        .filter(|probe| {
            probe
                .rewrite_query
                .as_ref()
                .is_some_and(|rewrite| rewrite != &probe.query)
        })
        .count();
    let mut inputs = Vec::with_capacity(facts.len() + probes.len() + rewrite_input_count);
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
            fact_ids: fact_ids.clone(),
            probe_ids: vec![probe.probe_id.clone()],
        });
        if let Some(rewrite_query) = probe
            .rewrite_query
            .as_ref()
            .filter(|rewrite| *rewrite != &probe.query)
        {
            inputs.push(EmbeddingInput {
                input_id: format!("probe:{}:rewrite", probe.probe_id),
                kind: EmbeddingInputKind::Probe,
                text: rewrite_query.clone(),
                fact_ids,
                probe_ids: vec![probe.probe_id.clone()],
            });
        }
    }
    inputs
}
