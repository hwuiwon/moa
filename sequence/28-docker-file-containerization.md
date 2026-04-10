# Step 28: Docker File Tool Containerization

## What this step is about
When a Docker-backed hand is provisioned locally, `bash` commands execute inside the container via `docker exec`, but `file_read`, `file_write`, and `file_search` still operate on the host-mounted sandbox directory. This means Docker-backed hands are only partially containerized — command execution is isolated but file tools bypass the container boundary.

This step routes file tools through `docker exec` when a Docker hand is active, so all tool execution goes through the same isolation boundary.

## Files to read
- `moa-hands/src/local.rs` — `LocalHandProvider`, `execute_docker_tool()`, `DockerSandbox`, `provision_docker()`
- `moa-hands/src/tools/bash.rs` — `execute_docker()` for bash
- `moa-hands/src/tools/file_read.rs` — current host-path implementation
- `moa-hands/src/tools/file_write.rs` — current host-path implementation
- `moa-hands/src/tools/file_search.rs` — current host-path implementation
- `moa-hands/src/router.rs` — `ToolRouter::execute()`, `ToolContext`

## Goal
When a Docker hand exists for the session, file tools execute inside the container via `docker exec`. When no Docker hand exists (pure local mode), file tools continue using the host sandbox path. The decision is transparent to the brain.

## Rules
- File tools must work identically from the brain's perspective regardless of whether Docker is active.
- The container workspace mount point (e.g., `/workspace`) must be consistent between `bash` and file tools.
- Path traversal prevention must still apply — now validated against the container-internal workspace root, not the host path.
- File content flows through `docker exec` stdout/stdin — use `cat` for reads, heredoc or `tee` for writes, `find`/`grep` for search.
- If `docker exec` fails (container crashed, Docker daemon unreachable), file tools should fall back to host-path access with a warning, matching how provision already falls back gracefully.
- Do NOT change the `HandProvider` trait or `HandHandle` types. The `DockerSandbox` struct in `local.rs` already carries the `container_id` and `sandbox_dir`. Thread the execution mode decision through `LocalHandProvider::execute()`.

## Tasks

### 1. Add container-internal file helpers in `moa-hands/src/tools/`
Create a `docker_file_ops` module (or add to `bash.rs`) with helpers:

```rust
/// Read a file inside a running Docker container.
async fn docker_file_read(container_id: &str, path: &str, timeout: Duration) -> Result<String>;

/// Write content to a file inside a running Docker container.
async fn docker_file_write(container_id: &str, path: &str, content: &str, timeout: Duration) -> Result<()>;

/// Search for files inside a running Docker container using find + grep.
async fn docker_file_search(container_id: &str, pattern: &str, root: &str, timeout: Duration) -> Result<Vec<String>>;
```

Implementation notes:
- `docker_file_read`: `docker exec {id} cat {path}` — capture stdout
- `docker_file_write`: pipe content via stdin to `docker exec -i {id} tee {path} > /dev/null`
- `docker_file_search`: `docker exec {id} find {root} -name '{pattern}'` or `grep -rl` for content search
- All commands must validate the path is under `/workspace` (the container mount) before executing

### 2. Update `LocalHandProvider::execute_docker_tool()` in `local.rs`
Currently this method only routes `bash` to docker exec and falls back to host-path for file tools. Change to:

```rust
async fn execute_docker_tool(&self, container_id: &str, tool: &str, input: &str) -> Result<ToolOutput> {
    match tool {
        "bash" => bash::execute_docker(container_id, input, self.command_timeout).await,
        "file_read" => file_read::execute_docker(container_id, input, self.command_timeout).await,
        "file_write" => file_write::execute_docker(container_id, input, self.command_timeout).await,
        "file_search" => file_search::execute_docker(container_id, input, self.command_timeout).await,
        _ => {
            // Non-file, non-bash tools (memory, web) are built-in and don't need the container
            Err(MoaError::ToolError(format!("tool {tool} not supported in Docker mode")))
        }
    }
}
```

### 3. Add `execute_docker` methods to each file tool module
Each tool gets an `execute_docker()` function that:
- Parses the same JSON input as the host version
- Maps the path to the container workspace root (`/workspace`)
- Calls the docker file helper
- Returns the same `ToolOutput` shape

### 4. Path validation inside container
Create a container-path validator that:
- Ensures the requested path is under `/workspace` (or the configured container mount point)
- Prevents traversal (`../`, absolute paths outside workspace)
- The container mount point should come from the `DockerSandbox` struct (add a `workspace_mount: String` field if not already present)

### 5. Update `provision_docker()` to record the workspace mount
If the `DockerSandbox` struct doesn't already track the container-internal workspace mount, add it. The `-v` mount in `provision_docker()` sets this — record it so file tools know where `/workspace` is.

## Deliverables
```
moa-hands/src/local.rs              # Updated execute_docker_tool routing
moa-hands/src/tools/file_read.rs    # + execute_docker()
moa-hands/src/tools/file_write.rs   # + execute_docker()
moa-hands/src/tools/file_search.rs  # + execute_docker()
moa-hands/src/tools/docker_file.rs  # (optional) shared docker file helpers
```

## Acceptance criteria
1. When Docker is available and a container hand is provisioned, `file_read` executes via `docker exec ... cat`.
2. When Docker is available, `file_write` writes inside the container via `docker exec`.
3. When Docker is available, `file_search` searches inside the container via `docker exec ... find`.
4. Path traversal outside `/workspace` inside the container is rejected.
5. When Docker is NOT available, file tools continue using host-path sandbox (no regression).
6. If `docker exec` fails, the error is reported clearly (not a silent fallback to host).
7. All existing tests pass.

## Tests

**Unit tests (moa-hands):**
- `file_read::execute_docker` with a mock container — verify `docker exec cat` is invoked with correct path
- `file_write::execute_docker` — verify content is piped through stdin
- `file_search::execute_docker` — verify `docker exec find` is invoked
- Path traversal: `../../../etc/passwd` is rejected for container paths
- Path validation: `/workspace/src/main.rs` is allowed, `/etc/hosts` is rejected

**Integration tests (require Docker, gated by `#[cfg(feature = "docker-tests")]` or `#[ignore]`):**
- Provision a Docker hand → `file_write` a file → `file_read` it back → content matches
- Provision a Docker hand → `file_write` → `bash("cat /workspace/file")` → same content (proves shared filesystem)
- Provision a Docker hand → `file_search` for the written file → found

```bash
cargo test -p moa-hands
# Docker integration tests:
cargo test -p moa-hands -- --ignored
```
