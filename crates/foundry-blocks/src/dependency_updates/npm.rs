//! npm: direct dependencies from `package.json`, locked versions from
//! `package-lock.json` or Bun's text `bun.lock`, releases from the npm registry.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_sdk::payload::Ecosystem;

use super::{Declared, Scope, read_text, unclassified};

/// The `package.json` tables that declare what the project installs.
const TABLES: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

pub(super) fn scopes(root: &Path) -> Vec<Scope> {
    let manifest = match read_text(&root.join("package.json")) {
        Ok(t) => t,
        Err(reason) => return vec![unclassified(Ecosystem::Npm, ".", reason)],
    };
    let locked = if root.join("package-lock.json").is_file() {
        read_text(&root.join("package-lock.json")).and_then(|t| parse_package_lock(&t))
    } else if root.join("bun.lock").is_file() {
        read_text(&root.join("bun.lock")).and_then(|t| parse_bun_lock(&t))
    } else if root.join("bun.lockb").is_file() {
        Err(
            "bun.lockb is binary; switch to the text lockfile (bun install --save-text-lockfile)"
                .to_string(),
        )
    } else if root.join("yarn.lock").is_file() || root.join("pnpm-lock.yaml").is_file() {
        Err("yarn and pnpm lockfiles are not supported".to_string())
    } else {
        Err("no lockfile (package-lock.json or bun.lock)".to_string())
    };
    let locked = match locked {
        Ok(l) => l,
        Err(reason) => return vec![unclassified(Ecosystem::Npm, ".", reason)],
    };
    match parse_manifest(&manifest) {
        Ok(declared) => vec![Scope {
            ecosystem: Ecosystem::Npm,
            manifest: ".".to_string(),
            deps: declared
                .into_iter()
                // git, path and alias dependencies are not from the registry.
                .filter(|(_, req)| !super::requirement::npm_is_not_a_range(req.trim()))
                .map(|(name, req)| {
                    let current = locked.get(&name).cloned();
                    Declared::new(name, Some(req), current)
                })
                .collect(),
            unclassified: Vec::new(),
        }],
        Err(reason) => vec![unclassified(Ecosystem::Npm, ".", reason)],
    }
}

/// Declared dependencies: package name → range, first table wins.
pub(super) fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("package.json: {e}"))?;
    let mut declared = BTreeMap::new();
    for table in TABLES {
        for (name, range) in
            doc.get(table).and_then(serde_json::Value::as_object).into_iter().flatten()
        {
            if let Some(range) = range.as_str() {
                declared.entry(name.clone()).or_insert_with(|| range.to_string());
            }
        }
    }
    Ok(declared)
}

/// Top-level installed versions from a `package-lock.json` (v1, v2 or v3).
pub(super) fn parse_package_lock(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("package-lock.json: {e}"))?;
    let mut locked = BTreeMap::new();
    if let Some(packages) = doc.get("packages").and_then(serde_json::Value::as_object) {
        for (path, entry) in packages {
            // Only the top-level install: `node_modules/<name>`, not nested copies.
            let Some(name) = path.strip_prefix("node_modules/") else {
                continue;
            };
            if name.contains("/node_modules/") {
                continue;
            }
            if let Some(v) = entry.get("version").and_then(serde_json::Value::as_str) {
                locked.insert(name.to_string(), v.to_string());
            }
        }
    } else if let Some(deps) = doc.get("dependencies").and_then(serde_json::Value::as_object) {
        for (name, entry) in deps {
            if let Some(v) = entry.get("version").and_then(serde_json::Value::as_str) {
                locked.insert(name.clone(), v.to_string());
            }
        }
    }
    Ok(locked)
}

/// Top-level resolved versions from a text `bun.lock`.
///
/// The file is JSON with trailing commas. Each `packages` entry is an array
/// whose first element is `"<name>@<version>"`.
pub(super) fn parse_bun_lock(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(&strip_trailing_commas(text)).map_err(|e| format!("bun.lock: {e}"))?;
    let mut locked = BTreeMap::new();
    for (key, entry) in
        doc.get("packages").and_then(serde_json::Value::as_object).into_iter().flatten()
    {
        // Nested copies are keyed `parent/child`; a scoped name is `@scope/name`.
        let nested = if key.starts_with('@') {
            key.matches('/').count() > 1
        } else {
            key.contains('/')
        };
        if nested {
            continue;
        }
        let Some(spec) = entry.get(0).and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some((name, version)) = spec.rsplit_once('@')
            && !name.is_empty()
        {
            locked.insert(name.to_string(), version.to_string());
        }
    }
    Ok(locked)
}

/// Remove commas that directly precede `}` or `]`, outside strings.
fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// The registry URL path for a package (`@scope/name` → `@scope%2Fname`).
pub(super) fn registry_path(name: &str) -> String {
    name.replacen('/', "%2F", 1)
}

/// The `version` field of a `GET /<name>/latest` response.
pub(super) fn parse_latest(body: &str) -> Result<String, String> {
    let doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("npm registry: {e}"))?;
    doc.get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "npm registry response has no version".to_string())
}

/// Every version key of an (abbreviated) packument.
pub(super) fn parse_packument(body: &str) -> Result<Vec<String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("npm registry: {e}"))?;
    let versions = doc
        .get("versions")
        .and_then(serde_json::Value::as_object)
        .ok_or("npm registry response has no versions")?;
    Ok(versions.keys().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_reads_runtime_dev_and_optional_tables() {
        let text = r#"{"name":"x","dependencies":{"zod":"^3.22.0"},"devDependencies":{"jest":"^29.0.0","zod":"^9"},"peerDependencies":{"react":"*"}}"#;
        let declared = parse_manifest(text).unwrap();
        assert_eq!(declared["zod"], "^3.22.0", "the first table wins");
        assert_eq!(declared["jest"], "^29.0.0");
        assert!(
            !declared.contains_key("react"),
            "peer dependencies are not installed by the project"
        );
    }

    #[test]
    fn package_lock_v3_reads_top_level_installs_only() {
        let text = r#"{"lockfileVersion":3,"packages":{
            "":{"name":"x"},
            "node_modules/zod":{"version":"3.22.4"},
            "node_modules/@types/node":{"version":"20.1.0"},
            "node_modules/jest/node_modules/chalk":{"version":"4.1.0"}
        }}"#;
        let locked = parse_package_lock(text).unwrap();
        assert_eq!(locked["zod"], "3.22.4");
        assert_eq!(locked["@types/node"], "20.1.0");
        assert!(!locked.contains_key("chalk"));
    }

    #[test]
    fn package_lock_v1_reads_dependencies() {
        let text = r#"{"lockfileVersion":1,"dependencies":{"zod":{"version":"3.0.0"}}}"#;
        assert_eq!(parse_package_lock(text).unwrap()["zod"], "3.0.0");
    }

    #[test]
    fn bun_lock_tolerates_trailing_commas_and_skips_nested_copies() {
        let text = r#"{
  "lockfileVersion": 1,
  "workspaces": { "": { "name": "hone", "dependencies": { "zod": "^3.22.0", }, }, },
  "packages": {
    "zod": ["zod@3.25.1", "", {}, "sha512-x,"],
    "@types/bun": ["@types/bun@1.2.0", "", {}, "sha512-y"],
    "jest/chalk": ["chalk@4.1.0", "", {}, "sha512-z"],
    "@scope/pkg/dep": ["dep@1.0.0", "", {}, "sha512-w"],
  },
}"#;
        let locked = parse_bun_lock(text).unwrap();
        assert_eq!(locked["zod"], "3.25.1");
        assert_eq!(locked["@types/bun"], "1.2.0");
        assert!(!locked.contains_key("chalk"));
        assert!(!locked.contains_key("dep"));
    }

    #[test]
    fn scoped_names_encode_their_slash() {
        assert_eq!(registry_path("@types/node"), "@types%2Fnode");
        assert_eq!(registry_path("zod"), "zod");
    }

    #[test]
    fn registry_responses_parse() {
        assert_eq!(parse_latest(r#"{"name":"zod","version":"4.1.0"}"#).unwrap(), "4.1.0");
        let packument =
            r#"{"name":"zod","dist-tags":{"latest":"4.1.0"},"versions":{"3.0.0":{},"4.1.0":{}}}"#;
        assert_eq!(parse_packument(packument).unwrap(), ["3.0.0", "4.1.0"]);
        assert!(parse_latest("Not Found").is_err());
    }
}
