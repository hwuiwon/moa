//! Skill parsing, registry, distillation, and improvement support.

pub mod distiller;
pub mod format;
pub mod improver;
pub mod registry;

pub use distiller::maybe_distill_skill;
pub use format::{
    SkillDocument, SkillFrontmatter, SkillMoaFrontmatter, build_skill_path, parse_skill_markdown,
    render_skill_markdown, skill_from_wiki_page, skill_metadata_from_document,
    skill_metadata_from_page, slugify_skill_name, wiki_page_from_skill,
};
pub use improver::maybe_improve_skill;
pub use registry::SkillRegistry;
