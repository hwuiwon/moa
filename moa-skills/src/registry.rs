//! Workspace skill registry backed by the memory store.

use std::collections::HashMap;
use std::sync::Arc;

use moa_core::{MemoryStore, MoaError, PageType, Result, SkillMetadata, WorkspaceId};
use tokio::sync::RwLock;

use crate::format::{render_skill_markdown, skill_from_wiki_page, skill_metadata_from_page};

/// In-memory cache of workspace skill metadata and bodies.
pub struct SkillRegistry {
    memory: Arc<dyn MemoryStore>,
    skills: RwLock<HashMap<WorkspaceId, HashMap<String, SkillMetadata>>>,
}

impl SkillRegistry {
    /// Creates a skill registry backed by the provided memory store.
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self {
            memory,
            skills: RwLock::new(HashMap::new()),
        }
    }

    /// Reloads all skills for a workspace into the registry cache.
    pub async fn load(&self, workspace_id: &WorkspaceId) -> Result<()> {
        let summaries = self
            .memory
            .list_pages(
                moa_core::MemoryScope::Workspace(workspace_id.clone()),
                Some(PageType::Skill),
            )
            .await?;
        let mut skills = HashMap::new();

        for summary in summaries {
            let page = self.memory.read_page(&summary.path).await?;
            let metadata = skill_metadata_from_page(summary.path.clone(), &page)?;
            skills.insert(metadata.name.clone(), metadata);
        }

        self.skills
            .write()
            .await
            .insert(workspace_id.clone(), skills);
        Ok(())
    }

    /// Returns workspace skill metadata for Stage 4 pipeline injection.
    pub async fn list_for_pipeline(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<SkillMetadata>> {
        self.ensure_loaded(workspace_id).await?;
        let skills = self.skills.read().await;
        let mut metadata = skills
            .get(workspace_id)
            .cloned()
            .unwrap_or_default()
            .into_values()
            .collect::<Vec<_>>();
        metadata.sort_by(|left, right| {
            right
                .use_count
                .cmp(&left.use_count)
                .then_with(|| left.name.cmp(&right.name))
        });
        Ok(metadata)
    }

    /// Loads the full `SKILL.md` body for a named workspace skill.
    pub async fn load_full(&self, workspace_id: &WorkspaceId, skill_name: &str) -> Result<String> {
        self.ensure_loaded(workspace_id).await?;
        let path = {
            let skills = self.skills.read().await;
            let workspace_skills = skills
                .get(workspace_id)
                .ok_or_else(|| MoaError::WorkspaceNotFound(workspace_id.clone()))?;
            workspace_skills
                .get(skill_name)
                .map(|skill| skill.path.clone())
                .ok_or_else(|| {
                    MoaError::StorageError(format!("skill not found in workspace: {skill_name}"))
                })?
        };
        let page = self.memory.read_page(&path).await?;
        let skill = skill_from_wiki_page(&page)?;
        render_skill_markdown(&skill)
    }

    async fn ensure_loaded(&self, workspace_id: &WorkspaceId) -> Result<()> {
        let needs_load = {
            let skills = self.skills.read().await;
            !skills.contains_key(workspace_id)
        };
        if needs_load {
            self.load(workspace_id).await?;
        }
        Ok(())
    }
}
