# Step 68 — Expand Search Exclusions and Respect .gitignore

_Add Python, Java, Go, and other ecosystem vendored directories to the skip list. Optionally respect .moaignore for project-specific exclusions._

---

## 1. What this step is about

The `file_search` tool has a hardcoded `SKIPPED_SEARCH_DIRS` list that only covers JS/Rust directories (`.git`, `node_modules`, `target`, `.next`, `.turbo`, `dist`, `build`, `.direnv`). The 2026-04-15 e2e test showed the brain using bash+ripgrep to search a Django project, and the first search walked into `server/.venv` (a Python virtual environment with thousands of files), hit the 1,000-match truncation cap, and wasted an entire turn.

Even though the brain used `bash` rather than the built-in `file_search`, expanding the skip list has two benefits: (1) the built-in tool becomes usable for polyglot projects, and (2) the identity prompt (Step 70) can reference these exclusions so the brain knows to skip them when using bash+grep/rg too.

---

## 2. Files to read

- **`moa-hands/src/tools/file_search.rs`** — `SKIPPED_SEARCH_DIRS` constant and `should_skip_search_path` function.
- **`moa-hands/src/local.rs`** — `LocalHandProvider`. Where the workspace root is known and `.moaignore` would be loaded.

---

## 3. Goal

After this step:
1. `SKIPPED_SEARCH_DIRS` covers Python, Java, Go, Ruby, PHP, .NET, and general IDE/cache directories
2. An optional `.moaignore` file in the workspace root can add project-specific exclusions
3. The skip list is exposed as a public constant so the identity prompt (Step 70) can reference it

---

## 4. Rules

- **Keep the hardcoded list.** Do NOT replace it with `.gitignore` parsing via the `ignore` crate yet — that's a larger refactor for later. The hardcoded list is the 80/20 solution.
- **The list must be additive.** No existing entries should be removed.
- **`.moaignore` is optional and simple.** One directory name per line, lines starting with `#` are comments. Loaded once at workspace initialization.
- **The expanded list should also be used in Docker-backed file search.** The `execute_docker` path in `file_search.rs` already filters through `should_skip_search_path` after the Docker find, so it benefits automatically.

---

## 5. Tasks

### 5a. Expand `SKIPPED_SEARCH_DIRS`

Replace the current list with:

```rust
const SKIPPED_SEARCH_DIRS: &[&str] = &[
    // Version control
    ".git",
    ".svn",
    ".hg",
    // JavaScript / TypeScript
    "node_modules",
    ".next",
    ".nuxt",
    ".turbo",
    "dist",
    "build",
    ".output",
    // Rust
    "target",
    // Python
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".eggs",
    // Java / Kotlin
    ".gradle",
    ".mvn",
    // Go / PHP
    "vendor",
    // Ruby
    ".bundle",
    // .NET
    "obj",
    // iOS
    "Pods",
    // IDE / editor
    ".idea",
    ".vscode",
    ".direnv",
    // General caches
    ".cache",
    "coverage",
    "htmlcov",
    ".coverage",
    "__generated__",
];
```

Note: `bin` and `build` are intentionally kept only where already present — `bin/` is too common as a legitimate source directory in many projects.

### 5b. Add `.moaignore` support

Create a function that loads additional exclusions from the workspace root:

```rust
/// Loads additional skip directories from a `.moaignore` file in the workspace root.
pub fn load_moaignore(workspace_root: &Path) -> Vec<String> {
    let moaignore_path = workspace_root.join(".moaignore");
    match std::fs::read_to_string(&moaignore_path) {
        Ok(content) => content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    }
}
```

Make `should_skip_search_path` accept an optional extra list:

```rust
fn should_skip_search_path(path: &Path, extra_skips: &[String]) -> bool {
    path.components().any(|component| match component {
        Component::Normal(segment) => {
            SKIPPED_SEARCH_DIRS.iter().any(|ignored| segment == OsStr::new(ignored))
                || extra_skips.iter().any(|ignored| segment == OsStr::new(ignored.as_str()))
        }
        _ => false,
    })
}
```

Thread the extra skips through `execute()` and `collect_matches()`. The `LocalHandProvider` already knows the workspace root and can load `.moaignore` during provisioning.

### 5c. Make the skip list accessible for the identity prompt

Export the list so `moa-brain` can reference it without duplicating:

```rust
/// Returns the default skipped directory names for documentation/prompt purposes.
pub fn default_skipped_dirs() -> &'static [&'static str] {
    SKIPPED_SEARCH_DIRS
}
```

### 5d. Add tests

```rust
#[test]
fn skips_python_venv_directory() {
    let path = Path::new(".venv/lib/python3.12/site-packages/requests/api.py");
    assert!(should_skip_search_path(path, &[]));
}

#[test]
fn skips_pycache_directory() {
    let path = Path::new("server/core/__pycache__/views.cpython-312.pyc");
    assert!(should_skip_search_path(path, &[]));
}

#[test]
fn skips_custom_moaignore_entry() {
    let path = Path::new("data/fixtures/large-dataset.json");
    let extra = vec!["data".to_string()];
    assert!(should_skip_search_path(path, &extra));
}

#[test]
fn does_not_skip_normal_source_files() {
    let path = Path::new("server/core/views.py");
    assert!(!should_skip_search_path(path, &[]));
}

#[test]
fn skips_gradle_directory() {
    let path = Path::new(".gradle/caches/modules-2/files-2.1/com.google/guava.jar");
    assert!(should_skip_search_path(path, &[]));
}

#[test]
fn skips_vendor_directory() {
    let path = Path::new("vendor/github.com/pkg/errors/errors.go");
    assert!(should_skip_search_path(path, &[]));
}
```

---

## 6. Deliverables

- [ ] `moa-hands/src/tools/file_search.rs` — Expanded `SKIPPED_SEARCH_DIRS`, `.moaignore` loader, updated `should_skip_search_path` signature, public accessor
- [ ] Tests covering Python, Java, Go, custom exclusion, and non-excluded paths

---

## 7. Acceptance criteria

1. `file_search` with pattern `**/*.py` in a workspace containing `.venv/` does not return any `.venv` files.
2. A `.moaignore` file with `data` in the workspace root causes `file_search` to skip the `data/` directory.
3. All existing `file_search` tests still pass.
4. `cargo test -p moa-hands` passes.
