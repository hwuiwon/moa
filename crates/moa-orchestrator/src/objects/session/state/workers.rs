//! Session workers state behavior.

use super::*;

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

    /// Caches a terminal child result until the parent consumes it.
    pub fn mark_child_terminal(&mut self, input: MarkWorkerChildTerminalInput) -> bool {
        let Some(child) = self
            .children
            .iter_mut()
            .find(|child| child.id == input.worker_id)
        else {
            return false;
        };
        if child.terminal.is_some() {
            return false;
        }
        child.terminal = Some(input.terminal);
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
        // Drop any outstanding liveness watchdog for the now-removed child.
        self.clear_child_liveness(worker_id);
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
