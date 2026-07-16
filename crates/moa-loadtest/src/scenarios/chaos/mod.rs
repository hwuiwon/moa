//! Chaos experiment driver: steady load, one injected fault, a healed
//! recovery window, and a report whose window series shows all three phases.
//!
//! Every experiment is hypothesis-driven: establish steady state under open-
//! loop load, inject exactly one fault (container-level via docker compose,
//! or provider-level via a fault-scripted provider), heal it, and then let
//! the recovery window prove the system drained its backlog. Durability
//! invariants are asserted by the caller (see `moa_test_support::invariants`)
//! against the tenants recorded in the returned report.

mod experiments;
mod toxiproxy;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::*;

pub use experiments::*;
pub use toxiproxy::Toxiproxy;

/// How long to wait for the orchestrator to report ready after a recreate.
const ORCHESTRATOR_READY_TIMEOUT: Duration = Duration::from_secs(180);
/// Health endpoint published by compose for the orchestrator.
const ORCHESTRATOR_HEALTH_URL: &str = "http://localhost:10021/_health/ready";

/// Stack-level configuration for chaos runs.
#[derive(Debug, Clone)]
pub struct ChaosStackConfig {
    /// Directory containing docker-compose.yml.
    pub project_dir: PathBuf,
    /// Restate ingress endpoint fronting the orchestrator.
    pub endpoint: String,
    /// Toxiproxy control API (chaos overlay), for network faults.
    pub toxiproxy_url: String,
}

impl Default for ChaosStackConfig {
    fn default() -> Self {
        Self {
            project_dir: PathBuf::from("."),
            endpoint: "http://localhost:10010".to_string(),
            toxiproxy_url: "http://localhost:10060".to_string(),
        }
    }
}

/// One injected fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// SIGKILL a compose service; heal restarts it.
    KillService(&'static str),
    /// Stop a compose service for the fault window; heal starts it.
    StopService(&'static str),
    /// Restart a compose service (fault and heal in one step).
    RestartService(&'static str),
    /// No container fault; the fault lives in the provider script.
    ProviderScript,
    /// Downstream latency on a toxiproxy route (chaos overlay required).
    ToxiLatency {
        /// Proxy name from toxiproxy.json (`postgres`, `openfga`).
        proxy: &'static str,
        /// Added latency per read.
        latency: Duration,
        /// Latency jitter.
        jitter: Duration,
    },
    /// Full partition of a toxiproxy route (listener disabled).
    ToxiPartition {
        /// Proxy name from toxiproxy.json.
        proxy: &'static str,
    },
    /// Several simultaneous faults, injected in order and healed in reverse
    /// order. Must not nest another `Multi`.
    Multi(Vec<Fault>),
}

/// One chaos experiment definition.
#[derive(Debug, Clone)]
pub struct ChaosExperiment {
    /// Stable experiment name used in reports and logs.
    pub name: &'static str,
    /// Provider script (container path) to recreate the orchestrator with,
    /// e.g. `/loadtest-scripts/chaos-provider-storm.json`.
    pub provider_script: Option<&'static str>,
    /// The fault to inject after the steady window.
    pub fault: Fault,
    /// Steady-state window before injection.
    pub steady: Duration,
    /// Fault window between inject and heal.
    pub fault_window: Duration,
    /// Recovery window after heal; the run ends when it closes.
    pub recovery: Duration,
    /// Offered turn rate for the whole run.
    pub rate: f64,
    /// Session pool size.
    pub sessions: usize,
}

/// Result of one experiment run.
#[derive(Debug)]
pub struct ExperimentOutcome {
    /// Experiment name.
    pub name: String,
    /// Full load report covering steady, fault, and recovery phases.
    pub report: LoadTestReport,
}

impl ExperimentOutcome {
    /// Asserts the system recovered: the last active post-warmup window has
    /// zero turn errors and completed at least one turn.
    pub fn assert_recovered(&self) -> Result<()> {
        let last_active = self
            .report
            .windows
            .iter()
            .rfind(|window| !window.warmup && window_is_active(window))
            .context("no active post-warmup window; the run produced no work")?;
        if last_active.turn_errors > 0 {
            bail!(
                "{}: final active window [{:.0}s-{:.0}s] still failing: {} errors, {} completed",
                self.name,
                last_active.start_ms / 1_000.0,
                last_active.end_ms / 1_000.0,
                last_active.turn_errors,
                last_active.turns_completed
            );
        }
        if !report_produced_work(&self.report) {
            bail!("{}: no successful operations at all", self.name);
        }
        Ok(())
    }

    /// Ratio of the worst window p95 to the final active window p95.
    /// Faults that the system absorbs through retries never fail a turn but
    /// leave a latency bulge; a ratio well above 1 proves the fault landed
    /// and that the tail recovered by the end of the run.
    pub fn degradation_ratio(&self) -> f64 {
        let active: Vec<&WindowReport> = self
            .report
            .windows
            .iter()
            .filter(|window| !window.warmup && window.turns_completed > 0)
            .collect();
        let Some(last) = active.last() else {
            return 0.0;
        };
        let peak = active
            .iter()
            .map(|window| window.latency_corrected_ms.p95)
            .fold(0.0, f64::max);
        if last.latency_corrected_ms.p95 <= 0.0 {
            return 0.0;
        }
        peak / last.latency_corrected_ms.p95
    }

    /// Worst corrected-latency p95 across windows overlapping `[from, to)`.
    /// Latency faults that never fail turns still show up here.
    pub fn phase_p95_ms(&self, from: Duration, to: Duration) -> f64 {
        let from_ms = from.as_secs_f64() * 1_000.0;
        let to_ms = to.as_secs_f64() * 1_000.0;
        self.report
            .windows
            .iter()
            .filter(|window| window.end_ms > from_ms && window.start_ms < to_ms)
            .filter(|window| window.turns_completed > 0)
            .map(|window| window.latency_corrected_ms.p95)
            .fold(0.0, f64::max)
    }

    /// True when any window overlapping the fault phase saw turn errors or a
    /// throughput hole; used to confirm the fault actually landed.
    pub fn fault_phase_disrupted(&self, steady: Duration, fault_window: Duration) -> bool {
        let start_ms = steady.as_secs_f64() * 1_000.0;
        let end_ms = (steady + fault_window).as_secs_f64() * 1_000.0 + 10_000.0;
        self.report
            .windows
            .iter()
            .filter(|window| window.end_ms > start_ms && window.start_ms < end_ms)
            .any(|window| window.turn_errors > 0 || !window_is_successful(window))
    }
}

fn window_is_active(window: &WindowReport) -> bool {
    window.turns_completed + window.execution_admissions + window.turn_errors > 0
}

fn window_is_successful(window: &WindowReport) -> bool {
    window.turns_completed + window.execution_admissions > 0
}

fn report_produced_work(report: &LoadTestReport) -> bool {
    successful_operations_produced_work(report.successful_operations)
}

fn successful_operations_produced_work(successful_operations: u64) -> bool {
    successful_operations > 0
}

/// Runs `docker compose <args>` in the project directory.
async fn compose(cfg: &ChaosStackConfig, args: &[&str]) -> Result<()> {
    let status = tokio::process::Command::new("docker")
        .arg("compose")
        .args(args)
        .current_dir(&cfg.project_dir)
        .status()
        .await
        .with_context(|| format!("spawning docker compose {args:?}"))?;
    if !status.success() {
        bail!("docker compose {args:?} exited with {status}");
    }
    Ok(())
}

/// Runs `docker compose` with an extra environment variable.
async fn compose_with_env(
    cfg: &ChaosStackConfig,
    key: &str,
    value: &str,
    args: &[&str],
) -> Result<()> {
    let status = tokio::process::Command::new("docker")
        .arg("compose")
        .args(args)
        .env(key, value)
        .current_dir(&cfg.project_dir)
        .status()
        .await
        .with_context(|| format!("spawning docker compose {args:?}"))?;
    if !status.success() {
        bail!("docker compose {args:?} exited with {status}");
    }
    Ok(())
}

/// Waits until the orchestrator health endpoint reports ready.
async fn wait_orchestrator_ready() -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("health client")?;
    let deadline = tokio::time::Instant::now() + ORCHESTRATOR_READY_TIMEOUT;
    loop {
        if let Ok(response) = client.get(ORCHESTRATOR_HEALTH_URL).send().await
            && response.status().is_success()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("orchestrator did not become ready within {ORCHESTRATOR_READY_TIMEOUT:?}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Waits until Postgres answers `pg_isready` inside its container. Compose
/// health state lags a restart by several seconds, so ask the server itself.
async fn wait_postgres_ready(cfg: &ChaosStackConfig) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    loop {
        let status = tokio::process::Command::new("docker")
            .args([
                "compose",
                "exec",
                "-T",
                "postgres",
                "pg_isready",
                "-U",
                "moa_owner",
                "-d",
                "moa",
            ])
            .current_dir(&cfg.project_dir)
            .status()
            .await;
        if matches!(status, Ok(status) if status.success()) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("postgres did not answer pg_isready within 120s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Toxic name used for driver-added latency toxics.
const CHAOS_TOXIC_NAME: &str = "chaos_latency";

impl Fault {
    /// Flattens `Multi` into its parts; every other fault is a single part.
    fn parts(&self) -> Vec<&Fault> {
        match self {
            Fault::Multi(parts) => parts.iter().collect(),
            other => vec![other],
        }
    }

    async fn inject(&self, cfg: &ChaosStackConfig) -> Result<()> {
        for part in self.parts() {
            inject_part(part, cfg).await?;
        }
        Ok(())
    }

    async fn heal(&self, cfg: &ChaosStackConfig) -> Result<()> {
        for part in self.parts().into_iter().rev() {
            heal_part(part, cfg).await?;
        }
        Ok(())
    }
}

async fn inject_part(fault: &Fault, cfg: &ChaosStackConfig) -> Result<()> {
    match fault {
        Fault::KillService(service) => compose(cfg, &["kill", "-s", "SIGKILL", service]).await,
        Fault::StopService(service) => compose(cfg, &["stop", service]).await,
        Fault::RestartService(service) => {
            compose(cfg, &["restart", service]).await?;
            if *service == "postgres" {
                wait_postgres_ready(cfg).await?;
            }
            Ok(())
        }
        Fault::ProviderScript => Ok(()),
        Fault::ToxiLatency {
            proxy,
            latency,
            jitter,
        } => {
            Toxiproxy::new(&cfg.toxiproxy_url)?
                .add_latency(proxy, CHAOS_TOXIC_NAME, *latency, *jitter)
                .await
        }
        Fault::ToxiPartition { proxy } => {
            Toxiproxy::new(&cfg.toxiproxy_url)?
                .set_enabled(proxy, false)
                .await
        }
        Fault::Multi(_) => bail!("nested Multi faults are not supported"),
    }
}

async fn heal_part(fault: &Fault, cfg: &ChaosStackConfig) -> Result<()> {
    match fault {
        Fault::KillService(service) | Fault::StopService(service) => {
            compose(cfg, &["start", service]).await?;
            if *service == "moa-orchestrator" {
                wait_orchestrator_ready().await?;
            }
            if *service == "postgres" {
                wait_postgres_ready(cfg).await?;
            }
            Ok(())
        }
        Fault::RestartService(_) | Fault::ProviderScript => Ok(()),
        Fault::ToxiLatency { proxy, .. } => {
            Toxiproxy::new(&cfg.toxiproxy_url)?
                .remove_toxic(proxy, CHAOS_TOXIC_NAME)
                .await
        }
        Fault::ToxiPartition { proxy } => {
            Toxiproxy::new(&cfg.toxiproxy_url)?
                .set_enabled(proxy, true)
                .await
        }
        Fault::Multi(_) => bail!("nested Multi faults are not supported"),
    }
}

/// Runs one experiment end to end and returns its outcome.
pub async fn run_experiment(
    experiment: &ChaosExperiment,
    cfg: &ChaosStackConfig,
) -> Result<ExperimentOutcome> {
    if let Some(script) = experiment.provider_script {
        tracing::info!(
            experiment = experiment.name,
            script,
            "recreating orchestrator"
        );
        compose_with_env(
            cfg,
            "MOA_PROVIDERS_OVERRIDE",
            &format!("scripted:{script}"),
            &[
                "up",
                "-d",
                "--force-recreate",
                "moa-orchestrator",
                "restate-register",
            ],
        )
        .await?;
        wait_orchestrator_ready().await?;
    }

    let duration = experiment.steady + experiment.fault_window + experiment.recovery;
    let options = LoadTestOptions {
        mode: LoadMode::Mock,
        endpoint: cfg.endpoint.clone(),
        edge_endpoint: None,
        sessions: experiment.sessions,
        tenants: 2,
        identities_per_tenant: 1,
        profile: SessionProfileKind::Mixed,
        think_time: Duration::from_millis(500),
        rate: experiment.rate,
        shape: LoadShape::Steady,
        rate_end: None,
        spike_factor: 10.0,
        arrival: ArrivalProcess::Poisson,
        duration,
        warmup: Some((experiment.steady / 2).min(Duration::from_secs(5))),
        turn_timeout: Duration::from_secs(60),
        output: OutputFormat::Json,
        model: None,
        metrics_endpoint: None,
        seed: 42,
    };

    let load = tokio::spawn(run_loadtest(options));

    tokio::time::sleep(experiment.steady).await;
    tracing::info!(experiment = experiment.name, fault = ?experiment.fault, "injecting fault");
    experiment.fault.inject(cfg).await?;
    tokio::time::sleep(experiment.fault_window).await;
    tracing::info!(experiment = experiment.name, "healing fault");
    experiment.fault.heal(cfg).await?;

    let report = load
        .await
        .context("load task panicked")?
        .context("load run failed outright")?;
    Ok(ExperimentOutcome {
        name: experiment.name.to_string(),
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(answers: u64, admissions: u64, errors: u64) -> WindowReport {
        let zero = PercentileSummary {
            min: 0.0,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
        WindowReport {
            start_ms: 0.0,
            end_ms: 10_000.0,
            warmup: false,
            turns_completed: answers,
            execution_admissions: admissions,
            turn_errors: errors,
            latency_corrected_ms: zero.clone(),
            execution_admission_latency_corrected_ms: zero,
        }
    }

    #[test]
    fn chaos_activity_gates_include_execution_admissions() {
        // Pins: activity/no-work gates share the successful-operation definition while answer
        // latency gates remain answer-only elsewhere in this module.
        let admission_only = window(0, 1, 0);
        let answer_only = window(1, 0, 0);
        let mixed = window(1, 1, 0);
        let error_only = window(0, 0, 1);
        let empty = window(0, 0, 0);

        assert!(window_is_active(&admission_only));
        assert!(window_is_successful(&admission_only));
        assert!(window_is_active(&answer_only));
        assert!(window_is_successful(&answer_only));
        assert!(window_is_active(&mixed));
        assert!(window_is_successful(&mixed));
        assert!(window_is_active(&error_only));
        assert!(!window_is_successful(&error_only));
        assert!(!window_is_active(&empty));
        assert!(!window_is_successful(&empty));
        assert!(!successful_operations_produced_work(0));
        assert!(successful_operations_produced_work(1));
    }
}
