//! Skill package parsing, registry, rendering, and optional learning support.

#![recursion_limit = "256"]

#[cfg(feature = "skill-learning")]
pub mod distiller;
pub mod format;
#[cfg(feature = "skill-learning")]
pub mod improver;
pub mod lessons;
pub mod package;
pub mod registry;
#[cfg(feature = "skill-learning")]
pub mod regression;
pub mod render;

#[cfg(feature = "skill-learning")]
pub use distiller::{
    DistillationOutcome, DistillationSkipReason, maybe_distill_skill,
    maybe_distill_skill_with_learning,
};
pub use format::{
    SkillDocument, SkillFrontmatter, build_skill_path, parse_skill_markdown, render_skill_markdown,
    skill_metadata_from_document, slugify_skill_name,
};
#[cfg(feature = "skill-learning")]
pub use improver::{ImprovementResult, maybe_improve_skill, maybe_improve_skill_with_learning};
pub use lessons::{LessonContext, learn_lesson};
pub use package::{
    MAX_SKILL_PACKAGE_FILES, SkillPackage, SkillPackageFile, SkillPackageManifest,
    SkillPackageManifestFile, ValidatedSkillPackage, ValidatedSkillPackageFile,
};
pub use registry::{NewSkill, Skill, SkillRegistry, StoredSkillPackage};
#[cfg(feature = "internal-eval-runner")]
pub use regression::{SkillEvalRun, run_skill_regression, run_skill_suite};
#[cfg(feature = "skill-learning")]
pub use regression::{
    SkillRegressionDecision, SkillRegressionReport, SkillRegressionSummary,
    append_skill_regression_log, compare_scores, generate_skill_test_suite,
};
pub use render::{SkillRenderContext, render};
