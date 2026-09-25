//! PyPI: direct dependencies from `pyproject.toml`, locked versions from
//! `uv.lock`, releases from the PyPI JSON API.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_sdk::payload::Ecosystem;
use toml::Value;

use super::{Declared, Scope, read_text, unclassified};

pub(super) fn scopes(root: &Path) -> Vec<Scope> {
    let manifest = match read_text(&root.join("pyproject.toml")) {
        Ok(t) => t,
        Err(reason) => return vec![unclassified(Ecosystem::Pypi, ".", reason)],
    };
    if !root.join("uv.lock").is_file() {
        let reason = if root.join("poetry.lock").is_file() {
            "poetry.lock is not supported (uv.lock is)"
        } else {
            "no uv.lock"
        };
        return vec![unclassified(Ecosystem::Pypi, ".", reason.to_string())];
    }
    let lock = match read_text(&root.join("uv.lock")).and_then(|t| parse_uv_lock(&t)) {
        Ok(l) => l,
        Err(reason) => return vec![unclassified(Ecosystem::Pypi, ".", reason)],
    };
    match parse_pyproject(&manifest) {
        Ok(declared) => vec![Scope {
            ecosystem: Ecosystem::Pypi,
            manifest: ".".to_string(),
            deps: declared
                .into_iter()
                .map(|(name, req)| {
                    let current = lock.get(&normalize(&name)).cloned();
                    Declared::new(name, Some(req), current)
                })
                .collect(),
            unclassified: Vec::new(),
        }],
        Err(reason) => vec![unclassified(Ecosystem::Pypi, ".", reason)],
    }
}

/// PEP 503 name normalization: lowercase, runs of `-_.` become one `-`.
pub(super) fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut dash = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !dash {
                out.push('-');
            }
            dash = true;
        } else {
            out.push(c.to_ascii_lowercase());
            dash = false;
        }
    }
    out
}

/// Split a PEP 508 requirement into its name and version specifier. `None`
/// for a URL requirement (`name @ https://...`), which is not from PyPI.
pub(super) fn parse_pep508(text: &str) -> Option<(String, String)> {
    let text = text.split(';').next()?.trim();
    let name_len = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(text.len());
    if name_len == 0 {
        return None;
    }
    let name = &text[..name_len];
    let mut rest = text[name_len..].trim_start();
    if let Some(after_extras) = rest.strip_prefix('[') {
        rest = after_extras.split_once(']').map_or("", |(_, r)| r).trim_start();
    }
    if rest.starts_with('@') {
        return None;
    }
    let spec = rest.trim_start_matches('(').trim_end_matches(')').trim();
    Some((name.to_string(), spec.to_string()))
}

/// Declared PyPI dependencies: name → specifier. Reads `[project]`
/// dependencies and optional dependencies, `[dependency-groups]`, and uv's
/// legacy `[tool.uv] dev-dependencies`.
pub(super) fn parse_pyproject(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: Value = toml::from_str(text).map_err(|e| format!("pyproject.toml: {e}"))?;
    let mut lists: Vec<&Vec<Value>> = Vec::new();
    let project = doc.get("project");
    if let Some(deps) = project.and_then(|p| p.get("dependencies")).and_then(Value::as_array) {
        lists.push(deps);
    }
    for table in [
        project.and_then(|p| p.get("optional-dependencies")),
        doc.get("dependency-groups"),
    ] {
        for list in table.and_then(Value::as_table).into_iter().flat_map(toml::Table::values) {
            if let Some(list) = list.as_array() {
                lists.push(list);
            }
        }
    }
    if let Some(dev) = doc
        .get("tool")
        .and_then(|t| t.get("uv"))
        .and_then(|u| u.get("dev-dependencies"))
        .and_then(Value::as_array)
    {
        lists.push(dev);
    }
    let mut declared = BTreeMap::new();
    for requirement in lists.into_iter().flatten().filter_map(Value::as_str) {
        if let Some((name, spec)) = parse_pep508(requirement) {
            declared.entry(name).or_insert(spec);
        }
    }
    Ok(declared)
}

/// Registry packages in `uv.lock`: normalized name → version.
pub(super) fn parse_uv_lock(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: Value = toml::from_str(text).map_err(|e| format!("uv.lock: {e}"))?;
    let mut locked = BTreeMap::new();
    for package in doc.get("package").and_then(Value::as_array).into_iter().flatten() {
        let from_registry = package.get("source").and_then(|s| s.get("registry")).is_some();
        if let (true, Some(name), Some(version)) = (
            from_registry,
            package.get("name").and_then(Value::as_str),
            package.get("version").and_then(Value::as_str),
        ) {
            locked.insert(normalize(name), version.to_string());
        }
    }
    Ok(locked)
}

/// Releases with at least one file that is not yanked, from `GET /pypi/<name>/json`.
pub(super) fn parse_releases(body: &str) -> Result<Vec<String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("PyPI response: {e}"))?;
    let releases = doc
        .get("releases")
        .and_then(serde_json::Value::as_object)
        .ok_or("PyPI response has no releases")?;
    Ok(releases
        .iter()
        .filter(|(_, files)| {
            files.as_array().is_some_and(|f| {
                f.iter().any(|file| {
                    file.get("yanked").and_then(serde_json::Value::as_bool) != Some(true)
                })
            })
        })
        .map(|(v, _)| v.clone())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_normalize_per_pep_503() {
        assert_eq!(normalize("Pytest_Asyncio"), "pytest-asyncio");
        assert_eq!(normalize("zope.interface"), "zope-interface");
        assert_eq!(normalize("a--_b"), "a-b");
    }

    #[test]
    fn pep508_strings_split_into_name_and_specifier() {
        assert_eq!(parse_pep508("pydantic>=2.13.5"), Some(("pydantic".into(), ">=2.13.5".into())));
        assert_eq!(
            parse_pep508("mkdocstrings[python]>=1.0.6"),
            Some(("mkdocstrings".into(), ">=1.0.6".into()))
        );
        assert_eq!(
            parse_pep508("tomli>=2; python_version < '3.11'"),
            Some(("tomli".into(), ">=2".into()))
        );
        assert_eq!(
            parse_pep508("requests (>=2.0,<3)"),
            Some(("requests".into(), ">=2.0,<3".into()))
        );
        assert_eq!(parse_pep508("colorama"), Some(("colorama".into(), String::new())));
        assert_eq!(parse_pep508("pkg @ https://example.com/pkg.whl"), None);
    }

    #[test]
    fn pyproject_reads_project_optional_groups_and_legacy_dev() {
        let text = r#"
[project]
name = "x"
dependencies = ["pydantic>=2.13.5", "anthropic>=0.116.0"]
[project.optional-dependencies]
docs = ["mkdocs>=1.6"]
[dependency-groups]
dev = ["pytest>=9.1.1", { include-group = "docs" }]
[tool.uv]
dev-dependencies = ["ruff>=0.5"]
"#;
        let declared = parse_pyproject(text).unwrap();
        let names: Vec<&str> = declared.keys().map(String::as_str).collect();
        assert_eq!(names, ["anthropic", "mkdocs", "pydantic", "pytest", "ruff"]);
        assert_eq!(declared["anthropic"], ">=0.116.0");
    }

    #[test]
    fn uv_lock_keeps_registry_packages_by_normalized_name() {
        let text = r#"
version = 1
[[package]]
name = "anthropic"
version = "1.8.0"
source = { registry = "https://pypi.org/simple" }
[[package]]
name = "mojentic"
version = "1.5.0"
source = { editable = "." }
[[package]]
name = "Pytest_Asyncio"
version = "1.4.0"
source = { registry = "https://pypi.org/simple" }
"#;
        let locked = parse_uv_lock(text).unwrap();
        assert_eq!(locked["anthropic"], "1.8.0");
        assert_eq!(locked["pytest-asyncio"], "1.4.0");
        assert!(!locked.contains_key("mojentic"));
    }

    #[test]
    fn pypi_releases_skip_yanked_and_empty() {
        let body = r#"{"info":{"version":"2.0"},"releases":{
            "1.0":[{"yanked":false}],
            "1.1":[{"yanked":true}],
            "1.2":[],
            "2.0":[{"yanked":true},{"yanked":false}]
        }}"#;
        let mut got = parse_releases(body).unwrap();
        got.sort();
        assert_eq!(got, ["1.0", "2.0"]);
    }
}
