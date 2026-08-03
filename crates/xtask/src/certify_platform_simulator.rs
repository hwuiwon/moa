//! Operator command for recording platform simulator fidelity certification.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use moa_artifacts::release::{
    Digest32, PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID,
    PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION, PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
};
use moa_artifacts::simulation::SimulatorPolicyReference;
use moa_core::canonical_json::canonical_json_bytes;
use moa_experiments::simulator_policy::fidelity::{CertificationOutcome, FidelityStudyArtifact};
use moa_experiments::simulator_policy::store::SimulatorPolicyStore;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

const DATABASE_ADMIN_URL_ENV: &str = "MOA_DATABASE_ADMIN_URL";
const PLATFORM_RELEASE_DOMAIN: &str = "artifact-release";
const USAGE: &str = "usage: cargo run -p xtask --features eval-tools -- certify-platform-simulator --artifact <canonical-json-path> --mandate-id <uuid>";

#[derive(Debug, Eq, PartialEq)]
struct CertifyArgs {
    artifact: PathBuf,
    mandate_uid: Uuid,
}

#[derive(Serialize)]
struct SafeCertificationReport {
    policy_uid: Uuid,
    policy_revision: i32,
    policy_hash: Digest32,
    study_uid: Uuid,
    artifact_hash: Digest32,
    verdict: &'static str,
    certified_until: Option<DateTime<Utc>>,
}

/// Records a canonical fidelity study and succeeds only for a currently usable certification.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let args = parse_args(args)?;
    let source = fs::read(&args.artifact)
        .with_context(|| format!("read fidelity artifact {}", args.artifact.display()))?;
    let artifact = parse_canonical_artifact(&source)?;
    validate_platform_identity(&artifact)?;
    let artifact_hash = artifact
        .digest()
        .map_err(anyhow::Error::from)
        .context("hash fidelity artifact")?;

    let database_url = env::var(DATABASE_ADMIN_URL_ENV)
        .with_context(|| format!("{DATABASE_ADMIN_URL_ENV} is required"))?;
    if database_url.trim().is_empty() {
        bail!("{DATABASE_ADMIN_URL_ENV} must not be empty");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build platform simulator certification runtime")?;
    runtime.block_on(certify(
        &database_url,
        args.mandate_uid,
        &artifact,
        artifact_hash,
    ))
}

async fn certify(
    database_url: &str,
    mandate_uid: Uuid,
    artifact: &FidelityStudyArtifact,
    artifact_hash: Digest32,
) -> Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET ROLE moa_promoter")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .context("connect to Postgres as the simulator certification operator")?;
    let store = SimulatorPolicyStore::new(pool);
    let outcome = store
        .record_platform_study(mandate_uid, artifact, Utc::now())
        .await
        .map_err(anyhow::Error::from)
        .context("record platform simulator fidelity study")?;
    let report = safe_report(artifact, artifact_hash, &outcome);

    if matches!(&outcome, CertificationOutcome::Certified { .. }) {
        let reference = SimulatorPolicyReference {
            policy_uid: PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
            revision: PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
        };
        if store
            .resolve_platform_policy(reference, Utc::now())
            .await
            .is_ok()
        {
            write_report(&report)?;
            return Ok(());
        }
        write_report(&report)?;
        bail!("certified platform simulator policy is not currently resolvable");
    }

    write_report(&report)?;
    bail!(
        "platform simulator fidelity study recorded with {} verdict",
        outcome.verdict()
    )
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<CertifyArgs> {
    let mut values = BTreeMap::<String, String>::new();
    while let Some(flag) = args.next() {
        if !flag.starts_with("--") {
            bail!("unexpected argument `{flag}`; {USAGE}");
        }
        let value = args
            .next()
            .with_context(|| format!("{flag} requires a value; {USAGE}"))?;
        if values.insert(flag.clone(), value).is_some() {
            bail!("duplicate argument `{flag}`; {USAGE}");
        }
    }
    if let Some(unknown) = values
        .keys()
        .find(|flag| !matches!(flag.as_str(), "--artifact" | "--mandate-id"))
    {
        bail!("unknown argument `{unknown}`; {USAGE}");
    }
    let required = |flag: &str| -> Result<&str> {
        values
            .get(flag)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
            .with_context(|| format!("missing required argument {flag}; {USAGE}"))
    };
    let mandate_uid =
        Uuid::parse_str(required("--mandate-id")?).context("--mandate-id must be a UUID")?;
    if mandate_uid != PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID {
        bail!("--mandate-id must name the fixed platform release certification mandate");
    }
    Ok(CertifyArgs {
        artifact: PathBuf::from(required("--artifact")?),
        mandate_uid,
    })
}

fn parse_canonical_artifact(source: &[u8]) -> Result<FidelityStudyArtifact> {
    let artifact: FidelityStudyArtifact =
        parse_canonical_json(source).context("deserialize canonical FidelityStudyArtifact")?;
    artifact
        .validate()
        .map_err(anyhow::Error::from)
        .context("validate FidelityStudyArtifact")?;
    Ok(artifact)
}

fn parse_canonical_json<T>(source: &[u8]) -> Result<T>
where
    T: DeserializeOwned + Serialize,
{
    let value = serde_json::from_slice(source).context("deserialize JSON artifact")?;
    let canonical = canonical_json_bytes(&value).context("canonicalize JSON artifact")?;
    require_canonical_source(source, &canonical)?;
    Ok(value)
}

fn require_canonical_source(source: &[u8], canonical: &[u8]) -> Result<()> {
    if source != canonical {
        bail!("fidelity artifact must use its exact canonical JSON encoding");
    }
    Ok(())
}

fn validate_platform_identity(artifact: &FidelityStudyArtifact) -> Result<()> {
    validate_platform_identity_fields(
        artifact.policy_uid,
        artifact.policy_revision,
        artifact.domain.as_str(),
    )
}

fn validate_platform_identity_fields(
    policy_uid: Uuid,
    policy_revision: i32,
    domain: &str,
) -> Result<()> {
    if policy_uid != PLATFORM_RELEASE_SIMULATOR_POLICY_UID
        || policy_revision != PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION
        || domain != PLATFORM_RELEASE_DOMAIN
    {
        bail!("fidelity artifact does not target the fixed platform release simulator policy");
    }
    Ok(())
}

fn safe_report(
    artifact: &FidelityStudyArtifact,
    artifact_hash: Digest32,
    outcome: &CertificationOutcome,
) -> SafeCertificationReport {
    SafeCertificationReport {
        policy_uid: artifact.policy_uid,
        policy_revision: artifact.policy_revision,
        policy_hash: artifact.policy_hash,
        study_uid: artifact.study_uid,
        artifact_hash,
        verdict: outcome.verdict(),
        certified_until: outcome.window().map(|window| window.certified_until),
    }
}

fn write_report(report: &SafeCertificationReport) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, report).context("serialize safe certification report")?;
    stdout
        .write_all(b"\n")
        .context("write safe certification report")?;
    stdout.flush().context("flush safe certification report")
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[test]
    fn certify_platform_simulator_parses_only_exact_required_arguments() {
        // Pins: certification only submits one artifact against one independently
        // persisted mandate; the CLI cannot self-authorize with an id from the artifact.
        let mandate_uid = PLATFORM_RELEASE_SIMULATOR_CERTIFICATION_MANDATE_UID;
        let parsed = parse_args(
            [
                "--artifact",
                "study.json",
                "--mandate-id",
                &mandate_uid.to_string(),
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("complete certification command should parse");
        assert_eq!(
            parsed,
            CertifyArgs {
                artifact: PathBuf::from("study.json"),
                mandate_uid,
            }
        );

        for args in [
            vec!["--artifact", "study.json"],
            vec!["--artifact", "study.json", "--unknown", "value"],
            vec![
                "--artifact",
                "one.json",
                "--artifact",
                "two.json",
                "--mandate-id",
                "00000000-0000-0000-0000-000000000011",
            ],
            vec!["--artifact", "study.json", "--mandate-id", "not-a-uuid"],
            vec![
                "--artifact",
                "study.json",
                "--authorization-id",
                "artifact-owned-authorization",
            ],
        ] {
            assert!(
                parse_args(args.into_iter().map(str::to_string)).is_err(),
                "missing, unknown, and duplicate arguments must fail"
            );
        }
    }

    #[test]
    fn certify_platform_simulator_requires_canonical_source_bytes() {
        // Pins: formatting, extra fields, and duplicate fields cannot survive re-canonicalization.
        #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
        struct MinimalArtifact {
            a: u8,
        }

        assert_eq!(
            parse_canonical_json::<MinimalArtifact>(br#"{"a":1}"#)
                .expect("exact canonical JSON should deserialize"),
            MinimalArtifact { a: 1 }
        );
        for source in [
            br#"{ "a": 1 }"#.as_slice(),
            br#"{"a":1,"extra":2}"#.as_slice(),
            br#"{"a":1,"a":1}"#.as_slice(),
        ] {
            assert!(
                parse_canonical_json::<MinimalArtifact>(source).is_err(),
                "noncanonical source must fail"
            );
        }
    }

    #[test]
    fn certify_platform_simulator_requires_exact_platform_identity() {
        // Pins: no artifact can select another policy, revision, or domain. The
        // independent mandate owns authorization validation in the store.
        assert!(
            validate_platform_identity_fields(
                PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
                PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
                PLATFORM_RELEASE_DOMAIN,
            )
            .is_ok()
        );
        for (policy_uid, revision, domain) in [
            (
                Uuid::nil(),
                PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
                PLATFORM_RELEASE_DOMAIN,
            ),
            (
                PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
                2,
                PLATFORM_RELEASE_DOMAIN,
            ),
            (
                PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
                PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
                "other-domain",
            ),
        ] {
            assert!(
                validate_platform_identity_fields(policy_uid, revision, domain).is_err(),
                "every platform identity mismatch must fail"
            );
        }
    }

    #[test]
    fn certify_platform_simulator_report_contains_only_safe_fields() {
        // Pins: operator output cannot leak cohort, label, authorization, or measurement details.
        let expiry = Utc
            .timestamp_opt(2_000_000, 0)
            .single()
            .expect("fixed expiry should be valid");
        let report = SafeCertificationReport {
            policy_uid: PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
            policy_revision: PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
            policy_hash: Digest32([0x11; 32]),
            study_uid: Uuid::from_u128(7),
            artifact_hash: Digest32([0x22; 32]),
            verdict: "certified",
            certified_until: Some(expiry),
        };
        assert_eq!(
            serde_json::to_value(report).expect("safe report should serialize"),
            json!({
                "policy_uid": PLATFORM_RELEASE_SIMULATOR_POLICY_UID,
                "policy_revision": PLATFORM_RELEASE_SIMULATOR_POLICY_REVISION,
                "policy_hash": "1111111111111111111111111111111111111111111111111111111111111111",
                "study_uid": Uuid::from_u128(7),
                "artifact_hash": "2222222222222222222222222222222222222222222222222222222222222222",
                "verdict": "certified",
                "certified_until": expiry,
            })
        );
    }
}
