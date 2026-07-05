//! Synthetic session plan construction for load tests.

use crate::*;

#[derive(Clone)]
pub(crate) struct InspectionFiles {
    pub(crate) summary_file: String,
    pub(crate) detail_file: String,
}

#[derive(Clone)]
pub(crate) struct SessionPlan {
    pub(crate) profile: SessionProfileKind,
    pub(crate) title: String,
    pub(crate) turns: Vec<TurnPlan>,
}

#[derive(Clone)]
pub(crate) struct TurnPlan {
    pub(crate) prompt: String,
}

pub(crate) fn inspectable_files() -> InspectionFiles {
    InspectionFiles {
        summary_file: "Cargo.toml".to_string(),
        detail_file: "docs/02-brain-orchestration.md".to_string(),
    }
}

/// Builds a plan whose turn count is sampled geometrically around the
/// profile's nominal length, so pool churn does not synchronize and session
/// lengths follow a realistic heavy-ish tail.
pub(crate) fn sampled_session_plan(
    index: usize,
    requested_profile: SessionProfileKind,
    inspection_files: &InspectionFiles,
    rng: &mut rand::rngs::StdRng,
) -> SessionPlan {
    use rand::Rng as _;

    let mut plan = session_plan(index, requested_profile, inspection_files);
    let mean = plan.turns.len().max(1) as f64;
    let success_probability = 1.0 / mean;
    let uniform: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
    // Geometric inverse-CDF; clamp to [1, 3*mean] to bound stragglers.
    let sampled = ((1.0 - uniform).ln() / (1.0 - success_probability).ln()).ceil() as usize;
    let target = sampled.clamp(1, (mean * 3.0) as usize);
    let base = plan.turns.clone();
    plan.turns = (0..target)
        .map(|turn| base[turn % base.len()].clone())
        .collect();
    plan
}

/// Builds the plan for the `index`-th session of a run. Mixed traffic keeps
/// one long tool-heavy session per four sessions.
pub(crate) fn session_plan(
    index: usize,
    requested_profile: SessionProfileKind,
    inspection_files: &InspectionFiles,
) -> SessionPlan {
    let profile = match requested_profile {
        SessionProfileKind::Short => SessionProfileKind::Short,
        SessionProfileKind::Long => SessionProfileKind::Long,
        SessionProfileKind::Mixed => {
            if index.is_multiple_of(4) {
                SessionProfileKind::Long
            } else {
                SessionProfileKind::Short
            }
        }
    };
    SessionPlan {
        profile,
        title: format!("loadtest-{profile:?}-{index:04}"),
        turns: match profile {
            SessionProfileKind::Short => short_profile_turns(inspection_files),
            SessionProfileKind::Long => long_profile_turns(inspection_files),
            SessionProfileKind::Mixed => unreachable!("mixed is resolved above"),
        },
    }
}

pub(crate) fn short_profile_turns(inspection_files: &InspectionFiles) -> Vec<TurnPlan> {
    vec![
        TurnPlan {
            prompt: "Give a concise one-sentence summary of this workspace.".to_string(),
        },
        TurnPlan {
            prompt: format!(
                "List the two most important facts you can infer from {}.",
                inspection_files.summary_file
            ),
        },
        TurnPlan {
            prompt: "What operational metric would you inspect first for session latency spikes?"
                .to_string(),
        },
        TurnPlan {
            prompt: format!(
                "Briefly explain what {} is likely used for.",
                inspection_files.detail_file
            ),
        },
        TurnPlan {
            prompt: "End with a one-line readiness summary for a coding agent runtime.".to_string(),
        },
    ]
}

pub(crate) fn long_profile_turns(inspection_files: &InspectionFiles) -> Vec<TurnPlan> {
    let prompts = [
        format!(
            "Use tools if needed and summarize the role of {} using lines 1-30.",
            inspection_files.summary_file
        ),
        "Name one likely latency bottleneck in a multi-turn agent loop.".to_string(),
        format!(
            "Inspect {} lines 1-40 and report one implementation detail worth monitoring.",
            inspection_files.detail_file
        ),
        "What runtime signal would indicate cache warmth improving over time?".to_string(),
        format!(
            "Read {} lines 31-60 and state one concrete string you expect to find.",
            inspection_files.summary_file
        ),
        format!(
            "Inspect {} lines 41-80 and call out one detail that would affect monitoring.",
            inspection_files.detail_file
        ),
        "What metric would you correlate with TTFT in a staging load test?".to_string(),
        format!(
            "Read {} lines 61-90 and name one concrete token or key you expect.",
            inspection_files.summary_file
        ),
    ];

    (0..40)
        .map(|index| {
            let prompt = prompts[index % prompts.len()].clone();
            TurnPlan { prompt }
        })
        .collect()
}
