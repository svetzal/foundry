use serde::{Deserialize, Serialize};

use super::GitSyncFailure;

// ---------------------------------------------------------------------------
// Release workflow
// ---------------------------------------------------------------------------

/// Payload for `MainBranchAudited`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MainBranchAuditedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub cve: String,
    #[serde(default)]
    pub vulnerable: bool,
    #[serde(default)]
    pub dirty: bool,
}

/// Payload for `ReleaseTagAudited`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseTagAuditedPayload {
    #[serde(default)]
    pub project: String,
    pub cve: String,
    #[serde(default)]
    pub tag: String,
    pub vulnerable: bool,
    /// Fallback dirty flag forwarded from upstream payloads when the scanner
    /// cannot run (project not in registry, no lockfile, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
    /// Set when the audit tool could not run. When `Some`, `vulnerable: false`
    /// means "unknown", not "clean".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_error: Option<String>,
}

#[cfg(test)]
mod release_payload_tests {
    use super::*;

    #[test]
    fn release_tag_audited_payload_omits_scan_error_key_when_none() {
        let p = ReleaseTagAuditedPayload {
            project: "proj".to_string(),
            cve: "none".to_string(),
            tag: String::new(),
            vulnerable: false,
            dirty: None,
            scan_error: None,
        };
        let v = serde_json::to_value(&p).expect("serializable");
        assert!(
            !v.as_object().unwrap().contains_key("scan_error"),
            "scan_error key must be absent when None"
        );
    }

    #[test]
    fn release_tag_audited_payload_scan_error_round_trips() {
        let p = ReleaseTagAuditedPayload {
            project: "proj".to_string(),
            cve: "CVE-2026-1".to_string(),
            tag: String::new(),
            vulnerable: false,
            dirty: None,
            scan_error: Some("boom".to_string()),
        };
        let json = serde_json::to_string(&p).expect("serializable");
        let p2: ReleaseTagAuditedPayload = serde_json::from_str(&json).expect("deserializable");
        assert_eq!(p2.scan_error.as_deref(), Some("boom"));
    }

    #[test]
    fn release_completed_payload_without_left_uncommitted_still_parses() {
        let p: ReleaseCompletedPayload =
            serde_json::from_str(r#"{"release":"manual","new_tag":null,"success":false}"#)
                .expect("a payload written before the field existed parses");
        assert!(p.left_uncommitted.is_empty());
        let v = serde_json::to_value(&p).expect("serializable");
        assert!(
            !v.as_object().unwrap().contains_key("left_uncommitted"),
            "left_uncommitted key must be absent when empty"
        );
    }

    #[test]
    fn release_completed_payload_left_uncommitted_round_trips() {
        let p: ReleaseCompletedPayload = serde_json::from_str(
            r#"{"release":"manual","success":false,"left_uncommitted":["mix.exs"]}"#,
        )
        .expect("deserializable");
        assert_eq!(p.left_uncommitted, ["mix.exs"]);
    }

    #[test]
    fn release_completed_payload_without_sync_fields_still_parses_and_omits_them() {
        let p: ReleaseCompletedPayload =
            serde_json::from_str(r#"{"release":"manual","new_tag":"v1.2.3","success":true}"#)
                .expect("a payload written before the fields existed parses");
        assert_eq!(p.sync_failure, None);
        assert_eq!(p.fast_forwarded, None);
        let v = serde_json::to_value(&p).expect("serializable");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("sync_failure"), "sync_failure key must be absent when None");
        assert!(
            !obj.contains_key("fast_forwarded"),
            "fast_forwarded key must be absent when None"
        );
    }

    #[test]
    fn release_completed_payload_sync_failure_round_trips_as_snake_case() {
        let p: ReleaseCompletedPayload = serde_json::from_str(
            r#"{"release":"manual","new_tag":null,"success":false,"sync_failure":"diverged"}"#,
        )
        .expect("deserializable");
        assert_eq!(p.sync_failure, Some(GitSyncFailure::Diverged));
        let v = serde_json::to_value(&p).expect("serializable");
        assert_eq!(v["sync_failure"], "diverged");
    }

    #[test]
    fn release_completed_payload_fast_forwarded_round_trips() {
        let p: ReleaseCompletedPayload = serde_json::from_str(
            r#"{"release":"patch","cve":"CVE-2026-1","new_tag":"v3.0.1","success":true,"fast_forwarded":2}"#,
        )
        .expect("deserializable");
        assert_eq!(p.fast_forwarded, Some(2));
        let v = serde_json::to_value(&p).expect("serializable");
        assert_eq!(v["fast_forwarded"], 2);
    }
}

/// Payload for `ReleaseRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequestedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub cve: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Optional version bump type (`patch`, `minor`, `major`). When absent
    /// the release agent determines the bump from the changelog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bump: Option<String>,
}

/// Payload for `ReleaseCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCompletedPayload {
    #[serde(default)]
    pub cve: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub new_tag: Option<String>,
    pub success: bool,
    /// Paths a failed release left uncommitted in the registered checkout that
    /// were clean when it started, relative to the repository root. Absent on
    /// success and when the release left nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub left_uncommitted: Vec<String>,
    /// Why the registered checkout could not be brought level with
    /// `origin/<branch>` before the release. When set, the release was
    /// refused before the agent ran: no version bump, commit, tag or publish
    /// happened. Absent when the sync succeeded or the checkout is not a git
    /// work tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_failure: Option<GitSyncFailure>,
    /// Commits fast-forwarded from `origin/<branch>` before the agent ran
    /// (`0` when the checkout was already level). Absent when the sync was
    /// refused or the checkout is not a git work tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_forwarded: Option<u32>,
}

/// Payload for `ReleasePipelineCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleasePipelineCompletedPayload {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

/// Payload for `LocalInstallCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalInstallCompletedPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Set to `"skipped"` when no install was performed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Human-readable explanation when `status` is `"skipped"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

/// Payload for `LocalSkillInstallCompleted`.
///
/// Emitted after `LocalInstallCompleted` when the project registry has an
/// `installs_skill` entry. Failure is soft: a failed skill install does not
/// fail the parent block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSkillInstallCompletedPayload {
    pub project: String,
    pub command: String,
    pub success: bool,
    /// Last few lines of stdout, for display in traces.
    pub stdout_tail: String,
    /// Last few lines of stderr, for display in traces.
    pub stderr_tail: String,
}
