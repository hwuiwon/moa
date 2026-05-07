//! Query and resolution signals used to rank available skills.

use std::collections::HashMap;

use moa_core::{Event, EventRange, Result, SkillResolutionRate, WorkingContext};

use crate::pipeline::memory::extract_search_keywords;

use super::{RECENT_EVENT_LIMIT, SkillInjector};

impl SkillInjector {
    pub(super) async fn query_keywords(&self, ctx: &WorkingContext) -> Result<Vec<String>> {
        if let Some(message) = ctx.last_user_message() {
            let keywords = extract_search_keywords(message);
            if !keywords.is_empty() {
                return Ok(keywords);
            }
        }

        let Some(session_store) = &self.session_store else {
            return Ok(Vec::new());
        };
        let events = session_store
            .get_events(ctx.session_id, EventRange::recent(RECENT_EVENT_LIMIT))
            .await?;
        Ok(extract_query_keywords_from_events(&events))
    }

    pub(super) async fn skill_resolution_rates(
        &self,
        ctx: &WorkingContext,
    ) -> Result<HashMap<String, f64>> {
        let Some(session_store) = &self.session_store else {
            return Ok(HashMap::new());
        };
        let rates = session_store
            .list_skill_resolution_rates(ctx.workspace_id.as_str(), None)
            .await?;
        Ok(skill_resolution_rate_map(&rates))
    }
}

fn skill_resolution_rate_map(rates: &[SkillResolutionRate]) -> HashMap<String, f64> {
    rates
        .iter()
        .map(|rate| {
            (
                rate.skill_name.clone(),
                rate.resolution_rate.clamp(0.0, 1.0),
            )
        })
        .collect()
}

fn extract_query_keywords_from_events(events: &[moa_core::EventRecord]) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(extract_search_keywords(text))
            }
            _ => None,
        })
        .unwrap_or_default()
}
