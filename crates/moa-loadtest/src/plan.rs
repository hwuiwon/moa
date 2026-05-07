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
    pub(crate) mock_behavior: MockTurnBehavior,
}

#[derive(Clone)]
pub(crate) enum MockTurnBehavior {
    Simple,
    FileRead {
        path: String,
        start_line: Option<usize>,
        end_line: Option<usize>,
    },
    #[cfg(test)]
    Bash {
        cmd: String,
    },
}

pub(crate) async fn inspectable_files(workspace_root: Option<&Path>) -> Result<InspectionFiles> {
    if let Some(root) = workspace_root {
        let summary_candidates = [
            "Cargo.toml",
            "README.md",
            "docs/00-direction.md",
            "docs/02-brain-orchestration.md",
        ];
        let detail_candidates = [
            "docs/02-brain-orchestration.md",
            "moa-core/src/runtime_metrics.rs",
            "Cargo.toml",
            "README.md",
        ];
        let summary_file = first_existing_relative_path(root, &summary_candidates)
            .await?
            .unwrap_or_else(|| "Cargo.toml".to_string());
        let detail_file = first_existing_relative_path(root, &detail_candidates)
            .await?
            .unwrap_or_else(|| summary_file.clone());
        return Ok(InspectionFiles {
            summary_file,
            detail_file,
        });
    }

    Ok(InspectionFiles {
        summary_file: "Cargo.toml".to_string(),
        detail_file: "docs/02-brain-orchestration.md".to_string(),
    })
}

pub(crate) async fn first_existing_relative_path(
    root: &Path,
    candidates: &[&str],
) -> Result<Option<String>> {
    for candidate in candidates {
        if tokio::fs::try_exists(root.join(candidate)).await? {
            return Ok(Some((*candidate).to_string()));
        }
    }
    Ok(None)
}

pub(crate) fn build_session_plans(
    sessions: usize,
    requested_profile: SessionProfileKind,
    inspection_files: &InspectionFiles,
) -> Vec<SessionPlan> {
    (0..sessions)
        .map(|index| {
            let profile = match requested_profile {
                SessionProfileKind::Short => SessionProfileKind::Short,
                SessionProfileKind::Long => SessionProfileKind::Long,
                SessionProfileKind::Mixed => {
                    if index % 4 == 0 {
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
        })
        .collect()
}

pub(crate) fn short_profile_turns(inspection_files: &InspectionFiles) -> Vec<TurnPlan> {
    vec![
        TurnPlan {
            prompt: "Give a concise one-sentence summary of this workspace.".to_string(),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: format!(
                "List the two most important facts you can infer from {}.",
                inspection_files.summary_file
            ),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: "What operational metric would you inspect first for session latency spikes?"
                .to_string(),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: format!(
                "Briefly explain what {} is likely used for.",
                inspection_files.detail_file
            ),
            mock_behavior: MockTurnBehavior::Simple,
        },
        TurnPlan {
            prompt: "End with a one-line readiness summary for a coding agent runtime.".to_string(),
            mock_behavior: MockTurnBehavior::Simple,
        },
    ]
}

pub(crate) fn long_profile_turns(inspection_files: &InspectionFiles) -> Vec<TurnPlan> {
    let prompts = [
        (
            format!(
                "Use tools if needed and summarize the role of {} using lines 1-30.",
                inspection_files.summary_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.summary_file.clone(),
                start_line: Some(1),
                end_line: Some(30),
            },
        ),
        (
            "Name one likely latency bottleneck in a multi-turn agent loop.".to_string(),
            MockTurnBehavior::Simple,
        ),
        (
            format!(
                "Inspect {} lines 1-40 and report one implementation detail worth monitoring.",
                inspection_files.detail_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.detail_file.clone(),
                start_line: Some(1),
                end_line: Some(40),
            },
        ),
        (
            "What runtime signal would indicate cache warmth improving over time?".to_string(),
            MockTurnBehavior::Simple,
        ),
        (
            format!(
                "Read {} lines 31-60 and state one concrete string you expect to find.",
                inspection_files.summary_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.summary_file.clone(),
                start_line: Some(31),
                end_line: Some(60),
            },
        ),
        (
            format!(
                "Inspect {} lines 41-80 and call out one detail that would affect monitoring.",
                inspection_files.detail_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.detail_file.clone(),
                start_line: Some(41),
                end_line: Some(80),
            },
        ),
        (
            "What metric would you correlate with TTFT in a staging load test?".to_string(),
            MockTurnBehavior::Simple,
        ),
        (
            format!(
                "Read {} lines 61-90 and name one concrete token or key you expect.",
                inspection_files.summary_file
            ),
            MockTurnBehavior::FileRead {
                path: inspection_files.summary_file.clone(),
                start_line: Some(61),
                end_line: Some(90),
            },
        ),
    ];

    (0..40)
        .map(|index| {
            let (prompt, behavior) = prompts[index % prompts.len()].clone();
            TurnPlan {
                prompt,
                mock_behavior: behavior,
            }
        })
        .collect()
}
