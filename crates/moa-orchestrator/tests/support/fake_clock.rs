//! Fixed clock used by replay-determinism tests.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};

/// Cloneable fixed clock whose current time can be advanced by tests.
#[derive(Debug, Clone)]
pub struct FakeClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl FakeClock {
    /// Creates a fake clock pinned to `now`.
    #[must_use]
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Arc::new(Mutex::new(now)),
        }
    }

    /// Returns the current fake instant.
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("fake clock mutex poisoned")
    }

    /// Advances the fake clock by `duration`.
    pub fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().expect("fake clock mutex poisoned");
        *now += duration;
    }
}
