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

/// Offered-rate shape over the run window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LoadShape {
    /// Constant rate; SLO validation.
    Steady,
    /// Linear ramp from `rate` to `rate_end`; finds the knee.
    Ramp,
    /// Constant rate with a `spike_factor` burst over the middle tenth of the
    /// window; exercises autoscaling and queue drain.
    Spike,
    /// Alias of steady intended for hours-long leak/compaction runs.
    Soak,
    /// Linear ramp past the expected knee (defaults to 4x when `rate_end` is
    /// unset); validates graceful degradation.
    Stress,
}

/// Rate parameters resolved from options.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RatePlan {
    pub(crate) shape: LoadShape,
    pub(crate) rate: f64,
    pub(crate) rate_end: Option<f64>,
    pub(crate) spike_factor: f64,
}

impl RatePlan {
    /// Offered rate at offset `t` seconds into a window of `total` seconds.
    fn rate_at(&self, t: f64, total: f64) -> f64 {
        match self.shape {
            LoadShape::Steady | LoadShape::Soak => self.rate,
            LoadShape::Ramp => {
                let end = self.rate_end.unwrap_or(self.rate);
                self.rate + (end - self.rate) * (t / total).clamp(0.0, 1.0)
            }
            LoadShape::Stress => {
                let end = self.rate_end.unwrap_or(self.rate * 4.0);
                self.rate + (end - self.rate) * (t / total).clamp(0.0, 1.0)
            }
            LoadShape::Spike => {
                if (0.45..0.55).contains(&(t / total)) {
                    self.rate * self.spike_factor
                } else {
                    self.rate
                }
            }
        }
    }

    /// Peak rate over the window, used for the schedule-size bound.
    fn peak_rate(&self) -> f64 {
        match self.shape {
            LoadShape::Steady | LoadShape::Soak => self.rate,
            LoadShape::Ramp => self.rate.max(self.rate_end.unwrap_or(self.rate)),
            LoadShape::Stress => self.rate.max(self.rate_end.unwrap_or(self.rate * 4.0)),
            LoadShape::Spike => self.rate * self.spike_factor,
        }
    }
}

/// Builds intended turn-start offsets from run start.
///
/// The gap before each arrival is sampled at the instantaneous offered rate,
/// so shaped schedules (ramp/spike/stress) stay open-loop: rate changes come
/// from the plan, never from target backpressure.
pub(crate) fn build_arrival_offsets(
    plan: RatePlan,
    duration: Duration,
    process: ArrivalProcess,
    seed: u64,
) -> Result<Vec<Duration>> {
    if plan.rate <= 0.0 || !plan.rate.is_finite() {
        return Err(MoaError::ValidationError(format!(
            "arrival rate must be a positive finite number; got {}",
            plan.rate
        )));
    }
    let total_secs = duration.as_secs_f64();
    let expected = (plan.peak_rate() * total_secs).ceil() as usize;
    if expected > MAX_SCHEDULED_TURNS {
        return Err(MoaError::ValidationError(format!(
            "schedule of up to {expected} turns exceeds the {MAX_SCHEDULED_TURNS} cap; \
             lower --rate or --duration, or shard across workers"
        )));
    }

    let mut offsets = Vec::with_capacity((plan.rate * total_secs).ceil() as usize);
    let mut rng = StdRng::seed_from_u64(seed);
    let mut elapsed = 0.0_f64;
    loop {
        let rate = plan.rate_at(elapsed, total_secs).max(f64::MIN_POSITIVE);
        let gap = match process {
            ArrivalProcess::Constant => 1.0 / rate,
            ArrivalProcess::Poisson => {
                // Inverse-CDF sample of Exp(rate); guard the open interval so
                // ln(0) can never produce an infinite gap.
                let uniform: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
                -uniform.ln() / rate
            }
        };
        elapsed += gap;
        if elapsed >= total_secs {
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

    fn steady(rate: f64) -> RatePlan {
        RatePlan {
            shape: LoadShape::Steady,
            rate,
            rate_end: None,
            spike_factor: 10.0,
        }
    }

    #[test]
    fn constant_schedule_matches_rate_and_duration() {
        // Pins: a constant 100 qps schedule over 2s yields ~199 offsets
        // (first arrival at 1/rate, none at or past the duration bound).
        let offsets = build_arrival_offsets(
            steady(100.0),
            Duration::from_secs(2),
            ArrivalProcess::Constant,
            42,
        )
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
        let first = build_arrival_offsets(
            steady(200.0),
            Duration::from_secs(5),
            ArrivalProcess::Poisson,
            7,
        )
        .expect("schedule");
        let second = build_arrival_offsets(
            steady(200.0),
            Duration::from_secs(5),
            ArrivalProcess::Poisson,
            7,
        )
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
        // attempting a multi-gigabyte allocation; the bound uses the PEAK
        // rate so spikes cannot smuggle in an oversized burst.
        let error = build_arrival_offsets(
            steady(1_000_000.0),
            Duration::from_secs(60),
            ArrivalProcess::Constant,
            1,
        )
        .expect_err("cap should reject");

        assert!(error.to_string().contains("cap"));
    }

    #[test]
    fn ramp_schedule_is_denser_in_the_second_half() {
        // Pins: a ramp from 10/s to 100/s puts most arrivals late in the
        // window, so knee-finding sweeps actually increase pressure.
        let offsets = build_arrival_offsets(
            RatePlan {
                shape: LoadShape::Ramp,
                rate: 10.0,
                rate_end: Some(100.0),
                spike_factor: 10.0,
            },
            Duration::from_secs(10),
            ArrivalProcess::Constant,
            42,
        )
        .expect("schedule");

        let midpoint = Duration::from_secs(5);
        let first_half = offsets.iter().filter(|offset| **offset < midpoint).count();
        let second_half = offsets.len() - first_half;
        assert!(
            second_half > first_half * 2,
            "expected late density, got {first_half} early vs {second_half} late"
        );
    }

    #[test]
    fn spike_schedule_bursts_only_in_the_middle_tenth() {
        // Pins: the spike window is the middle tenth at spike_factor times the
        // base rate; the shoulders stay at the base rate.
        let offsets = build_arrival_offsets(
            RatePlan {
                shape: LoadShape::Spike,
                rate: 10.0,
                rate_end: None,
                spike_factor: 10.0,
            },
            Duration::from_secs(10),
            ArrivalProcess::Constant,
            42,
        )
        .expect("schedule");

        let in_spike = offsets
            .iter()
            .filter(|offset| {
                **offset >= Duration::from_millis(4_500) && **offset < Duration::from_millis(5_500)
            })
            .count();
        // Spike second contributes ~100 arrivals vs ~10/s shoulders.
        assert!(
            in_spike > 80,
            "expected a ~100-arrival burst in the middle tenth, got {in_spike}"
        );
        assert!(
            offsets.len() < 220,
            "total {} suggests burst leaked",
            offsets.len()
        );
    }
}
