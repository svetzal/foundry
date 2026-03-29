use std::path::{Path, PathBuf};

use anyhow::Result;

const SKILL_MD: &str = include_str!("../../../skill/foundry/SKILL.md");
const EVENT_MODEL_MD: &str = include_str!("../../../skill/foundry/references/event-model.md");
const WORKFLOWS_MD: &str = include_str!("../../../skill/foundry/references/workflows.md");

/// Files to install, relative to the skill root directory.
const FILES: &[(&str, &str)] = &[
    ("SKILL.md", SKILL_MD),
    ("references/event-model.md", EVENT_MODEL_MD),
    ("references/workflows.md", WORKFLOWS_MD),
];

/// Write all skill files into the given base directory.
fn install_to(base: &Path) -> Result<()> {
    for (relative_path, content) in FILES {
        let dest = base.join(relative_path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, content)?;
        println!("  {}", dest.display());
    }
    Ok(())
}

pub fn run(global: bool) -> Result<()> {
    let base = if global {
        let home = std::env::var("HOME")?;
        PathBuf::from(home).join(".claude").join("skills").join("foundry")
    } else {
        PathBuf::from(".claude").join("skills").join("foundry")
    };

    install_to(&base)?;

    let location = if global { "global" } else { "local" };
    println!("Foundry skill installed ({location}).");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn installs_all_files_to_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("foundry");

        install_to(&base).unwrap();

        assert!(base.join("SKILL.md").exists());
        assert!(base.join("references/event-model.md").exists());
        assert!(base.join("references/workflows.md").exists());
    }

    #[test]
    fn installed_content_matches_embedded() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("foundry");

        install_to(&base).unwrap();

        let content = std::fs::read_to_string(base.join("SKILL.md")).unwrap();
        assert_eq!(content, SKILL_MD);
    }

    #[test]
    fn overwrites_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("foundry");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("SKILL.md"), "old content").unwrap();

        install_to(&base).unwrap();

        let content = std::fs::read_to_string(base.join("SKILL.md")).unwrap();
        assert!(content.starts_with("---"), "should overwrite with embedded content");
        assert!(!content.contains("old content"));
    }
}
