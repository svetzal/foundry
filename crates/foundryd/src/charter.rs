use std::path::Path;

/// Result of a charter validation check.
#[derive(Debug, Clone)]
pub struct CharterResult {
    /// Whether at least one source with sufficient content was found.
    pub passed: bool,
    /// Files that qualified as charter sources.
    pub sources: Vec<String>,
    /// Guidance message when the check fails.
    pub guidance: Option<String>,
}

const MIN_CONTENT_LENGTH: usize = 100;

/// Check a project directory for intent documentation.
///
/// Looks for (in order of preference):
/// 1. `CHARTER.md` (>= 100 chars)
/// 2. `CLAUDE.md` containing a `## Project Charter` section (>= 100 chars in that section)
/// 3. `README.md` (>= 100 chars)
/// 4. Package description: `package.json` "description", `Cargo.toml` `[package]` description,
///    `pyproject.toml` `[project]` description, `mix.exs` `@moduledoc`
pub fn check_charter(project_dir: &Path) -> CharterResult {
    let mut sources = Vec::new();

    // 1. CHARTER.md
    if check_file_length(project_dir, "CHARTER.md") {
        sources.push("CHARTER.md".to_string());
    }

    // 2. CLAUDE.md with "## Project Charter" section
    if check_claude_md_charter(project_dir) {
        sources.push("CLAUDE.md".to_string());
    }

    // 3. README.md
    if check_file_length(project_dir, "README.md") {
        sources.push("README.md".to_string());
    }

    // 4. Package descriptions
    if let Some(pkg_source) = check_package_description(project_dir) {
        sources.push(pkg_source);
    }

    if sources.is_empty() {
        CharterResult {
            passed: false,
            sources,
            guidance: Some(
                "Create a CHARTER.md (>=100 chars) describing the project's purpose and intent"
                    .to_string(),
            ),
        }
    } else {
        CharterResult {
            passed: true,
            sources,
            guidance: None,
        }
    }
}

/// Check if a file exists and has at least `MIN_CONTENT_LENGTH` characters.
fn check_file_length(dir: &Path, filename: &str) -> bool {
    let path = dir.join(filename);
    std::fs::read_to_string(path)
        .map(|content| content.trim().len() >= MIN_CONTENT_LENGTH)
        .unwrap_or(false)
}

/// Check if `CLAUDE.md` contains a `## Project Charter` section with sufficient content.
fn check_claude_md_charter(dir: &Path) -> bool {
    let path = dir.join("CLAUDE.md");
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };

    // Find "## Project Charter" heading and extract everything until the next ## heading
    let Some(start) = content.find("## Project Charter") else {
        return false;
    };

    let section_start = start + "## Project Charter".len();
    let section_content = if let Some(next_heading) = content[section_start..].find("\n## ") {
        &content[section_start..section_start + next_heading]
    } else {
        &content[section_start..]
    };

    section_content.trim().len() >= MIN_CONTENT_LENGTH
}

/// Check package manifest files for a description field.
fn check_package_description(dir: &Path) -> Option<String> {
    // package.json "description"
    if let Some(desc) = read_json_description(dir, "package.json") {
        if desc.trim().len() >= MIN_CONTENT_LENGTH {
            return Some("package.json".to_string());
        }
    }

    // Cargo.toml [package] description
    if let Some(desc) = read_cargo_description(dir) {
        if desc.trim().len() >= MIN_CONTENT_LENGTH {
            return Some("Cargo.toml".to_string());
        }
    }

    // pyproject.toml [project] description
    if let Some(desc) = read_pyproject_description(dir) {
        if desc.trim().len() >= MIN_CONTENT_LENGTH {
            return Some("pyproject.toml".to_string());
        }
    }

    // mix.exs @moduledoc
    if let Some(desc) = read_mix_moduledoc(dir) {
        if desc.trim().len() >= MIN_CONTENT_LENGTH {
            return Some("mix.exs".to_string());
        }
    }

    None
}

fn read_json_description(dir: &Path, filename: &str) -> Option<String> {
    let content = std::fs::read_to_string(dir.join(filename)).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&content).ok()?;
    parsed.get("description")?.as_str().map(String::from)
}

fn read_cargo_description(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    // Simple line-based parsing — look for description = "..." in [package] section
    let mut in_package = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[package]" {
            in_package = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_package = false;
            continue;
        }
        if in_package {
            if let Some(rest) = trimmed.strip_prefix("description") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    let val = rest.trim().trim_matches('"');
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn read_pyproject_description(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("pyproject.toml")).ok()?;
    let mut in_project = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[project]" {
            in_project = true;
            continue;
        }
        if trimmed.starts_with('[') {
            in_project = false;
            continue;
        }
        if in_project {
            if let Some(rest) = trimmed.strip_prefix("description") {
                let rest = rest.trim_start();
                if let Some(rest) = rest.strip_prefix('=') {
                    let val = rest.trim().trim_matches('"');
                    return Some(val.to_string());
                }
            }
        }
    }
    None
}

fn read_mix_moduledoc(dir: &Path) -> Option<String> {
    let content = std::fs::read_to_string(dir.join("mix.exs")).ok()?;
    // Look for @moduledoc """ ... """ or @moduledoc "..."
    let idx = content.find("@moduledoc")?;
    let after = content[idx + "@moduledoc".len()..].trim_start();

    if after.starts_with("\"\"\"") {
        // Multi-line heredoc
        let start = 3; // skip opening """
        let end = after[start..].find("\"\"\"")?;
        Some(after[start..start + end].to_string())
    } else if after.starts_with('"') {
        // Single-line string
        let start = 1;
        let end = after[start..].find('"')?;
        Some(after[start..start + end].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_content(prefix: &str) -> String {
        format!("{prefix}{}", "x".repeat(MIN_CONTENT_LENGTH))
    }

    #[test]
    fn charter_md_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), long_content("# Charter\n")).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"CHARTER.md".to_string()));
        assert!(result.guidance.is_none());
    }

    #[test]
    fn short_readme_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "short").unwrap();

        let result = check_charter(dir.path());

        assert!(!result.passed);
        assert!(result.sources.is_empty());
        assert!(result.guidance.is_some());
    }

    #[test]
    fn long_readme_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), long_content("# README\n")).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"README.md".to_string()));
    }

    #[test]
    fn claude_md_with_charter_section_passes() {
        let dir = tempfile::tempdir().unwrap();
        let content =
            format!("# CLAUDE.md\n\n## Project Charter\n\n{}", "x".repeat(MIN_CONTENT_LENGTH));
        std::fs::write(dir.path().join("CLAUDE.md"), content).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"CLAUDE.md".to_string()));
    }

    #[test]
    fn claude_md_without_charter_section_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("CLAUDE.md"),
            long_content("# CLAUDE.md\n\nSome instructions\n"),
        )
        .unwrap();

        let result = check_charter(dir.path());

        // CLAUDE.md without a ## Project Charter section does not qualify
        assert!(!result.passed);
    }

    #[test]
    fn claude_md_with_short_charter_section_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        let content = "# CLAUDE.md\n\n## Project Charter\n\nToo short\n\n## Other Section\n\nStuff";
        std::fs::write(dir.path().join("CLAUDE.md"), content).unwrap();

        let result = check_charter(dir.path());

        assert!(!result.sources.contains(&"CLAUDE.md".to_string()));
    }

    #[test]
    fn empty_directory_fails() {
        let dir = tempfile::tempdir().unwrap();

        let result = check_charter(dir.path());

        assert!(!result.passed);
        assert!(result.sources.is_empty());
        assert!(result.guidance.is_some());
        assert!(result.guidance.unwrap().contains("CHARTER.md"));
    }

    #[test]
    fn multiple_sources_all_collected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), long_content("")).unwrap();
        std::fs::write(dir.path().join("README.md"), long_content("")).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert_eq!(result.sources.len(), 2);
        assert!(result.sources.contains(&"CHARTER.md".to_string()));
        assert!(result.sources.contains(&"README.md".to_string()));
    }

    #[test]
    fn package_json_description_passes() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = serde_json::json!({
            "name": "my-project",
            "description": "x".repeat(MIN_CONTENT_LENGTH),
        });
        std::fs::write(dir.path().join("package.json"), pkg.to_string()).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"package.json".to_string()));
    }

    #[test]
    fn cargo_toml_description_passes() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[package]\nname = \"my-project\"\ndescription = \"{}\"\n",
            "x".repeat(MIN_CONTENT_LENGTH)
        );
        std::fs::write(dir.path().join("Cargo.toml"), toml).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn pyproject_toml_description_passes() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[project]\nname = \"my-project\"\ndescription = \"{}\"\n",
            "x".repeat(MIN_CONTENT_LENGTH)
        );
        std::fs::write(dir.path().join("pyproject.toml"), toml).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"pyproject.toml".to_string()));
    }

    #[test]
    fn mix_exs_moduledoc_passes() {
        let dir = tempfile::tempdir().unwrap();
        let mix = format!(
            "defmodule MyProject.MixProject do\n  @moduledoc \"\"\"\n  {}\n  \"\"\"\nend\n",
            "x".repeat(MIN_CONTENT_LENGTH)
        );
        std::fs::write(dir.path().join("mix.exs"), mix).unwrap();

        let result = check_charter(dir.path());

        assert!(result.passed);
        assert!(result.sources.contains(&"mix.exs".to_string()));
    }
}
