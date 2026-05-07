//! Shared helpers for the Postgres session store.

use super::*;

pub(super) fn checkpoint_view(events: &[EventRecord]) -> (Option<String>, Vec<EventRecord>) {
    let latest_checkpoint = events.iter().rev().find_map(|record| match &record.event {
        Event::Checkpoint {
            summary,
            events_summarized,
            ..
        } => Some((summary.clone(), (*events_summarized) as usize)),
        _ => None,
    });
    let summary = latest_checkpoint
        .as_ref()
        .map(|(summary, _)| summary.clone());
    let summarized = latest_checkpoint.map(|(_, count)| count).unwrap_or(0);
    let non_checkpoint = events
        .iter()
        .filter(|record| !matches!(record.event, Event::Checkpoint { .. }))
        .cloned()
        .collect::<Vec<_>>();
    let non_checkpoint_len = non_checkpoint.len();
    let recent_events = non_checkpoint
        .into_iter()
        .skip(summarized.min(non_checkpoint_len))
        .collect::<Vec<_>>();

    (summary, recent_events)
}

pub(super) fn redact_password(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("******"));
        }
        return parsed.to_string();
    }

    url.to_string()
}

pub(super) fn vector_literal(values: &[f32]) -> String {
    let mut literal = String::from("[");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            literal.push(',');
        }
        literal.push_str(&value.to_string());
    }
    literal.push(']');
    literal
}

impl PostgresSessionStore {
    pub(super) async fn refresh_active_session_metric(&self) -> Result<()> {
        let sessions = self.table_name("sessions");
        let active = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*)::BIGINT FROM {sessions} WHERE status IN ($1, $2)"
        ))
        .bind(session_status_to_db(&SessionStatus::Running))
        .bind(session_status_to_db(&SessionStatus::WaitingApproval))
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        let active = u64::try_from(active).map_err(|_| {
            MoaError::StorageError("active session count exceeded u64 range".to_string())
        })?;
        record_sessions_active(active);
        Ok(())
    }

    pub(super) async fn event_record_from_row(
        &self,
        row: &sqlx::postgres::PgRow,
    ) -> Result<EventRecord> {
        let event_type_text = row
            .try_get::<String, _>("event_type")
            .map_err(map_sqlx_error)?;
        let payload = row
            .try_get::<serde_json::Value, _>("payload")
            .map_err(map_sqlx_error)?;
        let session_id = moa_core::SessionId(
            row.try_get::<Uuid, _>("session_id")
                .map_err(map_sqlx_error)?,
        );
        let event =
            decode_event_from_storage(self.blob_store.as_ref(), &session_id, payload).await?;

        Ok(EventRecord {
            id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
            session_id,
            sequence_num: row
                .try_get::<i64, _>("sequence_num")
                .map_err(map_sqlx_error)? as u64,
            event_type: event_type_from_db(&event_type_text)?,
            event,
            timestamp: row
                .try_get::<chrono::DateTime<Utc>, _>("timestamp")
                .map_err(map_sqlx_error)?,
            brain_id: row
                .try_get::<Option<Uuid>, _>("brain_id")
                .map_err(map_sqlx_error)?
                .map(moa_core::BrainId),
            hand_id: row
                .try_get::<Option<String>, _>("hand_id")
                .map_err(map_sqlx_error)?,
            token_count: row
                .try_get::<Option<i32>, _>("token_count")
                .map_err(map_sqlx_error)?
                .map(|value| value as usize),
        })
    }
}

pub(super) fn event_hand_id(event: &Event) -> Option<String> {
    match event {
        Event::ToolCall { hand_id, .. } => hand_id.clone(),
        Event::HandProvisioned { hand_id, .. }
        | Event::HandDestroyed { hand_id, .. }
        | Event::HandError { hand_id, .. } => Some(hand_id.clone()),
        _ => None,
    }
}

pub(super) fn normalize_event_search_query(query: &str) -> String {
    query
        .split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn serialize_resolution_signal(
    score: Option<&ResolutionScore>,
) -> Result<Option<String>> {
    score
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            MoaError::StorageError(format!("failed to serialize resolution score: {error}"))
        })
}

pub(super) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(super) fn qualified_name(schema_name: &str, table_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(table_name)
    )
}

#[cfg(test)]
mod tests {
    use super::normalize_event_search_query;

    #[test]
    fn normalize_event_search_query_drops_punctuation() {
        assert_eq!(
            normalize_event_search_query("refresh-token failure"),
            "refresh token failure"
        );
        assert!(normalize_event_search_query("!!!").is_empty());
    }
}
