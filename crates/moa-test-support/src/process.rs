//! Panic- and cancellation-safe child-process ownership for integration tests.

use std::process::Child;
use std::time::{Duration, Instant};

/// Owns one child process and terminates it when the guard is dropped.
///
/// Tests should arm the guard immediately after spawning a long-lived service.
/// This keeps assertion panics, task cancellation, and early `?` returns from
/// leaking background processes into later test lanes.
pub struct TestChildGuard {
    child: Option<Child>,
}

impl TestChildGuard {
    /// Arms a guard for `child`.
    #[must_use]
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Returns mutable access to the guarded child while it remains armed.
    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Disarms the guard and transfers ownership of the live child.
    #[must_use]
    pub fn disarm(mut self) -> Option<Child> {
        self.child.take()
    }
}

impl Drop for TestChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            terminate_child(child);
        }
    }
}

/// Best-effort graceful termination followed by a bounded forced kill.
pub(crate) fn terminate_child(mut child: Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
