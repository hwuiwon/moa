//! Generator seed, profile, schedule, and embedding-input validation.

use super::model::{
    FactCategory, ScheduledFact, distinct_storage_partition_count, distinct_user_count,
    sessions_per_user,
};
use super::*;

pub(super) fn validate_seeds(seeds: &[u64]) -> Result<()> {
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

pub(super) fn validate_schedule_categories(scheduled_facts: &[ScheduledFact]) -> Result<()> {
    let categories = scheduled_facts
        .iter()
        .map(|scheduled| scheduled.category)
        .collect::<BTreeSet<_>>();
    for category in [
        FactCategory::Supersession,
        FactCategory::Contradiction,
        FactCategory::TenantShared,
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

pub(super) fn validate_profile_shape(corpus: &GeneratedMemoryEvalCorpus) -> Result<()> {
    validate_seeds(&corpus.manifest.seeds)?;
    let user_count = distinct_user_count(corpus);
    let tenant_count = distinct_storage_partition_count(corpus);
    match corpus.manifest.profile {
        CorpusProfile::Pr => {
            if user_count != PR_USER_COUNT {
                return invalid_config(format!(
                    "PR corpus must contain {PR_USER_COUNT} users; got {user_count}"
                ));
            }
            if tenant_count != PR_TENANT_COUNT {
                return invalid_config(format!(
                    "PR corpus must contain {PR_TENANT_COUNT} tenants; got {tenant_count}"
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
            if tenant_count != FULL_TENANT_COUNT {
                return invalid_config(format!(
                    "full corpus must contain {FULL_TENANT_COUNT} tenants; got {tenant_count}"
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

pub(super) fn validate_embedding_inputs(
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
