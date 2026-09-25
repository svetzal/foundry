//! Hex: direct dependencies from each Mix project's `mix.exs`, locked versions
//! from its `mix.lock`, releases from the hex.pm API.
//!
//! A repository can hold several Mix projects (Bedrock keeps `apps/bedrock`
//! and `vendor/roost` side by side). They are found the way the audit finds
//! them, so the classifier and the audit always cover the same projects.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_sdk::payload::Ecosystem;

use super::{Declared, Scope, read_text, unclassified};

/// One scope per Mix project in the repository.
pub(super) fn scopes(root: &Path) -> Vec<Scope> {
    let mut dirs = crate::scanner::mix_projects(root);
    if dirs.is_empty() && root.join("mix.exs").is_file() {
        dirs.push(root.to_path_buf());
    }
    dirs.iter()
        .map(|dir| {
            let manifest = dir
                .strip_prefix(root)
                .ok()
                .map(|p| p.display().to_string())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| ".".to_string());
            scope(dir, &manifest)
        })
        .collect()
}

fn scope(dir: &Path, manifest: &str) -> Scope {
    let mix_exs = match read_text(&dir.join("mix.exs")) {
        Ok(t) => t,
        Err(reason) => return unclassified(Ecosystem::Hex, manifest, reason),
    };
    let lock = match read_text(&dir.join("mix.lock")) {
        Ok(t) => t,
        Err(reason) => {
            return unclassified(Ecosystem::Hex, manifest, format!("{reason} (run mix deps.get)"));
        }
    };
    let locked = parse_lock(&lock);
    let deps = parse_mix_deps(&mix_exs)
        .into_iter()
        .filter_map(|(app, requirement)| {
            let (package, version) = locked.get(&app)?;
            Some(Declared::new(package.clone(), requirement, Some(version.clone())))
        })
        .collect();
    Scope {
        ecosystem: Ecosystem::Hex,
        manifest: manifest.to_string(),
        deps,
        unclassified: Vec::new(),
    }
}

/// Hex packages in `mix.lock`: app name → (hex package name, version).
///
/// Lock lines look like
/// `"jason": {:hex, :jason, "1.4.5", "…", [:mix], […], "hexpm", "…"},`.
/// Git and path entries are not Hex packages and are skipped.
pub(super) fn parse_lock(text: &str) -> BTreeMap<String, (String, String)> {
    let mut locked = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((app, rest)) = rest.split_once('"') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix(':').map(str::trim_start) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix("{:hex,") else {
            continue;
        };
        let mut fields = rest.split(',').map(str::trim);
        let package = fields.next().map(|p| p.trim_start_matches(':'));
        let version = fields.next().map(|v| v.trim_matches('"'));
        if let (Some(package), Some(version)) = (package, version) {
            locked.insert(app.to_string(), (package.to_string(), version.to_string()));
        }
    }
    locked
}

/// The dependency tuples in a `mix.exs`: app name → requirement string.
///
/// Reads `{:app, "~> 1.0"}` and `{:app, "~> 1.0", only: :test}`, resolving a
/// requirement held in a module attribute (`{:phoenix, @phoenix_version}`).
/// Tuples with no requirement (`{:app, path: ...}`, `{:app, github: ...}`)
/// are not Hex dependencies and are skipped. Comments are ignored.
pub(super) fn parse_mix_deps(text: &str) -> Vec<(String, Option<String>)> {
    let code: String = text.lines().map(strip_comment).collect::<Vec<_>>().join("\n");
    let attributes = module_attributes(&code);
    let mut deps = Vec::new();
    let mut rest = code.as_str();
    while let Some(start) = rest.find("{:") {
        rest = &rest[start + 2..];
        let name_len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if name_len == 0 {
            continue;
        }
        let app = &rest[..name_len];
        let after = rest[name_len..].trim_start();
        let Some(after) = after.strip_prefix(',') else {
            continue;
        };
        let after = after.trim_start();
        let requirement = if let Some(quoted) = after.strip_prefix('"') {
            quoted.split_once('"').map(|(req, _)| req.to_string())
        } else if let Some(attr) = after.strip_prefix('@') {
            let attr_len = attr
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(attr.len());
            attributes.get(&attr[..attr_len]).cloned()
        } else {
            // `{:app, path: ...}`, `{:app, github: ...}`: not from Hex.
            continue;
        };
        if requirement.is_some() && !deps.iter().any(|(a, _): &(String, _)| a == app) {
            deps.push((app.to_string(), requirement));
        }
    }
    deps
}

/// Remove an Elixir comment, leaving `#` inside a string alone.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

/// `@name "value"` module attributes holding a string.
fn module_attributes(code: &str) -> BTreeMap<String, String> {
    code.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix('@')?;
            let (name, value) = rest.split_once(char::is_whitespace)?;
            let value = value.trim().strip_prefix('"')?;
            let (value, _) = value.split_once('"')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

/// Release versions from a hex.pm `GET /api/packages/<name>` response.
pub(super) fn parse_releases(body: &str) -> Result<Vec<String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("hex.pm response: {e}"))?;
    let releases = doc
        .get("releases")
        .and_then(serde_json::Value::as_array)
        .ok_or("hex.pm response has no releases")?;
    Ok(releases
        .iter()
        .filter_map(|r| r.get("version").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_lines_map_apps_to_hex_packages() {
        let lock = r#"%{
  "jason": {:hex, :jason, "1.4.5", "abc", [:mix], [{:decimal, "~> 1.0 or ~> 2.0", [hex: :decimal, repo: "hexpm", optional: true]}], "hexpm", "def"},
  "roost": {:git, "https://github.com/x/roost.git", "abc", []},
  "renamed": {:hex, :actual_name, "0.2.0", "abc", [:mix], [], "hexpm", "def"},
}"#;
        let locked = parse_lock(lock);
        assert_eq!(locked["jason"], ("jason".to_string(), "1.4.5".to_string()));
        assert_eq!(locked["renamed"], ("actual_name".to_string(), "0.2.0".to_string()));
        assert!(!locked.contains_key("roost"));
    }

    #[test]
    fn mix_deps_reads_requirements_attributes_and_skips_non_hex() {
        let mix = r#"
defmodule App.MixProject do
  use Mix.Project
  @lv_version "~> 1.1"

  defp deps do
    [
      {:phoenix, "~> 1.8.1"},
      {:phoenix_live_view, @lv_version},
      {:credo, "~> 1.7", only: [:dev, :test], runtime: false},
      # {:commented, "~> 9.9"},
      {:roost, path: "../../vendor/roost"},
      {:heroicons, github: "tailwindlabs/heroicons", tag: "v2.2.0", sparse: "optimized"},
      {:jason, "~> 1.4"} # trailing # comment
    ]
  end
end
"#;
        assert_eq!(
            parse_mix_deps(mix),
            [
                ("phoenix".to_string(), Some("~> 1.8.1".to_string())),
                ("phoenix_live_view".to_string(), Some("~> 1.1".to_string())),
                ("credo".to_string(), Some("~> 1.7".to_string())),
                ("jason".to_string(), Some("~> 1.4".to_string())),
            ]
        );
    }

    #[test]
    fn hex_api_releases_parse() {
        let body = r#"{"name":"jason","releases":[{"version":"1.4.5","url":"x"},{"version":"1.5.0-alpha.1"}]}"#;
        assert_eq!(parse_releases(body).unwrap(), ["1.4.5", "1.5.0-alpha.1"]);
        assert!(parse_releases(r#"{"message":"Page not found"}"#).is_err());
    }
}
