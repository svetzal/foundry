use std::path::Path;

use foundry_sdk::gateway::ShellGateway;
use foundry_sdk::payload::SupplyChainFinding;
use foundry_sdk::registry::Stack;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct AppliedFix {
    pub files: Vec<String>,
    pub detail: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ApplyFailure {
    pub files: Vec<String>,
    pub detail: String,
}

pub(super) fn supports(stack: &Stack) -> bool {
    matches!(stack, Stack::Rust | Stack::TypeScript | Stack::Python)
}

pub(super) async fn apply_fix(
    shell: &dyn ShellGateway,
    path: &Path,
    stack: &Stack,
    finding: &SupplyChainFinding,
) -> Result<AppliedFix, ApplyFailure> {
    let version = finding.fix_version.as_deref().ok_or_else(|| ApplyFailure {
        files: vec![],
        detail: "finding has no precise fix version".to_string(),
    })?;
    let package = finding.fix_package.as_deref().unwrap_or(&finding.package);

    match stack {
        Stack::Rust => apply_rust(shell, path, package, version).await,
        Stack::TypeScript => apply_typescript(shell, path, package, version).await,
        Stack::Python => apply_python(shell, path, package, version).await,
        unsupported => Err(ApplyFailure {
            files: vec![],
            detail: format!("no auto-fix mechanism for {unsupported} yet"),
        }),
    }
}

async fn apply_rust(
    shell: &dyn ShellGateway,
    path: &Path,
    package: &str,
    version: &str,
) -> Result<AppliedFix, ApplyFailure> {
    let files = vec!["Cargo.lock".to_string()];
    let args = ["update", "-p", package, "--precise", version];
    if run_successfully(shell, path, "cargo", &args).await {
        Ok(AppliedFix {
            files,
            detail: format!("cargo precise update to {version}"),
        })
    } else {
        Err(ApplyFailure {
            files,
            detail: format!(
                "cargo update -p {package} --precise {version} failed (version likely out of the manifest's range)"
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeScriptManager {
    Bun,
    Npm,
}

impl TypeScriptManager {
    fn detect(path: &Path) -> Option<(Self, &'static str)> {
        if path.join("bun.lock").exists() {
            Some((Self::Bun, "bun.lock"))
        } else if path.join("bun.lockb").exists() {
            Some((Self::Bun, "bun.lockb"))
        } else if path.join("package-lock.json").exists() {
            Some((Self::Npm, "package-lock.json"))
        } else {
            None
        }
    }
}

async fn apply_typescript(
    shell: &dyn ShellGateway,
    path: &Path,
    package: &str,
    version: &str,
) -> Result<AppliedFix, ApplyFailure> {
    let Some((manager, lockfile)) = TypeScriptManager::detect(path) else {
        return Err(ApplyFailure {
            files: vec![],
            detail: "no supported TypeScript lockfile (bun.lock, bun.lockb, package-lock.json)"
                .to_string(),
        });
    };

    let manifest_path = path.join("package.json");
    let original = std::fs::read_to_string(&manifest_path).map_err(|e| ApplyFailure {
        files: vec![],
        detail: format!("could not read package.json: {e}"),
    })?;
    let rewritten = rewrite_json_requirement(&original, package, version);
    let manifest_changed = rewritten.as_ref().is_some_and(|content| content != &original);
    write_manifest_if_changed(
        &manifest_path,
        rewritten.as_deref(),
        manifest_changed,
        "package.json",
    )?;

    let mut files = vec![lockfile.to_string()];
    if manifest_changed {
        files.insert(0, "package.json".to_string());
    }

    let success =
        run_typescript_update(shell, path, manager, manifest_changed, package, version).await;

    if success {
        let mode = if manifest_changed {
            "manifest rewrite + lock refresh"
        } else {
            "lock update"
        };
        Ok(AppliedFix {
            files,
            detail: format!("{mode} for {package} to {version}"),
        })
    } else {
        Err(ApplyFailure {
            files,
            detail: format!("TypeScript dependency update failed for {package} to {version}"),
        })
    }
}

async fn run_typescript_update(
    shell: &dyn ShellGateway,
    path: &Path,
    manager: TypeScriptManager,
    manifest_changed: bool,
    package: &str,
    version: &str,
) -> bool {
    match (manager, manifest_changed) {
        (TypeScriptManager::Bun, true) => {
            run_successfully(
                shell,
                path,
                "bun",
                &["install", "--lockfile-only", "--ignore-scripts"],
            )
            .await
        }
        (TypeScriptManager::Bun, false) => {
            let spec = format!("{package}@{version}");
            run_successfully(
                shell,
                path,
                "bun",
                &[
                    "update",
                    spec.as_str(),
                    "--lockfile-only",
                    "--ignore-scripts",
                ],
            )
            .await
        }
        (TypeScriptManager::Npm, true) => {
            run_successfully(
                shell,
                path,
                "npm",
                &[
                    "install",
                    "--package-lock-only",
                    "--ignore-scripts",
                    "--no-audit",
                ],
            )
            .await
        }
        (TypeScriptManager::Npm, false) => {
            let spec = format!("{package}@{version}");
            run_successfully(
                shell,
                path,
                "npm",
                &[
                    "install",
                    spec.as_str(),
                    "--package-lock-only",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-save",
                ],
            )
            .await
        }
    }
}

async fn apply_python(
    shell: &dyn ShellGateway,
    path: &Path,
    package: &str,
    version: &str,
) -> Result<AppliedFix, ApplyFailure> {
    if !path.join("uv.lock").exists() {
        return Err(ApplyFailure {
            files: vec![],
            detail: "no uv.lock; Python auto-fix currently requires a uv project".to_string(),
        });
    }

    let manifest_path = path.join("pyproject.toml");
    let original = std::fs::read_to_string(&manifest_path).map_err(|e| ApplyFailure {
        files: vec![],
        detail: format!("could not read pyproject.toml: {e}"),
    })?;
    let rewritten = rewrite_python_requirements(&original, package, version);
    let manifest_changed = rewritten.as_ref().is_some_and(|content| content != &original);
    write_manifest_if_changed(
        &manifest_path,
        rewritten.as_deref(),
        manifest_changed,
        "pyproject.toml",
    )?;

    let mut files = vec!["uv.lock".to_string()];
    if manifest_changed {
        files.insert(0, "pyproject.toml".to_string());
    }
    let spec = format!("{package}=={version}");
    if run_successfully(shell, path, "uv", &["lock", "--upgrade-package", spec.as_str()]).await {
        let mode = if manifest_changed {
            "requirement rewrite + uv lock refresh"
        } else {
            "uv lock update"
        };
        Ok(AppliedFix {
            files,
            detail: format!("{mode} for {package} to {version}"),
        })
    } else {
        Err(ApplyFailure {
            files,
            detail: format!("uv lock update failed for {package} to {version}"),
        })
    }
}

fn write_manifest_if_changed(
    path: &Path,
    content: Option<&str>,
    changed: bool,
    file: &str,
) -> Result<(), ApplyFailure> {
    if let Some(content) = content
        && changed
    {
        std::fs::write(path, content).map_err(|e| ApplyFailure {
            files: vec![file.to_string()],
            detail: format!("could not rewrite {file} requirement: {e}"),
        })?;
    }
    Ok(())
}

async fn run_successfully(
    shell: &dyn ShellGateway,
    path: &Path,
    command: &str,
    args: &[&str],
) -> bool {
    shell
        .run(path, command, args, None, None)
        .await
        .is_ok_and(|result| result.success)
}

fn rewrite_json_requirement(content: &str, package: &str, version: &str) -> Option<String> {
    let key = serde_json::to_string(package).ok()?;
    let mut output = content.to_string();
    let mut search_from = 0usize;
    let mut changed = false;

    while let Some(relative) = output[search_from..].find(&key) {
        let key_start = search_from + relative;
        let mut cursor = key_start + key.len();
        cursor += output[cursor..].find(':')? + 1;
        cursor += output[cursor..].len() - output[cursor..].trim_start().len();
        if output.as_bytes().get(cursor) != Some(&b'"') {
            search_from = cursor;
            continue;
        }
        let value_end = json_string_end(&output, cursor)?;
        let old: String = serde_json::from_str(&output[cursor..value_end]).ok()?;
        let new = rewrite_version_token(&old, version)?;
        let encoded = serde_json::to_string(&new).ok()?;
        output.replace_range(cursor..value_end, &encoded);
        search_from = cursor + encoded.len();
        changed = true;
    }

    changed.then_some(output)
}

fn json_string_end(content: &str, start: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut escaped = false;
    for (offset, byte) in bytes.get(start + 1..)?.iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(start + offset + 2);
        }
    }
    None
}

fn rewrite_python_requirements(content: &str, package: &str, version: &str) -> Option<String> {
    let mut output = String::with_capacity(content.len());
    let mut chars = content.char_indices().peekable();
    let mut last = 0usize;
    let mut changed = false;

    while let Some((start, quote)) = chars.next() {
        if !matches!(quote, '\'' | '"') {
            continue;
        }
        let mut end = None;
        let mut escaped = false;
        for (index, ch) in chars.by_ref() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                end = Some(index);
                break;
            }
        }
        let Some(end) = end else { break };
        let value = &content[start + quote.len_utf8()..end];
        if let Some(new) = rewrite_python_requirement(value, package, version) {
            output.push_str(&content[last..start + quote.len_utf8()]);
            output.push_str(&new);
            output.push(quote);
            last = end + quote.len_utf8();
            changed = true;
        }
    }
    output.push_str(&content[last..]);
    changed.then_some(output)
}

fn rewrite_python_requirement(requirement: &str, package: &str, version: &str) -> Option<String> {
    let name_end = requirement
        .find(|c: char| {
            matches!(c, '[' | '<' | '>' | '=' | '!' | '~' | ';' | '@') || c.is_whitespace()
        })
        .unwrap_or(requirement.len());
    if normalize_python_name(&requirement[..name_end]) != normalize_python_name(package) {
        return None;
    }
    if requirement[name_end..].contains('@') {
        return None;
    }

    let marker = requirement.find(';').unwrap_or(requirement.len());
    let core = &requirement[..marker];
    let suffix = &requirement[marker..];
    let rewritten =
        rewrite_version_token(core, version).unwrap_or_else(|| format!("{core}>={version}"));
    Some(format!("{rewritten}{suffix}"))
}

fn normalize_python_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if matches!(ch, '_' | '.') {
                '-'
            } else {
                ch.to_ascii_lowercase()
            }
        })
        .collect()
}

fn rewrite_version_token(requirement: &str, version: &str) -> Option<String> {
    let start = requirement.find(|ch: char| ch.is_ascii_digit())?;
    let tail = &requirement[start..];
    let len = tail
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+')))
        .unwrap_or(tail.len());
    let mut output = requirement.to_string();
    output.replace_range(start..start + len, version);
    Some(output)
}

#[cfg(test)]
mod tests {
    use foundry_sdk::gateway::fakes::FakeShellGateway;

    use super::*;
    use crate::gateway::ProcessShellGateway;

    fn finding(package: &str, fix_package: Option<&str>, version: &str) -> SupplyChainFinding {
        SupplyChainFinding {
            cve: "CVE-1".to_string(),
            package: package.to_string(),
            fix_version: Some(version.to_string()),
            fix_package: fix_package.map(str::to_string),
            ..SupplyChainFinding::default()
        }
    }

    #[test]
    fn json_rewrite_updates_override_without_reformatting_manifest() {
        let input =
            "{\n  \"name\": \"demo\",\n  \"overrides\": {\n    \"esbuild\": \"^0.28.0\"\n  }\n}\n";
        let output = rewrite_json_requirement(input, "esbuild", "0.28.1").unwrap();
        assert_eq!(
            output,
            "{\n  \"name\": \"demo\",\n  \"overrides\": {\n    \"esbuild\": \"^0.28.1\"\n  }\n}\n"
        );
    }

    #[test]
    fn python_rewrite_updates_lower_bound_and_preserves_marker() {
        let input = "dependencies = [\n    \"chromadb>=1.5.9; python_version >= '3.11'\",\n]\n";
        let output = rewrite_python_requirements(input, "chromadb", "1.6.1").unwrap();
        assert!(output.contains("chromadb>=1.6.1; python_version >= '3.11'"));
        assert!(output.contains("'3.11'"), "unrelated quoted marker is preserved");
    }

    #[tokio::test]
    async fn apply_uses_npm_fix_package_target_for_transitive_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{\"name\":\"demo\"}\n").unwrap();
        std::fs::write(dir.path().join("package-lock.json"), "{}\n").unwrap();
        let shell = FakeShellGateway::success();
        let finding = finding("transitive", Some("direct-parent"), "3.2.1");

        let result = apply_fix(shell.as_ref(), dir.path(), &Stack::TypeScript, &finding).await;

        assert!(result.is_ok());
        let calls = shell.invocations();
        assert_eq!(calls[0].command, "npm");
        assert!(calls[0].args.contains(&"direct-parent@3.2.1".to_string()));
    }

    #[tokio::test]
    async fn apply_rewrites_bun_override_then_refreshes_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"overrides\": { \"esbuild\": \"^0.28.0\" }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("bun.lock"), "").unwrap();
        let shell = FakeShellGateway::success();

        let result = apply_fix(
            shell.as_ref(),
            dir.path(),
            &Stack::TypeScript,
            &finding("esbuild", None, "0.28.1"),
        )
        .await
        .unwrap();

        assert_eq!(result.files, vec!["package.json", "bun.lock"]);
        assert!(
            std::fs::read_to_string(dir.path().join("package.json"))
                .unwrap()
                .contains("^0.28.1")
        );
        assert_eq!(shell.invocations()[0].args[0], "install");
    }

    #[tokio::test]
    async fn apply_rewrites_python_requirement_then_runs_uv_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\ndependencies = [\"chromadb>=1.5.9\"]\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("uv.lock"), "").unwrap();
        let shell = FakeShellGateway::success();

        let result = apply_fix(
            shell.as_ref(),
            dir.path(),
            &Stack::Python,
            &finding("chromadb", None, "1.6.1"),
        )
        .await
        .unwrap();

        assert_eq!(result.files, vec!["pyproject.toml", "uv.lock"]);
        assert!(
            std::fs::read_to_string(dir.path().join("pyproject.toml"))
                .unwrap()
                .contains("chromadb>=1.6.1")
        );
        assert_eq!(shell.invocations()[0].command, "uv");
        assert_eq!(
            shell.invocations()[0].args,
            vec!["lock", "--upgrade-package", "chromadb==1.6.1"]
        );
    }

    #[tokio::test]
    #[ignore = "networked live validation of the real bun resolver"]
    async fn live_bun_override_rewrite_refreshes_a_real_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "name": "foundry-supply-chain-live-fixture",
  "private": true,
  "devDependencies": { "vite": "^6.4.3" },
  "overrides": { "esbuild": "^0.28.0" }
}
"#,
        )
        .unwrap();
        assert!(
            run_successfully(
                &ProcessShellGateway,
                dir.path(),
                "bun",
                &["install", "--lockfile-only", "--ignore-scripts"],
            )
            .await
        );
        assert!(dir.path().join("bun.lock").exists());

        let applied = apply_fix(
            &ProcessShellGateway,
            dir.path(),
            &Stack::TypeScript,
            &finding("esbuild", None, "0.28.1"),
        )
        .await
        .unwrap();

        assert_eq!(applied.files, vec!["package.json", "bun.lock"]);
        let manifest = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(manifest.contains("\"esbuild\": \"^0.28.1\""));
        assert!(dir.path().join("bun.lock").metadata().unwrap().len() > 0);
    }

    #[tokio::test]
    #[ignore = "networked live validation of the real npm resolver"]
    async fn live_npm_requirement_rewrite_refreshes_a_real_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{
  "name": "foundry-supply-chain-live-fixture",
  "private": true,
  "dependencies": { "lodash": "4.17.20" }
}
"#,
        )
        .unwrap();
        assert!(
            run_successfully(
                &ProcessShellGateway,
                dir.path(),
                "npm",
                &[
                    "install",
                    "--package-lock-only",
                    "--ignore-scripts",
                    "--no-audit"
                ],
            )
            .await
        );
        assert!(dir.path().join("package-lock.json").exists());

        let applied = apply_fix(
            &ProcessShellGateway,
            dir.path(),
            &Stack::TypeScript,
            &finding("lodash", None, "4.17.21"),
        )
        .await
        .unwrap();

        assert_eq!(applied.files, vec!["package.json", "package-lock.json"]);
        let manifest = std::fs::read_to_string(dir.path().join("package.json")).unwrap();
        assert!(manifest.contains("\"lodash\": \"4.17.21\""));
        let lock = std::fs::read_to_string(dir.path().join("package-lock.json")).unwrap();
        assert!(lock.contains("node_modules/lodash"));
        assert!(lock.contains("\"version\": \"4.17.21\""));
    }

    #[tokio::test]
    #[ignore = "networked live validation of the real uv resolver"]
    async fn live_python_requirement_rewrite_refreshes_a_real_uv_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            r#"[project]
name = "foundry-supply-chain-live-fixture"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["idna==3.6"]
"#,
        )
        .unwrap();
        assert!(run_successfully(&ProcessShellGateway, dir.path(), "uv", &["lock"]).await);
        assert!(dir.path().join("uv.lock").exists());

        let applied = apply_fix(
            &ProcessShellGateway,
            dir.path(),
            &Stack::Python,
            &finding("idna", None, "3.7"),
        )
        .await
        .unwrap();

        assert_eq!(applied.files, vec!["pyproject.toml", "uv.lock"]);
        let manifest = std::fs::read_to_string(dir.path().join("pyproject.toml")).unwrap();
        assert!(manifest.contains("idna==3.7"));
        let lock = std::fs::read_to_string(dir.path().join("uv.lock")).unwrap();
        assert!(lock.contains("name = \"idna\""));
        assert!(lock.contains("version = \"3.7\""));
    }
}
