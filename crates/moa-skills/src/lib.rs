//! Skill parsing, registry, distillation, and improvement support.

pub mod distiller;
pub mod format;
pub mod improver;
pub mod lessons;
pub mod registry;
pub mod regression;
pub mod render;

pub use distiller::{maybe_distill_skill, maybe_distill_skill_with_learning};
pub use format::{
    SkillDocument, SkillFrontmatter, build_skill_path, parse_skill_markdown, render_skill_markdown,
    skill_metadata_from_document, slugify_skill_name,
};
pub use improver::{maybe_improve_skill, maybe_improve_skill_with_learning};
pub use lessons::{LessonContext, learn_lesson};
pub use registry::{NewSkill, Skill, SkillRegistry};
pub use regression::{
    SkillEvalRun, SkillRegressionDecision, SkillRegressionReport, SkillRegressionSummary,
    append_skill_regression_log, compare_scores, generate_skill_test_suite, run_skill_regression,
    run_skill_suite,
};
pub use render::{SkillRenderContext, render};
