//! Typed payload structs for all Foundry event types.
//!
//! Each event has a corresponding `*Payload` struct that serializes to exactly
//! the same JSON shape as the `serde_json::json!()` macros it replaces. The
//! wire format is byte-for-byte identical.
//!
//! # Usage
//!
//! Constructing an event payload:
//! ```rust,ignore
//! let payload = GreetingComposedPayload { greeting: "Hello, world!".to_string() };
//! let event = trigger.with_payload(EventType::GreetingComposed, &payload)?;
//! ```
//!
//! Reading a typed payload from an incoming trigger:
//! ```rust,ignore
//! let p: GreetingRequestedPayload = trigger.parse_payload()?;
//! let name = p.name.as_deref().unwrap_or("world");
//! ```

pub mod agent_session;
pub mod campaign;
pub mod commit_digest;
pub mod context;
pub mod drift;
pub mod execution;
pub mod gate_orchestration;
pub mod greet;
pub mod iterate;
pub mod maintenance;
pub mod maintenance_triage;
pub mod ops_digest;
pub mod pipeline;
pub mod project_lifecycle;
pub mod release;
pub mod strategic;
pub mod supply_chain;
pub mod task;
pub mod validation;
pub mod vulnerability;

pub use agent_session::*;
pub use campaign::*;
pub use commit_digest::*;
pub use context::*;
pub use drift::*;
pub use execution::*;
pub use gate_orchestration::*;
pub use greet::*;
pub use iterate::*;
pub use maintenance::*;
pub use maintenance_triage::*;
pub use ops_digest::*;
pub use pipeline::*;
pub use project_lifecycle::*;
pub use release::*;
pub use strategic::*;
pub use supply_chain::*;
pub use task::*;
pub use validation::*;
pub use vulnerability::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_context_extract_and_merge_roundtrip() {
        let source = serde_json::json!({
            "actions": {"maintain": true},
            "prompt": "do the thing",
            "gates": [{"name": "fmt"}],
            "audit_name": "fix-audit",
            "loop_context": {"strategic": {"iteration": 2}},
            "agent_provider": "codex",
            "unrelated": "noise",
        });

        let chain = ChainContext::extract_from(&source);
        assert!(chain.actions.is_some());
        assert!(chain.prompt.is_some());
        assert!(chain.gates.is_some());
        assert_eq!(chain.audit_name.as_deref(), Some("fix-audit"));
        assert!(chain.loop_context.is_some());
        assert_eq!(chain.agent_provider.as_deref(), Some("codex"));

        let mut target = serde_json::json!({ "project": "test" });
        chain.merge_into(&mut target);

        assert_eq!(target["actions"]["maintain"], true);
        assert_eq!(target["prompt"], "do the thing");
        assert_eq!(target["gates"][0]["name"], "fmt");
        assert_eq!(target["audit_name"], "fix-audit");
        assert_eq!(target["loop_context"]["strategic"]["iteration"], 2);
        assert_eq!(target["agent_provider"], "codex");
        assert!(target.get("unrelated").is_none());
    }

    #[test]
    fn chain_context_default_serializes_no_fields() {
        let chain = ChainContext::default();
        let json = serde_json::to_value(&chain).unwrap();
        // All fields are None, so they should all be absent
        assert!(json.as_object().unwrap().is_empty());
    }

    #[test]
    fn greeting_composed_payload_round_trips() {
        let p = GreetingComposedPayload {
            greeting: "Hello, world!".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["greeting"], "Hello, world!");
        let p2: GreetingComposedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.greeting, "Hello, world!");
    }

    #[test]
    fn greeting_delivered_payload_omits_dry_run_when_none() {
        let p = GreetingDeliveredPayload {
            delivered: true,
            greeting: "Hello!".to_string(),
            dry_run: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("dry_run").is_none());
        assert_eq!(json["delivered"], true);
    }

    #[test]
    fn greeting_delivered_payload_includes_dry_run_when_set() {
        let p = GreetingDeliveredPayload {
            delivered: true,
            greeting: "Hello!".to_string(),
            dry_run: Some(true),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["dry_run"], true);
    }

    #[test]
    fn loop_context_extract_copies_task_execution_context() {
        let source = serde_json::json!({
            "loop_context": {"strategic": {"iteration": 1}},
            "actions": {"maintain": true},
            "prompt": "task objective",
            "gates": "ignored",
            "campaign": "campaign-a",
            "task_worktree": "/tmp/worktree",
            "task_branch": "foundry-task/a",
            "base_ref": "preserved-a",
            "agent_provider": "codex",
        });
        let lc = LoopContext::extract_from(&source);
        assert!(lc.loop_context.is_some());
        assert!(lc.actions.is_some());

        let json = serde_json::to_value(&lc).unwrap();
        assert_eq!(json["prompt"], "task objective");
        assert_eq!(json["campaign"], "campaign-a");
        assert_eq!(json["task_worktree"], "/tmp/worktree");
        assert_eq!(json["task_branch"], "foundry-task/a");
        assert_eq!(json["base_ref"], "preserved-a");
        assert_eq!(json["agent_provider"], "codex");
        assert!(json.get("gates").is_none());
    }

    #[test]
    fn preflight_completed_payload_flattens_chain() {
        let chain = ChainContext {
            actions: Some(serde_json::json!({"maintain": true})),
            ..ChainContext::default()
        };
        let p = PreflightCompletedPayload {
            project: "test".to_string(),
            workflow: "iterate".to_string(),
            all_passed: true,
            required_passed: true,
            results: vec![],
            skipped: None,
            chain,
        };
        let json = serde_json::to_value(&p).unwrap();
        // Flattened: actions should appear at top level
        assert_eq!(json["actions"]["maintain"], true);
        assert!(json.get("chain").is_none(), "chain should not appear as a key");
    }

    #[test]
    fn execution_completed_payload_flattens_loop_context() {
        let context = LoopContext {
            loop_context: Some(serde_json::json!({"strategic": {"iteration": 1}})),
            actions: Some(serde_json::json!({"maintain": true})),
            ..LoopContext::default()
        };
        let p = ExecutionCompletedPayload {
            project: "test".to_string(),
            workflow: "iterate".to_string(),
            success: true,
            summary: "done".to_string(),
            execution_output: None,
            dry_run: None,
            retry_count: None,
            changes_detected: None,
            files_changed: vec![],
            failure: crate::gateway::AgentFailureMetadata::default(),
            context,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["loop_context"]["strategic"]["iteration"], 1);
        assert_eq!(json["actions"]["maintain"], true);
        assert!(json.get("context").is_none(), "context should not appear as a key");
    }

    #[test]
    fn vulnerability_detected_payload_round_trips() {
        let p = VulnerabilityDetectedPayload {
            cve: "CVE-2024-1234".to_string(),
            vulnerable: true,
            dirty: false,
            package: "openssl".to_string(),
            severity: "high".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["cve"], "CVE-2024-1234");
        assert_eq!(json["vulnerable"], true);
        assert_eq!(json["dirty"], false);
        assert_eq!(json["package"], "openssl");
        assert_eq!(json["severity"], "high");
        let p2: VulnerabilityDetectedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.cve, "CVE-2024-1234");
        assert_eq!(p2.severity, "high");
    }

    #[test]
    fn main_branch_audited_payload_round_trips() {
        let p = MainBranchAuditedPayload {
            project: "my-project".to_string(),
            cve: "CVE-2024-5678".to_string(),
            vulnerable: true,
            dirty: true,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["cve"], "CVE-2024-5678");
        assert_eq!(json["vulnerable"], true);
        assert_eq!(json["dirty"], true);
        let p2: MainBranchAuditedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert!(p2.dirty);
    }

    #[test]
    fn greeting_requested_payload_optional_name_round_trips() {
        let with_name = GreetingRequestedPayload {
            name: Some("Alice".to_string()),
        };
        let json = serde_json::to_value(&with_name).unwrap();
        assert_eq!(json["name"], "Alice");
        let restored: GreetingRequestedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(restored.name.as_deref(), Some("Alice"));

        let without_name = GreetingRequestedPayload { name: None };
        let json = serde_json::to_value(&without_name).unwrap();
        assert!(json.get("name").is_none(), "name must be absent when None");
    }

    #[test]
    fn local_skill_install_completed_payload_round_trips() {
        let p = LocalSkillInstallCompletedPayload {
            project: "my-project".to_string(),
            command: "mytool init --global --force".to_string(),
            success: true,
            stdout_tail: "Skill installed.".to_string(),
            stderr_tail: String::new(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["command"], "mytool init --global --force");
        assert_eq!(json["success"], true);
        assert_eq!(json["stdout_tail"], "Skill installed.");
        assert_eq!(json["stderr_tail"], "");
        let p2: LocalSkillInstallCompletedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert_eq!(p2.command, "mytool init --global --force");
        assert!(p2.success);
    }

    #[test]
    fn local_skill_install_completed_payload_failure_round_trips() {
        let p = LocalSkillInstallCompletedPayload {
            project: "my-project".to_string(),
            command: "mytool init --global --force".to_string(),
            success: false,
            stdout_tail: String::new(),
            stderr_tail: "error: command not found".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["stderr_tail"], "error: command not found");
        let p2: LocalSkillInstallCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(!p2.success);
        assert_eq!(p2.stderr_tail, "error: command not found");
    }

    #[test]
    fn project_iteration_requested_payload_flattens_chain() {
        let chain = ChainContext {
            actions: Some(serde_json::json!({"maintain": true})),
            ..ChainContext::default()
        };
        let p = ProjectIterationRequestedPayload {
            project: "my-project".to_string(),
            workflow: "iterate".to_string(),
            strategic: Some(true),
            max_iterations: Some(3),
            strategic_prompt: None,
            chain,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["workflow"], "iterate");
        assert_eq!(json["strategic"], true);
        assert_eq!(json["max_iterations"], 3);
        assert!(json.get("strategic_prompt").is_none());
        // Chain flattened: actions at top level
        assert_eq!(json["actions"]["maintain"], true);
        assert!(json.get("chain").is_none(), "chain must not appear as a key");
        let p2: ProjectIterationRequestedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert_eq!(p2.strategic, Some(true));
        assert_eq!(p2.chain.actions.unwrap()["maintain"], true);
    }

    #[test]
    fn agent_session_started_payload_serializes_to_expected_json() {
        use std::path::PathBuf;
        let payload = AgentSessionStartedPayload {
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            agent_type: "claude-code".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp/demo"),
            source_log_path: PathBuf::from("/home/u/.foundry/agent-sessions/11111111.jsonl"),
            tier: "balanced".to_string(),
            effort: "medium".to_string(),
            access: "full".to_string(),
            started_at: "2026-05-09T12:00:00Z".to_string(),
            trace_id: "trace-abc".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["session_id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["agent_type"], "claude-code");
        assert_eq!(json["project"], "demo");
        assert_eq!(json["working_dir"], "/tmp/demo");
        assert_eq!(json["source_log_path"], "/home/u/.foundry/agent-sessions/11111111.jsonl");
        assert_eq!(json["tier"], "balanced");
        assert_eq!(json["effort"], "medium");
        assert_eq!(json["access"], "full");
        assert_eq!(json["started_at"], "2026-05-09T12:00:00Z");
        assert_eq!(json["trace_id"], "trace-abc");
    }

    #[test]
    fn agent_session_ended_payload_serializes_to_expected_json() {
        let payload = AgentSessionEndedPayload {
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            status: "ok".to_string(),
            exit_code: Some(0),
            ended_at: "2026-05-09T12:01:00Z".to_string(),
            bytes_written: 1234,
            error: None,
            failure: crate::gateway::AgentFailureMetadata::default(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["session_id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["ended_at"], "2026-05-09T12:01:00Z");
        assert_eq!(json["bytes_written"], 1234);
        assert!(json.get("error").is_none(), "error should be omitted when None");
    }

    #[test]
    fn agent_session_ended_payload_includes_error_when_set() {
        let payload = AgentSessionEndedPayload {
            session_id: "id".to_string(),
            status: "unavailable".to_string(),
            exit_code: None,
            ended_at: "2026-05-09T12:01:00Z".to_string(),
            bytes_written: 0,
            error: Some("spawn failed: claude not on PATH".to_string()),
            failure: crate::gateway::AgentFailureMetadata::default(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["error"], "spawn failed: claude not on PATH");
        assert!(json.get("exit_code").is_none(), "exit_code should be omitted when None");
    }

    // ---------------------------------------------------------------------
    // Commit-digest payloads
    // ---------------------------------------------------------------------

    fn sample_commit() -> CommitInfo {
        CommitInfo {
            sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            author: "Stacey Vetzal".to_string(),
            timestamp: "2026-05-28T16:30:00-04:00".to_string(),
            subject: "feat(slice2): add the commit-digest formation".to_string(),
        }
    }

    #[test]
    fn commit_digest_started_payload_defaults_project_count_to_zero() {
        let parsed: CommitDigestStartedPayload = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.project_count, 0);
    }

    #[test]
    fn commit_digest_started_payload_round_trips() {
        let payload = CommitDigestStartedPayload { project_count: 17 };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["project_count"], 17);
        let back: CommitDigestStartedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.project_count, 17);
    }

    #[test]
    fn project_commits_with_error_omits_commits_when_serialized() {
        let p = ProjectCommits {
            name: "broken".to_string(),
            branch: "main".to_string(),
            commits: vec![],
            error: Some("git log exited 128: fatal: not a git repository".to_string()),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["name"], "broken");
        assert_eq!(json["branch"], "main");
        assert_eq!(json["error"], "git log exited 128: fatal: not a git repository");
        assert_eq!(json["commits"], serde_json::json!([]));
    }

    #[test]
    fn project_commits_without_error_omits_error_field() {
        let p = ProjectCommits {
            name: "ok".to_string(),
            branch: "main".to_string(),
            commits: vec![sample_commit()],
            error: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("error").is_none(), "error should be omitted when None");
        assert_eq!(json["commits"][0]["sha"], sample_commit().sha);
    }

    #[test]
    fn commits_observed_payload_total_and_project_count_helpers() {
        let payload = CommitsObservedPayload {
            window_hours: 24,
            projects: vec![
                ProjectCommits {
                    name: "alpha".to_string(),
                    branch: "main".to_string(),
                    commits: vec![sample_commit(), sample_commit()],
                    error: None,
                },
                ProjectCommits {
                    name: "broken".to_string(),
                    branch: "main".to_string(),
                    commits: vec![],
                    error: Some("nope".to_string()),
                },
            ],
        };
        assert_eq!(payload.project_count(), 2);
        assert_eq!(payload.total_commits(), 2, "errored project contributes zero");
    }

    #[test]
    fn commits_observed_payload_round_trips_through_json() {
        let payload = CommitsObservedPayload {
            window_hours: 24,
            projects: vec![ProjectCommits {
                name: "foundry".to_string(),
                branch: "main".to_string(),
                commits: vec![sample_commit()],
                error: None,
            }],
        };
        let json = serde_json::to_value(&payload).unwrap();
        let back: CommitsObservedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.window_hours, 24);
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.projects[0].commits[0], sample_commit());
    }

    #[test]
    fn commit_summary_completed_payload_round_trips() {
        let payload = CommitSummaryCompletedPayload {
            markdown: "# Commit Digest\n\nNothing today.\n".to_string(),
            project_count: 17,
            total_commits: 0,
        };
        let json = serde_json::to_value(&payload).unwrap();
        let back: CommitSummaryCompletedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.markdown, payload.markdown);
        assert_eq!(back.project_count, 17);
        assert_eq!(back.total_commits, 0);
    }

    #[test]
    fn commit_digest_completed_payload_omits_digest_path_when_none() {
        let payload = CommitDigestCompletedPayload {
            success: true,
            digest_path: None,
            project_count: 0,
            total_commits: 0,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("digest_path").is_none(), "digest_path should be omitted when None");
        assert_eq!(json["success"], true);
    }

    #[test]
    fn commit_digest_completed_payload_round_trips_with_path() {
        let payload = CommitDigestCompletedPayload {
            success: true,
            digest_path: Some("/Users/svetzal/.foundry/digests/2026-05-28.md".to_string()),
            project_count: 17,
            total_commits: 42,
        };
        let json = serde_json::to_value(&payload).unwrap();
        let back: CommitDigestCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(back.success);
        assert_eq!(back.digest_path.as_deref(), payload.digest_path.as_deref());
        assert_eq!(back.project_count, 17);
        assert_eq!(back.total_commits, 42);
    }

    // -------------------------------------------------------------------------
    // MaintenanceTriageCompleted payload
    // -------------------------------------------------------------------------

    #[test]
    fn maintenance_triage_completed_payload_defaults_from_empty_json() {
        let parsed: MaintenanceTriageCompletedPayload = serde_json::from_str("{}").unwrap();
        assert!(!parsed.success);
        assert!(!parsed.skipped);
        assert!(parsed.digest_path.is_none());
        assert!(parsed.verdicts.is_empty());
        assert!(parsed.infra_incidents.is_empty());
        assert_eq!(parsed.total_failures, 0);
        assert_eq!(parsed.suppressed_count, 0);
        assert_eq!(parsed.auto_fixable_count, 0);
        assert_eq!(parsed.policy_count, 0);
        assert_eq!(parsed.investigation_count, 0);
        assert_eq!(parsed.escalation_count, 0);
    }

    #[test]
    fn maintenance_triage_completed_payload_round_trips() {
        use crate::triage::{Decision, FailureClass, FailureVerdict, InfraIncident};

        let payload = MaintenanceTriageCompletedPayload {
            success: true,
            skipped: false,
            digest_path: Some("~/.foundry/triage/2026-06-12.md".to_string()),
            verdicts: vec![FailureVerdict {
                project: "alpha".to_string(),
                gate: "fmt".to_string(),
                class: FailureClass::FormatAndLintDrift,
                decision: Decision::AutoFixable,
                evidence: "cargo fmt produced diffs".to_string(),
                proposed_command: Some("cargo fmt".to_string()),
            }],
            infra_incidents: vec![InfraIncident {
                signature: "os_error_2".to_string(),
                decision: Decision::SuppressInfra,
                projects: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                sample_evidence: "os error 2".to_string(),
            }],
            total_failures: 5,
            suppressed_count: 3,
            auto_fixable_count: 1,
            policy_count: 0,
            investigation_count: 1,
            escalation_count: 0,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["total_failures"], 5);
        assert_eq!(json["verdicts"].as_array().unwrap().len(), 1);
        assert_eq!(json["infra_incidents"].as_array().unwrap().len(), 1);
        assert_eq!(json["digest_path"], "~/.foundry/triage/2026-06-12.md");

        let back: MaintenanceTriageCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(back.success);
        assert_eq!(back.total_failures, 5);
        assert_eq!(back.verdicts.len(), 1);
        assert_eq!(back.infra_incidents.len(), 1);
        assert_eq!(back.digest_path.as_deref(), Some("~/.foundry/triage/2026-06-12.md"));
    }

    #[test]
    fn maintenance_triage_completed_payload_omits_digest_path_when_none() {
        let payload = MaintenanceTriageCompletedPayload {
            success: true,
            ..MaintenanceTriageCompletedPayload::default()
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            json.get("digest_path").is_none(),
            "digest_path must be absent from JSON when None"
        );
    }
}
