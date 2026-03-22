use std::path::Path;

use anyhow::Result;
use foundry_core::registry::Stack;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
}

/// The aggregated result of running an audit scan.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AuditResult {
    /// All vulnerabilities found. Empty when the project is clean.
    pub vulnerabilities: Vec<Vulnerability>,
    /// Set when the audit tool could not run or returned an unexpected error.
    pub error: Option<String>,
}

/// Run the appropriate audit tool for the given stack and return parsed results.
///
/// Returns `Err` only for unrecoverable I/O failures (e.g. disk read error).
/// When the audit tool is not installed or returns a non-vulnerability failure,
/// the error is captured in [`AuditResult::error`] and `Ok` is returned.
pub async fn run_audit(path: &Path, stack: &Stack) -> Result<AuditResult> {
    let (command, args) = audit_command(stack);

    let result = match crate::shell::run(path, command, &args, None, None).await {
        Err(e) => {
            // Command could not be spawned — likely not installed.
            let msg = e.to_string();
            tracing::warn!(stack = %stack, %msg, "audit tool not available");
            return Ok(AuditResult {
                vulnerabilities: vec![],
                error: Some(msg),
            });
        }
        Ok(output) => output,
    };

    // Some tools exit non-zero when vulnerabilities are found; that is not a failure.
    if !result.success && !is_audit_vuln_exit_code(stack, result.exit_code) {
        let msg = format!("Audit tool failed (exit {}): {}", result.exit_code, result.stderr);
        tracing::warn!(stack = %stack, %msg, "audit tool reported failure");
        return Ok(AuditResult {
            vulnerabilities: vec![],
            error: Some(msg),
        });
    }

    Ok(parse_audit_output(stack, &result.stdout))
}

/// Map each stack to its canonical audit command and arguments.
fn audit_command(stack: &Stack) -> (&'static str, Vec<&'static str>) {
    match stack {
        Stack::Rust => ("cargo", vec!["audit", "--json"]),
        Stack::TypeScript => ("npm", vec!["audit", "--json"]),
        Stack::Python => ("pip-audit", vec!["--format=json"]),
        Stack::Elixir => ("mix", vec!["deps.audit", "--format=json"]),
    }
}

/// Return true when the given non-zero exit code is the tool's conventional way
/// of signalling "vulnerabilities found" rather than "tool failed".
fn is_audit_vuln_exit_code(stack: &Stack, exit_code: i32) -> bool {
    // `npm audit` exits with 1 when vulnerabilities are present.
    matches!(stack, Stack::TypeScript) && exit_code == 1
}

/// Dispatch JSON parsing to the stack-specific parser.
fn parse_audit_output(stack: &Stack, output: &str) -> AuditResult {
    match stack {
        Stack::Rust => parse_cargo_audit(output),
        Stack::TypeScript => parse_npm_audit(output),
        Stack::Python | Stack::Elixir => parse_generic_audit(output),
    }
}

/// Parse `cargo audit --json` output.
///
/// Expected shape:
/// ```json
/// {
///   "vulnerabilities": {
///     "found": true,
///     "count": 1,
///     "list": [{
///       "advisory": {
///         "id": "RUSTSEC-2021-0001",
///         "package": "some-crate",
///         "cvss": "7.5"
///       },
///       "package": { "name": "some-crate", "version": "0.1.0" }
///     }]
///   }
/// }
/// ```
fn parse_cargo_audit(output: &str) -> AuditResult {
    if output.trim().is_empty() {
        return AuditResult::default();
    }

    let root: Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(e) => {
            return AuditResult {
                vulnerabilities: vec![],
                error: Some(format!("cargo audit JSON parse error: {e}")),
            };
        }
    };

    let Some(list) = root["vulnerabilities"]["list"].as_array() else {
        return AuditResult::default();
    };

    let vulnerabilities = list
        .iter()
        .map(|item| {
            let advisory = &item["advisory"];
            let pkg = &item["package"];

            let cve = advisory["id"].as_str().map(str::to_owned);
            let package = advisory["package"]
                .as_str()
                .or_else(|| pkg["name"].as_str())
                .unwrap_or("unknown")
                .to_owned();
            let version = pkg["version"].as_str().map(str::to_owned);

            // cargo audit reports CVSS scores, not a named severity tier.
            // Map score to a human-readable label for a consistent interface.
            let severity = advisory["cvss"]
                .as_str()
                .and_then(|s| s.parse::<f32>().ok())
                .map(cvss_to_severity)
                .map(str::to_owned);

            Vulnerability {
                cve,
                severity,
                package,
                version,
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
    }
}

/// Map a CVSS v3 numeric score to a named severity tier.
fn cvss_to_severity(score: f32) -> &'static str {
    match score {
        s if s >= 9.0 => "critical",
        s if s >= 7.0 => "high",
        s if s >= 4.0 => "medium",
        s if s > 0.0 => "low",
        _ => "none",
    }
}

/// Parse `npm audit --json` output (npm v7+ format).
///
/// Expected shape:
/// ```json
/// {
///   "vulnerabilities": {
///     "lodash": {
///       "name": "lodash",
///       "severity": "high",
///       "via": ["CVE-2021-23337"],
///       "range": ">=0.0.1",
///       "nodes": ["node_modules/lodash"]
///     }
///   }
/// }
/// ```
fn parse_npm_audit(output: &str) -> AuditResult {
    if output.trim().is_empty() {
        return AuditResult::default();
    }

    let root: Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(e) => {
            return AuditResult {
                vulnerabilities: vec![],
                error: Some(format!("npm audit JSON parse error: {e}")),
            };
        }
    };

    let Some(vulns_map) = root["vulnerabilities"].as_object() else {
        return AuditResult::default();
    };

    let vulnerabilities = vulns_map
        .values()
        .map(|entry| {
            let package = entry["name"].as_str().unwrap_or("unknown").to_owned();
            let severity = entry["severity"].as_str().map(str::to_owned);

            // `via` can be a mix of strings (CVE IDs) and objects (nested vulns).
            let cve = entry["via"]
                .as_array()
                .and_then(|arr| arr.iter().find_map(|v| v.as_str()))
                .map(str::to_owned);

            Vulnerability {
                cve,
                severity,
                package,
                version: None,
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
    }
}

/// Parse generic JSON audit output (pip-audit, mix deps.audit).
///
/// Tries to interpret the output as a JSON array of objects with fields
/// that map loosely to [`Vulnerability`]. Falls back to an empty clean
/// result rather than propagating a parse error.
fn parse_generic_audit(output: &str) -> AuditResult {
    if output.trim().is_empty() {
        return AuditResult::default();
    }

    // pip-audit --format=json emits an array of vulnerability objects.
    // Each object looks like: {"name": "pkg", "version": "1.0", "vulns": [{"id": "CVE-...", "fix_versions": [...]}]}
    let Ok(root) = serde_json::from_str::<Value>(output) else {
        return AuditResult::default();
    };

    let Some(items) = root.as_array() else {
        return AuditResult::default();
    };

    let mut vulnerabilities = Vec::new();
    for item in items {
        let package = item["name"].as_str().unwrap_or("unknown").to_owned();
        let version = item["version"].as_str().map(str::to_owned);

        // pip-audit nests individual CVEs under a "vulns" array.
        if let Some(vulns) = item["vulns"].as_array() {
            for vuln in vulns {
                let cve = vuln["id"].as_str().map(str::to_owned);
                let severity = vuln["severity"].as_str().map(str::to_owned);
                vulnerabilities.push(Vulnerability {
                    cve,
                    severity,
                    package: package.clone(),
                    version: version.clone(),
                });
            }
        } else {
            // Flat object — treat the whole item as one vulnerability.
            let cve = item["id"].as_str().or_else(|| item["cve"].as_str()).map(str::to_owned);
            let severity = item["severity"].as_str().map(str::to_owned);
            vulnerabilities.push(Vulnerability {
                cve,
                severity,
                package,
                version,
            });
        }
    }

    AuditResult {
        vulnerabilities,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Command selection ---

    #[test]
    fn rust_uses_cargo_audit() {
        let (cmd, args) = audit_command(&Stack::Rust);
        assert_eq!(cmd, "cargo");
        assert_eq!(args, ["audit", "--json"]);
    }

    #[test]
    fn typescript_uses_npm_audit() {
        let (cmd, args) = audit_command(&Stack::TypeScript);
        assert_eq!(cmd, "npm");
        assert_eq!(args, ["audit", "--json"]);
    }

    #[test]
    fn python_uses_pip_audit() {
        let (cmd, args) = audit_command(&Stack::Python);
        assert_eq!(cmd, "pip-audit");
        assert_eq!(args, ["--format=json"]);
    }

    #[test]
    fn elixir_uses_mix_deps_audit() {
        let (cmd, args) = audit_command(&Stack::Elixir);
        assert_eq!(cmd, "mix");
        assert_eq!(args, ["deps.audit", "--format=json"]);
    }

    // --- npm exit-code convention ---

    #[test]
    fn npm_exit_code_1_is_not_failure() {
        assert!(is_audit_vuln_exit_code(&Stack::TypeScript, 1));
    }

    #[test]
    fn npm_exit_code_2_is_failure() {
        assert!(!is_audit_vuln_exit_code(&Stack::TypeScript, 2));
    }

    #[test]
    fn rust_non_zero_is_always_failure() {
        assert!(!is_audit_vuln_exit_code(&Stack::Rust, 1));
    }

    // --- cargo audit JSON parsing ---

    #[test]
    fn parse_cargo_audit_with_one_vulnerability() {
        let json = r#"
        {
          "vulnerabilities": {
            "found": true,
            "count": 1,
            "list": [{
              "advisory": {
                "id": "RUSTSEC-2021-0001",
                "package": "some-crate",
                "cvss": "7.5"
              },
              "package": {
                "name": "some-crate",
                "version": "0.1.0"
              }
            }]
          }
        }"#;

        let result = parse_cargo_audit(json);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 1);

        let vuln = &result.vulnerabilities[0];
        assert_eq!(vuln.cve.as_deref(), Some("RUSTSEC-2021-0001"));
        assert_eq!(vuln.package, "some-crate");
        assert_eq!(vuln.version.as_deref(), Some("0.1.0"));
        assert_eq!(vuln.severity.as_deref(), Some("high")); // 7.5 → high
    }

    #[test]
    fn parse_cargo_audit_clean_project() {
        let json = r#"{"vulnerabilities": {"found": false, "count": 0, "list": []}}"#;
        let result = parse_cargo_audit(json);
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_cargo_audit_empty_output() {
        let result = parse_cargo_audit("");
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_cargo_audit_malformed_json() {
        let result = parse_cargo_audit("not json at all");
        assert!(result.error.is_some());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn cvss_score_mapping() {
        assert_eq!(cvss_to_severity(9.0), "critical");
        assert_eq!(cvss_to_severity(9.8), "critical");
        assert_eq!(cvss_to_severity(7.0), "high");
        assert_eq!(cvss_to_severity(8.9), "high");
        assert_eq!(cvss_to_severity(4.0), "medium");
        assert_eq!(cvss_to_severity(6.9), "medium");
        assert_eq!(cvss_to_severity(0.1), "low");
        assert_eq!(cvss_to_severity(3.9), "low");
        assert_eq!(cvss_to_severity(0.0), "none");
    }

    // --- npm audit JSON parsing ---

    #[test]
    fn parse_npm_audit_with_one_vulnerability() {
        let json = r#"
        {
          "vulnerabilities": {
            "lodash": {
              "name": "lodash",
              "severity": "high",
              "via": ["CVE-2021-23337"],
              "range": ">=0.0.1",
              "nodes": ["node_modules/lodash"]
            }
          }
        }"#;

        let result = parse_npm_audit(json);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 1);

        let vuln = &result.vulnerabilities[0];
        assert_eq!(vuln.package, "lodash");
        assert_eq!(vuln.severity.as_deref(), Some("high"));
        assert_eq!(vuln.cve.as_deref(), Some("CVE-2021-23337"));
    }

    #[test]
    fn parse_npm_audit_multiple_packages() {
        let json = r#"
        {
          "vulnerabilities": {
            "lodash": {
              "name": "lodash",
              "severity": "high",
              "via": ["CVE-2021-23337"]
            },
            "minimist": {
              "name": "minimist",
              "severity": "critical",
              "via": ["CVE-2021-44906"]
            }
          }
        }"#;

        let result = parse_npm_audit(json);
        assert_eq!(result.vulnerabilities.len(), 2);
    }

    #[test]
    fn parse_npm_audit_empty_vulnerabilities() {
        let json = r#"{"vulnerabilities": {}}"#;
        let result = parse_npm_audit(json);
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_npm_audit_malformed_json() {
        let result = parse_npm_audit("{bad json}");
        assert!(result.error.is_some());
    }

    // --- Generic (pip-audit / mix) JSON parsing ---

    #[test]
    fn parse_pip_audit_nested_vulns() {
        let json = r#"
        [
          {
            "name": "requests",
            "version": "2.25.1",
            "vulns": [
              {"id": "CVE-2023-32681", "fix_versions": ["2.31.0"]}
            ]
          }
        ]"#;

        let result = parse_generic_audit(json);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 1);

        let vuln = &result.vulnerabilities[0];
        assert_eq!(vuln.package, "requests");
        assert_eq!(vuln.version.as_deref(), Some("2.25.1"));
        assert_eq!(vuln.cve.as_deref(), Some("CVE-2023-32681"));
    }

    #[test]
    fn parse_generic_audit_empty_array() {
        let result = parse_generic_audit("[]");
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_generic_audit_empty_output() {
        let result = parse_generic_audit("");
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_generic_audit_non_array_returns_clean() {
        // If the tool emits a non-array JSON (unexpected), return clean rather than error.
        let result = parse_generic_audit(r#"{"status": "ok"}"#);
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }
}
