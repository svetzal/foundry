use anyhow::{Result, bail};
use cmx_core::context::AppContext;
use cmx_core::production::ProductionContext;
use cmx_core::skill_fs::SkillFile;
use cmx_core::skill_install::{
    BundledSkill, InstallPlan, RemoveReport, Report, Scope, SkillInstaller, TargetAction,
    ToolIdentity,
};
use serde::Serialize;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const SKILL_MD: &str = include_str!("../../../skill/foundry/SKILL.md");
const EVENT_MODEL_MD: &str = include_str!("../../../skill/foundry/references/event-model.md");
const WORKFLOWS_MD: &str = include_str!("../../../skill/foundry/references/workflows.md");

fn scope_from_flags(local: bool) -> Scope {
    if local { Scope::Local } else { Scope::Global }
}

fn bundled_skill() -> BundledSkill {
    BundledSkill::from_files(vec![
        SkillFile::text("SKILL.md", SKILL_MD),
        SkillFile::text("references/event-model.md", EVENT_MODEL_MD),
        SkillFile::text("references/workflows.md", WORKFLOWS_MD),
    ])
}

fn make_installer() -> SkillInstaller {
    SkillInstaller::new(ToolIdentity::new("foundry", VERSION))
}

// ── JSON output shapes ────────────────────────────────────────────────────

#[derive(Serialize)]
struct JsonTargetResult {
    platform: String,
    action: String,
    dest_dir: String,
    files_written: usize,
}

#[derive(Serialize)]
struct JsonInstallOutput {
    success: bool,
    message: String,
    version: String,
    targets: Vec<JsonTargetResult>,
}

#[derive(Serialize)]
struct JsonRemoveOutput {
    success: bool,
    message: String,
    removed_dirs: Vec<String>,
    was_on_disk: bool,
    was_tracked: bool,
}

// ── Output rendering ──────────────────────────────────────────────────────

fn render_install_output(_plan: &InstallPlan, report: &Report, json: bool) -> Result<()> {
    if json {
        let targets: Vec<JsonTargetResult> = report
            .targets
            .iter()
            .map(|o| {
                let action = if o.action.will_write() {
                    match &o.action {
                        TargetAction::Install => "installed".to_string(),
                        TargetAction::Update { .. } => "updated".to_string(),
                        TargetAction::Downgrade { .. } => "downgraded".to_string(),
                        _ => "written".to_string(),
                    }
                } else {
                    "up_to_date".to_string()
                };
                JsonTargetResult {
                    platform: format!("{}", o.platform),
                    action,
                    dest_dir: o.dest_dir.to_string_lossy().into_owned(),
                    files_written: o.files_written,
                }
            })
            .collect();

        let applied_count = report.applied().count();
        let skipped_count = report.skipped().count();
        let message = format!(
            "foundry skill v{VERSION}: {applied_count} installed, {skipped_count} up to date"
        );

        let output = JsonInstallOutput {
            success: true,
            message,
            version: VERSION.to_string(),
            targets,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{report}");
    }
    Ok(())
}

fn render_remove_output(report: &RemoveReport, json: bool) -> Result<()> {
    if json {
        let message = if report.was_on_disk {
            format!("foundry skill removed (v{VERSION})")
        } else {
            "foundry skill was not installed (nothing to remove)".to_string()
        };
        let output = JsonRemoveOutput {
            success: true,
            message,
            removed_dirs: report
                .removed_dirs
                .iter()
                .map(|d| d.to_string_lossy().into_owned())
                .collect(),
            was_on_disk: report.was_on_disk,
            was_tracked: report.was_tracked,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{report}");
    }
    Ok(())
}

// ── Core implementation (accepts injected AppContext for testability) ────────

fn run_impl(
    scope: Scope,
    force: bool,
    remove: bool,
    json: bool,
    ctx: &AppContext<'_>,
) -> Result<()> {
    let installer = make_installer();

    if remove {
        let report = installer.remove(scope, ctx)?;
        render_remove_output(&report, json)?;
    } else {
        let skill = bundled_skill();
        let plan = installer.plan(&skill, scope, force, ctx)?;

        if plan.is_blocked() {
            let reasons: Vec<String> = plan
                .targets
                .iter()
                .filter_map(|t| {
                    if let TargetAction::RefuseNewer { installed } = &t.action {
                        Some(format!(
                            "{}: installed v{installed} is newer than bundled v{VERSION}; \
                             use --force to downgrade",
                            t.platform
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            bail!("{}", reasons.join("\n"));
        }

        let report = installer.apply(&skill, &plan, ctx)?;
        render_install_output(&plan, &report, json)?;
    }

    Ok(())
}

// ── Public entry points ───────────────────────────────────────────────────

/// Install the Foundry companion skill.
///
/// - `local`: when true, install into `./.claude/skills/foundry/` (project scope);
///   when false (default), install into `~/.claude/skills/foundry/` (global).
/// - `force`: override the newer-installed refusal (downgrade).
/// - `json`: emit machine-readable JSON instead of human output.
pub fn run(local: bool, force: bool, json: bool) -> Result<()> {
    let scope = scope_from_flags(local);
    let pctx = ProductionContext::claude()?;
    let ctx = pctx.ctx();
    run_impl(scope, force, false, json, &ctx)
}

/// Remove the installed Foundry companion skill.
///
/// - `local`: when true, remove from `./.claude/skills/foundry/` (project scope);
///   when false (default), remove from `~/.claude/skills/foundry/` (global).
/// - `json`: emit machine-readable JSON instead of human output.
pub fn remove(local: bool, json: bool) -> Result<()> {
    let scope = scope_from_flags(local);
    let pctx = ProductionContext::claude()?;
    let ctx = pctx.ctx();
    run_impl(scope, false, true, json, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmx_core::lockfile;
    use cmx_core::test_support::TestContext;
    use cmx_core::types::InstallScope;

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

    // ── Scope helper ────────────────────────────────────────────────────────

    #[test]
    fn no_local_flag_gives_global_scope() {
        assert!(matches!(scope_from_flags(false), Scope::Global));
    }

    #[test]
    fn local_flag_gives_local_scope() {
        assert!(matches!(scope_from_flags(true), Scope::Local));
    }

    // ── BundledSkill shape ──────────────────────────────────────────────────

    #[test]
    fn bundled_skill_has_skill_md_at_root() {
        let skill = bundled_skill();
        assert!(skill.has_skill_md(), "BundledSkill must contain SKILL.md at root");
    }

    #[test]
    fn bundled_skill_contains_all_three_files() {
        let skill = bundled_skill();
        assert_eq!(skill.files.len(), 3, "BundledSkill should contain 3 files");
        let paths: Vec<String> =
            skill.files.iter().map(|f| f.rel_path.to_string_lossy().into_owned()).collect();
        assert!(paths.iter().any(|p| p == "SKILL.md"), "SKILL.md must be present");
        assert!(
            paths.iter().any(|p| p == "references/event-model.md"),
            "references/event-model.md must be present"
        );
        assert!(
            paths.iter().any(|p| p == "references/workflows.md"),
            "references/workflows.md must be present"
        );
    }

    // ── Integration tests (fake filesystem via test-support) ─────────────────

    #[test]
    fn installs_all_files_to_target_directory() {
        let t = TestContext::new();
        let ctx = t.ctx();

        run_impl(Scope::Global, false, false, false, &ctx).unwrap();

        let home = &t.paths.home_dir;
        let skill_dir = home.join(".claude").join("skills").join("foundry");

        assert!(t.fs.file_exists(&skill_dir.join("SKILL.md")), "SKILL.md must be installed");
        assert!(
            t.fs.file_exists(&skill_dir.join("references").join("event-model.md")),
            "references/event-model.md must be installed"
        );
        assert!(
            t.fs.file_exists(&skill_dir.join("references").join("workflows.md")),
            "references/workflows.md must be installed"
        );

        // Installed SKILL.md is byte-identical to bundled content — no version stamp
        let installed =
            t.fs.get_file_content(&skill_dir.join("SKILL.md"))
                .expect("SKILL.md should be present");
        assert_eq!(
            installed,
            SKILL_MD.as_bytes(),
            "installed SKILL.md must be byte-identical to the embedded constant (no stamp)"
        );

        // Lock entry was written
        let lock = lockfile::load(InstallScope::Global, &t.fs, &t.paths)
            .expect("lock file should be loadable");
        assert!(lock.packages.contains_key("foundry"), "lock entry for 'foundry' must exist");
    }

    #[test]
    fn second_install_is_idempotent() {
        let t = TestContext::new();
        let ctx = t.ctx();

        run_impl(Scope::Global, false, false, false, &ctx).unwrap();
        // Second call with the same version must not fail
        run_impl(Scope::Global, false, false, false, &ctx).unwrap();

        let lock = lockfile::load(InstallScope::Global, &t.fs, &t.paths).unwrap();
        assert!(
            lock.packages.contains_key("foundry"),
            "lock entry must persist after second run"
        );
    }

    #[test]
    fn remove_uninstalls_all_files_and_clears_lock() {
        let t = TestContext::new();
        let ctx = t.ctx();

        // Install first
        run_impl(Scope::Global, false, false, false, &ctx).unwrap();

        let home = &t.paths.home_dir;
        let skill_dir = home.join(".claude").join("skills").join("foundry");
        assert!(t.fs.file_exists(&skill_dir.join("SKILL.md")), "must be installed before remove");

        // Remove
        run_impl(Scope::Global, false, true, false, &ctx).unwrap();

        // Skill files should be gone
        assert!(!t.fs.file_exists(&skill_dir.join("SKILL.md")), "SKILL.md should be removed");
        assert!(
            !t.fs.file_exists(&skill_dir.join("references").join("event-model.md")),
            "references/event-model.md should be removed"
        );
        assert!(
            !t.fs.file_exists(&skill_dir.join("references").join("workflows.md")),
            "references/workflows.md should be removed"
        );

        // Lock entry should be cleared
        let lock = lockfile::load(InstallScope::Global, &t.fs, &t.paths).unwrap();
        assert!(
            !lock.packages.contains_key("foundry"),
            "lock entry for 'foundry' must be removed"
        );
    }

    // ── JSON output shape ─────────────────────────────────────────────────

    #[test]
    fn json_install_output_has_expected_fields() {
        let output = JsonInstallOutput {
            success: true,
            message: "foundry skill v0.1.0: 1 installed, 0 up to date".to_string(),
            version: "0.1.0".to_string(),
            targets: vec![JsonTargetResult {
                platform: "Claude".to_string(),
                action: "installed".to_string(),
                dest_dir: "/home/user/.claude/skills/foundry".to_string(),
                files_written: 3,
            }],
        };
        let json_str = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert!(parsed["success"].is_boolean(), "success must be boolean");
        assert!(parsed["message"].is_string(), "message must be string");
        assert!(parsed["version"].is_string(), "version must be string");
        assert!(parsed["targets"].is_array(), "targets must be array");
        let targets = parsed["targets"].as_array().unwrap();
        assert!(!targets.is_empty(), "targets must not be empty");
        assert!(targets[0]["platform"].is_string(), "target.platform must be string");
        assert!(targets[0]["action"].is_string(), "target.action must be string");
        assert!(targets[0]["dest_dir"].is_string(), "target.dest_dir must be string");
        assert!(targets[0]["files_written"].is_number(), "target.files_written must be number");
    }

    #[test]
    fn json_remove_output_has_expected_fields() {
        let output = JsonRemoveOutput {
            success: true,
            message: "foundry skill removed (v0.1.0)".to_string(),
            removed_dirs: vec!["/home/user/.claude/skills/foundry".to_string()],
            was_on_disk: true,
            was_tracked: true,
        };
        let json_str = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        assert!(parsed["success"].is_boolean());
        assert!(parsed["message"].is_string());
        assert!(parsed["removed_dirs"].is_array());
        assert!(parsed["was_on_disk"].is_boolean());
        assert!(parsed["was_tracked"].is_boolean());
    }
}
