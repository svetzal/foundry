use std::path::Path;

use anyhow::Result;
use foundry_sdk::registry::Stack;
use serde_json::Value;

// `AuditResult` and `Vulnerability` are part of the SDK gateway contract.
// Re-exported here so the existing `crate::scanner::…` paths keep resolving.
pub use foundry_sdk::gateway::{AuditResult, Vulnerability};

/// Run the appropriate audit tool for the given stack and return parsed results.
///
/// Returns `Err` only for unrecoverable I/O failures (e.g. disk read error).
/// When the audit tool is not installed or returns a non-vulnerability failure,
/// the error is captured in [`AuditResult::error`] and `Ok` is returned.
pub async fn run_audit(path: &Path, stack: &Stack) -> Result<AuditResult> {
    if *stack == Stack::Cpp {
        tracing::info!("no standard audit tool for C++ projects");
        return Ok(AuditResult {
            vulnerabilities: vec![],
            error: None,
        });
    }

    // Resolve the audit tool. Most stacks audit through their global package
    // manager (`cargo`, `npm`, `mix`), which reads the project's local lockfile
    // and resolves any project-declared audit dependency itself. Python is the
    // exception: `pip-audit` is an ordinary project dependency that lives in the
    // project's own virtualenv, so we run it from `.venv/bin/` rather than any
    // global PATH — language/project tooling never belongs on a global path.
    let venv_pip_audit;
    let (command, args): (&str, Vec<&str>) = if *stack == Stack::Python {
        let tool = path.join(".venv/bin/pip-audit");
        if !tool.exists() {
            tracing::info!(path = %path.display(), "pip-audit not present in project .venv");
            return Ok(AuditResult {
                vulnerabilities: vec![],
                error: Some(
                    "pip-audit not found in .venv (add it as a dev dependency)".to_string(),
                ),
            });
        }
        venv_pip_audit = tool.to_string_lossy().into_owned();
        (venv_pip_audit.as_str(), vec!["--format=json"])
    } else {
        audit_command(stack)
    };

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

/// Map each global-package-manager stack to its audit command and arguments.
///
/// Python is *not* handled here — `pip-audit` is resolved from the project's
/// `.venv` in [`run_audit`], because it is a project dependency rather than a
/// global tool. C++ early-returns before this is reached.
fn audit_command(stack: &Stack) -> (&'static str, Vec<&'static str>) {
    match stack {
        Stack::Rust => ("cargo", vec!["audit", "--json"]),
        Stack::TypeScript => ("npm", vec!["audit", "--json"]),
        Stack::Elixir => ("mix", vec!["deps.audit", "--format=json"]),
        Stack::Python => unreachable!("Python resolves pip-audit from .venv in run_audit"),
        Stack::Cpp => unreachable!("C++ early-returns before audit_command"),
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
        Stack::Python => parse_pip_audit(output),
        Stack::Elixir => parse_generic_audit(output),
        Stack::Cpp => unreachable!("C++ early-returns before parse_audit_output"),
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

            // `versions.patched` is a list of version requirements that resolve
            // the advisory (e.g. `[">= 0.2.5"]`). The first, reduced to a bare
            // version, is the fix target.
            let fix_version = item["versions"]["patched"]
                .as_array()
                .and_then(|reqs| reqs.iter().find_map(|r| r.as_str()))
                .and_then(bare_version);

            Vulnerability {
                cve,
                severity,
                package,
                version,
                fix_version,
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
    }
}

/// Reduce a version requirement (`">= 0.2.5"`, `"^1.2.3"`, `"0.2.5, < 0.3"`) to
/// a bare `major.minor.patch` token (`"0.2.5"`, `"1.2.3"`). Returns `None` when
/// the string carries no version-like token. Prerelease/build metadata is
/// dropped — adequate for the fixable-vs-no-fix triage anchor; precise pinning
/// is the remediation block's concern.
fn bare_version(req: &str) -> Option<String> {
    let start = req.find(|c: char| c.is_ascii_digit())?;
    let rest = &req[start..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    Some(rest[..end].to_string())
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

            // `fixAvailable` is `false` (no fix), `true` (a fix exists but npm
            // gives no version at this node), or an object `{name, version, …}`.
            // Only the object form yields a precise fix version.
            let fix_version = entry["fixAvailable"]["version"].as_str().and_then(bare_version);

            Vulnerability {
                cve,
                severity,
                package,
                version: None,
                fix_version,
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
    }
}

/// Parse `pip-audit --format=json` output.
///
/// The real shape is an object, not a bare array:
/// ```json
/// {
///   "dependencies": [
///     {"name": "chromadb", "version": "1.5.9", "vulns": [
///       {"id": "CVE-2026-45829", "fix_versions": [], "aliases": ["GHSA-…"]}
///     ]}
///   ],
///   "fixes": []
/// }
/// ```
/// `id` is the advisory identifier; `fix_versions` is empty when no fix exists
/// (a policy call). pip-audit does not report a severity tier in this form.
fn parse_pip_audit(output: &str) -> AuditResult {
    if output.trim().is_empty() {
        return AuditResult::default();
    }

    let root: Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(e) => {
            return AuditResult {
                vulnerabilities: vec![],
                error: Some(format!("pip-audit JSON parse error: {e}")),
            };
        }
    };

    let Some(deps) = root["dependencies"].as_array() else {
        return AuditResult::default();
    };

    let mut vulnerabilities = Vec::new();
    for dep in deps {
        let package = dep["name"].as_str().unwrap_or("unknown").to_owned();
        let version = dep["version"].as_str().map(str::to_owned);
        let Some(vulns) = dep["vulns"].as_array() else {
            continue;
        };
        for vuln in vulns {
            let cve = vuln["id"].as_str().map(str::to_owned);
            let fix_version = vuln["fix_versions"]
                .as_array()
                .and_then(|fvs| fvs.iter().find_map(|v| v.as_str()))
                .and_then(bare_version);
            vulnerabilities.push(Vulnerability {
                cve,
                severity: None,
                package: package.clone(),
                version: version.clone(),
                fix_version,
            });
        }
    }

    AuditResult {
        vulnerabilities,
        error: None,
    }
}

/// Parse generic JSON audit output (`mix deps.audit`).
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
                // pip-audit lists resolving versions under "fix_versions".
                let fix_version = vuln["fix_versions"]
                    .as_array()
                    .and_then(|fvs| fvs.iter().find_map(|v| v.as_str()))
                    .and_then(bare_version);
                vulnerabilities.push(Vulnerability {
                    cve,
                    severity,
                    package: package.clone(),
                    version: version.clone(),
                    fix_version,
                });
            }
        } else {
            // Flat object — treat the whole item as one vulnerability.
            let cve = item["id"].as_str().or_else(|| item["cve"].as_str()).map(str::to_owned);
            let severity = item["severity"].as_str().map(str::to_owned);
            let fix_version = item["fix_versions"]
                .as_array()
                .and_then(|fvs| fvs.iter().find_map(|v| v.as_str()))
                .and_then(bare_version);
            vulnerabilities.push(Vulnerability {
                cve,
                severity,
                package,
                version,
                fix_version,
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

    #[tokio::test]
    async fn python_missing_venv_reports_cleanly() {
        // No `.venv/bin/pip-audit` under the path → a clean, informative error
        // (not a spawn failure, and never an `Err`). Project tooling lives in
        // the project's own environment.
        let dir = tempfile::tempdir().unwrap();
        let result = run_audit(dir.path(), &Stack::Python).await.unwrap();
        assert!(result.vulnerabilities.is_empty());
        assert_eq!(
            result.error.as_deref(),
            Some("pip-audit not found in .venv (add it as a dev dependency)")
        );
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
              "versions": {
                "patched": [">= 0.2.5"],
                "unaffected": []
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
        assert_eq!(
            vuln.fix_version.as_deref(),
            Some("0.2.5"),
            "patched req reduced to bare version"
        );
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
              "nodes": ["node_modules/lodash"],
              "fixAvailable": {"name": "lodash", "version": "4.17.21", "isSemVerMajor": false}
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
        assert_eq!(
            vuln.fix_version.as_deref(),
            Some("4.17.21"),
            "fixAvailable.version → fix_version"
        );
    }

    #[test]
    fn parse_npm_audit_bare_fix_available_has_no_version() {
        // `fixAvailable: true` means a fix exists but npm gives no version at
        // this node — conservatively classified as no-precise-fix.
        let json = r#"
        {
          "vulnerabilities": {
            "transitive": {
              "name": "transitive",
              "severity": "moderate",
              "via": ["CVE-2024-0001"],
              "fixAvailable": true
            }
          }
        }"#;
        let result = parse_npm_audit(json);
        assert_eq!(result.vulnerabilities.len(), 1);
        assert!(result.vulnerabilities[0].fix_version.is_none());
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

    // --- pip-audit JSON parsing ---

    #[test]
    fn parse_pip_audit_dependencies_wrapper_with_fix() {
        // Real pip-audit shape: a `{"dependencies": [...]}` object, each dep
        // carrying its own `vulns` list — not a bare array.
        let json = r#"
        {
          "dependencies": [
            {"name": "clean-pkg", "version": "1.0.0", "vulns": []},
            {
              "name": "requests",
              "version": "2.25.1",
              "vulns": [
                {"id": "CVE-2023-32681", "fix_versions": ["2.31.0"], "aliases": ["GHSA-x"]}
              ]
            }
          ],
          "fixes": []
        }"#;

        let result = parse_pip_audit(json);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 1, "clean deps contribute no findings");

        let vuln = &result.vulnerabilities[0];
        assert_eq!(vuln.package, "requests");
        assert_eq!(vuln.version.as_deref(), Some("2.25.1"));
        assert_eq!(vuln.cve.as_deref(), Some("CVE-2023-32681"));
        assert_eq!(vuln.fix_version.as_deref(), Some("2.31.0"), "first fix_versions entry");
    }

    #[test]
    fn parse_pip_audit_empty_fix_versions_is_policy_call() {
        // The chromadb case observed in production: a real advisory with no fix.
        let json = r#"
        {
          "dependencies": [
            {"name": "chromadb", "version": "1.5.9", "vulns": [
              {"id": "CVE-2026-45829", "fix_versions": [], "aliases": ["GHSA-f4j7-r4q5-qw2c"]}
            ]}
          ],
          "fixes": []
        }"#;

        let result = parse_pip_audit(json);
        assert_eq!(result.vulnerabilities.len(), 1);
        let vuln = &result.vulnerabilities[0];
        assert_eq!(vuln.cve.as_deref(), Some("CVE-2026-45829"));
        assert!(vuln.fix_version.is_none(), "no fix → policy call");
    }

    #[test]
    fn parse_pip_audit_clean_environment() {
        let json =
            r#"{"dependencies": [{"name": "safe", "version": "1.0", "vulns": []}], "fixes": []}"#;
        let result = parse_pip_audit(json);
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_pip_audit_malformed_json_reports_error() {
        let result = parse_pip_audit("{not json");
        assert!(result.error.is_some());
        assert!(result.vulnerabilities.is_empty());
    }

    // --- Generic (mix deps.audit) JSON parsing ---

    // --- bare_version reduction ---

    #[test]
    fn bare_version_strips_comparators_and_ranges() {
        assert_eq!(bare_version(">= 0.2.5").as_deref(), Some("0.2.5"));
        assert_eq!(bare_version("^0.28.1").as_deref(), Some("0.28.1"));
        assert_eq!(bare_version("0.2.5, < 0.3").as_deref(), Some("0.2.5"));
        assert_eq!(bare_version("4.17.21").as_deref(), Some("4.17.21"));
        assert_eq!(bare_version("1.2.3-rc1").as_deref(), Some("1.2.3"), "prerelease dropped");
        assert_eq!(bare_version("not a version"), None);
        assert_eq!(bare_version(""), None);
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
