//! In-memory durable-step recorder used by workflow replay tests.

use std::fmt::Debug;

use serde::Serialize;
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
}

/// Records durable steps for deterministic helper tests.
#[derive(Debug, Default)]
pub struct Recorder {
    steps: Vec<DurableStep>,
}

impl Recorder {
    /// Creates a recorder for the first workflow run.
    #[must_use]
    pub fn recording() -> Self {
        Self::default()
    }

    /// Records a journaled `ctx.run(...)` step and returns its output.
    pub fn run<I, O>(&mut self, name: &str, input: &I, produce: impl FnOnce() -> O) -> O
    where
        I: Serialize,
        O: Serialize,
    {
        let input_canonical_json = canonical_json(input);
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
        self.steps.push(DurableStep::Invoke {
            service: service.to_string(),
            method: method.to_string(),
            input_canonical_json: canonical_json(input),
        });
        produce()
    }

    /// Returns the captured durable trace.
    #[must_use]
    pub fn finish(self) -> Vec<DurableStep> {
        self.steps
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
