//! Cache-related metrics for long-conversation eval runs.

/// Token usage for one evaluated conversation turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TurnUsage {
    /// Total input tokens in the provider request.
    pub input_tokens: usize,
    /// Input tokens served from provider cache.
    pub cached_input_tokens: usize,
}

/// Serialized provider request bytes plus the stable prefix boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompiledRequest {
    /// Exact serialized provider request bytes.
    pub bytes: Vec<u8>,
    /// Number of leading bytes that should remain stable across adjacent turns.
    pub stable_prefix_len: usize,
}

/// Computes the aggregate cached-input ratio across all turns.
#[must_use]
pub fn compute_input_cached_ratio(per_turn_usage: &[TurnUsage]) -> f64 {
    let input_tokens = per_turn_usage
        .iter()
        .map(|usage| usage.input_tokens)
        .sum::<usize>();
    if input_tokens == 0 {
        return 0.0;
    }

    let cached_input_tokens = per_turn_usage
        .iter()
        .map(|usage| usage.cached_input_tokens)
        .sum::<usize>();
    cached_input_tokens as f64 / input_tokens as f64
}

/// Returns whether adjacent turns preserve their declared stable prefix bytes.
#[must_use]
pub fn compute_prefix_stability(turns: &[CompiledRequest]) -> bool {
    for (index, pair) in turns.windows(2).enumerate() {
        let left = &pair[0];
        let right = &pair[1];
        let prefix_len = left
            .stable_prefix_len
            .min(right.stable_prefix_len)
            .min(left.bytes.len())
            .min(right.bytes.len());
        if left.bytes[..prefix_len] != right.bytes[..prefix_len] {
            tracing::warn!(
                from_turn = index,
                to_turn = index + 1,
                stable_prefix_len = prefix_len,
                "compiled request stable prefix drifted"
            );
            return false;
        }
    }

    true
}
