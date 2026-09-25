//! Cargo: direct dependencies from `Cargo.toml` (and workspace members),
//! locked versions from `Cargo.lock`, releases from the crates.io sparse index.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_sdk::payload::Ecosystem;
use toml::Value;

use super::requirement::Requirement;
use super::version::Version;
use super::{Declared, Scope, read_text, unclassified};

/// The dependency tables a manifest may declare, at top level or under a
/// `[target.'cfg(...)']` table.
const TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// Every crates.io dependency declared by the root manifest and its workspace
/// members, with its locked version.
pub(super) fn scopes(root: &Path, rel: &str) -> Vec<Scope> {
    let manifest = match read_text(&root.join("Cargo.toml")) {
        Ok(text) => text,
        Err(reason) => return vec![unclassified(Ecosystem::Cargo, rel, reason)],
    };
    let lock = match read_text(&root.join("Cargo.lock")) {
        Ok(text) => text,
        Err(reason) => {
            return vec![unclassified(
                Ecosystem::Cargo,
                rel,
                format!("{reason} (commit Cargo.lock to classify)"),
            )];
        }
    };
    let root_manifest: Value = match toml::from_str(&manifest) {
        Ok(v) => v,
        Err(e) => {
            return vec![unclassified(
                Ecosystem::Cargo,
                rel,
                format!("Cargo.toml: {e}"),
            )];
        }
    };
    let mut manifests = vec![root_manifest.clone()];
    for member in member_dirs(root, &root_manifest) {
        match read_text(&member.join("Cargo.toml")).and_then(|t| {
            toml::from_str::<Value>(&t).map_err(|e| format!("{}: {e}", member.display()))
        }) {
            Ok(v) => manifests.push(v),
            Err(reason) => return vec![unclassified(Ecosystem::Cargo, rel, reason)],
        }
    }
    match parse_lock(&lock) {
        Ok(locked) => vec![Scope {
            ecosystem: Ecosystem::Cargo,
            manifest: rel.to_string(),
            deps: declared(&root_manifest, &manifests, &locked),
            unclassified: Vec::new(),
        }],
        Err(reason) => vec![unclassified(Ecosystem::Cargo, rel, reason)],
    }
}

/// Workspace member directories named by `[workspace] members`, expanding a
/// trailing `/*` glob and honouring `exclude`.
fn member_dirs(root: &Path, manifest: &Value) -> Vec<std::path::PathBuf> {
    let Some(workspace) = manifest.get("workspace") else {
        return Vec::new();
    };
    let list = |key: &str| -> Vec<String> {
        workspace
            .get(key)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    };
    let excluded = list("exclude");
    let mut dirs = Vec::new();
    for member in list("members") {
        if let Some(parent) = member.strip_suffix("/*") {
            let Ok(entries) = std::fs::read_dir(root.join(parent)) else {
                continue;
            };
            let mut found: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("Cargo.toml").is_file())
                .collect();
            found.sort();
            dirs.extend(found);
        } else if member != "." {
            dirs.push(root.join(&member));
        }
    }
    dirs.retain(|d| !excluded.iter().any(|x| d.ends_with(x)));
    dirs
}

/// crates.io packages in `Cargo.lock`: name → every locked version.
pub(super) fn parse_lock(text: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    let lock: Value = toml::from_str(text).map_err(|e| format!("Cargo.lock: {e}"))?;
    let mut locked: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in lock.get("package").and_then(Value::as_array).into_iter().flatten() {
        let from_crates_io = package
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("registry+") || s.starts_with("sparse+"));
        if let (true, Some(name), Some(version)) = (
            from_crates_io,
            package.get("name").and_then(Value::as_str),
            package.get("version").and_then(Value::as_str),
        ) {
            locked.entry(name.to_string()).or_default().push(version.to_string());
        }
    }
    Ok(locked)
}

/// A dependency entry's registry name and requirement, or `None` for a path,
/// git or alternate-registry dependency.
fn entry(
    name: &str,
    spec: &Value,
    workspace_deps: Option<&Value>,
) -> Option<(String, Option<String>)> {
    match spec {
        Value::String(req) => Some((name.to_string(), Some(req.clone()))),
        Value::Table(table) => {
            if table.get("workspace").and_then(Value::as_bool) == Some(true) {
                let inherited = workspace_deps?.get(name)?;
                return entry(name, inherited, None);
            }
            if table.contains_key("git") || table.contains_key("registry") {
                return None;
            }
            let version = table.get("version").and_then(Value::as_str).map(str::to_string);
            if table.contains_key("path") && version.is_none() {
                return None;
            }
            let registry_name =
                table.get("package").and_then(Value::as_str).unwrap_or(name).to_string();
            Some((registry_name, version))
        }
        _ => None,
    }
}

fn dependency_tables(manifest: &Value) -> Vec<&toml::Table> {
    let mut tables: Vec<&toml::Table> = TABLES
        .iter()
        .filter_map(|t| manifest.get(*t).and_then(Value::as_table))
        .collect();
    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            tables.extend(TABLES.iter().filter_map(|t| target.get(*t).and_then(Value::as_table)));
        }
    }
    tables
}

/// The declared crates.io dependencies across `manifests`, one per crate.
pub(super) fn declared(
    root_manifest: &Value,
    manifests: &[Value],
    locked: &BTreeMap<String, Vec<String>>,
) -> Vec<Declared> {
    let workspace_deps = root_manifest.get("workspace").and_then(|w| w.get("dependencies"));
    let mut requirements: BTreeMap<String, Option<String>> = BTreeMap::new();
    if let Some(table) = workspace_deps.and_then(Value::as_table) {
        for (name, spec) in table {
            if let Some((crate_name, req)) = entry(name, spec, None) {
                requirements.entry(crate_name).or_insert(req);
            }
        }
    }
    for manifest in manifests {
        for table in dependency_tables(manifest) {
            for (name, spec) in table {
                if let Some((crate_name, req)) = entry(name, spec, workspace_deps) {
                    requirements.entry(crate_name).or_insert(req);
                }
            }
        }
    }
    requirements
        .into_iter()
        .filter(|(name, _)| locked.contains_key(name))
        .map(|(name, requirement)| {
            let current = locked_version(locked.get(&name), requirement.as_deref());
            Declared::new(name, requirement, current)
        })
        .collect()
}

/// The locked version a requirement resolves to: the newest locked copy that
/// satisfies it (a lockfile may hold several versions of one crate).
fn locked_version(versions: Option<&Vec<String>>, requirement: Option<&str>) -> Option<String> {
    let req = requirement.and_then(|r| Requirement::parse(Ecosystem::Cargo, r));
    let mut parsed: Vec<Version> =
        versions?.iter().filter_map(|v| Version::parse(Ecosystem::Cargo, v)).collect();
    parsed.sort();
    let pick = parsed
        .iter()
        .rev()
        .find(|v| req.as_ref().is_none_or(|r| r.admits(v)))
        .or_else(|| parsed.last())?;
    Some(pick.as_str().to_string())
}

/// The crates.io sparse-index path for a crate name.
pub(super) fn index_path(name: &str) -> String {
    let name = name.to_ascii_lowercase();
    match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

/// Unyanked versions from a sparse-index file (one JSON object per line).
pub(super) fn parse_index(body: &str) -> Result<Vec<String>, String> {
    let mut versions = Vec::new();
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        let entry: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("crates.io index: {e}"))?;
        if entry.get("yanked").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(v) = entry.get("vers").and_then(serde_json::Value::as_str) {
            versions.push(v.to_string());
        }
    }
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.100"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "toml"
version = "0.8.2"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "toml"
version = "0.9.1"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "my-sdk"
version = "0.1.0"

[[package]]
name = "forked"
version = "2.0.0"
source = "git+https://github.com/x/forked#abc"
"#;

    fn parse(text: &str) -> Value {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_lock_keeps_only_registry_packages() {
        let locked = parse_lock(LOCK).unwrap();
        assert_eq!(locked["toml"], ["0.8.2", "0.9.1"]);
        assert!(!locked.contains_key("my-sdk"));
        assert!(!locked.contains_key("forked"));
    }

    #[test]
    fn declared_reads_every_table_and_skips_path_and_git_dependencies() {
        let root = parse(
            r#"
[package]
name = "app"
[dependencies]
serde = { version = "1", features = ["derive"] }
my-sdk = { path = "crates/sdk" }
forked = { git = "https://github.com/x/forked" }
[target.'cfg(unix)'.dev-dependencies]
toml = "0.9"
"#,
        );
        let locked = parse_lock(LOCK).unwrap();
        let deps = declared(&root, std::slice::from_ref(&root), &locked);
        let got: Vec<_> = deps
            .iter()
            .map(|d| (d.package.as_str(), d.requirement.as_deref(), d.current.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("serde", Some("1"), Some("1.0.100")),
                ("toml", Some("0.9"), Some("0.9.1"))
            ]
        );
    }

    #[test]
    fn workspace_inheritance_and_renames_resolve() {
        let root = parse(
            r#"
[workspace]
members = ["crates/*"]
[workspace.dependencies]
serde = "1"
toml-old = { package = "toml", version = "0.8" }
"#,
        );
        let member = parse(
            r"
[dependencies]
serde = { workspace = true }
toml-old = { workspace = true }
",
        );
        let locked = parse_lock(LOCK).unwrap();
        let deps = declared(&root, &[root.clone(), member], &locked);
        let toml = deps.iter().find(|d| d.package == "toml").unwrap();
        assert_eq!(toml.requirement.as_deref(), Some("0.8"));
        assert_eq!(
            toml.current.as_deref(),
            Some("0.8.2"),
            "the locked copy the requirement admits"
        );
    }

    #[test]
    fn index_paths_follow_the_sparse_layout() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("Serde"), "se/rd/serde");
    }

    #[test]
    fn index_parsing_skips_yanked_releases() {
        let body = r#"{"name":"x","vers":"1.0.0","yanked":false}
{"name":"x","vers":"1.0.1","yanked":true}
{"name":"x","vers":"1.1.0","yanked":false}
"#;
        assert_eq!(parse_index(body).unwrap(), ["1.0.0", "1.1.0"]);
        assert!(parse_index("<html>").is_err());
    }

    #[test]
    fn members_expand_globs_and_honour_exclude() {
        let dir = tempfile::tempdir().unwrap();
        for m in ["crates/a", "crates/b", "crates/skip"] {
            std::fs::create_dir_all(dir.path().join(m)).unwrap();
            std::fs::write(dir.path().join(m).join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        }
        let root = parse("[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/skip\"]\n");
        let dirs = member_dirs(dir.path(), &root);
        assert_eq!(dirs, [dir.path().join("crates/a"), dir.path().join("crates/b")]);
    }
}
