//! Shutdown-signal coverage for the real `moa-edge` process.
//!
//! This drives the shipped binary rather than a library helper because the
//! behaviour under test only exists in `main`: which signals the process
//! installs a handler for, and whether receiving one produces a graceful exit
//! or a kernel-delivered death. Neither is observable from inside the crate —
//! a unit test can call the handler function, but calling it proves nothing
//! about whether the process would have survived the signal long enough to
//! reach it.

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use moa_session::testing;

/// How long the child gets to bind and serve `/healthz`.
const STARTUP_BUDGET: Duration = Duration::from_secs(60);
/// How long the child gets to exit after a shutdown signal.
///
/// Generous on purpose: the point of the assertion is the KIND of exit, and a
/// tight bound would turn a loaded machine into a failure that reads like a
/// missing handler.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(30);

#[tokio::test]
async fn edge_exits_gracefully_on_sigterm_and_sigint_db() {
    // Pins: the shipped edge installs handlers for BOTH shutdown signals and
    // exits normally on each. Without a SIGTERM handler the kernel's default
    // disposition terminates the process outright, so the run under a rolling
    // update performs no drain at all - the process is simply gone. That failure
    // is invisible to every assertion about exit TIMING (a signal-killed process
    // exits sooner, not later) and shows up only in the exit STATUS: terminated
    // by signal 15 versus exited with code 0.
    //
    // Both arms are driven because pinning only SIGTERM would pass for an
    // implementation that routed SIGTERM into the SIGINT branch, and the log
    // line each arm emits is what distinguishes them.
    let (database_url, schema_name) = testing::provision_cloned_database()
        .await
        .expect("provision isolated Postgres database");

    for (signal, expected_log) in [
        ("TERM", "moa-edge received SIGTERM"),
        ("INT", "moa-edge received SIGINT"),
    ] {
        let edge = SpawnedEdge::start(&database_url).await;
        edge.signal(signal);
        let outcome = edge.wait_for_exit();

        assert!(
            outcome.status.signal().is_none(),
            "SIG{signal} killed the edge instead of being handled: terminated by signal {:?}, \
             so no graceful drain ran. stdout:\n{}\nstderr:\n{}",
            outcome.status.signal(),
            outcome.stdout,
            outcome.stderr
        );
        assert_eq!(
            outcome.status.code(),
            Some(0),
            "edge did not exit cleanly after SIG{signal}: {:?}. stdout:\n{}\nstderr:\n{}",
            outcome.status,
            outcome.stdout,
            outcome.stderr
        );
        assert!(
            outcome.stdout.contains(expected_log),
            "edge did not report the SIG{signal} arm; expected `{expected_log}`. stdout:\n{}\n\
             stderr:\n{}",
            outcome.stdout,
            outcome.stderr
        );
    }

    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated Postgres database");
}

/// One running `moa-edge` child process and the port it serves.
struct SpawnedEdge {
    child: Child,
    port: u16,
}

/// What a terminated child left behind.
struct EdgeExit {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl SpawnedEdge {
    async fn start(database_url: &str) -> Self {
        let port = free_loopback_port().await;
        let mut command = Command::new(env!("CARGO_BIN_EXE_moa-edge"));
        command
            // An ambient developer environment must not decide what this child
            // does. `.env.example` alone would point it at another database and
            // make it bind a Prometheus scrape port shared with every sibling.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap_or_default())
            .env("MOA_DATABASE_URL", database_url)
            .env("MOA_EDGE_BIND", format!("127.0.0.1:{port}"))
            .env("MOA_EDGE_UPSTREAM", "http://127.0.0.1:1")
            .env(
                "MOA_EDGE_CONNECTOR_CREDENTIAL_UPSTREAM",
                "http://127.0.0.1:1",
            )
            .env("MOA_METRICS_EXPORTER", "disabled")
            // The shutdown arms log at info; the process default is warn, which
            // would leave the distinguishing line out of stdout entirely.
            .env("RUST_LOG", "moa_edge=info")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(target_os = "macos")]
        if let Some(dylib_path) = std::env::var_os("DYLD_FALLBACK_LIBRARY_PATH") {
            // `prefer-dynamic` test binaries need Cargo's Rust dylib path even
            // though application configuration remains fully sanitized.
            command.env("DYLD_FALLBACK_LIBRARY_PATH", dylib_path);
        }
        let child = command.spawn().expect("spawn moa-edge binary");
        let mut edge = Self { child, port };
        edge.await_ready().await;
        edge
    }

    async fn await_ready(&mut self) {
        let client = reqwest::Client::new();
        let health = format!("http://127.0.0.1:{}/healthz", self.port);
        let deadline = Instant::now() + STARTUP_BUDGET;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("poll moa-edge child") {
                let exit = self.collect_output(status);
                panic!(
                    "moa-edge exited during startup with {:?}. stdout:\n{}\nstderr:\n{}",
                    exit.status, exit.stdout, exit.stderr
                );
            }
            if client
                .get(&health)
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = self.child.kill();
        let status = self.child.wait().expect("reap unresponsive moa-edge");
        let exit = self.collect_output(status);
        panic!(
            "moa-edge never served /healthz on port {} within {STARTUP_BUDGET:?}. stdout:\n{}\n\
             stderr:\n{}",
            self.port, exit.stdout, exit.stderr
        );
    }

    /// Sends one signal by name, exactly as an orchestrator or a shell would.
    fn signal(&self, name: &str) {
        let delivered = Command::new("/bin/kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(
            delivered.success(),
            "could not deliver SIG{name} to pid {}: {delivered:?}",
            self.child.id()
        );
    }

    fn wait_for_exit(mut self) -> EdgeExit {
        let deadline = Instant::now() + SHUTDOWN_BUDGET;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll moa-edge child") {
                return self.collect_output(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let status = self.child.wait().expect("reap unresponsive moa-edge");
                let exit = self.collect_output(status);
                panic!(
                    "moa-edge did not exit within {SHUTDOWN_BUDGET:?} of the shutdown signal; \
                     killed it. stdout:\n{}\nstderr:\n{}",
                    exit.stdout, exit.stderr
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn collect_output(&mut self, status: ExitStatus) -> EdgeExit {
        EdgeExit {
            status,
            stdout: read_pipe(self.child.stdout.take()),
            stderr: read_pipe(self.child.stderr.take()),
        }
    }
}

impl Drop for SpawnedEdge {
    fn drop(&mut self) {
        // A test that panics before reaping would otherwise leave a process
        // holding the isolated database open, which blocks its DROP.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn read_pipe<R: Read>(pipe: Option<R>) -> String {
    let mut buffer = String::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_string(&mut buffer);
    }
    buffer
}

/// Reserves a loopback port by binding and releasing it.
async fn free_loopback_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let port = listener.local_addr().expect("read ephemeral port").port();
    drop(listener);
    port
}
