//! Session lifecycle state behavior.

use super::*;

impl SessionVoState {
    /// Initializes the projection from persisted session metadata.
    pub fn set_meta(&mut self, meta: SessionMeta) {
        self.status = Some(meta.status.clone());
        self.meta = Some(meta);
    }

    /// Returns the current lifecycle status, defaulting to `Created` when state is empty.
    pub fn current_status(&self) -> SessionStatus {
        self.status.clone().unwrap_or(SessionStatus::Created)
    }

    /// Loads only the lifecycle status key for hot read-only status polls, so the
    /// handler skips deserializing children, pending, and narration state.
    pub(in crate::objects::session) async fn load_status<R: VoReader>(
        reader: &R,
    ) -> Result<SessionStatus, HandlerError> {
        Ok(reader
            .get_json(K_STATUS)
            .await?
            .unwrap_or(SessionStatus::Created))
    }

    /// Ensures that session metadata has been initialized before mutations proceed.
    pub fn ensure_initialized(&self) -> MoaResult<&SessionMeta> {
        self.meta.as_ref().ok_or_else(|| {
                MoaError::ValidationError(
                    "Session metadata missing. Initialize the VO via SessionStore/init_session_vo first."
                        .to_string(),
                )
            })
    }

    /// Keeps the owning session active after a detached execution run is admitted.
    pub fn apply_accepted_execution_turn(&mut self, now: DateTime<Utc>) {
        self.last_turn_summary = Some("Execution accepted.".to_string());
        self.set_status(SessionStatus::Running, now);
    }

    /// Clears the in-memory projection back to an empty VO.
    pub fn destroy(&mut self) {
        *self = Self::default();
    }

    /// Updates both the hot lifecycle state and its persisted metadata mirror.
    pub(in crate::objects::session) fn set_status(
        &mut self,
        status: SessionStatus,
        now: DateTime<Utc>,
    ) {
        self.status = Some(status.clone());
        if let Some(meta) = self.meta.as_mut() {
            meta.status = status.clone();
            meta.updated_at = now;
            if matches!(
                status,
                SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
            ) && meta.completed_at.is_none()
            {
                meta.completed_at = Some(now);
            }
        }
    }
}
