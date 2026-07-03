//! Open-loop arrival schedules for turn starts.
//!
//! The dispatcher walks a pre-computed list of intended start offsets and
//! never lets target slowness delay the schedule itself (wrk2 model): if the
//! system falls behind, the gap shows up as dispatch delay and corrected
//! latency instead of silently lowering the offered rate.

use std::time::Duration;

use rand::{Rng, SeedableRng, rngs::StdRng};

use crate::*;

/// Hard cap on schedule length to bound generator memory.
const MAX_SCHEDULED_TURNS: usize = 5_000_000;

/// Inter-arrival process for the turn schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ArrivalProcess {
    /// Fixed inter-arrival gaps (constant throughput).
    Constant,
    /// Exponential inter-arrival gaps (Poisson arrivals).
    Poisson,
}

/// Builds intended turn-start offsets from run start.
pub(crate) fn build_arrival_offsets(
    rate_qps: f64,
    duration: Duration,
    process: ArrivalProcess,
    seed: u64,
) -> Result<Vec<Duration>> {
    if rate_qps <= 0.0 || !rate_qps.is_finite() {
        return Err(MoaError::ValidationError(format!(
            "arrival rate must be a positive finite number; got {rate_qps}"
        )));
    }
    let expected = (rate_qps * duration.as_secs_f64()).ceil() as usize;
    if expected > MAX_SCHEDULED_TURNS {
        return Err(MoaError::ValidationError(format!(
            "schedule of {expected} turns exceeds the {MAX_SCHEDULED_TURNS} cap; \
             lower --rate or --duration, or shard across workers"
        )));
    }

    let mut offsets = Vec::with_capacity(expected);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut elapsed = 0.0_f64;
    loop {
        let gap = match process {
            ArrivalProcess::Constant => 1.0 / rate_qps,
            ArrivalProcess::Poisson => {
                // Inverse-CDF sample of Exp(rate); guard the open interval so
                // ln(0) can never produce an infinite gap.
                let uniform: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
                -uniform.ln() / rate_qps
            }
        };
        elapsed += gap;
        if elapsed >= duration.as_secs_f64() {
            break;
        }
        offsets.push(Duration::from_secs_f64(elapsed));
        if offsets.len() >= MAX_SCHEDULED_TURNS {
            break;
        }
    }
    Ok(offsets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_schedule_matches_rate_and_duration() {
        // Pins: a constant 100 qps schedule over 2s yields ~199 offsets
        // (first arrival at 1/rate, none at or past the duration bound).
        let offsets =
            build_arrival_offsets(100.0, Duration::from_secs(2), ArrivalProcess::Constant, 42)
                .expect("schedule");

        assert_eq!(offsets.len(), 199);
        assert!(offsets.first().expect("first") >= &Duration::from_millis(9));
        assert!(offsets.last().expect("last") < &Duration::from_secs(2));
        assert!(offsets.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn poisson_schedule_is_seed_deterministic_with_expected_mass() {
        // Pins: Poisson arrivals are reproducible per seed and land within a
        // loose tolerance of rate*duration so runs are comparable.
        let first =
            build_arrival_offsets(200.0, Duration::from_secs(5), ArrivalProcess::Poisson, 7)
                .expect("schedule");
        let second =
            build_arrival_offsets(200.0, Duration::from_secs(5), ArrivalProcess::Poisson, 7)
                .expect("schedule");

        assert_eq!(first, second);
        let expected = 200.0 * 5.0;
        assert!(
            (first.len() as f64) > expected * 0.8 && (first.len() as f64) < expected * 1.2,
            "got {} arrivals for expectation {expected}",
            first.len()
        );
    }

    #[test]
    fn oversized_schedule_is_rejected_not_allocated() {
        // Pins: a schedule beyond the memory cap fails validation instead of
        // attempting a multi-gigabyte allocation.
        let error = build_arrival_offsets(
            1_000_000.0,
            Duration::from_secs(60),
            ArrivalProcess::Constant,
            1,
        )
        .expect_err("cap should reject");

        assert!(error.to_string().contains("cap"));
    }
}
