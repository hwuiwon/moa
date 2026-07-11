//! Deterministic storage failpoints for chaos tests.
//!
//! Compiled only with the `failpoints` feature; production builds carry no
//! trace of them. A failpoint fails the first `budget` calls that pass
//! through it with a `StorageError`, then permanently disarms. Container-
//! killing chaos can rarely land a fault between "row committed" and "ack
//! returned"; failpoints make exactly that instant reproducible.
//!
//! Arm programmatically with [`arm`] (in-process tests) or via environment
//! (`MOA_FAILPOINT_<NAME>=<n>`, upper-cased) for containerized processes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;

use moa_core::error::MoaError;

struct FailpointState {
    budget: u64,
    seen: u64,
}

static STATE: OnceLock<Mutex<HashMap<String, FailpointState>>> = OnceLock::new();

fn state() -> &'static Mutex<HashMap<String, FailpointState>> {
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Arms `name` to fail its next `budget` calls.
pub fn arm(name: &str, budget: u64) {
    if let Ok(mut map) = state().lock() {
        map.insert(name.to_string(), FailpointState { budget, seen: 0 });
    }
}

/// Disarms `name` and forgets its counter.
pub fn reset(name: &str) {
    if let Ok(mut map) = state().lock() {
        map.remove(name);
    }
}

/// Returns the injected failure for `name` while its fail budget lasts.
pub fn hit(name: &str) -> Option<MoaError> {
    let mut map = state().lock().ok()?;
    if !map.contains_key(name) {
        let env_key = format!("MOA_FAILPOINT_{}", name.to_uppercase());
        let budget: u64 = std::env::var(env_key).ok()?.trim().parse().ok()?;
        map.insert(name.to_string(), FailpointState { budget, seen: 0 });
    }
    let entry = map.get_mut(name)?;
    if entry.seen >= entry.budget {
        return None;
    }
    entry.seen += 1;
    Some(MoaError::StorageError(format!(
        "failpoint {name} injected failure {}/{}",
        entry.seen, entry.budget
    )))
}
