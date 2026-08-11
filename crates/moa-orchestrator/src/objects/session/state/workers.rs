//! Session workers state behavior.

use super::*;

impl WorkerFanInState {
    /// Loads the independent Session-owned worker fan-in key.
    pub async fn load<R: VoReader>(reader: &R) -> Result<Self, HandlerError> {
        Ok(reader
            .get_json(K_WORKER_FAN_IN_STATE)
            .await?
            .unwrap_or_default())
    }

    /// Persists the independent Session-owned worker fan-in key.
    pub fn persist(&self, ctx: &ObjectContext<'_>) {
        ctx.set(K_WORKER_FAN_IN_STATE, Json::from(self.clone()));
    }

    /// Advances the fan-in generation after a child registration succeeds.
    pub fn register_child(&mut self, children: &[WorkerChildRef]) {
        self.generation = self.generation.saturating_add(1);
        self.failure_generation = if children.iter().any(|child| {
            child
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.state == WorkerState::Failed)
        }) {
            self.generation
        } else {
            0
        };
    }

    /// Suppresses a success/cancelled fan-in wake for the current child-set generation.
    pub fn suppress_current_generation(&mut self) {
        self.failure_generation = self.generation;
    }

    /// Caches one validated terminal delivery and computes the one-shot fan-in transition.
    pub fn record_terminal(
        &mut self,
        state: &mut SessionVoState,
        input: &RecordWorkerChildTerminalInput,
    ) -> WorkerTerminalRecord {
        let Some(child_index) = state
            .children
            .iter()
            .position(|child| child.id == input.worker_id)
        else {
            return WorkerTerminalRecord::Duplicate;
        };
        if self.terminal_deliveries.iter().any(|delivery| {
            delivery.worker_id == input.worker_id && delivery.generation >= input.generation
        }) {
            return WorkerTerminalRecord::Duplicate;
        }

        state.remove_child_terminal_blob(&input.worker_id);
        state.children[child_index].terminal = Some(input.terminal.clone());
        self.terminal_deliveries
            .retain(|delivery| delivery.worker_id != input.worker_id);
        self.terminal_deliveries.push(WorkerTerminalDeliveryFence {
            worker_id: input.worker_id.clone(),
            generation: input.generation,
        });
        if self.terminal_deliveries.len() > MAX_WORKER_TERMINAL_DELIVERY_FENCES {
            self.terminal_deliveries.remove(0);
        }
        if input.terminal.state == WorkerState::Failed {
            self.suppress_current_generation();
        }

        let all_settled = !state.children.is_empty()
            && state.children.iter().all(|child| child.terminal.is_some());
        let settled = if all_settled && self.settled_generation != self.generation {
            self.settled_generation = self.generation;
            (self.failure_generation != self.generation).then_some(input.terminal.state)
        } else {
            None
        };
        WorkerTerminalRecord::Accepted { settled }
    }
}

impl SessionVoState {
    /// Loads only the child-refs key for hot read-only child polls.
    pub(in crate::objects::session) async fn load_children<R: VoReader>(
        reader: &R,
    ) -> Result<Vec<WorkerChildRef>, HandlerError> {
        Ok(reader.get_json(K_CHILDREN).await?.unwrap_or_default())
    }

    /// Adds a root-owned child worker reference if it is not already registered.
    pub fn register_child(&mut self, child: WorkerChildRef) -> bool {
        if self.children.iter().any(|existing| existing.id == child.id) {
            return false;
        }
        self.children.push(child);
        true
    }

    /// Removes and returns a cached terminal child result.
    pub fn consume_child_terminal(&mut self, worker_id: &str) -> Option<WorkerTerminalResult> {
        let index = self
            .children
            .iter()
            .position(|child| child.id == worker_id && child.terminal.is_some())?;
        self.children.remove(index).terminal
    }

    /// Removes a root-owned child worker reference by id.
    pub fn remove_child(&mut self, worker_id: &str) -> bool {
        let before = self.children.len();
        self.children.retain(|child| child.id != worker_id);
        // A child that left the fan-out can no longer be answered, so any reply target
        // it still advertises would swallow the next plain user message.
        self.clear_worker_input_targets_for_worker(worker_id);
        // Drop any claim-check reference for the now-removed child's output; the blob is
        // reclaimed at session teardown.
        self.remove_child_terminal_blob(worker_id);
        self.children.len() != before
    }

    /// Returns the full output of a terminal child when it exceeds the claim-check
    /// threshold and is still stored inline, so the handler can offload it to a blob.
    #[must_use]
    pub fn large_child_terminal_output(&self, worker_id: &str) -> Option<String> {
        let child = self.children.iter().find(|child| child.id == worker_id)?;
        let terminal = child.terminal.as_ref()?;
        (terminal.result.output.len() >= CHILD_OUTPUT_CLAIM_CHECK_THRESHOLD_BYTES)
            .then(|| terminal.result.output.clone())
    }

    /// Replaces a terminal child's inline output with a preview after its full body was
    /// offloaded to `claim_check`, recording the reference for later hydration.
    pub fn compact_child_terminal_output(&mut self, worker_id: &str, claim_check: ClaimCheck) {
        {
            let Some(child) = self.children.iter_mut().find(|child| child.id == worker_id) else {
                return;
            };
            let Some(terminal) = child.terminal.as_mut() else {
                return;
            };
            let preview = child_output_preview(&terminal.result.output);
            terminal.result.output = preview;
        }
        // One reference per worker: replace any stale entry so a revived/re-marked child
        // cannot accumulate duplicates.
        self.child_terminal_blobs
            .retain(|reference| reference.worker_id != worker_id);
        self.child_terminal_blobs.push(ChildTerminalOutputRef {
            worker_id: worker_id.to_string(),
            claim_check,
        });
    }

    /// Removes and returns a terminal child's output claim-check reference, if any, so the
    /// consuming handler can hydrate the full body.
    pub fn take_child_terminal_blob(&mut self, worker_id: &str) -> Option<ClaimCheck> {
        let index = self
            .child_terminal_blobs
            .iter()
            .position(|reference| reference.worker_id == worker_id)?;
        Some(self.child_terminal_blobs.remove(index).claim_check)
    }

    /// Drops a terminal child's output claim-check reference without returning it.
    fn remove_child_terminal_blob(&mut self, worker_id: &str) {
        self.child_terminal_blobs
            .retain(|reference| reference.worker_id != worker_id);
    }

    /// Returns whether the session currently owns the child worker id.
    #[must_use]
    pub fn owns_child(&self, worker_id: &str) -> bool {
        self.children.iter().any(|child| child.id == worker_id)
    }

    /// Returns whether a child signal belongs to this session's worker tree.
    #[must_use]
    pub fn owns_signal_worker(&self, signal: &WorkerSignal) -> bool {
        self.owns_child(&signal.worker_id)
    }
}

/// Truncated, human-readable preview retained inline for a claim-checked child output.
#[must_use]
fn child_output_preview(output: &str) -> String {
    output.chars().take(CHILD_OUTPUT_PREVIEW_CHARS).collect()
}
