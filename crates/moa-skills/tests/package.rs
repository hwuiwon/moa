//! Tests for skill package validation invariants.

use moa_skills::{MAX_SKILL_PACKAGE_FILES, SkillPackage, SkillPackageFile};

const VALID_SKILL: &str = r#"---
name: package-skill
description: "Skill package fixture"
allowed-tools: bash file_read
metadata:
  moa-tags: "package, fixture"
  moa-use-count: "3"
  moa-success-rate: "0.75"
  moa-estimated-tokens: "120"
---

# Package Skill

Run the helper script when needed.
"#;

#[test]
fn validates_required_skill_md_and_deterministic_file_manifest() {
    // Pins: package validation sorts files and hashes the package independent of input order.
    let first = SkillPackage::new(vec![
        SkillPackageFile::new("scripts/run.py", b"print('ok')".to_vec()).with_executable(true),
        SkillPackageFile::new("SKILL.md", VALID_SKILL.as_bytes().to_vec())
            .with_content_type("text/markdown; charset=utf-8"),
    ])
    .validate()
    .expect("valid package");
    let second = SkillPackage::new(vec![
        SkillPackageFile::new("SKILL.md", VALID_SKILL.as_bytes().to_vec())
            .with_content_type("text/markdown; charset=utf-8"),
        SkillPackageFile::new("scripts/run.py", b"print('ok')".to_vec()).with_executable(true),
    ])
    .validate()
    .expect("valid package with different input order");

    assert_eq!(first.name, "package-skill");
    assert_eq!(first.description, "Skill package fixture");
    assert_eq!(first.tags, vec!["package", "fixture"]);
    assert_eq!(first.file_count, 2);
    assert_eq!(
        first
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["SKILL.md", "scripts/run.py"]
    );
    assert_eq!(first.package_hash, second.package_hash);
    assert_eq!(first.manifest.skill_md_estimated_tokens, 120);
    assert_eq!(first.manifest.allowed_tools, vec!["bash", "file_read"]);
    assert_eq!(first.manifest.use_count, 3);
    assert_eq!(first.manifest.success_rate, 0.75);
    assert!(first.manifest.files[1].executable);
}

#[test]
fn package_hash_changes_when_content_type_changes() {
    // Pins: persisted package metadata changes are version-significant.
    let markdown = SkillPackageFile::new("SKILL.md", VALID_SKILL.as_bytes().to_vec())
        .with_content_type("text/markdown; charset=utf-8");
    let shell_script =
        SkillPackageFile::new("scripts/run.sh", b"printf ok\n".to_vec()).with_executable(true);
    let typed = SkillPackage::new(vec![
        markdown.clone(),
        shell_script.clone().with_content_type("text/x-shellscript"),
    ])
    .validate()
    .expect("typed package should be valid");
    let untyped = SkillPackage::new(vec![markdown, shell_script])
        .validate()
        .expect("untyped package should be valid");

    assert_ne!(typed.package_hash, untyped.package_hash);
}

#[test]
fn rejects_packages_without_root_skill_md() {
    // Pins: packages without a root SKILL.md are rejected before persistence.
    let error = SkillPackage::new(vec![SkillPackageFile::new(
        "docs/SKILL.md",
        VALID_SKILL.as_bytes().to_vec(),
    )])
    .validate()
    .expect_err("missing root SKILL.md should fail");

    assert!(error.to_string().contains("root SKILL.md"));
}

#[test]
fn rejects_unsafe_package_paths() {
    // Pins: package paths cannot escape the sandbox materialization directory.
    for path in [
        "../SKILL.md",
        "/SKILL.md",
        "scripts/../run.py",
        "scripts\\run.py",
    ] {
        let error = SkillPackage::new(vec![
            SkillPackageFile::new("SKILL.md", VALID_SKILL.as_bytes().to_vec()),
            SkillPackageFile::new(path, b"bad".to_vec()),
        ])
        .validate()
        .expect_err("unsafe path should fail");

        assert!(
            error.to_string().contains("invalid segment")
                || error.to_string().contains("POSIX relative path")
        );
    }
}

#[test]
fn rejects_non_utf8_skill_md() {
    // Pins: SKILL.md must be UTF-8 so frontmatter can be parsed deterministically.
    let error = SkillPackage::new(vec![SkillPackageFile::new("SKILL.md", vec![0xff, 0xfe])])
        .validate()
        .expect_err("non-UTF-8 SKILL.md should fail");

    assert!(error.to_string().contains("UTF-8"));
}

#[test]
fn rejects_too_many_package_files() {
    // Pins: packages cannot use many tiny files to bypass package write-cost limits.
    let mut files = vec![SkillPackageFile::new(
        "SKILL.md",
        VALID_SKILL.as_bytes().to_vec(),
    )];
    files.extend(
        (0..MAX_SKILL_PACKAGE_FILES)
            .map(|index| SkillPackageFile::new(format!("refs/{index}.txt"), b"x".to_vec())),
    );

    let error = SkillPackage::new(files)
        .validate()
        .expect_err("package over the file-count cap should fail");

    assert!(error.to_string().contains("files"));
}
