//! Session segments state behavior.

use super::*;

impl SessionVoState {
    /// Replaces the active task segment.
    pub fn set_current_segment(&mut self, segment: ActiveSegment) {
        self.current_segment = Some(segment);
    }

    /// Records a tool usage on the active task segment.
    pub fn record_segment_tool_use(&mut self, tool_name: &str) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        if !segment.tools_used.iter().any(|tool| tool == tool_name) {
            segment.tools_used.push(tool_name.to_string());
        }
    }

    /// Records that the model engaged a skill on the active task segment.
    pub fn record_segment_skill_use(&mut self, skill_name: &str) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        if !segment.skills_used.iter().any(|skill| skill == skill_name) {
            segment.skills_used.push(skill_name.to_string());
        }
    }

    /// Records one completed model turn on the active task segment.
    pub fn record_segment_turn_usage(&mut self, token_cost: u64) {
        let Some(segment) = self.current_segment.as_mut() else {
            return;
        };
        segment.turn_count = segment.turn_count.saturating_add(1);
        segment.token_cost = segment.token_cost.saturating_add(token_cost);
    }
}
