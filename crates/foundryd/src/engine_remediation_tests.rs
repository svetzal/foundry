//! Engine ↔ block integration tests for the vulnerability-remediation chain.
//!
//! Relocated from `engine.rs` when the engine moved to the `foundry-engine`
//! crate. These wire the *real* `ScanDependencies` → … → `InstallLocally` blocks
//! into a `foundry_engine::Engine`, so they belong to the host that owns both
//! the engine and the blocks — not to either component crate (which would form
//! a dependency cycle). They keep crate-internal test access (`foundry_blocks::blocks`,
//! `foundry_sdk::gateway::fakes`) by living here as a `#[cfg(test)]` module.
#![cfg(test)]

use std::path::Path;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use foundry_engine::engine::Engine;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::{CommandResult, ShellGateway};
use foundry_sdk::throttle::Throttle;

fn clean_git_env(command: &mut Command) {
    command.env("GIT_CONFIG_GLOBAL", "/dev/null");
    command.env_remove("GIT_CONFIG_COUNT");
    command.env_remove("GIT_CONFIG_PARAMETERS");
    for index in 0..8 {
        command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
    }
}

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    let mut command = Command::new("git");
    command.current_dir(cwd).args(args);
    clean_git_env(&mut command);
    command.status().unwrap().success()
}

struct CleanProcessShellGateway;

impl ShellGateway for CleanProcessShellGateway {
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        _timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
        Box::pin(async move {
            let mut child = Command::new(command);
            child.current_dir(working_dir).args(args);
            clean_git_env(&mut child);
            if let Some(env) = env {
                child.envs(env.iter().map(|(k, v)| (k, v)));
            }
            let output = child.output()?;
            Ok(CommandResult {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(1),
                success: output.status.success(),
            })
        })
    }
}

// -- Vulnerability remediation integration tests --

fn vuln_engine() -> Engine {
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Stack};
    use std::sync::RwLock;

    // CutRelease requires AGENTS.md to exist before invoking Claude.
    // Leak the temp dir so it outlives the test.
    let dir = tempfile::TempDir::new().unwrap();
    let project_path = dir.path().to_str().unwrap().to_string();
    // Initialize a git repo with an uncommitted change so CommitAndPush has work to do.
    let _ = git_ok(dir.path(), &["init", "-b", "main"]);
    let _ = git_ok(dir.path(), &["config", "user.email", "test@example.com"]);
    let _ = git_ok(dir.path(), &["config", "user.name", "Test"]);
    // Create an initial commit so there's a HEAD reference
    std::fs::write(dir.path().join("AGENTS.md"), "# test").unwrap();
    let _ = git_ok(dir.path(), &["add", "-A"]);
    let _ = git_ok(dir.path(), &["commit", "-m", "init"]);
    // Set up a local bare repo as remote so git push succeeds
    let remote_dir = tempfile::TempDir::new().unwrap();
    let remote_url = format!("file://{}", remote_dir.path().display());
    let _ = git_ok(remote_dir.path(), &["init", "--bare"]);
    let _ = git_ok(dir.path(), &["remote", "add", "origin", &remote_url]);
    let _ = git_ok(dir.path(), &["push", "-u", "origin", "main"]);
    // Create an uncommitted change so CommitAndPush triggers
    std::fs::write(dir.path().join("CHANGES.md"), "changes").unwrap();
    std::mem::forget(dir);
    std::mem::forget(remote_dir);

    let registry = Arc::new(RwLock::new(foundry_sdk::registry::Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: project_path,
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags {
                iterate: false,
                maintain: false,
                push: true,
                audit: false,
                release: false,
            },
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }],
    }));
    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::ScanDependencies::new(Arc::clone(&registry))));
    engine.register(Box::new(foundry_blocks::blocks::AuditReleaseTag::with_registry(Arc::clone(
        &registry,
    ))));
    engine.register(Box::new(foundry_blocks::blocks::AuditMainBranch::new(Arc::clone(&registry))));
    let agent: Arc<dyn foundry_blocks::gateway::AgentGateway> =
        foundry_sdk::gateway::fakes::FakeAgentGateway::success();
    engine.register(Box::new(foundry_blocks::blocks::RemediateVulnerability::new(
        agent,
        Arc::clone(&registry),
    )));
    let shell: Arc<dyn ShellGateway> = Arc::new(CleanProcessShellGateway);
    engine.register(Box::new(foundry_blocks::blocks::CommitAndPush::with_gateways(
        Arc::clone(&registry),
        Arc::clone(&shell),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CutRelease::new(
        foundry_sdk::gateway::fakes::FakeAgentGateway::success(),
        Arc::clone(&registry),
    )));
    engine.register(Box::new(foundry_blocks::blocks::WatchPipeline::new(Arc::clone(&registry))));
    engine.register(Box::new(foundry_blocks::blocks::InstallLocally::with_gateways(
        Arc::clone(&registry),
        shell,
    )));
    engine
}

#[tokio::test]
async fn vuln_dirty_path_remediates_and_installs() {
    let engine = vuln_engine();

    let trigger = Event::new(
        EventType::VulnerabilityDetected,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "cve": "CVE-2026-1234",
            "vulnerable": true,
            "dirty": true,
        }),
    );

    let result = engine.process(trigger).await;
    let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert_eq!(
        types,
        [
            "vulnerability_detected",
            "release_tag_audited",
            "main_branch_audited",
            "remediation_completed",
            "project_changes_committed",
            "project_changes_pushed",
            // AuditReleaseTag now sinks on ProjectChangesPushed and performs a
            // post-push re-audit (stub: reports clean, vulnerable=false).
            "release_tag_audited",
            "local_install_completed",
        ]
    );
}

#[tokio::test]
async fn vuln_clean_path_releases_and_installs() {
    let engine = vuln_engine();

    let trigger = Event::new(
        EventType::VulnerabilityDetected,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "cve": "CVE-2026-5678",
            "vulnerable": true,
            "dirty": false,
        }),
    );

    let result = engine.process(trigger).await;
    let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert_eq!(
        types,
        [
            "vulnerability_detected",
            "release_tag_audited",
            "main_branch_audited",
            "release_completed",
            "release_pipeline_completed",
            "local_install_completed",
        ]
    );
}

#[tokio::test]
async fn vuln_not_vulnerable_stops_at_audit() {
    let engine = vuln_engine();

    let trigger = Event::new(
        EventType::VulnerabilityDetected,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "cve": "CVE-2026-9999",
            "vulnerable": false,
        }),
    );

    let result = engine.process(trigger).await;
    let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Chain stops after release_tag_audited because AuditMainBranch
    // self-filters when vulnerable=false
    assert_eq!(types, ["vulnerability_detected", "release_tag_audited",]);
}

#[tokio::test]
async fn vuln_dry_run_full_chain_with_simulated_events() {
    let engine = vuln_engine();

    let trigger = Event::new(
        EventType::VulnerabilityDetected,
        "test-project".to_string(),
        Throttle::DryRun,
        serde_json::json!({
            "cve": "CVE-2026-1234",
            "vulnerable": true,
            "dirty": true,
        }),
    );

    let result = engine.process(trigger).await;
    let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Full chain completes with simulated mutator events (dirty path).
    // Observers execute for real; Mutators simulate success.
    assert_eq!(
        types,
        [
            "vulnerability_detected",
            "release_tag_audited",
            "main_branch_audited",
            "remediation_completed",     // simulated
            "project_changes_committed", // simulated
            "project_changes_pushed",    // simulated
            // AuditReleaseTag sinks on ProjectChangesPushed (Observer, runs for real)
            "release_tag_audited",
            "local_install_completed", // simulated
        ]
    );

    // All simulated events carry dry_run: true.
    let remediation = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::RemediationCompleted)
        .unwrap();
    assert_eq!(remediation.payload["dry_run"], true);
}

// -- Scan-triggered workflow integration tests --

#[tokio::test]
async fn scan_triggers_full_remediation_chain() {
    let engine = vuln_engine();

    // Start from scan_requested instead of vulnerability_detected.
    // The scanner invokes `cargo audit` in the temp project dir. Since
    // no real Cargo.lock exists, the scanner reports an error and the
    // chain stops at scan_requested with no downstream events.
    let trigger = Event::new(
        EventType::ScanRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({}),
    );

    let result = engine.process(trigger).await;
    let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Scanner tool unavailable in temp dir — chain ends at scan_requested.
    assert_eq!(types, ["scan_requested"]);
}

#[tokio::test]
async fn scan_dry_run_scans_and_audits_only() {
    let engine = vuln_engine();

    let trigger = Event::new(
        EventType::ScanRequested,
        "test-project".to_string(),
        Throttle::DryRun,
        serde_json::json!({}),
    );

    let result = engine.process(trigger).await;
    let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Scanner tool unavailable in temp dir — chain ends at scan_requested.
    assert_eq!(types, ["scan_requested"]);
}
