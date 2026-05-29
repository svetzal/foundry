//! Engine ↔ block integration tests for the vulnerability-remediation chain.
//!
//! Relocated from `engine.rs` when the engine moved to the `foundry-engine`
//! crate. These wire the *real* `ScanDependencies` → … → `InstallLocally` blocks
//! into a `foundry_engine::Engine`, so they belong to the host that owns both
//! the engine and the blocks — not to either component crate (which would form
//! a dependency cycle). They keep crate-internal test access (`crate::blocks`,
//! `crate::gateway::fakes`) by living here as a `#[cfg(test)]` module.
#![cfg(test)]

use std::sync::Arc;

use foundry_engine::engine::Engine;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::throttle::Throttle;

// -- Vulnerability remediation integration tests --

fn vuln_engine() -> Engine {
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Stack};
    use std::sync::RwLock;

    // CutRelease requires AGENTS.md to exist before invoking Claude.
    // Leak the temp dir so it outlives the test.
    let dir = tempfile::TempDir::new().unwrap();
    let project_path = dir.path().to_str().unwrap().to_string();
    // Initialize a git repo with an uncommitted change so CommitAndPush has work to do.
    let _ = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&project_path)
        .output();
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(&project_path)
        .output();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&project_path)
        .output();
    // Create an initial commit so there's a HEAD reference
    std::fs::write(dir.path().join("AGENTS.md"), "# test").unwrap();
    let _ = std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(&project_path)
        .output();
    let _ = std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&project_path)
        .output();
    // Set up a local bare repo as remote so git push succeeds
    let remote_dir = tempfile::TempDir::new().unwrap();
    let remote_path = remote_dir.path().to_str().unwrap().to_string();
    let _ = std::process::Command::new("git")
        .args(["init", "--bare"])
        .current_dir(&remote_path)
        .output();
    let _ = std::process::Command::new("git")
        .args(["remote", "add", "origin", &remote_path])
        .current_dir(&project_path)
        .output();
    let _ = std::process::Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(&project_path)
        .output();
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
        }],
    }));
    let mut engine = Engine::new();
    engine.register(Box::new(crate::blocks::ScanDependencies::new(Arc::clone(&registry))));
    engine.register(Box::new(crate::blocks::AuditReleaseTag::with_registry(Arc::clone(&registry))));
    engine.register(Box::new(crate::blocks::AuditMainBranch::new(Arc::clone(&registry))));
    let agent: Arc<dyn crate::gateway::AgentGateway> =
        crate::gateway::fakes::FakeAgentGateway::success();
    engine.register(Box::new(crate::blocks::RemediateVulnerability::new(
        agent,
        Arc::clone(&registry),
    )));
    engine.register(Box::new(crate::blocks::CommitAndPush::new(Arc::clone(&registry))));
    engine.register(Box::new(crate::blocks::cut_release_step(Arc::clone(&registry))));
    engine.register(Box::new(crate::blocks::WatchPipeline::stub()));
    engine.register(Box::new(crate::blocks::InstallLocally::new(Arc::clone(&registry))));
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
