//! Restate deployment-drain observation owned by the maintenance role.
//!
//! Bounded activations exist so that a handler deployment stops accepting new
//! work the moment a newer revision registers, then retires once the
//! invocations already pinned to it finish. Nothing else in the runtime
//! observes whether that retirement actually completes: a wedged old revision
//! holding pinned invocations keeps serving forever and looks identical to a
//! healthy fleet. This lane is the only observer of that state.
//!
//! It reads Restate's admin introspection tables, never Kubernetes. Everything
//! it exports is a fleet-level aggregate with no `deployment_id` label, because
//! deployment identity is unbounded over the lifetime of a cluster.

use std::time::Duration;

use anyhow::{Context as AnyhowContext, Result};
use reqwest::Client;
use serde::Deserialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Restate's default admin port, used when only the ingress URL is configured.
const RESTATE_ADMIN_PORT: u16 = 9070;
/// Environment variable naming the Restate admin API, when it is not derivable.
const RESTATE_ADMIN_URL_ENV: &str = "MOA_RESTATE_ADMIN_URL";
/// Per-request ceiling for one admin introspection call.
const ADMIN_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// Steady cadence between drain observations.
///
/// Deployment registration is operator-paced, so this is deliberately far
/// slower than the correctness lanes. The drain alert compares against an hour;
/// five minutes resolves it with room to spare while keeping this lane's cost
/// on the Restate query engine negligible.
const OBSERVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Upper bound on the exponential backoff after failed observations.
const OBSERVE_MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

/// One row of per-deployment drain state from Restate's introspection tables.
///
/// `age_seconds` is the age of the deployment registration itself. Supersession
/// age — the number that answers "how long has this been draining" — is derived
/// from it in [`aggregate_drain_state`], because a deployment only begins
/// draining when a newer one registers.
///
/// `blocking_invocations` counts non-terminal invocations that still name this
/// deployment, using the same predicate
/// `crate::runtime::bootstrap::active_invocations_query` and
/// `scripts/cutover-long-horizon-execution.sh` use to refuse deregistration.
/// The gauge therefore reads as work remaining before the revision may retire.
const DRAIN_QUERY: &str = "SELECT d.id AS deployment_id, \
     date_part('epoch', now() - d.created_at) AS age_seconds, \
     COUNT(i.id) AS blocking_invocations \
     FROM sys_deployment d \
     LEFT JOIN sys_invocation i \
     ON (i.pinned_deployment_id = d.id OR i.last_attempt_deployment_id = d.id) \
     AND i.status NOT IN ('completed', 'killed') \
     GROUP BY d.id, d.created_at";

#[derive(Debug, Deserialize)]
struct DrainQueryResponse {
    rows: Vec<DeploymentDrainRow>,
}

#[derive(Debug, Deserialize)]
struct DeploymentDrainRow {
    deployment_id: String,
    age_seconds: f64,
    blocking_invocations: i64,
}

/// Fleet-level drain aggregate exported as unlabeled gauges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainObservation {
    /// Superseded deployments that still hold non-terminal invocations.
    pub deployments: u64,
    /// Non-terminal invocations blocking retirement of those deployments.
    pub blocking_invocations: u64,
    /// Time since the longest-draining of those deployments was superseded.
    pub oldest_drain_age: Duration,
    /// Identities of the draining deployments, for logs only — never a label.
    pub draining_deployment_ids: Vec<String>,
}

impl DrainObservation {
    /// The healthy fleet: one current revision and nothing left to retire.
    fn drained() -> Self {
        Self {
            deployments: 0,
            blocking_invocations: 0,
            oldest_drain_age: Duration::ZERO,
            draining_deployment_ids: Vec::new(),
        }
    }
}

/// Resolves the Restate admin API from the environment or the ingress URL.
///
/// The maintenance pod is given an ingress URL, not an admin URL, so the
/// derived form is the one that works unconfigured: the admin API is the same
/// host on Restate's admin port. `MOA_RESTATE_ADMIN_URL` overrides it for
/// deployments whose two ports are not colocated, such as a local stack that
/// remaps both.
pub fn resolve_admin_url(ingress_url: &str) -> Result<String> {
    if let Ok(configured) = std::env::var(RESTATE_ADMIN_URL_ENV) {
        let configured = configured.trim().trim_end_matches('/');
        if !configured.is_empty() {
            return Ok(configured.to_string());
        }
    }
    derive_admin_url_from_ingress(ingress_url)
}

/// Rewrites an ingress URL onto Restate's admin port.
fn derive_admin_url_from_ingress(ingress_url: &str) -> Result<String> {
    let mut derived = reqwest::Url::parse(ingress_url)
        .with_context(|| format!("parse Restate ingress URL `{ingress_url}`"))?;
    derived
        .set_port(Some(RESTATE_ADMIN_PORT))
        .map_err(|()| anyhow::anyhow!("Restate ingress URL `{ingress_url}` accepts no port"))?;
    Ok(derived.as_str().trim_end_matches('/').to_string())
}

/// Starts the maintenance-owned Restate drain observer.
///
/// The returned task never fails. An unreachable or erroring admin API is
/// logged and retried with bounded backoff, because the maintenance role runs
/// as a single non-redundant replica under a `Recreate` strategy and must not
/// be taken down by a transient dependency it only observes. The task returns
/// only when `shutdown` is cancelled.
pub fn spawn_restate_drain_observer(
    admin_url: String,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = match Client::builder().timeout(ADMIN_HTTP_TIMEOUT).build() {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "Restate drain observer could not build its HTTP client; drain telemetry is disabled"
                );
                return;
            }
        };
        tracing::info!(
            admin_url = %admin_url,
            interval_secs = OBSERVE_INTERVAL.as_secs(),
            "Restate deployment drain observer started"
        );

        let mut consecutive_failures = 0_u32;
        loop {
            match observe_drain_state(&client, &admin_url).await {
                Ok(observation) => {
                    consecutive_failures = 0;
                    export(&observation);
                }
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    // The gauges keep their last observed values rather than
                    // reporting a fabricated zero: an unreachable admin API is
                    // not evidence that nothing is draining.
                    tracing::warn!(
                        %error,
                        consecutive_failures,
                        retry_delay_secs = observe_delay(consecutive_failures).as_secs(),
                        "Restate deployment drain observation failed; retrying with bounded backoff"
                    );
                }
            }

            let delay = observe_delay(consecutive_failures);
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    })
}

/// Writes the aggregate to the drain gauges, including the healthy zero.
///
/// The zero must be written on every successful pass. The drain alert is
/// `absent()`-guarded, so a fleet with nothing draining that never reports
/// would page exactly like a fleet that stopped reporting.
fn export(observation: &DrainObservation) {
    moa_observability::runtime_metrics::record_restate_draining_deployments(
        observation.deployments,
        observation.blocking_invocations,
        observation.oldest_drain_age,
    );
    if observation.deployments > 0 {
        tracing::info!(
            draining_deployments = observation.deployments,
            blocking_invocations = observation.blocking_invocations,
            oldest_drain_age_secs = observation.oldest_drain_age.as_secs(),
            deployment_ids = %observation.draining_deployment_ids.join(","),
            "Restate deployment revisions are still draining"
        );
    }
}

/// Runs one admin introspection pass and aggregates the result.
async fn observe_drain_state(client: &Client, admin_url: &str) -> Result<DrainObservation> {
    let response = client
        .post(format!("{admin_url}/query"))
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .json(&serde_json::json!({ "query": DRAIN_QUERY }))
        .send()
        .await
        .context("query Restate deployment drain state")?
        .error_for_status()
        .context("Restate deployment drain query failed")?;
    let payload = response
        .json::<DrainQueryResponse>()
        .await
        .context("decode Restate deployment drain query")?;
    Ok(aggregate_drain_state(payload.rows))
}

/// Reduces per-deployment rows to the unlabeled fleet aggregate.
///
/// A deployment is draining when a newer deployment exists — Restate routes new
/// invocations to the newest revision registering a service — and non-terminal
/// invocations still name it. Its drain age is measured from the moment it was
/// superseded, which is the registration age of the next-newer deployment, not
/// from its own registration. Measuring from its own registration would report
/// the deployment's entire lifetime as drain time and put every rollout
/// instantly over the alert threshold.
fn aggregate_drain_state(mut rows: Vec<DeploymentDrainRow>) -> DrainObservation {
    if rows.len() < 2 {
        // A single registered deployment is the current one, and no deployment
        // at all is a fleet mid-bootstrap. Neither can be draining.
        return DrainObservation::drained();
    }
    // Oldest first. The last row is the current revision, which by definition
    // still accepts new work and is therefore never counted as draining.
    rows.sort_by(|left, right| {
        right
            .age_seconds
            .total_cmp(&left.age_seconds)
            .then_with(|| left.deployment_id.cmp(&right.deployment_id))
    });

    let mut observation = DrainObservation::drained();
    for index in 0..rows.len() - 1 {
        let deployment = &rows[index];
        if deployment.blocking_invocations <= 0 {
            // Superseded and fully drained. It may still be registered awaiting
            // deregistration, but it is holding nothing.
            continue;
        }
        // The next-newer deployment's own age is the elapsed time since it
        // registered, which is exactly when this one stopped taking new work.
        let drain_age = Duration::try_from_secs_f64(rows[index + 1].age_seconds.max(0.0))
            .unwrap_or(Duration::ZERO);
        observation.deployments = observation.deployments.saturating_add(1);
        observation.blocking_invocations = observation
            .blocking_invocations
            .saturating_add(deployment.blocking_invocations.unsigned_abs());
        observation.oldest_drain_age = observation.oldest_drain_age.max(drain_age);
        observation
            .draining_deployment_ids
            .push(deployment.deployment_id.clone());
    }
    observation
}

/// Exponential backoff from the steady cadence, capped at [`OBSERVE_MAX_BACKOFF`].
fn observe_delay(consecutive_failures: u32) -> Duration {
    let multiplier = 1_u32
        .checked_shl(consecutive_failures.min(31))
        .unwrap_or(u32::MAX);
    OBSERVE_INTERVAL
        .saturating_mul(multiplier)
        .min(OBSERVE_MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, age_seconds: f64, blocking_invocations: i64) -> DeploymentDrainRow {
        DeploymentDrainRow {
            deployment_id: id.to_string(),
            age_seconds,
            blocking_invocations,
        }
    }

    // Pins: drain age is measured from supersession, not from registration. A
    // long-lived deployment superseded one minute ago has been draining for one
    // minute; reporting its registration age would put every rollout instantly
    // past the one-hour drain alert.
    #[test]
    fn drain_age_measures_time_since_supersession_offline() {
        let observation = aggregate_drain_state(vec![
            row("dp_old", 30.0 * 24.0 * 60.0 * 60.0, 4),
            row("dp_current", 60.0, 0),
        ]);

        assert_eq!(observation.deployments, 1);
        assert_eq!(observation.blocking_invocations, 4);
        assert_eq!(observation.oldest_drain_age, Duration::from_secs(60));
        assert_eq!(observation.draining_deployment_ids, vec!["dp_old"]);
    }

    // Pins: the healthy fleet reports an explicit zero rather than no sample,
    // because the drain alert is absent()-guarded and silence pages.
    #[test]
    fn fully_drained_fleet_reports_explicit_zero_offline() {
        let observation = aggregate_drain_state(vec![
            row("dp_retired", 7_200.0, 0),
            row("dp_current", 300.0, 11),
        ]);

        assert_eq!(observation, DrainObservation::drained());
    }

    // Pins: the newest deployment is never draining however much work it holds,
    // and a fleet that has only ever registered one deployment reports zero.
    #[test]
    fn current_deployment_is_never_counted_as_draining_offline() {
        assert_eq!(
            aggregate_drain_state(vec![row("dp_current", 42.0, 99)]),
            DrainObservation::drained()
        );
        assert_eq!(
            aggregate_drain_state(Vec::new()),
            DrainObservation::drained()
        );
    }

    // Pins: with three live revisions, each draining deployment is aged from the
    // revision that superseded it, so the oldest drain age is the supersession
    // age of the earliest still-blocking revision, and counts sum across both.
    #[test]
    fn multiple_draining_revisions_aggregate_without_deployment_labels_offline() {
        let observation = aggregate_drain_state(vec![
            row("dp_current", 100.0, 3),
            row("dp_v1", 9_000.0, 2),
            row("dp_v2", 5_000.0, 5),
        ]);

        assert_eq!(observation.deployments, 2);
        assert_eq!(observation.blocking_invocations, 7);
        // dp_v1 was superseded when dp_v2 registered 5000s ago; dp_v2 was
        // superseded when dp_current registered 100s ago.
        assert_eq!(observation.oldest_drain_age, Duration::from_secs(5_000));
        assert_eq!(observation.draining_deployment_ids, vec!["dp_v1", "dp_v2"]);
    }

    // Pins: clock skew between Restate and this process cannot produce a
    // negative duration or panic the maintenance singleton.
    #[test]
    fn negative_registration_age_from_clock_skew_clamps_to_zero_offline() {
        let observation =
            aggregate_drain_state(vec![row("dp_old", 600.0, 1), row("dp_current", -5.0, 0)]);

        assert_eq!(observation.deployments, 1);
        assert_eq!(observation.oldest_drain_age, Duration::ZERO);
    }

    // Pins: the admin URL is derivable from the ingress URL the maintenance pod
    // already receives, so drain telemetry needs no new deployment setting.
    #[test]
    fn admin_url_derives_restate_admin_port_from_ingress_offline() {
        let derived =
            derive_admin_url_from_ingress("http://restate.moa-restate.svc.cluster.local:8080")
                .expect("derive admin URL");

        assert_eq!(derived, "http://restate.moa-restate.svc.cluster.local:9070");
    }

    // Pins: repeated admin failures back off toward the hour cap instead of
    // hammering an unhealthy Restate, and one success returns to the cadence.
    #[test]
    fn failed_observations_back_off_to_the_bounded_ceiling_offline() {
        assert_eq!(observe_delay(0), OBSERVE_INTERVAL);
        assert_eq!(observe_delay(1), OBSERVE_INTERVAL * 2);
        assert_eq!(observe_delay(31), OBSERVE_MAX_BACKOFF);
    }
}
