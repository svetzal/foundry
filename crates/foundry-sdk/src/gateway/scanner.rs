use std::path::Path;
use std::pin::Pin;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::registry::Stack;

/// A single vulnerability discovered by an audit tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vulnerability {
    /// CVE identifier, RUSTSEC advisory ID, or equivalent (when available).
    pub cve: Option<String>,
    /// Severity rating reported by the audit tool (e.g. "high", "critical").
    pub severity: Option<String>,
    /// The name of the affected package or crate.
    pub package: String,
    /// The installed version of the affected package (when available).
    pub version: Option<String>,
    /// The earliest version that resolves the advisory, when the audit tool
    /// reports one. `Some` means a fix exists (mechanically auto-fixable);
    /// `None` means no fix is available yet (a human policy call). This is the
    /// triage anchor the remediation block branches on.
    #[serde(default)]
    pub fix_version: Option<String>,
    /// Package the audit tool says must be upgraded to resolve the advisory.
    /// Usually identical to `package`; npm may instead name a direct ancestor
    /// whose upgrade removes a vulnerable transitive dependency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_package: Option<String>,
}

/// The aggregated result of running an audit scan.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditResult {
    /// All vulnerabilities found. Empty when the project is clean.
    pub vulnerabilities: Vec<Vulnerability>,
    /// Set when the audit tool could not run or returned an unexpected error.
    pub error: Option<String>,
}

/// Abstracts over vulnerability scanning so that task blocks can be tested
/// without running real audit tools.
pub trait ScannerGateway: Send + Sync {
    fn run_audit<'a>(
        &'a self,
        path: &'a Path,
        stack: &'a Stack,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<AuditResult>> + Send + 'a>>;
}
