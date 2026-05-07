//! Neon checkpoint command handlers.

use super::*;

pub(crate) async fn checkpoint_create_report(config: &MoaConfig, label: &str) -> Result<String> {
    let manager = load_branch_manager(config)?;
    let handle = manager
        .create_checkpoint(label, None)
        .await
        .context("creating Neon checkpoint")?;
    Ok(format!(
        "created checkpoint\nid: {}\nlabel: {}\ncreated_at: {}\nconnection_url: {}\n",
        handle.id, handle.label, handle.created_at, handle.connection_url
    ))
}

pub(crate) async fn checkpoint_list_report(config: &MoaConfig) -> Result<String> {
    let manager = load_branch_manager(config)?;
    let checkpoints = manager
        .list_checkpoints()
        .await
        .context("listing Neon checkpoints")?;
    if checkpoints.is_empty() {
        return Ok("no active checkpoints\n".to_string());
    }

    let mut lines = Vec::with_capacity(checkpoints.len() + 1);
    lines.push("active checkpoints:".to_string());
    for checkpoint in checkpoints {
        let age = format_checkpoint_age(checkpoint.handle.created_at);
        lines.push(format!(
            "- {}  {}  age={} parent={} size_bytes={}",
            checkpoint.handle.id,
            checkpoint.handle.label,
            age,
            checkpoint.parent_branch,
            checkpoint
                .size_bytes
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ));
    }
    Ok(format!("{}\n", lines.join("\n")))
}

pub(crate) async fn checkpoint_rollback_report(mut config: MoaConfig, id: &str) -> Result<String> {
    let manager = load_branch_manager(&config)?;
    let checkpoint = manager
        .get_checkpoint(id)
        .await
        .context("loading checkpoint metadata")?
        .with_context(|| format!("checkpoint {id} not found"))?;
    manager
        .rollback_to(&checkpoint.handle)
        .await
        .context("preparing checkpoint rollback")?;
    config.database.url = checkpoint.handle.connection_url.clone();
    config.save_async().await.context("saving config")?;
    Ok(format!(
        "rolled back to checkpoint\nid: {}\nlabel: {}\ndatabase_url: {}\n",
        checkpoint.handle.id, checkpoint.handle.label, checkpoint.handle.connection_url
    ))
}

pub(crate) async fn checkpoint_cleanup_report(config: &MoaConfig) -> Result<String> {
    let manager = load_branch_manager(config)?;
    let deleted = manager
        .cleanup_expired()
        .await
        .context("cleaning up expired checkpoints")?;
    Ok(format!("deleted_expired_checkpoints: {deleted}\n"))
}

pub(crate) fn format_checkpoint_age(created_at: chrono::DateTime<chrono::Utc>) -> String {
    let age = chrono::Utc::now() - created_at;
    if age.num_hours() >= 1 {
        return format!("{}h", age.num_hours());
    }
    if age.num_minutes() >= 1 {
        return format!("{}m", age.num_minutes());
    }
    format!("{}s", age.num_seconds().max(0))
}
