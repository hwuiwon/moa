//! Turn-level fingerprinting used to detect repeated tool-call loops.

use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

const OUTPUT_PREFIX_LEN: usize = 200;

/// Tracks recent completed-turn fingerprints to detect circular agent behavior.
#[derive(Debug, Clone)]
pub struct LoopDetector {
    threshold: u32,
    recent_fingerprints: VecDeque<u64>,
}

impl LoopDetector {
    /// Creates a new loop detector with the provided repetition threshold.
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            recent_fingerprints: VecDeque::with_capacity(threshold as usize + 1),
        }
    }

    /// Records one completed turn's tool-call summaries and returns `true` when a loop is detected.
    pub fn record_turn(&mut self, tool_calls: &[(String, String)]) -> bool {
        if self.threshold == 0 {
            return false;
        }

        let fingerprint = self.fingerprint(tool_calls);
        self.recent_fingerprints.push_back(fingerprint);

        while self.recent_fingerprints.len() > self.threshold as usize {
            let _ = self.recent_fingerprints.pop_front();
        }

        if self.recent_fingerprints.len() < self.threshold as usize {
            return false;
        }

        let Some(first) = self.recent_fingerprints.front().copied() else {
            return false;
        };
        self.recent_fingerprints
            .iter()
            .all(|fingerprint| *fingerprint == first)
    }

    /// Clears all recorded turn fingerprints.
    pub fn reset(&mut self) {
        self.recent_fingerprints.clear();
    }

    fn fingerprint(&self, tool_calls: &[(String, String)]) -> u64 {
        let mut hasher = DefaultHasher::new();
        for (tool_name, output) in tool_calls {
            tool_name.hash(&mut hasher);
            output
                .chars()
                .take(OUTPUT_PREFIX_LEN)
                .collect::<String>()
                .hash(&mut hasher);
        }
        hasher.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::LoopDetector;

    #[test]
    fn loop_detector_triggers_after_threshold() {
        let mut detector = LoopDetector::new(3);
        let calls = vec![(
            "file_read".to_string(),
            "contents of views.py...".to_string(),
        )];
        assert!(!detector.record_turn(&calls));
        assert!(!detector.record_turn(&calls));
        assert!(detector.record_turn(&calls));
    }

    #[test]
    fn loop_detector_does_not_trigger_on_varied_calls() {
        let mut detector = LoopDetector::new(3);
        assert!(!detector.record_turn(&[("file_read".into(), "a".into())]));
        assert!(!detector.record_turn(&[("bash".into(), "b".into())]));
        assert!(!detector.record_turn(&[("file_write".into(), "c".into())]));
    }

    #[test]
    fn loop_detector_disabled_at_zero_threshold() {
        let mut detector = LoopDetector::new(0);
        let calls = vec![("file_read".into(), "same".into())];
        for _ in 0..10 {
            assert!(!detector.record_turn(&calls));
        }
    }

    #[test]
    fn loop_detector_resets() {
        let mut detector = LoopDetector::new(3);
        let calls = vec![("bash".into(), "output".into())];
        assert!(!detector.record_turn(&calls));
        assert!(!detector.record_turn(&calls));
        detector.reset();
        assert!(!detector.record_turn(&calls));
    }

    #[test]
    fn loop_detector_sliding_window() {
        let mut detector = LoopDetector::new(3);
        let a = vec![("bash".into(), "output_a".into())];
        let b = vec![("bash".into(), "output_b".into())];
        assert!(!detector.record_turn(&a));
        assert!(!detector.record_turn(&a));
        assert!(!detector.record_turn(&b));
        assert!(!detector.record_turn(&b));
        assert!(detector.record_turn(&b));
    }
}
