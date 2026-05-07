//! In-memory durable-step recorder used by workflow replay tests.

use std::fmt::Debug;

use moa_core::compute_unified_diff;
use serde::{Serialize, de::DeserializeOwned};
use serde_canonical_json::CanonicalFormatter;
use serde_json::Serializer;

/// Durable operation shape pinned by replay-determinism tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DurableStep {
    /// Journaled `ctx.run(...)` closure.
    Run {
        /// Stable Restate run name.
        name: String,
        /// Canonical JSON input the workflow used to derive this durable call.
        input_canonical_json: String,
        /// Canonical JSON output captured by the durable call.
        output_canonical_json: String,
    },
    /// Durable service, object, or workflow invocation.
    Invoke {
        /// Target service or object name.
        service: String,
        /// Target handler method.
        method: String,
        /// Canonical JSON invocation input.
        input_canonical_json: String,
    },
    /// Durable sleep.
    Sleep {
        /// Sleep duration in milliseconds.
        duration_ms: u64,
    },
    /// Durable state operation.
    State {
        /// State operation name.
        op: String,
        /// State key.
        key: String,
        /// Canonical JSON state value, when the operation carries one.
        value_canonical_json: Option<String>,
    },
}

/// Records first-run durable steps or replays from a previously captured trace.
#[derive(Debug, Default)]
pub struct Recorder {
    replay_source: Option<Vec<DurableStep>>,
    cursor: usize,
    steps: Vec<DurableStep>,
}

impl Recorder {
    /// Creates a recorder for the first workflow run.
    #[must_use]
    pub fn recording() -> Self {
        Self::default()
    }

    /// Creates a recorder that replays journaled run outputs from a previous trace.
    #[must_use]
    pub fn replaying(source: Vec<DurableStep>) -> Self {
        Self {
            replay_source: Some(source),
            cursor: 0,
            steps: Vec::new(),
        }
    }

    /// Records a journaled `ctx.run(...)` step and returns its output.
    pub fn run<I, O>(&mut self, name: &str, input: &I, produce: impl FnOnce() -> O) -> O
    where
        I: Serialize,
        O: Serialize + DeserializeOwned,
    {
        let input_canonical_json = canonical_json(input);
        if let Some(DurableStep::Run {
            name: expected_name,
            output_canonical_json,
            ..
        }) = self.next_replay_step()
            && expected_name == name
        {
            let output = serde_json::from_str(&output_canonical_json).unwrap_or_else(|error| {
                panic!("replayed run output for `{name}` did not deserialize: {error}")
            });
            self.steps.push(DurableStep::Run {
                name: name.to_string(),
                input_canonical_json,
                output_canonical_json: output_canonical_json.clone(),
            });
            return output;
        }

        let output = produce();
        self.steps.push(DurableStep::Run {
            name: name.to_string(),
            input_canonical_json,
            output_canonical_json: canonical_json(&output),
        });
        output
    }

    /// Records a durable invocation and returns the supplied fixture output.
    pub fn invoke<I, O>(
        &mut self,
        service: &str,
        method: &str,
        input: &I,
        produce: impl FnOnce() -> O,
    ) -> O
    where
        I: Serialize,
    {
        let _ = self.next_replay_step();
        self.steps.push(DurableStep::Invoke {
            service: service.to_string(),
            method: method.to_string(),
            input_canonical_json: canonical_json(input),
        });
        produce()
    }

    /// Records a durable sleep.
    pub fn sleep(&mut self, duration_ms: u64) {
        let _ = self.next_replay_step();
        self.steps.push(DurableStep::Sleep { duration_ms });
    }

    /// Records a durable state operation.
    pub fn state<V: Serialize>(&mut self, op: &str, key: &str, value: Option<&V>) {
        let _ = self.next_replay_step();
        self.steps.push(DurableStep::State {
            op: op.to_string(),
            key: key.to_string(),
            value_canonical_json: value.map(canonical_json),
        });
    }

    /// Returns the captured durable trace.
    #[must_use]
    pub fn finish(self) -> Vec<DurableStep> {
        self.steps
    }

    fn next_replay_step(&mut self) -> Option<DurableStep> {
        let step = self
            .replay_source
            .as_ref()
            .and_then(|source| source.get(self.cursor))
            .cloned();
        if step.is_some() {
            self.cursor = self.cursor.saturating_add(1);
        }
        step
    }
}

/// Serializes a value to canonical JSON with lexically sorted object keys.
#[must_use]
pub fn canonical_json(value: &impl Serialize) -> String {
    let mut serializer = Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
    value
        .serialize(&mut serializer)
        .expect("serialize canonical JSON");
    String::from_utf8(serializer.into_inner()).expect("canonical JSON must be UTF-8")
}

/// Asserts two traces are identical and prints the first divergent step as a unified diff.
pub fn assert_traces_identical(first: &[DurableStep], second: &[DurableStep]) {
    if first == second {
        return;
    }

    let max_len = first.len().max(second.len());
    let mismatch = (0..max_len)
        .find(|index| first.get(*index) != second.get(*index))
        .unwrap_or(0);
    let before = first
        .get(mismatch)
        .map(step_canonical_json)
        .unwrap_or_else(|| "{\"missing\":true}".to_string());
    let after = second
        .get(mismatch)
        .map(step_canonical_json)
        .unwrap_or_else(|| "{\"missing\":true}".to_string());
    let diff = compute_unified_diff(&format!("durable-step-{mismatch}.json"), &before, &after, 3);
    panic!("durable traces diverged at step {mismatch}\n{diff}");
}

fn step_canonical_json(step: &DurableStep) -> String {
    canonical_json(step)
}
