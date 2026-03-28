use std::path::Path;

/// Result of validating a project's charter (intent documentation).
#[derive(Debug, Clone)]
pub struct CharterResult {
    /// Whether the project passed charter validation.
    pub passed: bool,
    /// Charter sources that were found (e.g., "CHARTER.md", "README.md").
    pub sources: Vec<String>,
    /// Human-readable guidance when validation fails.
    pub guidance: String,
}

/// Minimum content length (in characters) for a charter source to count.
const MIN_CONTENT_LENGTH: usize = 100;

/// Validate that a project has sufficient intent documentation.
///
/// Checks for any of:
/// - `CHARTER.md` (>= 100 chars)
/// - `CLAUDE.md` containing a `## Project Charter` section (>= 100 chars in that section)
/// - `README.md` (>= 100 chars)
/// - Package description files (`Cargo.toml` with `[package]` description,
///   `package.json` with `"description"`)
///
/// Returns [`CharterResult`] with `passed = true` if at least one source meets the threshold.
pub fn check_charter(project_dir: &Path) -> CharterResult {
    let mut sources = Vec::new();

    // Check CHARTER.md
    let charter_path = project_dir.join("CHARTER.md");
    if let Ok(content) = std::fs::read_to_string(&charter_path) {
        if content.trim().len() >= MIN_CONTENT_LENGTH {
            sources.push("CHARTER.md".to_string());
        }
    }

    // Check CLAUDE.md for "## Project Charter" section
    let claude_path = project_dir.join("CLAUDE.md");
    if let Ok(content) = std::fs::read_to_string(&claude_path) {
        if let Some(section) = extract_section(&content, "## Project Charter") {
            if section.trim().len() >= MIN_CONTENT_LENGTH {
                sources.push("CLAUDE.md (Project Charter section)".to_string());
            }
        }
    }

    // Check README.md
    let readme_path = project_dir.join("README.md");
    if let Ok(content) = std::fs::read_to_string(&readme_path) {
        if content.trim().len() >= MIN_CONTENT_LENGTH {
            sources.push("README.md".to_string());
        }
    }

    // Check Cargo.toml description
    let cargo_path = project_dir.join("Cargo.toml");
    if let Ok(content) = std::fs::read_to_string(&cargo_path) {
        if content.contains("[package]") && content.contains("description") {
            sources.push("Cargo.toml (package description)".to_string());
        }
    }

    // Check package.json description
    let pkg_path = project_dir.join("package.json");
    if let Ok(content) = std::fs::read_to_string(&pkg_path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if json
                .get("description")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|d| !d.is_empty())
            {
                sources.push("package.json (description)".to_string());
            }
        }
    }

    let passed = !sources.is_empty();
    let guidance = if passed {
        format!("Charter validated from: {}", sources.join(", "))
    } else {
        "No charter sources found. Create a CHARTER.md (>= 100 chars), \
         add a '## Project Charter' section to CLAUDE.md, \
         or ensure README.md has substantial content."
            .to_string()
    };

    CharterResult {
        passed,
        sources,
        guidance,
    }
}

/// Extract a markdown section starting with the given heading.
/// Returns the content from the heading to the next heading of equal or higher level, or EOF.
fn extract_section(content: &str, heading: &str) -> Option<String> {
    let heading_level = heading.chars().take_while(|c| *c == '#').count();
    let start = content.find(heading)?;
    let after_heading = start + heading.len();
    let rest = &content[after_heading..];

    // Find the next heading of equal or higher level
    let end = rest
        .lines()
        .skip(1) // skip the heading line itself
        .position(|line| {
            let trimmed = line.trim_start();
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            level > 0 && level <= heading_level
        });

    match end {
        Some(pos) => {
            // Count bytes up to that line
            let byte_end: usize = rest.lines().skip(1).take(pos).map(|l| l.len() + 1).sum();
            Some(rest[..byte_end].to_string())
        }
        None => Some(rest.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_with_charter_md() {
        let dir = tempfile::tempdir().unwrap();
        let content = "a".repeat(100);
        std::fs::write(dir.path().join("CHARTER.md"), &content).unwrap();

        let result = check_charter(dir.path());
        assert!(result.passed);
        assert!(result.sources.iter().any(|s| s == "CHARTER.md"));
    }

    #[test]
    fn fails_with_short_charter_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CHARTER.md"), "too short").unwrap();

        let result = check_charter(dir.path());
        assert!(!result.passed);
    }

    #[test]
    fn passes_with_readme_md() {
        let dir = tempfile::tempdir().unwrap();
        let content = "b".repeat(150);
        std::fs::write(dir.path().join("README.md"), &content).unwrap();

        let result = check_charter(dir.path());
        assert!(result.passed);
        assert!(result.sources.iter().any(|s| s == "README.md"));
    }

    #[test]
    fn passes_with_claude_md_charter_section() {
        let dir = tempfile::tempdir().unwrap();
        let content = format!(
            "# My Project\n\nSome text.\n\n## Project Charter\n\n{}\n\n## Other Section\n",
            "c".repeat(120)
        );
        std::fs::write(dir.path().join("CLAUDE.md"), &content).unwrap();

        let result = check_charter(dir.path());
        assert!(result.passed);
        assert!(result.sources.iter().any(|s| s.contains("CLAUDE.md")));
    }

    #[test]
    fn fails_with_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let result = check_charter(dir.path());
        assert!(!result.passed);
        assert!(result.guidance.contains("No charter sources found"));
    }

    #[test]
    fn passes_with_cargo_toml_description() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"\ndescription = \"A test project\"\n",
        )
        .unwrap();

        let result = check_charter(dir.path());
        assert!(result.passed);
        assert!(result.sources.iter().any(|s| s.contains("Cargo.toml")));
    }

    #[test]
    fn passes_with_package_json_description() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name": "test", "description": "A test project"}"#,
        )
        .unwrap();

        let result = check_charter(dir.path());
        assert!(result.passed);
        assert!(result.sources.iter().any(|s| s.contains("package.json")));
    }

    #[test]
    fn extract_section_returns_content_between_headings() {
        let content = "# Top\n\nIntro\n\n## Project Charter\n\nCharter content here.\n\n## Other\n\nOther content.";
        let section = extract_section(content, "## Project Charter").unwrap();
        assert!(section.contains("Charter content here."));
        assert!(!section.contains("Other content."));
    }

    #[test]
    fn extract_section_returns_none_when_missing() {
        let content = "# Top\n\nNo charter here.";
        assert!(extract_section(content, "## Project Charter").is_none());
    }
}
