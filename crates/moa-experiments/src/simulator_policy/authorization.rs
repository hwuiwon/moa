//! The gate a live, billed fidelity study must pass before it runs.
//!
//! A fidelity study calls a paid provider many times and reads consented human
//! interactions. Neither may happen because a test binary happened to be
//! scheduled. Four independent things must be true, and each missing one is
//! reported distinctly so an operator can tell "not authorized" from
//! "misconfigured":
//!
//! 1. an explicit opt-in flag;
//! 2. a positive budget, parsed into the same integer micro-USD the study's cost
//!    record uses;
//! 3. simulator provider credentials;
//! 4. a named human-data authorization record.
//!
//! [`LiveFidelityAuthorization::resolve`] takes a lookup function rather than
//! reading the process environment, so tests can exercise every refusal without
//! mutating shared global state and without serializing against each other.
//! [`LiveFidelityAuthorization::from_env`] is the thin wrapper the live test
//! binary uses.

/// Environment variable that opts a run in to live fidelity studies.
pub const LIVE_FIDELITY_FLAG_ENV: &str = "MOA_RUN_LIVE_FIDELITY_TESTS";

/// Environment variable carrying the study budget in decimal USD.
pub const LIVE_FIDELITY_BUDGET_ENV: &str = "MOA_FIDELITY_STUDY_BUDGET_USD";

/// Environment variable carrying the simulator provider credential.
pub const LIVE_FIDELITY_CREDENTIAL_ENV: &str = "MOA_FIDELITY_SIMULATOR_API_KEY";

/// Environment variable naming the approved human-data authorization record.
pub const LIVE_FIDELITY_HUMAN_DATA_ENV: &str = "MOA_FIDELITY_HUMAN_DATA_AUTHORIZATION";

/// Micro-USD in one US dollar.
const MICRO_USD_PER_USD: u64 = 1_000_000;

/// Authorization to run one live, billed fidelity study.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveFidelityAuthorization {
    /// Budget the study may spend, in micro-USD.
    pub budget_micro_usd: u64,
    /// Human-data authorization record the study runs under.
    pub human_data_authorization_id: String,
}

/// Why a live fidelity study was not authorized.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LiveFidelityRefusal {
    /// The explicit opt-in flag is absent or not `1`.
    #[error("live fidelity studies require {LIVE_FIDELITY_FLAG_ENV}=1")]
    NotOptedIn,
    /// No budget was supplied.
    #[error("live fidelity studies require a positive {LIVE_FIDELITY_BUDGET_ENV}")]
    BudgetMissing,
    /// The budget was supplied but is not a positive decimal USD amount.
    #[error("{LIVE_FIDELITY_BUDGET_ENV} must be a positive decimal USD amount, got `{value}`")]
    BudgetInvalid {
        /// Raw value that could not be used.
        value: String,
    },
    /// The simulator provider credential is absent or empty.
    #[error("live fidelity studies require {LIVE_FIDELITY_CREDENTIAL_ENV}")]
    CredentialMissing,
    /// No approved human-data authorization record was named.
    #[error(
        "live fidelity studies require {LIVE_FIDELITY_HUMAN_DATA_ENV} naming an approved \
         human-data authorization"
    )]
    HumanDataAuthorizationMissing,
}

impl LiveFidelityAuthorization {
    /// Resolves authorization from a variable lookup.
    ///
    /// Every check is independent, and the first unmet one is reported: an
    /// operator fixing the configuration sees one actionable reason at a time
    /// rather than a single opaque refusal.
    ///
    /// # Errors
    ///
    /// Returns [`LiveFidelityRefusal`] when the opt-in flag, budget, credential,
    /// or human-data authorization is missing or unusable.
    pub fn resolve<F>(lookup: F) -> Result<Self, LiveFidelityRefusal>
    where
        F: Fn(&str) -> Option<String>,
    {
        let flag = lookup(LIVE_FIDELITY_FLAG_ENV).unwrap_or_default();
        if flag.trim() != "1" {
            return Err(LiveFidelityRefusal::NotOptedIn);
        }
        let raw_budget = lookup(LIVE_FIDELITY_BUDGET_ENV)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or(LiveFidelityRefusal::BudgetMissing)?;
        let budget_micro_usd = parse_budget_micro_usd(&raw_budget)?;
        let credential = lookup(LIVE_FIDELITY_CREDENTIAL_ENV).unwrap_or_default();
        if credential.trim().is_empty() {
            return Err(LiveFidelityRefusal::CredentialMissing);
        }
        let human_data_authorization_id = lookup(LIVE_FIDELITY_HUMAN_DATA_ENV)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or(LiveFidelityRefusal::HumanDataAuthorizationMissing)?;
        Ok(Self {
            budget_micro_usd,
            human_data_authorization_id,
        })
    }

    /// Resolves authorization from the process environment.
    ///
    /// # Errors
    ///
    /// Returns [`LiveFidelityRefusal`] for the same reasons as
    /// [`LiveFidelityAuthorization::resolve`].
    pub fn from_env() -> Result<Self, LiveFidelityRefusal> {
        Self::resolve(|name| std::env::var(name).ok())
    }
}

/// Parses a decimal USD budget into integer micro-USD.
///
/// Integer micro-USD rather than a float: a hard spend ceiling must not be
/// decided by binary rounding, and the study's cost record is already micro-USD.
fn parse_budget_micro_usd(value: &str) -> Result<u64, LiveFidelityRefusal> {
    let invalid = || LiveFidelityRefusal::BudgetInvalid {
        value: value.to_string(),
    };
    let (whole_part, fraction_part) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (value, ""),
    };
    if whole_part.is_empty() && fraction_part.is_empty() {
        return Err(invalid());
    }
    if !whole_part.chars().all(|ch| ch.is_ascii_digit())
        || !fraction_part.chars().all(|ch| ch.is_ascii_digit())
        || fraction_part.len() > 6
    {
        return Err(invalid());
    }
    let whole: u64 = if whole_part.is_empty() {
        0
    } else {
        whole_part.parse().map_err(|_| invalid())?
    };
    let mut fraction: u64 = if fraction_part.is_empty() {
        0
    } else {
        fraction_part.parse().map_err(|_| invalid())?
    };
    for _ in fraction_part.len()..6 {
        fraction = fraction.checked_mul(10).ok_or_else(invalid)?;
    }
    let micro = whole
        .checked_mul(MICRO_USD_PER_USD)
        .and_then(|scaled| scaled.checked_add(fraction))
        .ok_or_else(invalid)?;
    if micro == 0 {
        return Err(invalid());
    }
    Ok(micro)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn complete() -> Vec<(&'static str, &'static str)> {
        vec![
            (LIVE_FIDELITY_FLAG_ENV, "1"),
            (LIVE_FIDELITY_BUDGET_ENV, "12.50"),
            (LIVE_FIDELITY_CREDENTIAL_ENV, "sk-fixture"),
            (LIVE_FIDELITY_HUMAN_DATA_ENV, "hda-2026-q2"),
        ]
    }

    // Pins: a fully authorized run yields the budget in integer micro-USD and the
    // named human-data authorization.
    #[test]
    fn complete_authorization_yields_budget_and_record_offline() {
        let authorization =
            LiveFidelityAuthorization::resolve(lookup(&complete())).expect("fully authorized");
        assert_eq!(
            authorization,
            LiveFidelityAuthorization {
                budget_micro_usd: 12_500_000,
                human_data_authorization_id: "hda-2026-q2".to_string(),
            }
        );
    }

    // Pins: each of the four requirements independently refuses the run, so no
    // single environment variable can authorize a billed study on its own.
    #[test]
    fn every_missing_requirement_refuses_independently_offline() {
        for (removed, expected) in [
            (LIVE_FIDELITY_FLAG_ENV, LiveFidelityRefusal::NotOptedIn),
            (LIVE_FIDELITY_BUDGET_ENV, LiveFidelityRefusal::BudgetMissing),
            (
                LIVE_FIDELITY_CREDENTIAL_ENV,
                LiveFidelityRefusal::CredentialMissing,
            ),
            (
                LIVE_FIDELITY_HUMAN_DATA_ENV,
                LiveFidelityRefusal::HumanDataAuthorizationMissing,
            ),
        ] {
            let pairs: Vec<(&str, &str)> = complete()
                .into_iter()
                .filter(|(key, _)| *key != removed)
                .collect();
            assert_eq!(
                LiveFidelityAuthorization::resolve(lookup(&pairs))
                    .expect_err("a missing requirement must refuse"),
                expected,
                "removing {removed} must refuse"
            );
        }

        // Present but empty is the same as absent.
        let mut blanked = complete();
        blanked[2].1 = "   ";
        assert_eq!(
            LiveFidelityAuthorization::resolve(lookup(&blanked))
                .expect_err("blank credential must refuse"),
            LiveFidelityRefusal::CredentialMissing
        );
    }

    // Pins: a zero or malformed budget refuses, so "authorized with no money" is
    // never a runnable state.
    #[test]
    fn non_positive_or_malformed_budget_refuses_offline() {
        for bad in ["0", "0.000000", "-5", "abc", "1.2345678", "", "1..2", "1e3"] {
            let mut pairs = complete();
            pairs[1].1 = bad;
            let error = LiveFidelityAuthorization::resolve(lookup(&pairs))
                .expect_err("budget `{bad}` must refuse");
            assert!(
                matches!(
                    error,
                    LiveFidelityRefusal::BudgetInvalid { .. } | LiveFidelityRefusal::BudgetMissing
                ),
                "budget `{bad}` produced {error}"
            );
        }
    }

    // Pins: decimal USD converts to exact integer micro-USD.
    #[test]
    fn budget_parses_to_exact_micro_usd_offline() {
        for (raw, expected) in [
            ("1", 1_000_000_u64),
            ("0.5", 500_000),
            ("0.000001", 1),
            ("250.125", 250_125_000),
        ] {
            let mut pairs = complete();
            pairs[1].1 = raw;
            let authorization =
                LiveFidelityAuthorization::resolve(lookup(&pairs)).expect("valid budget");
            assert_eq!(
                authorization.budget_micro_usd, expected,
                "budget `{raw}` must parse exactly"
            );
        }
    }
}
