use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;
use foundry_sdk::registry::Stack;
use serde_json::Value;

// `AuditResult` and `Vulnerability` are part of the SDK gateway contract.
// Re-exported here so the existing `crate::scanner::…` paths keep resolving.
pub use foundry_sdk::gateway::{AuditResult, Vulnerability};
use foundry_sdk::supply_chain::{AllowDecision, SupplyChainAllowlist};

/// Where a Kotlin project's `OWASP` Dependency-Check aggregate report lands,
/// relative to the project root: the plugin's default output directory before
/// Dependency-Check 13, and the `dependency-check/` subdirectory from 13 on.
const DEPENDENCY_CHECK_REPORTS: [&str; 2] = [
    "build/reports/dependency-check-report.json",
    "build/reports/dependency-check/dependency-check-report.json",
];

/// Dependency-Check downloads and refreshes the NVD database before it scans,
/// which routinely takes several minutes and far longer on a cold cache. The
/// shell's five-minute default would kill a healthy scan.
const KOTLIN_AUDIT_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How a stack's audit runs and where its findings are read from.
#[derive(Debug, PartialEq)]
enum AuditPlan {
    /// The stack has no audit tool wired (C++).
    NotAudited,
    /// A precondition failed before any tool could run. The message becomes
    /// [`AuditResult::error`], so the scan is reported as "not scanned", never
    /// as clean.
    Unavailable(String),
    /// Run the tool and parse the JSON it prints on stdout.
    Stdout { command: String, args: Vec<String> },
    /// Run `mix deps.audit --format=json` in each Mix project that opts into
    /// `mix_audit`, and merge the findings.
    MixProjects { dirs: Vec<PathBuf> },
    /// Run the tool and parse the JSON report file it writes, from whichever
    /// of `reports` this run wrote most recently. A report must be written by
    /// *this* run; a stale report from an earlier run is an error.
    ReportFile {
        command: String,
        args: Vec<String>,
        reports: Vec<PathBuf>,
        timeout: Duration,
        /// The project's own failure threshold: findings scored below it do
        /// not count. `None` counts every live finding.
        min_cvss: Option<f32>,
    },
}

/// Run the appropriate audit tool for the given stack and return parsed results.
///
/// Returns `Err` only for unrecoverable I/O failures (e.g. disk read error).
/// When the audit tool is not installed or returns a non-vulnerability failure,
/// the error is captured in [`AuditResult::error`] and `Ok` is returned.
pub async fn run_audit(path: &Path, stack: &Stack) -> Result<AuditResult> {
    match audit_plan(path, stack) {
        AuditPlan::NotAudited => {
            tracing::info!("no standard audit tool for C++ projects");
            Ok(AuditResult {
                vulnerabilities: vec![],
                error: None,
                below_threshold: 0,
            })
        }
        AuditPlan::Unavailable(msg) => {
            tracing::info!(path = %path.display(), stack = %stack, %msg, "audit precondition not met");
            Ok(tool_error(msg))
        }
        AuditPlan::Stdout { command, args } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = match crate::shell::run(path, &command, &args, None, None).await {
                Ok(output) => output,
                Err(e) => return Ok(spawn_failure(stack, &e)),
            };

            // Some tools exit non-zero when vulnerabilities are found; that is not a failure.
            if !result.success && !is_audit_vuln_exit_code(stack, result.exit_code) {
                return Ok(exit_failure(stack, result.exit_code, &result.stderr));
            }

            Ok(parse_audit_output(stack, &result.stdout))
        }
        AuditPlan::MixProjects { dirs } => {
            let (command, args) = audit_command(stack);
            let mut runs = Vec::with_capacity(dirs.len());
            for dir in &dirs {
                let rel = dir
                    .strip_prefix(path)
                    .ok()
                    .map(|r| r.display().to_string())
                    .filter(|r| !r.is_empty())
                    .unwrap_or_else(|| ".".to_string());
                runs.push((rel, crate::shell::run(dir, command, &args, None, None).await));
            }
            let result = merge_mix_runs(runs);
            if let Some(err) = &result.error {
                tracing::warn!(stack = %stack, %err, "mix deps.audit did not produce a complete report");
            }
            Ok(result)
        }
        AuditPlan::ReportFile {
            command,
            args,
            reports,
            timeout,
            min_cvss,
        } => {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let started = SystemTime::now();
            let result = match crate::shell::run(path, &command, &args, None, Some(timeout)).await {
                Ok(output) => output,
                Err(e) => return Ok(spawn_failure(stack, &e)),
            };

            if !result.success && !is_audit_vuln_exit_code(stack, result.exit_code) {
                return Ok(exit_failure(stack, result.exit_code, &result.stderr));
            }

            // The exit code alone cannot tell a finding from a broken build
            // (Gradle exits 1 for both), so a fresh report is the proof the
            // scan ran. Without one the run is a failure, whatever it exited.
            match read_newest_fresh_report(&reports, started) {
                Some(contents) => Ok(parse_dependency_check(&contents, min_cvss)),
                None => Ok(exit_failure_without_report(
                    stack,
                    result.exit_code,
                    &reports,
                    &result.stderr,
                )),
            }
        }
    }
}

/// Decide how to audit a project of the given stack.
///
/// Most stacks audit through a global tool (`cargo`, `npm`, `mix`,
/// `osv-scanner`) that reads the project's committed lockfile. Two stacks run
/// project-local tooling instead, because language/project tooling never
/// belongs on a global path: Python runs `pip-audit` from the project's own
/// `.venv`, and Kotlin runs the project's own Gradle wrapper so its configured
/// Dependency-Check task (with its suppression file) is the one that decides.
fn audit_plan(path: &Path, stack: &Stack) -> AuditPlan {
    match stack {
        Stack::Cpp => AuditPlan::NotAudited,
        Stack::Python => {
            let tool = path.join(".venv/bin/pip-audit");
            if !tool.exists() {
                return AuditPlan::Unavailable(
                    "pip-audit not found in .venv (add it as a dev dependency)".to_string(),
                );
            }
            AuditPlan::Stdout {
                command: tool.to_string_lossy().into_owned(),
                args: vec!["--format=json".to_string()],
            }
        }
        Stack::Swift if !path.join("Package.resolved").exists() => AuditPlan::Unavailable(
            "Package.resolved not found (commit the resolved SwiftPM lockfile to audit it)"
                .to_string(),
        ),
        Stack::Kotlin => {
            let wrapper = path.join("gradlew");
            if !wrapper.exists() {
                return AuditPlan::Unavailable(
                    "gradlew not found (Kotlin audit runs the project's own Gradle wrapper)"
                        .to_string(),
                );
            }
            AuditPlan::ReportFile {
                command: wrapper.to_string_lossy().into_owned(),
                // `--rerun` (Gradle 7.6+) runs the task even when Gradle
                // considers it UP-TO-DATE, so every audit checks the current
                // vulnerability data and writes a fresh report.
                args: [
                    "dependencyCheckAggregate",
                    "--rerun",
                    "--no-parallel",
                    "--no-daemon",
                ]
                .map(str::to_string)
                .to_vec(),
                reports: DEPENDENCY_CHECK_REPORTS.iter().map(|r| path.join(r)).collect(),
                timeout: KOTLIN_AUDIT_TIMEOUT,
                min_cvss: fail_build_on_cvss(path),
            }
        }
        Stack::Elixir => {
            let dirs = mix_audit_projects(path);
            if dirs.is_empty() {
                return AuditPlan::Unavailable(
                    "no Mix project in this repository declares mix_audit \
                     (add {:mix_audit, \"~> 2.1\", only: [:dev, :test], runtime: false} to audit it)"
                        .to_string(),
                );
            }
            AuditPlan::MixProjects { dirs }
        }
        Stack::Rust | Stack::TypeScript | Stack::Swift => {
            let (command, args) = audit_command(stack);
            AuditPlan::Stdout {
                command: command.to_string(),
                args: args.into_iter().map(str::to_string).collect(),
            }
        }
    }
}

/// Map each global-tool stack to its audit command and arguments.
///
/// Python and Kotlin are *not* handled here — they run project-local tooling
/// resolved in [`audit_plan`]. C++ has no audit tool.
fn audit_command(stack: &Stack) -> (&'static str, Vec<&'static str>) {
    match stack {
        Stack::Rust => ("cargo", vec!["audit", "--json"]),
        Stack::TypeScript => ("npm", vec!["audit", "--json"]),
        Stack::Elixir => ("mix", vec!["deps.audit", "--format=json"]),
        Stack::Swift => (
            "osv-scanner",
            vec![
                "scan",
                "source",
                "--format",
                "json",
                "--lockfile",
                "Package.resolved",
            ],
        ),
        Stack::Python => unreachable!("Python resolves pip-audit from .venv in audit_plan"),
        Stack::Kotlin => unreachable!("Kotlin runs the project's Gradle wrapper in audit_plan"),
        Stack::Cpp => unreachable!("C++ has no audit tool; audit_plan never asks for one"),
    }
}

/// Return true when the given non-zero exit code is the tool's conventional way
/// of signalling "vulnerabilities found" rather than "tool failed".
fn is_audit_vuln_exit_code(stack: &Stack, exit_code: i32) -> bool {
    // `cargo audit`, `npm audit`, `pip-audit`, and `osv-scanner` exit 1 when
    // vulnerabilities are present (the report still goes to stdout; stderr may
    // carry unrelated warnings). osv-scanner's other non-zero codes (127
    // general error, 128 no packages found) are failures.
    //
    // Gradle exits 1 when Dependency-Check fails the build on a finding at or
    // above the project's `failBuildOnCVSS` — but also for any other build
    // failure. For Kotlin, exit 1 only means "read the report"; the report's
    // freshness decides whether the scan actually ran.
    matches!(
        stack,
        Stack::Rust
            | Stack::TypeScript
            | Stack::Python
            | Stack::Swift
            | Stack::Kotlin
            | Stack::Elixir
    ) && exit_code == 1
}

/// Dispatch JSON parsing to the stack-specific parser.
fn parse_audit_output(stack: &Stack, output: &str) -> AuditResult {
    match stack {
        Stack::Rust => parse_cargo_audit(output),
        Stack::TypeScript => parse_npm_audit(output),
        Stack::Python => parse_pip_audit(output),
        Stack::Elixir => unreachable!("Elixir merges per-project mix_audit runs in run_audit"),
        Stack::Swift => parse_osv_scanner(output),
        Stack::Kotlin => unreachable!("Kotlin parses its report file in run_audit"),
        Stack::Cpp => unreachable!("C++ has no audit output to parse"),
    }
}

/// Read the most recently written of `reports` that was written at or after
/// `started`. Missing, unreadable, and older reports are ignored; `None` when
/// no candidate is fresh.
fn read_newest_fresh_report(reports: &[PathBuf], started: SystemTime) -> Option<String> {
    // Allow for filesystems that store modification times at one-second
    // resolution: a report written in the same second as the run started
    // must still count as fresh.
    let threshold = started.checked_sub(Duration::from_secs(1)).unwrap_or(started);
    let (_, report) = reports
        .iter()
        .filter_map(|r| {
            let modified = std::fs::metadata(r).and_then(|m| m.modified()).ok()?;
            (modified >= threshold).then_some((modified, r))
        })
        .max_by_key(|(modified, _)| *modified)?;
    match std::fs::read_to_string(report) {
        Ok(contents) => Some(contents),
        Err(e) => {
            tracing::warn!(report = %report.display(), error = %e, "audit report unreadable");
            None
        }
    }
}

fn tool_error(msg: String) -> AuditResult {
    AuditResult {
        vulnerabilities: vec![],
        error: Some(msg),
        below_threshold: 0,
    }
}

/// The audit command could not be spawned (likely not installed) or timed out.
fn spawn_failure(stack: &Stack, e: &anyhow::Error) -> AuditResult {
    let msg = format!("{e:#}");
    tracing::warn!(stack = %stack, %msg, "audit tool not available");
    tool_error(msg)
}

fn exit_failure(stack: &Stack, exit_code: i32, stderr: &str) -> AuditResult {
    let msg = format!("Audit tool failed (exit {exit_code}): {stderr}");
    tracing::warn!(stack = %stack, %msg, "audit tool reported failure");
    tool_error(msg)
}

fn exit_failure_without_report(
    stack: &Stack,
    exit_code: i32,
    reports: &[PathBuf],
    stderr: &str,
) -> AuditResult {
    let locations: Vec<String> = reports.iter().map(|r| r.display().to_string()).collect();
    let msg = format!(
        "Audit tool exited {exit_code} without writing a fresh report at {}: {}",
        locations.join(" or "),
        tail(stderr, 2000)
    );
    tracing::warn!(stack = %stack, %msg, "audit report missing or stale");
    tool_error(msg)
}

/// The last `max` bytes of `s`, cut on a character boundary. Gradle failures
/// put the useful part at the end of a long log.
fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
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
                below_threshold: 0,
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
                fix_package: None,
                aliases: string_array(&advisory["aliases"]),
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
        below_threshold: 0,
    }
}

/// The strings in a JSON array; empty when the value is absent or not an array.
fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
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
                below_threshold: 0,
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
            let fix_package = entry["fixAvailable"]["name"].as_str().map(str::to_owned);

            Vulnerability {
                cve,
                severity,
                package,
                version: None,
                fix_version,
                fix_package,
                aliases: Vec::new(),
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
        below_threshold: 0,
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
                below_threshold: 0,
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
                fix_package: None,
                aliases: string_array(&vuln["aliases"]),
            });
        }
    }

    AuditResult {
        vulnerabilities,
        error: None,
        below_threshold: 0,
    }
}

/// Directories never searched for Mix projects: dependency, build, and
/// tool caches carry other packages' `mix.exs` files.
const MIX_SKIP_DIRS: [&str; 3] = ["deps", "_build", "node_modules"];

/// How deep below the repository root to look for Mix projects
/// (`vendor/roost/fixtures/phoenix_app` is depth 4).
const MIX_SEARCH_DEPTH: usize = 4;

/// The Mix projects in a repository that opt into `mix_audit`, sorted.
///
/// A repository may have no root `mix.exs` (bedrock keeps its Mix projects
/// under `apps/` and `vendor/`). The project decides which of its Mix projects
/// are audited by declaring `mix_audit` as a dependency, exactly as its own
/// gates do; a Mix project without it (fixtures, prototypes, apps that do not
/// ship the tool) cannot run `mix deps.audit` and is not audited. Dependency,
/// build and hidden directories are never searched.
fn mix_audit_projects(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        if declares_mix_audit(&dir.join("mix.exs")) {
            found.push(dir.to_path_buf());
        }
        if depth == MIX_SEARCH_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || MIX_SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                visit(&entry.path(), depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    visit(root, 0, &mut found);
    found.sort();
    found
}

/// Whether a `mix.exs` lists `mix_audit` outside a comment.
fn declares_mix_audit(mix_exs: &Path) -> bool {
    std::fs::read_to_string(mix_exs).is_ok_and(|text| {
        text.lines()
            .map(|line| line.split('#').next().unwrap_or(""))
            .any(|code| code.contains(":mix_audit"))
    })
}

/// Merge `mix deps.audit` runs, one per Mix project (`rel` is its path
/// relative to the repository root). Any project that did not produce a
/// report makes the whole scan an error naming that project: a partial scan
/// must never read as clean. The same advisory on the same package version is
/// reported once.
fn merge_mix_runs(runs: Vec<(String, Result<crate::shell::CommandResult>)>) -> AuditResult {
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let mut vulnerabilities = Vec::new();
    for (rel, run) in runs {
        let output = match run {
            Err(e) => {
                errors.push(format!("mix deps.audit in {rel} could not run: {e:#}"));
                continue;
            }
            Ok(output) => output,
        };
        if !(output.exit_code == 0 || output.exit_code == 1) {
            errors.push(format!(
                "mix deps.audit in {rel} failed (exit {}): {}",
                output.exit_code,
                tail(output.stderr.trim(), 500)
            ));
            continue;
        }
        let report = parse_mix_audit(&output.stdout);
        if let Some(err) = report.error {
            let detail = format!("{}\n{}", output.stdout.trim(), output.stderr.trim());
            errors.push(format!(
                "mix deps.audit in {rel} (exit {}): {err}: {}",
                output.exit_code,
                tail(detail.trim(), 500)
            ));
            continue;
        }
        for v in report.vulnerabilities {
            if seen.insert((v.package.clone(), v.version.clone(), v.cve.clone())) {
                vulnerabilities.push(v);
            }
        }
    }
    AuditResult {
        vulnerabilities,
        error: (!errors.is_empty()).then(|| errors.join("; ")),
        below_threshold: 0,
    }
}

/// Parse `mix deps.audit --format=json` output (`mix_audit` 2.x).
///
/// `mix_audit` encodes its report struct as one JSON line:
/// ```json
/// {"pass": false, "vulnerabilities": [{
///   "advisory": {"id": "GHSA-…", "package": "absinthe", "severity": "high",
///                "first_patched_versions": ["1.10.2"], …},
///   "dependency": {"package": "absinthe", "version": "1.7.8", "lockfile": "…"}
/// }]}
/// ```
/// Mix may print dependency compilation lines to stdout first, so the report
/// is the last line that parses as a `{"pass", "vulnerabilities"}` object.
/// Output without one is an error, never a clean scan.
fn parse_mix_audit(output: &str) -> AuditResult {
    let report = output
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| l.starts_with('{'))
        .find_map(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .filter(|v| v["pass"].is_boolean() && v["vulnerabilities"].is_array())
        });
    let Some(report) = report else {
        return tool_error("mix deps.audit printed no JSON report".to_owned());
    };

    let vulnerabilities = report["vulnerabilities"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|item| {
            let advisory = &item["advisory"];
            let dependency = &item["dependency"];
            Vulnerability {
                cve: advisory["id"].as_str().map(str::to_owned),
                severity: advisory["severity"].as_str().map(str::to_ascii_lowercase),
                package: dependency["package"]
                    .as_str()
                    .or_else(|| advisory["package"].as_str())
                    .unwrap_or("unknown")
                    .to_owned(),
                version: dependency["version"].as_str().map(str::to_owned),
                fix_version: advisory["first_patched_versions"]
                    .as_array()
                    .and_then(|vs| vs.iter().find_map(Value::as_str))
                    .and_then(bare_version),
                fix_package: None,
                aliases: Vec::new(),
            }
        })
        .collect();

    AuditResult {
        vulnerabilities,
        error: None,
        below_threshold: 0,
    }
}

/// Parse `osv-scanner scan source --format json` output (Swift).
///
/// Expected shape (osv-scanner v2):
/// ```json
/// {
///   "results": [{
///     "source": {"path": ".../Package.resolved", "type": "lockfile"},
///     "packages": [{
///       "package": {"name": "github.com/apple/swift-nio-http2", "version": "1.19.0", "ecosystem": "SwiftURL"},
///       "vulnerabilities": [{"id": "GHSA-…", "aliases": ["CVE-…"], "affected": [{
///         "package": {"name": "github.com/apple/swift-nio-http2"},
///         "ranges": [{"events": [{"introduced": "1.0.0"}, {"fixed": "1.19.2"}]}]
///       }]}],
///       "groups": [{"ids": ["GHSA-…"], "aliases": ["CVE-…", "GHSA-…"], "max_severity": "7.5"}]
///     }]
///   }]
/// }
/// ```
///
/// Each *group* is one advisory (osv-scanner merges aliases into a group), so
/// each group becomes one finding. Its identifier is the CVE alias when one
/// exists, otherwise the group's first id, so allowlists and audit exceptions
/// can name the advisory the way the rest of Foundry does. The fix version is
/// the first `fixed` event recorded for this package.
///
/// Package names are the `SwiftURL` ecosystem's repository URLs, not `SwiftPM`
/// package identities.
fn parse_osv_scanner(output: &str) -> AuditResult {
    let root: Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(e) => return tool_error(format!("osv-scanner JSON parse error: {e}")),
    };

    let Some(results) = root["results"].as_array() else {
        return tool_error("osv-scanner JSON: expected a top-level \"results\" array".to_owned());
    };

    let mut vulnerabilities = Vec::new();
    for pkg in results.iter().filter_map(|r| r["packages"].as_array()).flatten() {
        let package = pkg["package"]["name"].as_str().unwrap_or("unknown").to_owned();
        let version = pkg["package"]["version"].as_str().map(str::to_owned);
        let vulns = pkg["vulnerabilities"].as_array().map_or(&[][..], Vec::as_slice);

        for group in osv_groups(pkg, vulns) {
            let primary_id = group.ids.first().copied();
            let cve = group
                .aliases
                .iter()
                .chain(group.ids.iter())
                .find(|id| id.starts_with("CVE-"))
                .or(group.ids.first())
                .map(|id| (*id).to_owned());
            let severity = group
                .max_severity
                .and_then(|s| s.parse::<f32>().ok())
                .map(|score| cvss_to_severity(score).to_owned());
            let fix_version = primary_id
                .and_then(|id| vulns.iter().find(|v| v["id"].as_str() == Some(id)))
                .and_then(|v| osv_fixed_version(v, &package));
            // Every other identifier osv-scanner knows for this advisory, so
            // an allowlist naming any of them matches.
            let mut aliases: Vec<String> = Vec::new();
            for id in group.ids.iter().chain(group.aliases.iter()) {
                if Some(*id) != cve.as_deref() && !aliases.iter().any(|a| a == id) {
                    aliases.push((*id).to_owned());
                }
            }

            vulnerabilities.push(Vulnerability {
                cve,
                severity,
                package: package.clone(),
                version: version.clone(),
                fix_version,
                fix_package: None,
                aliases,
            });
        }
    }

    AuditResult {
        vulnerabilities,
        error: None,
        below_threshold: 0,
    }
}

/// One advisory as osv-scanner groups it.
struct OsvGroup<'a> {
    ids: Vec<&'a str>,
    aliases: Vec<&'a str>,
    max_severity: Option<&'a str>,
}

/// The advisory groups for one package. osv-scanner always emits `groups`;
/// if it is absent, each vulnerability stands as its own group so no finding
/// is lost.
fn osv_groups<'a>(pkg: &'a Value, vulns: &'a [Value]) -> Vec<OsvGroup<'a>> {
    let strs = |v: &'a Value| -> Vec<&'a str> {
        v.as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    };
    match pkg["groups"].as_array() {
        Some(groups) => groups
            .iter()
            .map(|g| OsvGroup {
                ids: strs(&g["ids"]),
                aliases: strs(&g["aliases"]),
                max_severity: g["max_severity"].as_str(),
            })
            .collect(),
        None => vulns
            .iter()
            .map(|v| OsvGroup {
                ids: v["id"].as_str().into_iter().collect(),
                aliases: strs(&v["aliases"]),
                max_severity: None,
            })
            .collect(),
    }
}

/// The first `fixed` version an OSV record gives for `package`, reduced to a
/// bare version. Records can list several ecosystems; only entries for this
/// package count.
fn osv_fixed_version(vuln: &Value, package: &str) -> Option<String> {
    vuln["affected"]
        .as_array()?
        .iter()
        .filter(|a| a["package"]["name"].as_str() == Some(package))
        .filter_map(|a| a["ranges"].as_array())
        .flatten()
        .filter_map(|r| r["events"].as_array())
        .flatten()
        .find_map(|e| e["fixed"].as_str())
        .and_then(bare_version)
}

/// Parse an `OWASP` Dependency-Check JSON report (Kotlin).
///
/// Expected shape (report schema 1.1):
/// ```json
/// {
///   "reportSchema": "1.1",
///   "dependencies": [{
///     "fileName": "kotlin-stdlib-2.2.0.jar",
///     "packages": [{"id": "pkg:maven/org.jetbrains.kotlin/kotlin-stdlib@2.2.0"}],
///     "vulnerabilities": [{"name": "CVE-2026-53914", "severity": "CRITICAL"}]
///   }]
/// }
/// ```
///
/// Only `vulnerabilities` are findings; anything the project's suppression
/// file matched is reported under `suppressedVulnerabilities` and is ignored
/// here, because the project has already made that call. Dependency-Check
/// names no fix version, so every finding is a policy call. The same advisory
/// on the same package is reported once.
///
/// `min_cvss` is the project's `failBuildOnCVSS`. A finding whose highest CVSS
/// score (v2, v3 or v4, the same rule Dependency-Check applies when it fails
/// the build) is below it does not count; the project's build treats it as
/// triage, not a failure. A finding with no score is kept. The number of
/// findings below the threshold is logged so the filter is never silent.
fn parse_dependency_check(output: &str, min_cvss: Option<f32>) -> AuditResult {
    let root: Value = match serde_json::from_str(output) {
        Ok(v) => v,
        Err(e) => return tool_error(format!("Dependency-Check report JSON parse error: {e}")),
    };

    let Some(dependencies) = root["dependencies"].as_array() else {
        return tool_error(
            "Dependency-Check report: expected a top-level \"dependencies\" array".to_owned(),
        );
    };

    let mut seen = HashSet::new();
    let mut vulnerabilities = Vec::new();
    let mut below_threshold = 0usize;
    for dep in dependencies {
        let Some(vulns) = dep["vulnerabilities"].as_array() else {
            continue;
        };
        let (package, version) = dependency_check_coordinates(dep);
        for vuln in vulns {
            let cve = vuln["name"].as_str().map(str::to_owned);
            if !seen.insert((package.clone(), version.clone(), cve.clone())) {
                continue;
            }
            if let (Some(threshold), Some(score)) = (min_cvss, highest_cvss(vuln))
                && score < threshold
            {
                below_threshold += 1;
                continue;
            }
            vulnerabilities.push(Vulnerability {
                cve,
                severity: vuln["severity"].as_str().map(str::to_ascii_lowercase),
                package: package.clone(),
                version: version.clone(),
                fix_version: None,
                fix_package: None,
                aliases: Vec::new(),
            });
        }
    }

    if below_threshold > 0 {
        tracing::info!(
            below_threshold,
            min_cvss = ?min_cvss,
            counted = vulnerabilities.len(),
            "Dependency-Check findings below the project's failBuildOnCVSS not counted"
        );
    }

    AuditResult {
        vulnerabilities,
        error: None,
        below_threshold: u32::try_from(below_threshold).unwrap_or(u32::MAX),
    }
}

/// The highest CVSS score Dependency-Check recorded for a finding, across v2,
/// v3 and v4. `None` when the finding carries no score.
fn highest_cvss(vuln: &Value) -> Option<f32> {
    [
        &vuln["cvssv2"]["score"],
        &vuln["cvssv3"]["baseScore"],
        &vuln["cvssv4"]["baseScore"],
    ]
    .into_iter()
    .filter_map(Value::as_f64)
    .map(|score| {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "CVSS scores are 0.0 to 10.0"
        )]
        let score = score as f32;
        score
    })
    .reduce(f32::max)
}

/// Read the project's own `failBuildOnCVSS` from `build.gradle.kts` or
/// `build.gradle`. Only a numeric literal counts (`failBuildOnCVSS = 7.0f`,
/// `failBuildOnCVSS 7`); anything computed yields `None`, and every live
/// finding then counts.
fn fail_build_on_cvss(path: &Path) -> Option<f32> {
    ["build.gradle.kts", "build.gradle"].into_iter().find_map(|file| {
        let text = std::fs::read_to_string(path.join(file)).ok()?;
        text.lines()
            .map(str::trim_start)
            .filter(|line| !line.starts_with("//"))
            .find_map(|line| line.split_once("failBuildOnCVSS").map(|(_, rest)| rest))
            .and_then(|rest| {
                let value = rest.trim_start().trim_start_matches('=').trim_start();
                let end =
                    value.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(value.len());
                let (number, tail) = value.split_at(end);
                let tail = tail.trim_start_matches(['f', 'F']);
                let literal_ends =
                    tail.is_empty() || tail.starts_with(|c: char| c.is_whitespace() || c == ')');
                if literal_ends {
                    number.parse::<f32>().ok()
                } else {
                    None
                }
            })
    })
}

/// Name a Dependency-Check dependency as `group:artifact` plus version, from
/// its Maven package URL. Falls back to the file name when there is no
/// package URL.
fn dependency_check_coordinates(dep: &Value) -> (String, Option<String>) {
    let purl = dep["packages"]
        .as_array()
        .and_then(|p| p.iter().find_map(|p| p["id"].as_str()))
        .and_then(|id| id.strip_prefix("pkg:maven/"))
        .map(|purl| purl.split(['?', '#']).next().unwrap_or(purl));
    if let Some(purl) = purl {
        let (coordinates, version) = match purl.split_once('@') {
            Some((c, v)) => (c, Some(v.to_owned())),
            None => (purl, None),
        };
        return (coordinates.replacen('/', ":", 1), version);
    }
    (dep["fileName"].as_str().unwrap_or("unknown").to_owned(), None)
}

/// Collapse a scanner gateway call into either a usable result or the reason
/// it failed.
///
/// A scan that did not run is never reported as a clean scan:
/// - `Err(e)` from the gateway (spawn failure, I/O error) → `Err(e.to_string())`
/// - `Ok(result)` where `result.error.is_some()` (tool not installed, etc.) → `Err(error)`
/// - `Ok(result)` with no error → `Ok(result)`
pub(crate) fn audit_outcome(audit: anyhow::Result<AuditResult>) -> Result<AuditResult, String> {
    match audit {
        Err(e) => Err(e.to_string()),
        Ok(result) => {
            if let Some(err_msg) = result.error {
                Err(err_msg)
            } else {
                Ok(AuditResult {
                    vulnerabilities: result.vulnerabilities,
                    error: None,
                    below_threshold: 0,
                })
            }
        }
    }
}

/// A finding the project has accepted, and why.
#[derive(Debug)]
pub struct AcceptedFinding<'a> {
    pub finding: &'a Vulnerability,
    /// `"audit_exceptions"` or the allowlist entry's reason.
    pub reason: String,
}

/// A finding whose allowlist acceptance has lapsed. It is also live.
#[derive(Debug)]
pub struct LapsedFinding<'a> {
    pub finding: &'a Vulnerability,
    pub expired_on: String,
}

/// An audit result split by the project's acceptance records.
#[derive(Debug, Default)]
pub struct FindingTriage<'a> {
    /// Findings that count: not accepted, or accepted with a lapsed expiry.
    pub live: Vec<&'a Vulnerability>,
    /// Findings accepted by the registry's `audit_exceptions` or an active
    /// `.supply-chain-allow.json` entry.
    pub accepted: Vec<AcceptedFinding<'a>>,
    /// Allowlist acceptances past their expiry (their findings are in `live`).
    pub lapsed: Vec<LapsedFinding<'a>>,
}

/// Split `result` by the project's acceptance records.
///
/// A finding matches a record when any of its identifiers (its ID or an alias
/// the scanner reported, e.g. PYSEC, CVE and GHSA for one advisory) names it,
/// case-insensitively. Two records apply:
/// - the repository's `.supply-chain-allow.json` (the preferred record: a
///   reason and an expiry, committed to git), with the supply-chain scan's
///   semantics: an active entry accepts, a lapsed one resurfaces the finding;
/// - the registry's `audit_exceptions` (kept for compatibility; no expiry).
///
/// Findings with no identifier are always live. Each acceptance is logged at
/// info level so suppression is never silent.
#[must_use]
pub fn triage_findings<'a>(
    result: &'a AuditResult,
    exceptions: &[String],
    allowlist: &SupplyChainAllowlist,
    today: chrono::NaiveDate,
) -> FindingTriage<'a> {
    let mut triage = FindingTriage::default();
    for finding in &result.vulnerabilities {
        let ids: Vec<&str> = finding.ids().collect();
        if ids.is_empty() {
            triage.live.push(finding);
            continue;
        }
        match allowlist.decide_any(&ids, today) {
            AllowDecision::Active { reason } => {
                tracing::info!(ids = ?ids, %reason, "finding accepted by .supply-chain-allow.json");
                triage.accepted.push(AcceptedFinding { finding, reason });
                continue;
            }
            AllowDecision::Expired { expired_on, .. } => {
                tracing::warn!(ids = ?ids, %expired_on, "allowlist acceptance has lapsed");
                triage.lapsed.push(LapsedFinding {
                    finding,
                    expired_on,
                });
                triage.live.push(finding);
                continue;
            }
            AllowDecision::NotListed => {}
        }
        if ids.iter().any(|id| exceptions.iter().any(|e| e.eq_ignore_ascii_case(id))) {
            tracing::info!(ids = ?ids, "finding accepted by registry audit_exceptions");
            triage.accepted.push(AcceptedFinding {
                finding,
                reason: "audit_exceptions".to_string(),
            });
            continue;
        }
        triage.live.push(finding);
    }
    triage
}

/// Read the project's `.supply-chain-allow.json`. A missing file is an empty
/// allowlist; a malformed one is treated as empty with a warning, so every
/// advisory surfaces rather than none.
#[must_use]
pub fn read_allowlist_or_empty(project: &str, path: &Path) -> SupplyChainAllowlist {
    match foundry_sdk::supply_chain::read_allowlist(path) {
        Ok(allowlist) => allowlist,
        Err(e) => {
            tracing::warn!(
                %project,
                error = %e,
                "unreadable .supply-chain-allow.json; treating as empty (all advisories surface)"
            );
            SupplyChainAllowlist::default()
        }
    }
}

/// A short, human-readable note on accepted and lapsed findings for a block's
/// result line, e.g. `"; accepted: PYSEC-2026-311 (allowlist)"`. Empty when
/// nothing was accepted or lapsed.
#[must_use]
pub fn acceptance_note(triage: &FindingTriage<'_>) -> String {
    let name = |v: &Vulnerability| v.cve.clone().unwrap_or_else(|| v.package.clone());
    let mut note = String::new();
    if !triage.accepted.is_empty() {
        let items: Vec<String> = triage
            .accepted
            .iter()
            .map(|a| {
                let source = if a.reason == "audit_exceptions" {
                    "audit_exceptions"
                } else {
                    "allowlist"
                };
                format!("{} ({source})", name(a.finding))
            })
            .collect();
        note.push_str("; accepted: ");
        note.push_str(&items.join(", "));
    }
    if !triage.lapsed.is_empty() {
        let items: Vec<String> = triage
            .lapsed
            .iter()
            .map(|l| format!("{} (expired {})", name(l.finding), l.expired_on))
            .collect();
        note.push_str("; lapsed acceptance: ");
        note.push_str(&items.join(", "));
    }
    note
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

    // --- Elixir (mix_audit) ---

    #[test]
    fn elixir_uses_mix_deps_audit() {
        let (cmd, args) = audit_command(&Stack::Elixir);
        assert_eq!(cmd, "mix");
        assert_eq!(args, ["deps.audit", "--format=json"]);
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    const MIX_WITH_AUDIT: &str =
        "defp deps do\n  [{:mix_audit, \"~> 2.1\", only: [:dev, :test], runtime: false}]\nend\n";
    const MIX_WITHOUT_AUDIT: &str = "defp deps do\n  [{:jason, \"~> 1.4\"}]\nend\n";

    #[test]
    fn mix_projects_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mix.exs", MIX_WITH_AUDIT);
        assert_eq!(mix_audit_projects(dir.path()), [dir.path().to_path_buf()]);
    }

    #[test]
    fn mix_projects_in_a_repo_without_a_root_mix_exs() {
        // The bedrock layout: apps/bedrock and vendor/roost opt into mix_audit;
        // apps/workshop_executive, a prototype and fixtures do not.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "apps/bedrock/mix.exs", MIX_WITH_AUDIT);
        write(dir.path(), "apps/workshop_executive/mix.exs", MIX_WITHOUT_AUDIT);
        write(dir.path(), "vendor/roost/mix.exs", MIX_WITH_AUDIT);
        write(dir.path(), "vendor/roost/fixtures/phoenix_app/mix.exs", MIX_WITHOUT_AUDIT);
        write(dir.path(), "prototypes/harness/mix.exs", MIX_WITHOUT_AUDIT);

        assert_eq!(
            mix_audit_projects(dir.path()),
            [
                dir.path().join("apps/bedrock"),
                dir.path().join("vendor/roost")
            ]
        );
    }

    #[test]
    fn mix_projects_never_come_from_dependency_or_build_trees() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mix.exs", MIX_WITH_AUDIT);
        write(dir.path(), "deps/some_lib/mix.exs", MIX_WITH_AUDIT);
        write(dir.path(), "_build/dev/lib/x/mix.exs", MIX_WITH_AUDIT);
        write(dir.path(), "assets/node_modules/y/mix.exs", MIX_WITH_AUDIT);
        write(dir.path(), ".elixir_ls/z/mix.exs", MIX_WITH_AUDIT);

        assert_eq!(mix_audit_projects(dir.path()), [dir.path().to_path_buf()]);
    }

    #[test]
    fn a_commented_out_mix_audit_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mix.exs", "# {:mix_audit, \"~> 2.1\"}\n");
        assert!(mix_audit_projects(dir.path()).is_empty());
    }

    #[tokio::test]
    async fn elixir_without_any_mix_audit_project_is_not_scanned() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "mix.exs", MIX_WITHOUT_AUDIT);
        let result = run_audit(dir.path(), &Stack::Elixir).await.unwrap();
        assert!(
            result.error.as_deref().is_some_and(|e| e.contains("mix_audit")),
            "{:?}",
            result.error
        );
    }

    #[test]
    fn mix_audit_exit_1_means_read_the_report() {
        assert!(is_audit_vuln_exit_code(&Stack::Elixir, 1));
        assert!(!is_audit_vuln_exit_code(&Stack::Elixir, 2));
    }

    /// Real clean output from `mix_audit` 2.1 on ops-01 (`coach_phoenix`,
    /// `roost`, `bedrock-system-template`, 2026-09-25).
    const MIX_AUDIT_CLEAN: &str = r#"{"pass":true,"vulnerabilities":[]}"#;

    /// `mix_audit`'s JSON encoding of its `Report`, `Vulnerability`, `Advisory`
    /// and `Dependency` structs, with an advisory from its advisory mirror.
    const MIX_AUDIT_ONE_FINDING: &str = r#"{"pass":false,"vulnerabilities":[{"advisory":{"id":"GHSA-9mhv-8h52-q7q2","package":"absinthe","disclosure_date":"2026-05-14","url":"https://github.com/advisories/GHSA-9mhv-8h52-q7q2","title":"Absinthe: Quadratic fragment-name uniqueness check","description":"...","vulnerable_version_ranges":[">= 1.2.0, < 1.10.2"],"first_patched_versions":["1.10.2"],"severity":"high"},"dependency":{"package":"absinthe","version":"1.7.8","lockfile":"/p/mix.lock"}}]}"#;

    #[test]
    fn parse_mix_audit_clean_report() {
        let result = parse_mix_audit(MIX_AUDIT_CLEAN);
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_mix_audit_finding() {
        let result = parse_mix_audit(MIX_AUDIT_ONE_FINDING);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 1);
        let v = &result.vulnerabilities[0];
        assert_eq!(v.cve.as_deref(), Some("GHSA-9mhv-8h52-q7q2"));
        assert_eq!(v.package, "absinthe");
        assert_eq!(v.version.as_deref(), Some("1.7.8"));
        assert_eq!(v.severity.as_deref(), Some("high"));
        assert_eq!(v.fix_version.as_deref(), Some("1.10.2"));
    }

    #[test]
    fn parse_mix_audit_skips_compile_preamble() {
        // Mix prints dependency compilation to stdout before the task runs.
        let output = format!(
            "==> yaml_elixir\nCompiling 6 files (.ex)\nGenerated yaml_elixir app\n==> mix_audit\nCompiling 15 files (.ex)\nGenerated mix_audit app\n{MIX_AUDIT_CLEAN}\n"
        );
        let result = parse_mix_audit(&output);
        assert!(result.error.is_none(), "{:?}", result.error);
    }

    #[test]
    fn parse_mix_audit_without_a_report_is_an_error() {
        for output in [
            "",
            "** (Mix) The task \"deps.audit\" could not be found",
            "{\"status\": 1}",
        ] {
            let result = parse_mix_audit(output);
            assert!(result.error.is_some(), "{output:?} must not read as clean");
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "matches the shell gateway's return type"
    )]
    fn ran(stdout: &str, exit_code: i32) -> anyhow::Result<crate::shell::CommandResult> {
        Ok(crate::shell::CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code,
            success: exit_code == 0,
        })
    }

    #[test]
    fn mix_runs_merge_findings_across_projects() {
        let result = merge_mix_runs(vec![
            ("apps/bedrock".to_string(), ran(MIX_AUDIT_ONE_FINDING, 1)),
            ("vendor/roost".to_string(), ran(MIX_AUDIT_ONE_FINDING, 1)),
            ("apps/other".to_string(), ran(MIX_AUDIT_CLEAN, 0)),
        ]);
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.vulnerabilities.len(), 1, "same advisory on the same version once");
    }

    #[test]
    fn one_failed_mix_project_fails_the_whole_scan_and_names_it() {
        let result = merge_mix_runs(vec![
            ("apps/bedrock".to_string(), ran(MIX_AUDIT_CLEAN, 0)),
            (
                "vendor/roost".to_string(),
                ran("** (Mix) The task \"deps.audit\" could not be found", 1),
            ),
        ]);
        let err = result.error.expect("a partial scan is not a clean scan");
        assert!(err.contains("vendor/roost"), "{err}");
    }

    #[test]
    fn mix_run_with_unexpected_exit_code_is_an_error() {
        let result = merge_mix_runs(vec![(".".to_string(), ran(MIX_AUDIT_CLEAN, 2))]);
        assert!(result.error.as_deref().is_some_and(|e| e.contains("exit 2")));
    }

    #[test]
    fn mix_run_that_could_not_spawn_is_an_error() {
        let result = merge_mix_runs(vec![(".".to_string(), Err(anyhow::anyhow!("mix not found")))]);
        assert!(result.error.as_deref().is_some_and(|e| e.contains("mix not found")));
    }

    // --- exit-code convention ---

    #[test]
    fn npm_exit_code_1_is_not_failure() {
        assert!(is_audit_vuln_exit_code(&Stack::TypeScript, 1));
    }

    #[test]
    fn pip_audit_exit_code_1_is_not_failure() {
        // pip-audit exits 1 when vulnerabilities are found; the JSON is still
        // on stdout, so this must be parsed, not discarded as a tool failure.
        assert!(is_audit_vuln_exit_code(&Stack::Python, 1));
    }

    #[test]
    fn npm_exit_code_2_is_failure() {
        assert!(!is_audit_vuln_exit_code(&Stack::TypeScript, 2));
    }

    #[test]
    fn python_exit_code_2_is_failure() {
        assert!(!is_audit_vuln_exit_code(&Stack::Python, 2));
    }

    #[test]
    fn cargo_audit_exit_code_1_is_not_failure() {
        // cargo-audit exits 1 when vulnerabilities are found; its valid JSON
        // report must still reach the Rust parser.
        assert!(is_audit_vuln_exit_code(&Stack::Rust, 1));
    }

    #[test]
    fn rust_exit_code_2_is_failure() {
        assert!(!is_audit_vuln_exit_code(&Stack::Rust, 2));
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
              "fixAvailable": {"name": "direct-parent", "version": "4.17.21", "isSemVerMajor": false}
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
        assert_eq!(
            vuln.fix_package.as_deref(),
            Some("direct-parent"),
            "fixAvailable.name → fix_package"
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
    fn parse_pip_audit_carries_aliases() {
        let json = r#"{"dependencies": [{"name": "chromadb", "version": "1.5.9", "vulns": [
            {"id": "PYSEC-2026-311", "fix_versions": [], "aliases": ["CVE-2026-45829", "GHSA-f4j7-r4q5-qw2c"]}
        ]}], "fixes": []}"#;
        let result = parse_pip_audit(json);
        let v = &result.vulnerabilities[0];
        assert_eq!(v.cve.as_deref(), Some("PYSEC-2026-311"));
        assert_eq!(v.aliases, ["CVE-2026-45829", "GHSA-f4j7-r4q5-qw2c"]);
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

    // --- Swift (osv-scanner) ---

    #[test]
    fn swift_uses_osv_scanner_on_package_resolved() {
        let (cmd, args) = audit_command(&Stack::Swift);
        assert_eq!(cmd, "osv-scanner");
        assert_eq!(
            args,
            [
                "scan",
                "source",
                "--format",
                "json",
                "--lockfile",
                "Package.resolved"
            ]
        );
    }

    #[test]
    fn swift_plan_runs_osv_scanner_when_package_resolved_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Package.resolved"), "{}").unwrap();
        assert!(matches!(
            audit_plan(dir.path(), &Stack::Swift),
            AuditPlan::Stdout { ref command, .. } if command == "osv-scanner"
        ));
    }

    #[tokio::test]
    async fn swift_without_package_resolved_is_not_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_audit(dir.path(), &Stack::Swift).await.unwrap();
        assert!(result.vulnerabilities.is_empty());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|e| e.contains("Package.resolved not found")),
            "{:?}",
            result.error
        );
    }

    #[test]
    fn osv_scanner_exit_code_1_is_not_failure() {
        assert!(is_audit_vuln_exit_code(&Stack::Swift, 1));
    }

    #[test]
    fn osv_scanner_error_exit_codes_are_failures() {
        // 127: general error; 128: no packages found.
        assert!(!is_audit_vuln_exit_code(&Stack::Swift, 127));
        assert!(!is_audit_vuln_exit_code(&Stack::Swift, 128));
    }

    /// Trimmed from a real osv-scanner 2.6.0 run against a Package.resolved
    /// pinning swift-nio-http2 1.19.0.
    const OSV_SWIFT_NIO_HTTP2: &str = r#"
    {
      "results": [{
        "source": {"path": "/p/Package.resolved", "type": "lockfile"},
        "packages": [{
          "package": {"name": "github.com/apple/swift-nio-http2", "version": "1.19.0", "ecosystem": "SwiftURL"},
          "vulnerabilities": [
            {"id": "GHSA-w3f6-pc54-gfw7", "aliases": ["CVE-2022-24667"], "affected": [{
              "package": {"ecosystem": "SwiftURL", "name": "github.com/apple/swift-nio-http2"},
              "ranges": [{"type": "SEMVER", "events": [{"introduced": "1.0.0"}, {"fixed": "1.19.2"}]}]
            }]},
            {"id": "GHSA-qppj-fm5r-hxr3", "aliases": ["CVE-2023-44487"], "affected": [
              {"package": {"ecosystem": "Go", "name": "golang.org/x/net"},
               "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": "0.17.0"}]}]},
              {"package": {"ecosystem": "SwiftURL", "name": "github.com/apple/swift-nio-http2"},
               "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}, {"fixed": "1.28.0"}]}]}
            ]},
            {"id": "GHSA-xvr7-p2c6-j83w", "affected": [{
              "package": {"ecosystem": "SwiftURL", "name": "github.com/apple/swift-nio-http2"},
              "ranges": [{"type": "SEMVER", "events": [{"introduced": "0"}]}]
            }]}
          ],
          "groups": [
            {"ids": ["GHSA-w3f6-pc54-gfw7"], "aliases": ["CVE-2022-24667", "GHSA-w3f6-pc54-gfw7"], "max_severity": "7.5"},
            {"ids": ["GHSA-qppj-fm5r-hxr3"], "aliases": ["BIT-golang-2023-44487", "CVE-2023-44487", "GHSA-qppj-fm5r-hxr3"], "max_severity": "6.9"},
            {"ids": ["GHSA-xvr7-p2c6-j83w"], "aliases": ["GHSA-xvr7-p2c6-j83w"], "max_severity": "6.3"}
          ]
        }]
      }]
    }"#;

    #[test]
    fn parse_osv_scanner_emits_one_finding_per_group() {
        let result = parse_osv_scanner(OSV_SWIFT_NIO_HTTP2);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 3);

        let first = &result.vulnerabilities[0];
        assert_eq!(first.cve.as_deref(), Some("CVE-2022-24667"), "CVE alias preferred");
        assert_eq!(first.package, "github.com/apple/swift-nio-http2");
        assert_eq!(first.version.as_deref(), Some("1.19.0"));
        assert_eq!(first.severity.as_deref(), Some("high"), "max_severity 7.5 → high");
        assert_eq!(first.fix_version.as_deref(), Some("1.19.2"));
        assert!(first.fix_package.is_none());
    }

    #[test]
    fn parse_osv_scanner_takes_fix_version_for_this_package_only() {
        let result = parse_osv_scanner(OSV_SWIFT_NIO_HTTP2);
        let multi = &result.vulnerabilities[1];
        assert_eq!(multi.cve.as_deref(), Some("CVE-2023-44487"));
        assert_eq!(
            multi.fix_version.as_deref(),
            Some("1.28.0"),
            "the Go entry's fixed version must not leak into the Swift finding"
        );
        assert_eq!(multi.severity.as_deref(), Some("medium"));
    }

    #[test]
    fn parse_osv_scanner_carries_the_other_ids_as_aliases() {
        let result = parse_osv_scanner(OSV_SWIFT_NIO_HTTP2);
        let first = &result.vulnerabilities[0];
        assert_eq!(first.cve.as_deref(), Some("CVE-2022-24667"));
        assert_eq!(first.aliases, ["GHSA-w3f6-pc54-gfw7"], "every other id, primary excluded");
    }

    #[test]
    fn parse_cargo_audit_carries_advisory_aliases() {
        let json = r#"{"vulnerabilities": {"list": [{
            "advisory": {"id": "RUSTSEC-2026-0001", "package": "c", "aliases": ["CVE-2026-9", "GHSA-y"]},
            "package": {"name": "c", "version": "1.0.0"}
        }]}}"#;
        let result = parse_cargo_audit(json);
        assert_eq!(result.vulnerabilities[0].aliases, ["CVE-2026-9", "GHSA-y"]);
    }

    #[test]
    fn parse_osv_scanner_without_cve_alias_uses_group_id_and_no_fix() {
        let result = parse_osv_scanner(OSV_SWIFT_NIO_HTTP2);
        let no_cve = &result.vulnerabilities[2];
        assert_eq!(no_cve.cve.as_deref(), Some("GHSA-xvr7-p2c6-j83w"));
        assert!(no_cve.fix_version.is_none(), "no fixed event → policy call");
    }

    #[test]
    fn parse_osv_scanner_falls_back_to_vulnerabilities_without_groups() {
        let json = r#"{"results": [{"packages": [{
            "package": {"name": "github.com/x/y", "version": "1.0.0"},
            "vulnerabilities": [{"id": "GHSA-aaaa", "aliases": ["CVE-2026-1"]}]
        }]}]}"#;
        let result = parse_osv_scanner(json);
        assert_eq!(result.vulnerabilities.len(), 1);
        assert_eq!(result.vulnerabilities[0].cve.as_deref(), Some("CVE-2026-1"));
    }

    #[test]
    fn parse_osv_scanner_clean_scan() {
        // Real clean-run output shape from osv-scanner 2.6.0.
        let json = r#"{"results": [], "experimental_config": {"licenses": {"summary": false, "allowlist": null}}}"#;
        let result = parse_osv_scanner(json);
        assert!(result.error.is_none());
        assert!(result.vulnerabilities.is_empty());
    }

    #[test]
    fn parse_osv_scanner_records_unexpected_shape_and_bad_json() {
        assert!(parse_osv_scanner(r#"{"status": "ok"}"#).error.is_some());
        assert!(parse_osv_scanner("").error.is_some(), "no output is not a clean scan");
        assert!(parse_osv_scanner("not json").error.is_some());
    }

    // --- Kotlin (OWASP Dependency-Check via the project's Gradle wrapper) ---

    #[test]
    fn kotlin_plan_runs_project_gradle_wrapper_with_long_timeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gradlew"), "").unwrap();
        let plan = audit_plan(dir.path(), &Stack::Kotlin);
        let AuditPlan::ReportFile {
            command,
            args,
            reports,
            timeout,
            min_cvss,
        } = plan
        else {
            panic!("Kotlin reads a report file: {plan:?}");
        };
        assert_eq!(command, dir.path().join("gradlew").to_string_lossy());
        // `--rerun` forces the task to execute: Gradle otherwise reports it
        // UP-TO-DATE, leaves the previous report in place, and the scan
        // never checks the current vulnerability data.
        assert_eq!(
            args,
            [
                "dependencyCheckAggregate",
                "--rerun",
                "--no-parallel",
                "--no-daemon"
            ]
        );
        assert_eq!(
            reports,
            [
                dir.path().join("build/reports/dependency-check-report.json"),
                dir.path().join("build/reports/dependency-check/dependency-check-report.json"),
            ],
            "Dependency-Check 12 and 13 report locations"
        );
        assert!(timeout > Duration::from_secs(300), "longer than the shell default");
        assert_eq!(min_cvss, None, "no build file, so no project threshold");
    }

    #[tokio::test]
    async fn kotlin_without_gradle_wrapper_is_not_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();
        assert!(result.vulnerabilities.is_empty());
        assert!(
            result.error.as_deref().is_some_and(|e| e.contains("gradlew not found")),
            "{:?}",
            result.error
        );
    }

    #[test]
    fn gradle_exit_code_1_means_read_the_report() {
        assert!(is_audit_vuln_exit_code(&Stack::Kotlin, 1));
        assert!(!is_audit_vuln_exit_code(&Stack::Kotlin, 2));
    }

    /// A stand-in `gradlew` that runs `body` as a shell script. The real
    /// wrapper is a shell script too, so this exercises the same spawn path.
    ///
    /// The file is written by a child `sh`, never through a file descriptor in
    /// this process: tests run in parallel, and a sibling test forking while we
    /// held the script open for writing would make exec fail with ETXTBSY
    /// ("Text file busy") on Linux.
    #[cfg(unix)]
    fn fake_gradlew(dir: &Path, body: &str) {
        let script = format!("#!/bin/sh\n{body}\n");
        let status = std::process::Command::new("sh")
            .current_dir(dir)
            .args([
                "-c",
                "printf '%s' \"$1\" > gradlew && chmod 755 gradlew",
                "sh",
                &script,
            ])
            .status()
            .unwrap();
        assert!(status.success(), "writing the fake gradlew failed");
    }

    const DEPENDENCY_CHECK_ONE_FINDING: &str = r#"{
      "reportSchema": "1.1",
      "dependencies": [
        {"fileName": "clean.jar", "packages": [{"id": "pkg:maven/org.example/clean@1.0.0"}]},
        {"fileName": "kotlin-stdlib-2.2.0.jar",
         "packages": [{"id": "pkg:maven/org.jetbrains.kotlin/kotlin-stdlib@2.2.0"}],
         "vulnerabilities": [{"source": "NVD", "name": "CVE-2026-53914", "severity": "CRITICAL",
                              "cvssv3": {"baseScore": 9.8}}],
         "suppressedVulnerabilities": [{"name": "CVE-2020-29582", "severity": "MEDIUM"}]}
      ]
    }"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn kotlin_fresh_report_after_exit_1_is_parsed_as_findings() {
        let dir = tempfile::tempdir().unwrap();
        let report_dir = dir.path().join("build/reports");
        std::fs::create_dir_all(&report_dir).unwrap();
        std::fs::write(dir.path().join("report.json"), DEPENDENCY_CHECK_ONE_FINDING).unwrap();
        // Writes the report, then fails the build as failBuildOnCVSS does.
        fake_gradlew(
            dir.path(),
            "cp report.json build/reports/dependency-check-report.json\necho 'BUILD FAILED' >&2\nexit 1",
        );

        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();

        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.vulnerabilities.len(), 1);
        assert_eq!(result.vulnerabilities[0].cve.as_deref(), Some("CVE-2026-53914"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kotlin_exit_1_with_only_a_stale_report_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let report_dir = dir.path().join("build/reports");
        std::fs::create_dir_all(&report_dir).unwrap();
        let report = report_dir.join("dependency-check-report.json");
        std::fs::write(&report, DEPENDENCY_CHECK_ONE_FINDING).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&report)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // A broken build: exits 1 and never reaches the scan.
        fake_gradlew(dir.path(), "echo 'Could not resolve plugin' >&2\nexit 1");

        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();

        assert!(result.vulnerabilities.is_empty(), "stale findings must not be reported");
        let err = result.error.expect("a stale report is not a scan");
        assert!(err.contains("without writing a fresh report"), "{err}");
        assert!(err.contains("Could not resolve plugin"), "stderr carried through: {err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kotlin_clean_exit_without_report_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        fake_gradlew(dir.path(), "exit 0");

        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();

        assert!(result.error.is_some(), "exit 0 without a report is not a clean scan");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kotlin_other_exit_codes_are_failures() {
        let dir = tempfile::tempdir().unwrap();
        fake_gradlew(dir.path(), "exit 2");

        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();

        assert!(result.error.as_deref().is_some_and(|e| e.contains("exit 2")));
    }

    /// Write `contents` at `rel` under `dir` with the given modification time.
    fn report_at(dir: &Path, rel: &str, contents: &str, modified: SystemTime) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    const LEGACY_REPORT: &str = "build/reports/dependency-check-report.json";
    const DC13_REPORT: &str = "build/reports/dependency-check/dependency-check-report.json";

    #[test]
    fn fresh_report_found_in_the_dependency_check_13_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let started = SystemTime::now() - Duration::from_secs(60);
        report_at(dir.path(), DC13_REPORT, "dc13", SystemTime::now());
        let reports = [dir.path().join(LEGACY_REPORT), dir.path().join(DC13_REPORT)];

        assert_eq!(read_newest_fresh_report(&reports, started).as_deref(), Some("dc13"));
    }

    #[test]
    fn fresh_report_found_in_the_legacy_location() {
        let dir = tempfile::tempdir().unwrap();
        let started = SystemTime::now() - Duration::from_secs(60);
        report_at(dir.path(), LEGACY_REPORT, "legacy", SystemTime::now());
        let reports = [dir.path().join(LEGACY_REPORT), dir.path().join(DC13_REPORT)];

        assert_eq!(read_newest_fresh_report(&reports, started).as_deref(), Some("legacy"));
    }

    #[test]
    fn newest_fresh_report_wins_when_both_layouts_exist() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let started = now - Duration::from_secs(60);
        report_at(dir.path(), LEGACY_REPORT, "older", now - Duration::from_secs(30));
        report_at(dir.path(), DC13_REPORT, "newer", now);
        let reports = [dir.path().join(LEGACY_REPORT), dir.path().join(DC13_REPORT)];

        assert_eq!(read_newest_fresh_report(&reports, started).as_deref(), Some("newer"));
    }

    #[test]
    fn a_stale_report_never_beats_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let started = now - Duration::from_secs(60);
        report_at(dir.path(), LEGACY_REPORT, "stale", now - Duration::from_secs(3600));
        report_at(dir.path(), DC13_REPORT, "fresh", now);
        let reports = [dir.path().join(LEGACY_REPORT), dir.path().join(DC13_REPORT)];

        assert_eq!(read_newest_fresh_report(&reports, started).as_deref(), Some("fresh"));
    }

    #[test]
    fn no_report_when_neither_location_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let started = now - Duration::from_secs(60);
        report_at(dir.path(), LEGACY_REPORT, "stale", now - Duration::from_secs(3600));
        report_at(dir.path(), DC13_REPORT, "stale", now - Duration::from_secs(7200));
        let reports = [dir.path().join(LEGACY_REPORT), dir.path().join(DC13_REPORT)];

        assert_eq!(read_newest_fresh_report(&reports, started), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kotlin_clean_exit_with_a_dependency_check_13_report_is_parsed() {
        // The mojentic-kt case: Dependency-Check 13 exits 0 and writes into
        // build/reports/dependency-check/.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.json"), DEPENDENCY_CHECK_ONE_FINDING).unwrap();
        fake_gradlew(
            dir.path(),
            "mkdir -p build/reports/dependency-check\ncp report.json build/reports/dependency-check/dependency-check-report.json\nexit 0",
        );

        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();

        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.vulnerabilities.len(), 1);
    }

    // --- Kotlin: the project's failBuildOnCVSS threshold decides what counts ---

    #[test]
    fn fail_build_threshold_read_from_kotlin_dsl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("build.gradle.kts"),
            "dependencyCheck {\n    failBuildOnCVSS = 7.0f\n    formats = listOf(\"JSON\")\n}\n",
        )
        .unwrap();
        assert_eq!(fail_build_on_cvss(dir.path()), Some(7.0));
    }

    #[test]
    fn fail_build_threshold_read_from_groovy_dsl() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("build.gradle"),
            "dependencyCheck {\n  failBuildOnCVSS 8\n}\n",
        )
        .unwrap();
        assert_eq!(fail_build_on_cvss(dir.path()), Some(8.0));
    }

    #[test]
    fn fail_build_threshold_absent_or_not_a_literal_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(fail_build_on_cvss(dir.path()), None, "no build file");
        std::fs::write(dir.path().join("build.gradle.kts"), "dependencyCheck { }\n").unwrap();
        assert_eq!(fail_build_on_cvss(dir.path()), None, "no setting");
        std::fs::write(
            dir.path().join("build.gradle.kts"),
            "dependencyCheck { failBuildOnCVSS = threshold.toFloat() }\n",
        )
        .unwrap();
        assert_eq!(fail_build_on_cvss(dir.path()), None, "not a literal");
    }

    const DEPENDENCY_CHECK_MIXED_SCORES: &str = r#"{"dependencies": [
      {"fileName": "a.jar", "packages": [{"id": "pkg:maven/g/a@1"}], "vulnerabilities": [
        {"name": "CVE-MEDIUM", "severity": "MEDIUM", "cvssv3": {"baseScore": 5.5}},
        {"name": "CVE-AT", "severity": "HIGH", "cvssv3": {"baseScore": 7.0}},
        {"name": "CVE-V2-HIGH", "severity": "HIGH", "cvssv2": {"score": 7.5}, "cvssv3": {"baseScore": 6.1}},
        {"name": "CVE-UNSCORED", "severity": "MEDIUM"}
      ]}
    ]}"#;

    #[test]
    fn findings_below_the_project_threshold_do_not_count() {
        let result = parse_dependency_check(DEPENDENCY_CHECK_MIXED_SCORES, Some(7.0));
        let cves: Vec<&str> =
            result.vulnerabilities.iter().filter_map(|v| v.cve.as_deref()).collect();
        // Dependency-Check fails the build on the highest CVSS score it has for
        // a finding, so the same rule decides here. Unscored findings cannot be
        // judged and are kept.
        assert_eq!(cves, ["CVE-AT", "CVE-V2-HIGH", "CVE-UNSCORED"]);
    }

    #[test]
    fn below_threshold_findings_are_counted_for_the_result_line() {
        let result = parse_dependency_check(DEPENDENCY_CHECK_MIXED_SCORES, Some(7.0));
        assert_eq!(result.below_threshold, 1, "CVE-MEDIUM left out");
    }

    #[test]
    fn without_a_threshold_every_live_finding_counts() {
        let result = parse_dependency_check(DEPENDENCY_CHECK_MIXED_SCORES, None);
        assert_eq!(result.vulnerabilities.len(), 4);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kotlin_scan_with_only_sub_threshold_findings_is_clean() {
        // mojentic-kt on 2026-09-24: exit 0, 22 MEDIUM findings, failBuildOnCVSS = 7.0.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("build.gradle.kts"),
            "dependencyCheck {\n    failBuildOnCVSS = 7.0f\n}\n",
        )
        .unwrap();
        let report = r#"{"dependencies": [{"fileName": "kotlin-stdlib-2.0.21.jar",
            "packages": [{"id": "pkg:maven/org.jetbrains.kotlin/kotlin-stdlib@2.0.21"}],
            "vulnerabilities": [{"name": "CVE-2020-29582", "severity": "MEDIUM", "cvssv3": {"baseScore": 5.3}}],
            "suppressedVulnerabilities": [{"name": "CVE-2026-53914", "severity": "CRITICAL", "cvssv3": {"baseScore": 9.8}}]}]}"#;
        std::fs::write(dir.path().join("report.json"), report).unwrap();
        fake_gradlew(
            dir.path(),
            "mkdir -p build/reports/dependency-check\ncp report.json build/reports/dependency-check/dependency-check-report.json\nexit 0",
        );

        let result = run_audit(dir.path(), &Stack::Kotlin).await.unwrap();

        assert!(result.error.is_none(), "{:?}", result.error);
        assert!(result.vulnerabilities.is_empty(), "{:?}", result.vulnerabilities);
    }

    #[test]
    fn parse_dependency_check_reports_live_findings_only() {
        let result = parse_dependency_check(DEPENDENCY_CHECK_ONE_FINDING, None);
        assert!(result.error.is_none());
        assert_eq!(result.vulnerabilities.len(), 1, "suppressed findings are the project's call");

        let vuln = &result.vulnerabilities[0];
        assert_eq!(vuln.cve.as_deref(), Some("CVE-2026-53914"));
        assert_eq!(vuln.package, "org.jetbrains.kotlin:kotlin-stdlib");
        assert_eq!(vuln.version.as_deref(), Some("2.2.0"));
        assert_eq!(vuln.severity.as_deref(), Some("critical"));
        assert!(vuln.fix_version.is_none(), "Dependency-Check names no fix → policy call");
    }

    #[test]
    fn parse_dependency_check_deduplicates_repeated_findings() {
        let json = r#"{"dependencies": [
          {"fileName": "a.jar", "packages": [{"id": "pkg:maven/g/a@1?type=jar"}],
           "vulnerabilities": [{"name": "CVE-1", "severity": "HIGH"}, {"name": "CVE-1", "severity": "HIGH"}]},
          {"fileName": "a.jar", "packages": [{"id": "pkg:maven/g/a@1"}],
           "vulnerabilities": [{"name": "CVE-1", "severity": "HIGH"}]}
        ]}"#;
        let result = parse_dependency_check(json, None);
        assert_eq!(result.vulnerabilities.len(), 1);
        assert_eq!(result.vulnerabilities[0].package, "g:a");
        assert_eq!(result.vulnerabilities[0].version.as_deref(), Some("1"));
    }

    #[test]
    fn parse_dependency_check_falls_back_to_file_name() {
        let json = r#"{"dependencies": [
          {"fileName": "vendored.jar", "vulnerabilities": [{"name": "CVE-2", "severity": "LOW"}]}
        ]}"#;
        let result = parse_dependency_check(json, None);
        assert_eq!(result.vulnerabilities[0].package, "vendored.jar");
        assert!(result.vulnerabilities[0].version.is_none());
    }

    #[test]
    fn parse_dependency_check_records_unexpected_shape_and_bad_json() {
        assert!(parse_dependency_check(r#"{"scanInfo": {}}"#, None).error.is_some());
        assert!(parse_dependency_check("", None).error.is_some());
        assert!(parse_dependency_check("{nope", None).error.is_some());
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("aé", 1), "", "never splits a multi-byte char");
    }

    // --- C++ ---

    #[test]
    fn cpp_is_explicitly_not_audited() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(audit_plan(dir.path(), &Stack::Cpp), AuditPlan::NotAudited);
    }

    // --- filter_audit_exceptions ---

    fn vuln(cve: Option<&str>) -> Vulnerability {
        Vulnerability {
            cve: cve.map(str::to_owned),
            severity: None,
            package: "test-pkg".to_string(),
            version: None,
            fix_version: None,
            fix_package: None,
            aliases: Vec::new(),
        }
    }

    fn result_with(vulns: Vec<Vulnerability>) -> AuditResult {
        AuditResult {
            vulnerabilities: vulns,
            error: None,
            below_threshold: 0,
        }
    }

    fn day(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn allow(entries: &[(&str, Option<&str>)]) -> SupplyChainAllowlist {
        SupplyChainAllowlist {
            version: 1,
            allowed: entries
                .iter()
                .map(|(cve, expires)| foundry_sdk::supply_chain::AllowEntry {
                    cve: (*cve).to_string(),
                    reason: "chromadb path not exposed".to_string(),
                    expires: expires.map(str::to_string),
                })
                .collect(),
        }
    }

    fn triage_ids(
        result: &AuditResult,
        exceptions: &[String],
        allowlist: &SupplyChainAllowlist,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        let t = triage_findings(result, exceptions, allowlist, day("2026-09-25"));
        let id = |v: &Vulnerability| v.cve.clone().unwrap_or_default();
        (
            t.live.iter().map(|v| id(v)).collect(),
            t.accepted.iter().map(|a| id(a.finding)).collect(),
            t.lapsed.iter().map(|a| id(a.finding)).collect(),
        )
    }

    #[test]
    fn audit_exception_suppresses_matching_cve() {
        let result = result_with(vec![vuln(Some("CVE-2026-45829"))]);
        let (live, accepted, _) =
            triage_ids(&result, &["CVE-2026-45829".to_string()], &SupplyChainAllowlist::default());
        assert!(live.is_empty());
        assert_eq!(accepted, ["CVE-2026-45829"]);
    }

    #[test]
    fn audit_exception_is_case_insensitive_and_keeps_non_matching() {
        let result = result_with(vec![vuln(Some("CVE-2026-45829")), vuln(Some("CVE-2026-99999"))]);
        let (live, _, _) =
            triage_ids(&result, &["cve-2026-45829".to_string()], &SupplyChainAllowlist::default());
        assert_eq!(live, ["CVE-2026-99999"]);
    }

    #[test]
    fn unnamed_findings_are_always_live() {
        let result = result_with(vec![vuln(None)]);
        let (live, _, _) = triage_ids(
            &result,
            &["CVE-2026-45829".to_string()],
            &allow(&[("CVE-2026-45829", None)]),
        );
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn allowlist_accepts_a_finding_by_alias() {
        // zk-chat/researcher-cli on 2026-09-25: pip-audit reports the PYSEC id;
        // the allowlist names the CVE.
        let mut v = vuln(Some("PYSEC-2026-311"));
        v.aliases = vec![
            "CVE-2026-45829".to_string(),
            "GHSA-f4j7-r4q5-qw2c".to_string(),
        ];
        let result = result_with(vec![v]);
        let (live, accepted, _) =
            triage_ids(&result, &[], &allow(&[("CVE-2026-45829", Some("2026-12-24"))]));
        assert!(live.is_empty(), "accepted, not vulnerable");
        assert_eq!(accepted, ["PYSEC-2026-311"]);
    }

    #[test]
    fn audit_exception_matches_an_alias_too() {
        let mut v = vuln(Some("PYSEC-2026-311"));
        v.aliases = vec!["CVE-2026-45829".to_string()];
        let result = result_with(vec![v]);
        let (live, _, _) =
            triage_ids(&result, &["CVE-2026-45829".to_string()], &SupplyChainAllowlist::default());
        assert!(live.is_empty());
    }

    #[test]
    fn a_lapsed_allowlist_entry_resurfaces_the_finding() {
        let result = result_with(vec![vuln(Some("CVE-2026-45829"))]);
        let (live, accepted, lapsed) =
            triage_ids(&result, &[], &allow(&[("CVE-2026-45829", Some("2026-09-01"))]));
        assert_eq!(live, ["CVE-2026-45829"], "lapsed acceptance is live again");
        assert!(accepted.is_empty());
        assert_eq!(lapsed, ["CVE-2026-45829"]);
    }

    // --- audit_outcome ---

    #[test]
    fn audit_outcome_err_returns_err() {
        let result = super::audit_outcome(Err(anyhow::anyhow!("spawn failed")));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("spawn failed"));
    }

    #[test]
    fn audit_outcome_audit_result_with_error_returns_err() {
        let audit = Ok(AuditResult {
            vulnerabilities: vec![],
            error: Some("tool not installed".to_string()),
            below_threshold: 0,
        });
        let result = super::audit_outcome(audit);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "tool not installed");
    }

    #[test]
    fn audit_outcome_clean_result_returns_ok() {
        let audit = Ok(AuditResult::default());
        let result = super::audit_outcome(audit);
        assert!(result.is_ok());
        assert!(result.unwrap().vulnerabilities.is_empty());
    }
}
