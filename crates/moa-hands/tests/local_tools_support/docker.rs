// Docker-backed local-tools test support.

use std::path::Path;
use std::time::Duration;

use moa_core::{traits::HandProvider, types::hands::SandboxTier};
use moa_hands::LocalHandProvider;
use serde_json::json;
use tempfile::{TempDir, tempdir, tempdir_in};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn docker_mountable_tempdir() -> TempDir {
    let macos_docker_tmp = Path::new("/private/tmp");
    if macos_docker_tmp.exists() {
        return tempdir_in(macos_docker_tmp).expect("create Docker-mountable tempdir");
    }
    tempdir().expect("create tempdir")
}
