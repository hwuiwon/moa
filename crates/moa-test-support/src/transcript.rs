//! JSONL transcript fixtures for recorded provider-response tests.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use moa_core::{StopReason, TokenUsage, ToolCallContent};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A recorded scenario transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    /// Transcript schema version.
    pub version: u32,
    /// Stable scenario identifier.
    pub scenario: String,
    /// Recorded turns in replay order.
    pub turns: Vec<Turn>,
}

/// One user turn and the provider events expected in response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// User utterance that starts the turn.
    pub user: UserUtterance,
    /// Provider events expected for the turn.
    pub expected: Vec<ProviderEvent>,
}

/// User text captured in a transcript turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserUtterance {
    /// User text for this turn.
    pub text: String,
}

/// Provider-side events consumed by recorded scripted providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderEvent {
    /// One streamed text delta.
    TextDelta {
        /// Text chunk emitted by the provider.
        text: String,
    },
    /// One structured tool call emitted by the provider.
    ToolCall {
        /// Canonical provider tool-call content.
        call: ToolCallContent,
    },
    /// Normalized usage counters emitted by the provider.
    Usage {
        /// Token usage for the completed provider response.
        usage: TokenUsage,
    },
    /// Terminal provider event for the turn.
    Terminal {
        /// Stop reason for the completed provider response.
        stop_reason: StopReason,
    },
}

/// Errors returned while reading, writing, or validating transcripts.
#[derive(Debug, Error)]
pub enum TranscriptError {
    /// Filesystem I/O failed.
    #[error("transcript I/O failed for {path}: {source}")]
    Io {
        /// Path being read or written.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// JSON parsing or serialization failed.
    #[error("transcript JSON error at {location}: {source}")]
    Json {
        /// Human-readable line or location.
        location: String,
        /// Underlying JSON error.
        #[source]
        source: serde_json::Error,
    },
    /// The metadata line was missing.
    #[error("transcript is missing leading metadata line")]
    MissingMetadata,
    /// The transcript has no turns.
    #[error("transcript {scenario} must contain at least one turn")]
    EmptyTranscript {
        /// Scenario id.
        scenario: String,
    },
    /// A turn had an empty user utterance.
    #[error("transcript {scenario} turn {turn_index} has an empty user utterance")]
    EmptyUserUtterance {
        /// Scenario id.
        scenario: String,
        /// Zero-based turn index.
        turn_index: usize,
    },
    /// A turn did not end in a terminal event.
    #[error("transcript {scenario} turn {turn_index} must end with a terminal event")]
    MissingTerminalEvent {
        /// Scenario id.
        scenario: String,
        /// Zero-based turn index.
        turn_index: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MetadataLine {
    version: u32,
    scenario: String,
}

impl Transcript {
    /// Reads a JSONL transcript from disk.
    ///
    /// The first non-empty line must be metadata in the form
    /// `{"version":1,"scenario":"..."}`. Each following non-empty line is one
    /// [`Turn`]. The returned transcript is validated before it is returned.
    pub fn read_jsonl(path: &Path) -> Result<Self, TranscriptError> {
        let file = File::open(path).map_err(|source| TranscriptError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines().enumerate();
        let Some((metadata_line_index, metadata_line)) = lines.find_map(|(index, line)| {
            let line = match line {
                Ok(line) if line.trim().is_empty() => return None,
                other => other,
            };
            Some((index, line))
        }) else {
            return Err(TranscriptError::MissingMetadata);
        };
        let metadata_line = metadata_line.map_err(|source| TranscriptError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let metadata: MetadataLine =
            serde_json::from_str(&metadata_line).map_err(|source| TranscriptError::Json {
                location: format!("line {}", metadata_line_index + 1),
                source,
            })?;
        let mut turns = Vec::new();
        for (line_index, line) in lines {
            let line = line.map_err(|source| TranscriptError::Io {
                path: path.display().to_string(),
                source,
            })?;
            if line.trim().is_empty() {
                continue;
            }
            let turn = serde_json::from_str(&line).map_err(|source| TranscriptError::Json {
                location: format!("line {}", line_index + 1),
                source,
            })?;
            turns.push(turn);
        }

        let transcript = Self {
            version: metadata.version,
            scenario: metadata.scenario,
            turns,
        };
        transcript.validate()?;
        Ok(transcript)
    }

    /// Writes a JSONL transcript to disk using the canonical metadata-plus-turns format.
    pub fn write_jsonl(&self, path: &Path) -> Result<(), TranscriptError> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TranscriptError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let file = File::create(path).map_err(|source| TranscriptError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(
            &mut writer,
            &MetadataLine {
                version: self.version,
                scenario: self.scenario.clone(),
            },
        )
        .map_err(|source| TranscriptError::Json {
            location: "metadata".to_string(),
            source,
        })?;
        writer
            .write_all(b"\n")
            .map_err(|source| TranscriptError::Io {
                path: path.display().to_string(),
                source,
            })?;
        for turn in &self.turns {
            serde_json::to_writer(&mut writer, turn).map_err(|source| TranscriptError::Json {
                location: "turn".to_string(),
                source,
            })?;
            writer
                .write_all(b"\n")
                .map_err(|source| TranscriptError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
        }
        writer.flush().map_err(|source| TranscriptError::Io {
            path: path.display().to_string(),
            source,
        })
    }

    /// Validates transcript shape before replay.
    pub fn validate(&self) -> Result<(), TranscriptError> {
        if self.turns.is_empty() {
            return Err(TranscriptError::EmptyTranscript {
                scenario: self.scenario.clone(),
            });
        }
        for (turn_index, turn) in self.turns.iter().enumerate() {
            if turn.user.text.trim().is_empty() {
                return Err(TranscriptError::EmptyUserUtterance {
                    scenario: self.scenario.clone(),
                    turn_index,
                });
            }
            if !matches!(turn.expected.last(), Some(ProviderEvent::Terminal { .. })) {
                return Err(TranscriptError::MissingTerminalEvent {
                    scenario: self.scenario.clone(),
                    turn_index,
                });
            }
        }
        Ok(())
    }
}
