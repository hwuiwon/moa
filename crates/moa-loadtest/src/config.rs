//! Configuration and workspace helpers for load tests.

use crate::*;

pub(crate) fn load_config(path: Option<&Path>) -> Result<MoaConfig> {
    match path {
        Some(path) => MoaConfig::load_from_path(path),
        None => MoaConfig::load(),
    }
}

pub(crate) fn resolve_workspace_root(path: Option<&Path>) -> Result<PathBuf> {
    let root = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().map_err(|error| {
            MoaError::ProviderError(format!("failed to resolve current directory: {error}"))
        })?,
    };
    root.canonicalize()
        .or(Ok(root))
        .map_err(|error: std::io::Error| {
            MoaError::ProviderError(format!("failed to canonicalize workspace root: {error}"))
        })
}

pub(crate) fn workspace_id_for_root(root: &Path, suffix: &str) -> WorkspaceId {
    let label = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_workspace_label)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "workspace".to_string());
    WorkspaceId::new(format!(
        "{label}-loadtest-{suffix}-{}",
        &Uuid::now_v7().simple().to_string()[..8]
    ))
}

pub(crate) fn sanitize_workspace_label(label: &str) -> String {
    let mut sanitized = String::with_capacity(label.len());
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
    }
    sanitized.trim_matches('-').to_string()
}

pub(crate) fn expand_local_path(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }
    PathBuf::from(path)
}
