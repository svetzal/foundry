//! The maintain run's suppression guard.
//!
//! Maintenance has no reviewer. Twice on 2026-09-25 an agent made an advisory
//! go away by suppressing it rather than upgrading, though a fixed release
//! existed. The prompt now forbids it ([`ADVISORY_RULES`]); this guard checks
//! the result. After the maintain agent, everything the run changed relative
//! to `origin/<branch>` is scanned for new suppression entries:
//!
//! - any entry added to `.supply-chain-allow.json`;
//! - `ignore_advisories` (or an advisory ID) added to a `mix.exs`, or to a
//!   `mix_audit` skips file;
//! - a `<suppress>` added to a Dependency-Check suppressions XML;
//! - a `pip-audit --ignore-vuln` flag;
//! - an advisory ID added to `deny.toml` or `audit.toml`, or a
//!   `cargo audit --ignore` flag;
//! - an npm `overrides`/`resolutions` entry that pins a package to a version
//!   the latest supply-chain scan says is vulnerable.
//!
//! Any of these fails the run with "needs review: …". It is never green, and
//! a failed run's commits are never pushed.

use std::path::Path;

use foundry_sdk::payload::{Ecosystem, SupplyChainFinding};

use crate::dependency_updates::requirement::Requirement;
use crate::dependency_updates::version::Version;
use crate::gateway::ShellGateway;

/// Rules every coding prompt that touches dependencies carries.
pub(crate) const ADVISORY_RULES: &str = "\
Advisory rules (these override any other guidance):\n\
- Never suppress, ignore or allowlist an advisory that has a fixed release. Upgrade to the \
fixed release instead, even when that goes past the update policy's ceiling.\n\
- Never edit .supply-chain-allow.json, mix.exs ignore_advisories, Dependency-Check \
suppression files, pip-audit --ignore-vuln flags, cargo deny/audit ignore lists, npm \
overrides that pin a vulnerable version, or any equivalent suppression. Accepting an \
advisory is a human decision.\n\
- Never write in a CHANGELOG, commit message or comment that no fix exists unless you cite \
the evidence: the registry or OSV entry that shows no fixed release.\n";

/// Whether a token looks like an advisory identifier.
fn advisory_id(token: &str) -> bool {
    const PREFIXES: [&str; 7] = ["CVE-", "GHSA-", "PYSEC-", "RUSTSEC-", "OSV-", "GO-", "npm-"];
    PREFIXES.iter().any(|p| token.starts_with(p) && token.len() > p.len())
}

fn advisory_ids(line: &str) -> Vec<String> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|t| advisory_id(t))
        .map(str::to_string)
        .collect()
}

/// Suppression entries added in a unified diff (`git diff -U0`).
pub(crate) fn added_suppressions(diff: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut file = String::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        let name = Path::new(&file)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let lower_name = name.to_ascii_lowercase();
        let ids = advisory_ids(added);
        let what = if name == ".supply-chain-allow.json"
            && (!ids.is_empty() || added.contains("\"cve\""))
        {
            Some("an allowlist entry in .supply-chain-allow.json")
        } else if name == "mix.exs" && (added.contains("ignore_advisories") || !ids.is_empty()) {
            Some("ignore_advisories in mix.exs")
        } else if name.contains("mix-audit") && !ids.is_empty() {
            Some("a mix_audit skip")
        } else if Path::new(&name).extension().is_some_and(|e| e.eq_ignore_ascii_case("xml"))
            && lower_name.contains("suppress")
            && (added.contains("<suppress")
                || added.contains("<cve>")
                || added.contains("<vulnerabilityName>"))
        {
            Some("a Dependency-Check suppression")
        } else if added.contains("--ignore-vuln") {
            Some("a pip-audit --ignore-vuln flag")
        } else if (name == "deny.toml" || name == "audit.toml") && !ids.is_empty() {
            Some("a cargo deny/audit ignore entry")
        } else if added.contains("cargo audit") && added.contains("--ignore") {
            Some("a cargo audit --ignore flag")
        } else {
            None
        };
        if let Some(what) = what {
            let detail = if ids.is_empty() {
                format!("{what} ({file})")
            } else {
                format!("{what} ({file}: {})", ids.join(", "))
            };
            if !found.contains(&detail) {
                found.push(detail);
            }
        }
    }
    found
}

/// The `overrides` and `resolutions` entries of a `package.json`, flattened
/// to `name → spec` (nested overrides use their leaf names).
fn overrides(package_json: &str) -> Vec<(String, String)> {
    fn walk(value: &serde_json::Value, out: &mut Vec<(String, String)>) {
        for (name, spec) in value.as_object().into_iter().flatten() {
            match spec {
                serde_json::Value::String(s) => {
                    // `resolutions` keys may be paths (`a/**/b`); the package is the last part.
                    let base = name.rsplit("/**/").next().unwrap_or(name);
                    // Drop a version selector (`pkg@1.x`), keeping a scope's `@`.
                    let package = match base.strip_prefix('@') {
                        Some(rest) => format!("@{}", rest.split('@').next().unwrap_or(rest)),
                        None => base.split('@').next().unwrap_or(base).to_string(),
                    };
                    out.push((package, s.clone()));
                }
                nested @ serde_json::Value::Object(_) => walk(nested, out),
                _ => {}
            }
        }
    }
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(package_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["overrides", "resolutions"] {
        if let Some(v) = doc.get(key) {
            walk(v, &mut out);
        }
    }
    out
}

/// npm overrides added or changed between two `package.json` texts that pin
/// a package to a version an advisory says is vulnerable.
pub(crate) fn vulnerable_overrides(
    before: Option<&str>,
    after: &str,
    findings: &[SupplyChainFinding],
) -> Vec<String> {
    let old = before.map(overrides).unwrap_or_default();
    let mut flagged = Vec::new();
    for (package, spec) in overrides(after) {
        if old.iter().any(|(p, s)| *p == package && *s == spec) {
            continue;
        }
        let pinned = spec.trim().trim_start_matches(['^', '~', '=', 'v', ' ']);
        let Some(version) = Version::parse(Ecosystem::Npm, pinned) else {
            continue;
        };
        for finding in findings.iter().filter(|f| f.package == package) {
            let below_fix = finding
                .fix_version
                .as_deref()
                .and_then(|f| Version::parse(Ecosystem::Npm, f))
                .is_some_and(|fix| version < fix);
            let in_range = finding
                .vulnerable_range
                .as_deref()
                .and_then(|r| Requirement::parse(Ecosystem::Npm, r))
                .is_some_and(|r| r.admits(&version));
            if below_fix || in_range {
                flagged.push(format!(
                    "an npm override pinning {package} to {spec}, which {} marks vulnerable",
                    finding.cve
                ));
                break;
            }
        }
    }
    flagged
}

async fn git_output(shell: &dyn ShellGateway, dir: &Path, args: &[&str]) -> Option<String> {
    shell
        .run(dir, "git", args, None, None)
        .await
        .ok()
        .filter(|r| r.success)
        .map(|r| r.stdout)
}

/// Every suppression the maintain run added relative to `origin/<branch>`.
/// Empty when nothing was added, or when `origin/<branch>` cannot be read.
pub(crate) async fn run_suppressions(
    shell: &dyn ShellGateway,
    project_path: &Path,
    branch: &str,
    project: &str,
    events_dir: &Path,
) -> Vec<String> {
    let base = format!("origin/{branch}");
    let Some(diff) = git_output(shell, project_path, &["diff", "-U0", "--no-color", &base]).await
    else {
        // Best-effort: without the base there is nothing to compare; the
        // prompt rules still apply and the audit still reports advisories.
        tracing::warn!(%project, "suppression guard could not diff against {base}");
        return Vec::new();
    };
    let mut found = added_suppressions(&diff);
    let package_json = project_path.join("package.json");
    if diff.contains("package.json")
        && let Ok(after) = std::fs::read_to_string(&package_json)
    {
        let spec = format!("{base}:package.json");
        let before = git_output(shell, project_path, &["show", &spec]).await;
        let findings =
            super::classify_dependency_updates::latest_advisories(events_dir, project).findings;
        found.extend(vulnerable_overrides(before.as_deref(), &after, &findings));
    }
    found
}

/// The failure reason for a run that added suppressions.
pub(crate) fn needs_review(found: &[String]) -> String {
    format!(
        "needs review: this maintenance run added advisory suppressions, which maintenance \
         must never do; upgrade to the fixed release instead: {}",
        found.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_and_mix_ignores_are_flagged() {
        let diff = r#"diff --git a/.supply-chain-allow.json b/.supply-chain-allow.json
--- a/.supply-chain-allow.json
+++ b/.supply-chain-allow.json
@@ -3,0 +4,1 @@
+    {"cve": "CVE-2026-64941", "reason": "no fix", "expires": "2026-12-31"},
diff --git a/mix.exs b/mix.exs
--- a/mix.exs
+++ b/mix.exs
@@ -40,0 +41 @@
+  def cli, do: [ignore_advisories: ["GHSA-abcd-efgh-ijkl"]]
"#;
        let found = added_suppressions(diff);
        assert_eq!(
            found,
            [
                "an allowlist entry in .supply-chain-allow.json (.supply-chain-allow.json: CVE-2026-64941)",
                "ignore_advisories in mix.exs (mix.exs: GHSA-abcd-efgh-ijkl)"
            ]
        );
    }

    #[test]
    fn dependency_check_pip_audit_and_cargo_ignores_are_flagged() {
        let diff = "+++ b/config/dependency-check-suppressions.xml\n\
+  <suppress><cve>CVE-2026-1</cve></suppress>\n\
+++ b/.hone-gates.json\n\
+    \"command\": \".venv/bin/pip-audit --ignore-vuln PYSEC-2026-3\"\n\
+++ b/deny.toml\n\
+ignore = [\"RUSTSEC-2026-0007\"]\n\
+++ b/.github/workflows/ci.yml\n\
+      - run: cargo audit --ignore RUSTSEC-2026-0008\n";
        let found = added_suppressions(diff);
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[0].starts_with("a Dependency-Check suppression"));
        assert!(found[1].starts_with("a pip-audit --ignore-vuln flag"));
        assert!(found[2].starts_with("a cargo deny/audit ignore entry"));
        assert!(found[3].starts_with("a cargo audit --ignore flag"));
    }

    #[test]
    fn ordinary_dependency_changes_are_not_flagged() {
        let diff = "+++ b/mix.exs\n+      {:phoenix, \"~> 1.8.15\"},\n\
+++ b/mix.lock\n+  \"phoenix\": {:hex, :phoenix, \"1.8.15\"},\n\
+++ b/CHANGELOG.md\n+- Upgraded jose to fix CVE-2026-2.\n";
        assert!(added_suppressions(diff).is_empty());
    }

    fn finding(package: &str, fix: Option<&str>, range: Option<&str>) -> SupplyChainFinding {
        SupplyChainFinding {
            cve: "GHSA-xj6q-8x83-jv6g".to_string(),
            package: package.to_string(),
            fix_version: fix.map(str::to_string),
            vulnerable_range: range.map(str::to_string),
            ..SupplyChainFinding::default()
        }
    }

    #[test]
    fn an_override_pinning_a_vulnerable_version_is_flagged() {
        let after = r#"{"overrides": {"axios": "1.17.0", "@scope/pkg": {"diff": "4.0.2"}}}"#;
        let found = vulnerable_overrides(
            Some(r#"{"overrides": {}}"#),
            after,
            &[
                finding("axios", None, Some("<1.18.0")),
                finding("diff", Some("4.0.4"), None),
            ],
        );
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|f| f.contains("pinning axios to 1.17.0")), "{found:?}");
        assert!(found.iter().any(|f| f.contains("pinning diff to 4.0.2")), "{found:?}");
    }

    #[test]
    fn scoped_and_selector_override_keys_name_the_package() {
        let after = r#"{"overrides": {"@types/node@<20": "18.0.0"}, "resolutions": {"a/**/diff": "4.0.2"}}"#;
        let names: Vec<String> = overrides(after).into_iter().map(|(p, _)| p).collect();
        assert_eq!(names, ["@types/node", "diff"]);
    }

    #[test]
    fn an_override_to_the_fixed_release_is_the_remedy_not_a_suppression() {
        let after = r#"{"overrides": {"axios": "^1.18.0"}}"#;
        assert!(
            vulnerable_overrides(None, after, &[finding("axios", Some("1.18.0"), Some("<1.18.0"))])
                .is_empty()
        );
        let unchanged = r#"{"overrides": {"axios": "1.17.0"}}"#;
        assert!(
            vulnerable_overrides(
                Some(unchanged),
                unchanged,
                &[finding("axios", None, Some("<1.18.0"))]
            )
            .is_empty(),
            "an override that was already there is not this run's doing"
        );
    }

    #[test]
    fn the_rules_cover_suppression_editing_and_unsupported_claims() {
        assert!(
            ADVISORY_RULES.contains(
                "Never suppress, ignore or allowlist an advisory that has a fixed release"
            )
        );
        assert!(ADVISORY_RULES.contains(".supply-chain-allow.json"));
        assert!(ADVISORY_RULES.contains("unless you cite"));
    }
}
