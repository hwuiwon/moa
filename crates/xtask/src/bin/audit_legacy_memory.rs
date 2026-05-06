//! Guardrail for the removed legacy `moa-memory` crate.

use std::fs;
use std::io;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    match audit() {
        Ok(violations) if violations.is_empty() => ExitCode::SUCCESS,
        Ok(violations) => {
            for violation in violations {
                eprintln!("audit_legacy_memory: {violation}");
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("audit_legacy_memory: {error}");
            ExitCode::FAILURE
        }
    }
}

fn audit() -> io::Result<Vec<String>> {
    let mut violations = Vec::new();

    if Path::new("crates/moa-memory/Cargo.toml").exists() {
        violations
            .push("crates/moa-memory/Cargo.toml exists; legacy crate has reappeared.".to_string());
    }

    let workspace_toml = fs::read_to_string("Cargo.toml")?;
    if contains_legacy_crate_reference(&workspace_toml) {
        violations.push("workspace Cargo.toml references the legacy moa-memory crate.".to_string());
    }

    walk_files(Path::new("crates"), &mut |path| {
        if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml") {
            let body = fs::read_to_string(path)?;
            if contains_legacy_crate_reference(&body) {
                violations.push(format!(
                    "{} references the legacy moa-memory crate.",
                    path.display()
                ));
            }
        }

        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            let body = fs::read_to_string(path)?;
            let path_import = ["use ", "moa_memory", "::"].concat();
            let crate_import = ["use ", "moa_memory", ";"].concat();
            if body.contains(&path_import) || body.contains(&crate_import) {
                violations.push(format!("{} imports moa_memory.", path.display()));
            }
        }

        Ok(())
    })?;

    Ok(violations)
}

fn contains_legacy_crate_reference(body: &str) -> bool {
    body.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed == "\"crates/moa-memory\""
            || trimmed.starts_with("\"crates/moa-memory\",")
            || trimmed.starts_with("moa-memory ")
            || trimmed.starts_with("moa-memory=")
            || trimmed.starts_with("moa-memory\t")
            || trimmed.contains("package = \"moa-memory\"")
    })
}

fn walk_files(root: &Path, visit: &mut impl FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            walk_files(&path, visit)?;
        } else if file_type.is_file() {
            visit(&path)?;
        }
    }

    Ok(())
}
