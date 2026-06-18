//! Scripted-user fixtures for long-conversation eval scenarios.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Scripted-user JSONL scenario fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptedUserScript {
    /// Script schema version.
    pub version: u32,
    /// Stable scenario identifier.
    pub scenario: String,
    /// Ordered user turns to drive through the long-conversation runner.
    pub turns: Vec<ScriptedUserTurn>,
    /// Fragments that must appear in the final collected answer.
    pub expected_final_answer_fragments: Vec<String>,
    /// Optional memory-eval probe identifiers associated with this script.
    pub probe_ids: Vec<String>,
}

/// One scripted user turn and associated probe metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptedUserTurn {
    /// User utterance that starts the turn.
    pub user: ScriptedUserUtterance,
    /// Optional memory-eval probe identifiers associated with this turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub probe_ids: Vec<String>,
}

/// User text captured in a scripted-user turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptedUserUtterance {
    /// User text for this turn.
    pub text: String,
}

/// Errors returned while reading, parsing, or validating scripted-user fixtures.
#[derive(Debug, Error)]
pub enum ScriptedUserError {
    /// Filesystem I/O failed.
    #[error("scripted-user I/O failed for {path}: {source}")]
    Io {
        /// Path being read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// JSON parsing failed.
    #[error("scripted-user JSON error at {location}: {source}")]
    Json {
        /// Human-readable line or location.
        location: String,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// The metadata line was missing.
    #[error("scripted-user script is missing leading metadata line")]
    MissingMetadata,
    /// The script scenario was empty.
    #[error("scripted-user script must set a non-empty scenario")]
    EmptyScenario,
    /// The script has no user turns.
    #[error("scripted-user script {scenario} must contain at least one turn")]
    EmptyScript {
        /// Scenario id.
        scenario: String,
    },
    /// A turn had an empty user utterance.
    #[error("scripted-user script {scenario} turn {turn_index} has an empty user utterance")]
    EmptyUserUtterance {
        /// Scenario id.
        scenario: String,
        /// Zero-based turn index.
        turn_index: usize,
    },
    /// The script did not define final answer fragments.
    #[error("scripted-user script {scenario} must define expected final answer fragments")]
    MissingExpectedFinalAnswerFragments {
        /// Scenario id.
        scenario: String,
    },
    /// A final answer fragment was empty.
    #[error(
        "scripted-user script {scenario} expected final answer fragment {fragment_index} is empty"
    )]
    EmptyExpectedFinalAnswerFragment {
        /// Scenario id.
        scenario: String,
        /// Zero-based fragment index.
        fragment_index: usize,
    },
    /// A probe id was empty.
    #[error("scripted-user script {scenario} has an empty probe id at {location}")]
    EmptyProbeId {
        /// Scenario id.
        scenario: String,
        /// Human-readable metadata or turn location.
        location: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct MetadataLine {
    version: u32,
    scenario: String,
    #[serde(default)]
    expected_final_answer_fragments: Vec<String>,
    #[serde(default)]
    probe_ids: Vec<String>,
}

impl ScriptedUserScript {
    /// Reads a scripted-user JSONL script from disk.
    ///
    /// The first non-empty line is metadata with `version`, `scenario`,
    /// `expected_final_answer_fragments`, and optional `probe_ids`. Each
    /// following non-empty line is one [`ScriptedUserTurn`].
    pub async fn read_jsonl(path: &Path) -> Result<Self, ScriptedUserError> {
        let raw =
            tokio::fs::read_to_string(path)
                .await
                .map_err(|source| ScriptedUserError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
        let mut lines = raw
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty());
        let Some((metadata_line_index, metadata_line)) = lines.next() else {
            return Err(ScriptedUserError::MissingMetadata);
        };
        let metadata: MetadataLine =
            serde_json::from_str(metadata_line).map_err(|source| ScriptedUserError::Json {
                location: format!("line {}", metadata_line_index + 1),
                source,
            })?;
        let mut turns = Vec::new();
        for (line_index, line) in lines {
            let turn = serde_json::from_str(line).map_err(|source| ScriptedUserError::Json {
                location: format!("line {}", line_index + 1),
                source,
            })?;
            turns.push(turn);
        }

        let script = Self {
            version: metadata.version,
            scenario: metadata.scenario,
            turns,
            expected_final_answer_fragments: metadata.expected_final_answer_fragments,
            probe_ids: metadata.probe_ids,
        };
        script.validate()?;
        Ok(script)
    }

    /// Validates the script shape before it is used by the runner.
    pub fn validate(&self) -> Result<(), ScriptedUserError> {
        if self.scenario.trim().is_empty() {
            return Err(ScriptedUserError::EmptyScenario);
        }
        if self.turns.is_empty() {
            return Err(ScriptedUserError::EmptyScript {
                scenario: self.scenario.clone(),
            });
        }
        if self.expected_final_answer_fragments.is_empty() {
            return Err(ScriptedUserError::MissingExpectedFinalAnswerFragments {
                scenario: self.scenario.clone(),
            });
        }
        for (fragment_index, fragment) in self.expected_final_answer_fragments.iter().enumerate() {
            if fragment.trim().is_empty() {
                return Err(ScriptedUserError::EmptyExpectedFinalAnswerFragment {
                    scenario: self.scenario.clone(),
                    fragment_index,
                });
            }
        }
        validate_probe_ids(&self.scenario, "metadata", &self.probe_ids)?;
        for (turn_index, turn) in self.turns.iter().enumerate() {
            if turn.user.text.trim().is_empty() {
                return Err(ScriptedUserError::EmptyUserUtterance {
                    scenario: self.scenario.clone(),
                    turn_index,
                });
            }
            validate_probe_ids(
                &self.scenario,
                format!("turn {turn_index}").as_str(),
                &turn.probe_ids,
            )?;
        }
        Ok(())
    }
}

fn validate_probe_ids(
    scenario: &str,
    location: &str,
    probe_ids: &[String],
) -> Result<(), ScriptedUserError> {
    for probe_id in probe_ids {
        if probe_id.trim().is_empty() {
            return Err(ScriptedUserError::EmptyProbeId {
                scenario: scenario.to_string(),
                location: location.to_string(),
            });
        }
    }
    Ok(())
}
