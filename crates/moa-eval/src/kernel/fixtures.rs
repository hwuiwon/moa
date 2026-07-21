//! Generic versioned JSONL fixture store for hermetic eval lanes.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;

use moa_eval_core::{Error, Result};

/// Record contract required by [`FixtureStore`].
pub trait FixtureRecord {
    /// Returns the stable hex lookup key for this fixture record.
    fn fixture_key(&self) -> &str;

    /// Returns the fixture schema or prompt version for this record.
    fn fixture_version(&self) -> &str;
}

/// Hash-keyed JSONL fixture store with version checks and stable writes.
#[derive(Debug, Clone)]
pub struct FixtureStore<T> {
    records: BTreeMap<String, T>,
    expected_version: String,
    remediation_command: Option<String>,
}

impl<T> FixtureStore<T>
where
    T: FixtureRecord + Serialize + DeserializeOwned,
{
    /// Reads a version-checked fixture store from a JSONL file.
    pub fn read_jsonl(path: &Path, expected_version: &str) -> Result<Self> {
        Self::read_jsonl_any(path, &[expected_version])
    }

    /// Reads a fixture store accepting any of the listed compatible versions.
    ///
    /// Suites use this when a prompt bump only adds optional output keys, so
    /// fixtures recorded under an older prompt version stay replayable. The
    /// first listed version is treated as current for remediation messages.
    pub fn read_jsonl_any(path: &Path, expected_versions: &[&str]) -> Result<Self> {
        let expected_version = expected_versions.first().copied().unwrap_or_default();
        let body = fs::read_to_string(path).map_err(|source| io_error(path, source))?;
        let mut records = BTreeMap::new();
        for (line_index, line) in body.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let record: T = serde_json::from_str(line).map_err(|error| {
                Error::InvalidConfig(format!(
                    "failed to parse fixture {} line {}: {error}",
                    path.display(),
                    line_index + 1
                ))
            })?;
            if !expected_versions.contains(&record.fixture_version()) {
                return Err(Error::InvalidConfig(format!(
                    "fixture {} key {} has version {}; expected one of {}",
                    path.display(),
                    record.fixture_key(),
                    record.fixture_version(),
                    expected_versions.join(", ")
                )));
            }
            let key = normalize_key(record.fixture_key());
            if records.insert(key.clone(), record).is_some() {
                return Err(Error::InvalidConfig(format!(
                    "fixture {} contains duplicate key {key}",
                    path.display()
                )));
            }
        }
        Ok(Self {
            records,
            expected_version: expected_version.to_string(),
            remediation_command: None,
        })
    }

    /// Writes records as sorted JSONL for stable diffs.
    pub fn write_jsonl(path: &Path, records: impl IntoIterator<Item = T>) -> Result<()> {
        let mut by_key = BTreeMap::new();
        for record in records {
            let key = normalize_key(record.fixture_key());
            if by_key.insert(key.clone(), record).is_some() {
                return Err(Error::InvalidConfig(format!(
                    "cannot write duplicate fixture key {key}"
                )));
            }
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
        }
        let mut file = fs::File::create(path).map_err(|source| io_error(path, source))?;
        for record in by_key.into_values() {
            let line = serde_json::to_string(&record)?;
            file.write_all(line.as_bytes())
                .map_err(|source| io_error(path, source))?;
            file.write_all(b"\n")
                .map_err(|source| io_error(path, source))?;
        }
        Ok(())
    }

    /// Adds a remediation command to missing-key errors.
    #[must_use]
    pub fn with_remediation_command(mut self, command: impl Into<String>) -> Self {
        self.remediation_command = Some(command.into());
        self
    }

    /// Returns a fixture by key or hard-fails with a remediation hint.
    pub fn get(&self, key: &str) -> Result<&T> {
        let normalized = normalize_key(key);
        self.records.get(&normalized).ok_or_else(|| {
            let mut message = format!(
                "missing fixture key {normalized} for version {}",
                self.expected_version
            );
            if let Some(command) = &self.remediation_command {
                message.push_str("; regenerate with: ");
                message.push_str(command);
            }
            Error::InvalidConfig(message)
        })
    }

    /// Returns a fixture by key without constructing a hard-fail error.
    #[must_use]
    pub fn get_optional(&self, key: &str) -> Option<&T> {
        self.records.get(&normalize_key(key))
    }

    /// Returns records in stable key order.
    pub fn records(&self) -> impl Iterator<Item = &T> {
        self.records.values()
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from(path),
        source,
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct TestRecord {
        key: String,
        version: String,
        value: String,
    }

    impl FixtureRecord for TestRecord {
        fn fixture_key(&self) -> &str {
            &self.key
        }

        fn fixture_version(&self) -> &str {
            &self.version
        }
    }

    #[test]
    fn fixture_store_rejects_version_mismatch() {
        // Pins: stale fixture records cannot silently satisfy a newer prompt/schema version.
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("fixtures.jsonl");
        FixtureStore::write_jsonl(
            &path,
            [TestRecord {
                key: "abc".to_string(),
                version: "v1".to_string(),
                value: "old".to_string(),
            }],
        )
        .expect("write fixture");

        let error = FixtureStore::<TestRecord>::read_jsonl(&path, "v2")
            .expect_err("version mismatch should fail");

        assert!(error.to_string().contains("expected one of v2"));
        assert!(error.to_string().contains("abc"));
    }

    #[test]
    fn fixture_store_hard_fails_on_missing_key_with_remediation() {
        // Pins: missing fixture errors name the key and the exact remediation command.
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("fixtures.jsonl");
        FixtureStore::write_jsonl(
            &path,
            [TestRecord {
                key: "abc".to_string(),
                version: "v1".to_string(),
                value: "old".to_string(),
            }],
        )
        .expect("write fixture");
        let store = FixtureStore::<TestRecord>::read_jsonl(&path, "v1")
            .expect("read fixture")
            .with_remediation_command(
                "cargo run -p xtask --features eval-tools -- record-memory-extractions",
            );

        let error = store.get("def").expect_err("missing key should fail");

        assert!(error.to_string().contains("def"));
        assert!(
            error
                .to_string()
                .contains("cargo run -p xtask --features eval-tools -- record-memory-extractions")
        );
    }

    #[test]
    fn fixture_store_writes_sorted_by_key() {
        // Pins: fixture writes are byte-stable regardless of caller record order.
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("fixtures.jsonl");

        FixtureStore::write_jsonl(
            &path,
            [
                TestRecord {
                    key: "b".to_string(),
                    version: "v1".to_string(),
                    value: "second".to_string(),
                },
                TestRecord {
                    key: "a".to_string(),
                    version: "v1".to_string(),
                    value: "first".to_string(),
                },
            ],
        )
        .expect("write fixture");

        let body = fs::read_to_string(path).expect("read fixture");
        let lines = body.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""key":"a""#));
        assert!(lines[1].contains(r#""key":"b""#));
    }
}
