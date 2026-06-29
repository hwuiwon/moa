//! Shared helpers for the Postgres session store.

use super::*;

pub(super) fn redact_password(url: &str) -> String {
    if let Ok(mut parsed) = url::Url::parse(url) {
        if parsed.password().is_some() {
            let _ = parsed.set_password(Some("******"));
        }
        return parsed.to_string();
    }

    url.to_string()
}

impl PostgresSessionStore {
    /// Refresh the process metric that tracks currently active sessions.
    pub async fn refresh_active_session_metric(&self) -> Result<()> {
        let sessions = self.table_name("sessions");
        let active = sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*)::BIGINT FROM {sessions} WHERE status = $1"
        ))
        .bind(SessionStatus::Running.as_str())
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
        let event_type_text = row.col::<String>("event_type")?;
        let payload = row.col::<serde_json::Value>("payload")?;
        let session_id = moa_core::SessionId(row.col::<Uuid>("session_id")?);
        let event =
            decode_event_from_storage(self.blob_store.as_ref(), &session_id, payload).await?;

        Ok(EventRecord {
            id: row.col::<Uuid>("id")?,
            session_id,
            sequence_num: row.col::<i64>("sequence_num")? as u64,
            event_type: from_db("event type", &event_type_text)?,
            event,
            timestamp: row.col::<chrono::DateTime<Utc>>("timestamp")?,
            brain_id: row.col::<Option<Uuid>>("brain_id")?.map(moa_core::BrainId),
            hand_id: row.col::<Option<String>>("hand_id")?,
            token_count: row
                .col::<Option<i32>>("token_count")?
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

pub(super) fn serialize_segment_assessment(
    assessment: Option<&SegmentAssessment>,
) -> Result<Option<String>> {
    assessment
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| {
            MoaError::StorageError(format!("failed to serialize segment assessment: {error}"))
        })
}

pub(super) fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
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
