use std::path::{Path, PathBuf};

use anyhow::Result;
use semver::Version;
use serde::Serialize;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const SKILL_MD: &str = include_str!("../../../skill/foundry/SKILL.md");
const EVENT_MODEL_MD: &str = include_str!("../../../skill/foundry/references/event-model.md");
const WORKFLOWS_MD: &str = include_str!("../../../skill/foundry/references/workflows.md");

/// Files to install, relative to the skill root directory.
const FILES: &[(&str, &str)] = &[
    ("SKILL.md", SKILL_MD),
    ("references/event-model.md", EVENT_MODEL_MD),
    ("references/workflows.md", WORKFLOWS_MD),
];

/// The outcome for a single installed file.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Created,
    Updated,
    UpToDate,
    Skipped,
}

impl Action {
    const fn icon(&self) -> char {
        match self {
            Self::Created => '+',
            Self::Updated => '~',
            Self::UpToDate => '=',
            Self::Skipped => '!',
        }
    }

    const fn label(&self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Updated => "Updated",
            Self::UpToDate => "Up to date",
            Self::Skipped => "Skipped",
        }
    }
}

#[derive(Serialize)]
struct FileResult {
    path: String,
    action: Action,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Serialize)]
struct JsonOutput {
    success: bool,
    message: String,
    version: String,
    files: Vec<FileResult>,
}

/// Inject `foundry-version: <version>` into YAML frontmatter, and update
/// `metadata.version` if present. If the body has no frontmatter, prepend
/// a minimal one.
fn stamp(body: &str, version: &str) -> String {
    let Some(rest) = body.strip_prefix("---\n") else {
        return format!("---\nfoundry-version: {version}\n---\n\n{body}");
    };

    let (frontmatter, after) = if let Some(pos) = rest.find("\n---\n") {
        (&rest[..pos], &rest[(pos + 5)..])
    } else if let Some(stripped) = rest.strip_suffix("\n---") {
        (stripped, "")
    } else {
        // Malformed frontmatter — treat as no frontmatter.
        return format!("---\nfoundry-version: {version}\n---\n\n{body}");
    };

    let stamped_fm = stamp_frontmatter(frontmatter, version);
    format!("---\n{stamped_fm}\n---\n{after}")
}

/// Inject/replace `foundry-version:` in a frontmatter block (the text between
/// the `---` delimiters, without the delimiters themselves). Also updates
/// `metadata.version` if present.
fn stamp_frontmatter(frontmatter: &str, version: &str) -> String {
    let mut lines: Vec<String> = frontmatter.lines().map(String::from).collect();
    let mut in_metadata = false;
    let mut metadata_version_updated = false;

    for line in &mut lines {
        if line.as_str() == "metadata:" {
            in_metadata = true;
            continue;
        }
        if in_metadata {
            if line.starts_with("  ") {
                if !metadata_version_updated && line.trim_start().starts_with("version:") {
                    *line = format!("  version: \"{version}\"");
                    metadata_version_updated = true;
                }
            } else if !line.is_empty() {
                in_metadata = false;
            }
        }
    }

    let version_line = format!("foundry-version: {version}");
    if let Some(pos) = lines.iter().position(|l| l.starts_with("foundry-version:")) {
        lines[pos] = version_line;
    } else {
        lines.push(version_line);
    }

    lines.join("\n")
}

/// Parse `foundry-version: X.Y.Z` from the leading frontmatter block.
/// Returns `None` if absent, unparseable, or there is no frontmatter.
fn parse_stamp(content: &str) -> Option<Version> {
    let rest = content.strip_prefix("---\n")?;

    let frontmatter = if let Some(pos) = rest.find("\n---\n") {
        &rest[..pos]
    } else if let Some(stripped) = rest.strip_suffix("\n---") {
        stripped
    } else {
        return None;
    };

    for line in frontmatter.lines() {
        if let Some(v) = line.strip_prefix("foundry-version:") {
            return Version::parse(v.trim()).ok();
        }
    }
    None
}

/// Determine the action for a single file without performing any I/O.
///
/// Returns `(Action, Option<warning_message>)`.
fn decide_action(
    on_disk: Option<&str>,
    stamped_content: &str,
    force: bool,
    binary_version: &Version,
) -> (Action, Option<String>) {
    let Some(existing) = on_disk else {
        return (Action::Created, None);
    };

    if let Some(installed_ver) = parse_stamp(existing) {
        if &installed_ver > binary_version {
            if force {
                let warning =
                    format!("Downgrading from v{installed_ver} to v{binary_version} (--force).");
                return (Action::Updated, Some(warning));
            }
            let warning = format!(
                "Installed v{installed_ver} is newer than binary v{binary_version}. Use --force to downgrade."
            );
            return (Action::Skipped, Some(warning));
        }
    }

    if existing == stamped_content {
        (Action::UpToDate, None)
    } else {
        (Action::Updated, None)
    }
}

/// Install all skill files into `base`, returning per-file results. Performs
/// the actual file I/O but does not print any output.
fn install_files(base: &Path, force: bool, binary_version: &Version) -> Result<Vec<FileResult>> {
    let mut results = Vec::new();

    for (relative_path, embedded) in FILES {
        let dest = base.join(relative_path);
        let version_str = binary_version.to_string();
        let stamped = stamp(embedded, &version_str);

        let existing_content = std::fs::read_to_string(&dest).ok();
        let installed_version =
            existing_content.as_deref().and_then(parse_stamp).map(|v| v.to_string());

        let (action, warning) =
            decide_action(existing_content.as_deref(), &stamped, force, binary_version);

        if matches!(action, Action::Created | Action::Updated) {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &stamped)?;
        }

        results.push(FileResult {
            path: (*relative_path).to_string(),
            action,
            installed_version,
            warning,
        });
    }

    Ok(results)
}

pub fn run(global: bool, force: bool, json: bool) -> Result<()> {
    let base = if global {
        let home = std::env::var("HOME")?;
        PathBuf::from(home).join(".claude").join("skills").join("foundry")
    } else {
        PathBuf::from(".claude").join("skills").join("foundry")
    };

    let binary_version = Version::parse(VERSION).expect("CARGO_PKG_VERSION is valid semver");
    let results = install_files(&base, force, &binary_version)?;

    let any_skipped = results.iter().any(|r| r.action == Action::Skipped);

    let created = results.iter().filter(|r| r.action == Action::Created).count();
    let updated = results.iter().filter(|r| r.action == Action::Updated).count();
    let up_to_date = results.iter().filter(|r| r.action == Action::UpToDate).count();
    let skipped = results.iter().filter(|r| r.action == Action::Skipped).count();

    let summary = format!(
        "Installed foundry skill (v{VERSION}): {created} created, {updated} updated, \
         {up_to_date} up to date, {skipped} skipped"
    );

    if json {
        let output = JsonOutput {
            success: !any_skipped,
            message: summary,
            version: VERSION.to_string(),
            files: results,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        for file in &results {
            if let Some(ref w) = file.warning {
                eprintln!("  warning: {w}");
            }
            println!("  {} {}: {}", file.action.icon(), file.action.label(), file.path);
        }
        println!("\n{summary}.");
    }

    if any_skipped {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Embedded content sanity ─────────────────────────────────────────────

    #[test]
    fn embedded_files_are_not_empty() {
        assert!(!SKILL_MD.is_empty(), "SKILL.md should not be empty");
        assert!(!EVENT_MODEL_MD.is_empty(), "event-model.md should not be empty");
        assert!(!WORKFLOWS_MD.is_empty(), "workflows.md should not be empty");
    }

    #[test]
    fn skill_md_has_frontmatter() {
        assert!(SKILL_MD.starts_with("---"), "SKILL.md should start with YAML frontmatter");
    }

    // ── stamp() unit tests ──────────────────────────────────────────────────

    #[test]
    fn stamp_injects_into_existing_frontmatter() {
        let result = stamp(SKILL_MD, "1.2.3");
        assert!(result.starts_with("---\n"), "should preserve frontmatter opening");
        assert!(
            result.contains("foundry-version: 1.2.3"),
            "should contain version stamp; got:\n{result}"
        );
    }

    #[test]
    fn stamp_prepends_frontmatter_when_absent() {
        let body = "# Some content\nHello world\n";
        let result = stamp(body, "1.2.3");
        assert!(
            result.starts_with("---\nfoundry-version: 1.2.3\n---\n\n"),
            "should prepend minimal frontmatter; got:\n{result}"
        );
        assert!(result.contains("# Some content"), "should preserve body");
    }

    #[test]
    fn stamp_updates_metadata_version_if_present() {
        let result = stamp(SKILL_MD, "5.6.7");
        assert!(
            result.contains("  version: \"5.6.7\""),
            "should update metadata.version; got:\n{result}"
        );
    }

    #[test]
    fn stamp_replaces_existing_foundry_version_on_restamp() {
        // Second stamp call should replace, not append.
        let first = stamp(SKILL_MD, "1.0.0");
        let second = stamp(&first, "2.0.0");
        let occurrences = second.matches("foundry-version:").count();
        assert_eq!(occurrences, 1, "should have exactly one foundry-version line; got:\n{second}");
        assert!(second.contains("foundry-version: 2.0.0"));
        assert!(!second.contains("foundry-version: 1.0.0"));
    }

    // ── parse_stamp() unit tests ────────────────────────────────────────────

    #[test]
    fn parse_stamp_returns_none_when_absent() {
        assert!(parse_stamp("# No frontmatter\nHello").is_none());
        assert!(parse_stamp("---\nname: test\n---\nBody").is_none());
    }

    #[test]
    fn parse_stamp_returns_version_when_present() {
        let content = "---\nname: test\nfoundry-version: 3.4.5\n---\nBody";
        let ver = parse_stamp(content).expect("should parse version");
        assert_eq!(ver, Version::parse("3.4.5").unwrap());
    }

    // ── decide_action() unit tests ──────────────────────────────────────────

    #[test]
    fn absent_file_is_created() {
        let ver = Version::parse("0.11.0").unwrap();
        let stamped = stamp(SKILL_MD, "0.11.0");
        let (action, warning) = decide_action(None, &stamped, false, &ver);
        assert_eq!(action, Action::Created);
        assert!(warning.is_none());
    }

    #[test]
    fn identical_content_is_up_to_date() {
        let ver = Version::parse("0.11.0").unwrap();
        let stamped = stamp(SKILL_MD, "0.11.0");
        let (action, warning) = decide_action(Some(&stamped), &stamped, false, &ver);
        assert_eq!(action, Action::UpToDate);
        assert!(warning.is_none());
    }

    #[test]
    fn older_installed_is_updated() {
        let ver = Version::parse("0.11.0").unwrap();
        let old_content = stamp(SKILL_MD, "0.1.0");
        let new_stamped = stamp(SKILL_MD, "0.11.0");
        let (action, warning) = decide_action(Some(&old_content), &new_stamped, false, &ver);
        assert_eq!(action, Action::Updated);
        assert!(warning.is_none());
    }

    #[test]
    fn newer_installed_is_skipped_without_force() {
        let ver = Version::parse("0.11.0").unwrap();
        let newer_content = stamp(SKILL_MD, "99.0.0");
        let current_stamped = stamp(SKILL_MD, "0.11.0");
        let (action, warning) = decide_action(Some(&newer_content), &current_stamped, false, &ver);
        assert_eq!(action, Action::Skipped);
        let w = warning.expect("should have a warning");
        assert!(
            w.contains("Use --force to downgrade"),
            "warning should mention --force; got: {w}"
        );
    }

    #[test]
    fn newer_installed_with_force_is_updated() {
        let ver = Version::parse("0.11.0").unwrap();
        let newer_content = stamp(SKILL_MD, "99.0.0");
        let current_stamped = stamp(SKILL_MD, "0.11.0");
        let (action, warning) = decide_action(Some(&newer_content), &current_stamped, true, &ver);
        assert_eq!(action, Action::Updated);
        let w = warning.expect("should have a downgrade warning even with --force");
        assert!(w.contains("Downgrading"), "warning should mention downgrade; got: {w}");
    }

    // ── install_files() integration tests ───────────────────────────────────

    #[test]
    fn installs_all_files_to_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("foundry");
        let ver = Version::parse(VERSION).unwrap();

        install_files(&base, false, &ver).unwrap();

        assert!(base.join("SKILL.md").exists());
        assert!(base.join("references/event-model.md").exists());
        assert!(base.join("references/workflows.md").exists());
    }

    #[test]
    fn installed_skill_md_contains_version_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("foundry");
        let ver = Version::parse(VERSION).unwrap();

        install_files(&base, false, &ver).unwrap();

        let content = std::fs::read_to_string(base.join("SKILL.md")).unwrap();
        assert!(
            content.contains(&format!("foundry-version: {VERSION}")),
            "installed SKILL.md should contain version stamp"
        );
    }

    // ── JSON output shape test ───────────────────────────────────────────────

    #[test]
    fn json_output_shape() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("foundry");
        let ver = Version::parse(VERSION).unwrap();
        let results = install_files(&base, false, &ver).unwrap();

        let output = JsonOutput {
            success: true,
            message: "test message".to_string(),
            version: VERSION.to_string(),
            files: results,
        };
        let json_str = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert!(parsed["success"].is_boolean(), "success should be boolean");
        assert!(parsed["message"].is_string(), "message should be string");
        assert!(parsed["version"].is_string(), "version should be string");
        assert!(parsed["files"].is_array(), "files should be array");

        let files = parsed["files"].as_array().unwrap();
        assert!(!files.is_empty(), "files should not be empty");
        assert!(files[0]["path"].is_string(), "file.path should be string");
        assert!(files[0]["action"].is_string(), "file.action should be string");
    }
}
