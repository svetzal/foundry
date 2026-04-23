//! Integration tests for the manual release workflow chain.
//!
//! Wires up the complete event chain with fake gateways and verifies:
//! - Happy path: `ReleaseRequested` -> `ReleaseCompleted` -> `ReleasePipelineCompleted`
//!   -> `LocalInstallCompleted`
//! - Action flag guard: release=false stops the chain
//! - Missing AGENTS.md: `ExecuteRelease` fails gracefully
//! - No install config: chain completes with `LocalInstallCompleted` { status: "skipped" }
//! - Dry run: synthetic events flow through the full chain

use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_core::throttle::Throttle;

use crate::engine::Engine;
use crate::gateway::AgentGateway;
use crate::gateway::fakes::FakeAgentGateway;

fn release_actions() -> ActionFlags {
    ActionFlags {
        release: true,
        ..ActionFlags::default()
    }
}

fn test_registry(project_path: &str) -> Arc<Registry> {
    Arc::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: project_path.to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(), // empty repo — WatchPipeline stubs success
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: release_actions(),
            install: None, // no install config — InstallLocally skips gracefully
            installs_skill: None,
            timeout_secs: None,
        }],
    })
}

fn release_requested_event(bump: Option<&str>) -> Event {
    let payload = match bump {
        Some(b) => serde_json::json!({ "bump": b }),
        None => serde_json::json!({}),
    };
    Event::new(EventType::ReleaseRequested, "test-project".to_string(), Throttle::Full, payload)
}

/// Build the full release chain engine with fake agent gateway.
///
/// Uses `InstallLocally::new()` which creates a production shell gateway, but
/// since test registries use `install: None`, the block skips gracefully without
/// spawning any processes.
fn release_engine(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Engine {
    let mut engine = Engine::new();

    // ExecuteRelease (composed step, sinks on ReleaseRequested)
    engine.register(Box::new(super::execute_release_step(agent, registry.clone())));
    // WatchPipeline (sinks on ReleaseCompleted)
    engine.register(Box::new(super::WatchPipeline::new(registry.clone())));
    // InstallLocally (sinks on ReleasePipelineCompleted, ProjectChangesPushed)
    engine.register(Box::new(super::InstallLocally::new(registry)));

    engine
}

#[tokio::test]
async fn happy_path_release_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Release Process\n1. Run gates\n2. Tag")
        .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    // Agent: ExecuteRelease succeeds with a version tag
    let agent = FakeAgentGateway::success_with("Release done!\nv1.5.0\nPushed.");

    let engine = release_engine(agent, registry);
    let result = engine.process(release_requested_event(Some("minor"))).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Verify the full chain
    assert!(
        event_types.contains(&"release_requested"),
        "chain should start with release_requested"
    );
    assert!(event_types.contains(&"release_completed"), "should emit release_completed");
    assert!(
        event_types.contains(&"release_pipeline_completed"),
        "should emit release_pipeline_completed"
    );
    assert!(
        event_types.contains(&"local_install_completed"),
        "should emit local_install_completed"
    );

    // Verify release_completed has success and tag
    let release_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ReleaseCompleted)
        .unwrap();
    assert_eq!(release_event.payload["success"], true);
    assert_eq!(release_event.payload["new_tag"], "v1.5.0");
    assert_eq!(release_event.payload["release"], "manual");

    // Install should be skipped (no install config)
    let install_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::LocalInstallCompleted)
        .unwrap();
    assert_eq!(install_event.payload["status"], "skipped");
}

#[tokio::test]
async fn action_flag_guard_stops_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();

    // Registry with release=false
    let registry = Arc::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: dir.path().to_str().unwrap().to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(), // release=false
            install: None,
            installs_skill: None,
            timeout_secs: None,
        }],
    });

    let agent = FakeAgentGateway::success();

    let engine = release_engine(agent, registry);
    let result = engine.process(release_requested_event(None)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // ExecuteRelease should skip — no downstream events
    assert!(
        !event_types.contains(&"release_completed"),
        "should NOT emit release_completed when action disabled"
    );
    assert!(
        !event_types.contains(&"release_pipeline_completed"),
        "should NOT emit release_pipeline_completed"
    );
    assert!(
        !event_types.contains(&"local_install_completed"),
        "should NOT emit local_install_completed"
    );
}

#[tokio::test]
async fn missing_agents_md_fails_gracefully() {
    // Registry points to a path without AGENTS.md
    let dir = tempfile::tempdir().unwrap();
    // No AGENTS.md file created

    let registry = Arc::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: dir.path().to_str().unwrap().to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: release_actions(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
        }],
    });

    let agent = FakeAgentGateway::success();

    let engine = release_engine(agent, registry);
    let result = engine.process(release_requested_event(None)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // ExecuteRelease should fail — no events emitted, chain stops
    assert!(
        !event_types.contains(&"release_completed"),
        "should NOT emit release_completed when AGENTS.md missing"
    );

    // Verify there's a block execution that failed
    assert!(!result.is_success(), "chain should report failure when AGENTS.md missing");
}

#[tokio::test]
async fn no_install_config_completes_with_skipped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    let agent = FakeAgentGateway::success_with("v1.0.1");

    let engine = release_engine(agent, registry);
    let result = engine.process(release_requested_event(None)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert!(event_types.contains(&"release_completed"), "should emit release_completed");
    assert!(
        event_types.contains(&"release_pipeline_completed"),
        "should emit release_pipeline_completed"
    );
    assert!(
        event_types.contains(&"local_install_completed"),
        "should emit local_install_completed"
    );

    // Install should be skipped
    let install_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::LocalInstallCompleted)
        .unwrap();
    assert_eq!(install_event.payload["status"], "skipped");
}

#[tokio::test]
async fn dry_run_synthetic_events_flow_through_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());
    let agent = FakeAgentGateway::success();

    let engine = release_engine(agent, registry);

    // Emit with DryRun throttle
    let trigger = Event::new(
        EventType::ReleaseRequested,
        "test-project".to_string(),
        Throttle::DryRun,
        serde_json::json!({}),
    );

    let result = engine.process(trigger).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // All three stages should produce dry_run events
    assert!(
        event_types.contains(&"release_completed"),
        "dry run should emit release_completed"
    );
    assert!(
        event_types.contains(&"release_pipeline_completed"),
        "dry run should emit release_pipeline_completed"
    );
    assert!(
        event_types.contains(&"local_install_completed"),
        "dry run should emit local_install_completed"
    );

    // Verify dry_run flags
    let release_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ReleaseCompleted)
        .unwrap();
    assert_eq!(release_event.payload["dry_run"], true);

    let pipeline_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ReleasePipelineCompleted)
        .unwrap();
    assert_eq!(pipeline_event.payload["dry_run"], true);

    let install_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::LocalInstallCompleted)
        .unwrap();
    assert_eq!(install_event.payload["dry_run"], true);
}
