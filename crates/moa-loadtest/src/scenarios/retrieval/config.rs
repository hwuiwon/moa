//! Perf-gate validation and metrics recorder setup.

use super::*;

pub(super) fn validate_config(cfg: &PerfGateConfig) -> Result<()> {
    if cfg.workspaces < 2 {
        bail!("perf_gate requires at least 2 workspaces for concurrent RLS probes");
    }
    if cfg.facts_per_workspace == 0 {
        bail!("facts_per_workspace must be greater than zero");
    }
    if cfg.qps == 0 {
        bail!("qps must be greater than zero");
    }
    if !(0.0..=1.0).contains(&cfg.cache_hit_floor) {
        bail!("cache_hit_floor must be between 0 and 1");
    }
    Ok(())
}

pub(super) fn validate_hardware_floor() -> Result<()> {
    let cpus = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    if cpus < 8 {
        bail!("hardware floor unmet: expected at least 8 vCPU, found {cpus}");
    }

    validate_x86_avx2()?;
    if let Some(memory_gb) = linux_memory_gb()?
        && memory_gb < 32
    {
        bail!("hardware floor unmet: expected at least 32 GB memory, found {memory_gb} GB");
    }
    Ok(())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub(super) fn validate_x86_avx2() -> Result<()> {
    if !std::is_x86_feature_detected!("avx2") {
        bail!("hardware floor unmet: AVX2 is required");
    }
    Ok(())
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub(super) fn validate_x86_avx2() -> Result<()> {
    bail!("hardware floor unmet: x86_64 with AVX2 is required");
}

pub(super) fn linux_memory_gb() -> Result<Option<u64>> {
    let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") else {
        return Ok(None);
    };
    let Some(line) = meminfo.lines().find(|line| line.starts_with("MemTotal:")) else {
        return Ok(None);
    };
    let kb = line
        .split_whitespace()
        .nth(1)
        .context("MemTotal line missing value")?
        .parse::<u64>()
        .context("MemTotal value was not an integer")?;
    Ok(Some(kb / 1024 / 1024))
}

pub(super) fn install_metrics_recorder() -> Result<PrometheusHandle> {
    PrometheusBuilder::new()
        .set_buckets(HISTOGRAM_BUCKETS_SECONDS)
        .context("failed to configure perf histogram buckets")?
        .set_buckets_for_metric(
            Matcher::Full("perf_gate_cache_hit_rate".to_string()),
            &[0.50, 0.60, 0.70, 0.80, 0.90, 0.95, 1.0],
        )
        .context("failed to configure cache hit rate buckets")?
        .install_recorder()
        .context("failed to install Prometheus metrics recorder")
}
