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
    /// Other identifiers for the same advisory (CVE, GHSA, PYSEC, RUSTSEC …)
    /// when the audit tool reports them. An allowlist or audit exception that
    /// names any of them matches the finding.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

impl Vulnerability {
    /// The finding's primary identifier followed by its aliases.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.cve.as_deref().into_iter().chain(self.aliases.iter().map(String::as_str))
    }
}

/// The aggregated result of running an audit scan.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditResult {
    /// All vulnerabilities found. Empty when the project is clean.
    pub vulnerabilities: Vec<Vulnerability>,
    /// Set when the audit tool could not run or returned an unexpected error.
    pub error: Option<String>,
    /// Findings the tool reported that do not count because they fall below
    /// the project's own failure threshold (Kotlin's `failBuildOnCVSS`).
    /// Carried so a result line can say how many were left out.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub below_threshold: u32,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if takes &T"
)]
fn is_zero(n: &u32) -> bool {
    *n == 0
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
