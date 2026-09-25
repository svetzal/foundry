//! SwiftPM: direct dependencies from `Package.swift`, resolved versions from
//! `Package.resolved`, releases from the package repository's git tags.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_sdk::payload::Ecosystem;

use super::{Declared, Scope, read_text, unclassified};

pub(super) fn scopes(root: &Path) -> Vec<Scope> {
    let manifest = match read_text(&root.join("Package.swift")) {
        Ok(t) => t,
        Err(reason) => return vec![unclassified(Ecosystem::Swiftpm, ".", reason)],
    };
    let resolved = match read_text(&root.join("Package.resolved")).and_then(|t| parse_resolved(&t))
    {
        Ok(r) => r,
        Err(reason) => {
            return vec![unclassified(
                Ecosystem::Swiftpm,
                ".",
                format!("{reason} (commit Package.resolved to classify)"),
            )];
        }
    };
    let deps = parse_package_swift(&manifest)
        .into_iter()
        .map(|dep| {
            let current = resolved.get(&normalize_url(&dep.url)).cloned();
            let mut declared = Declared::new(identity(&dep.url), dep.requirement, current);
            declared.lookup.sources = vec![dep.url];
            declared
        })
        .collect();
    vec![Scope {
        ecosystem: Ecosystem::Swiftpm,
        manifest: ".".to_string(),
        deps,
        unclassified: Vec::new(),
    }]
}

/// A remote package dependency declared in `Package.swift`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PackageDependency {
    pub url: String,
    /// Normalized: `from: 1.2.3`, `upToNextMinor: 1.2.0`, `exact: 1.0.0`,
    /// `"1.0.0"..<"2.0.0"`. `None` for `branch:`/`revision:` dependencies.
    pub requirement: Option<String>,
}

/// SwiftPM's package identity: the last URL path component, lowercased, no `.git`.
pub(super) fn identity(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(url)
        .trim_end_matches(".git")
        .to_ascii_lowercase()
}

fn normalize_url(url: &str) -> String {
    url.trim().trim_end_matches('/').trim_end_matches(".git").to_ascii_lowercase()
}

/// The first string literal in `text`.
fn first_string(text: &str) -> Option<&str> {
    let start = text.find('"')? + 1;
    let len = text[start..].find('"')?;
    Some(&text[start..start + len])
}

/// Every `.package(url: …, …)` call in a manifest.
pub(super) fn parse_package_swift(text: &str) -> Vec<PackageDependency> {
    let mut deps = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(".package(") {
        rest = &rest[start + ".package(".len()..];
        // The call's text, up to its balanced closing parenthesis.
        let mut depth = 1;
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let call = &rest[..end];
        rest = &rest[end..];
        let Some(url_at) = call.find("url:") else {
            continue; // `.package(path: ...)`: a local package.
        };
        let Some(url) = first_string(&call[url_at..]) else {
            continue;
        };
        let after_url = &call[url_at + 4..];
        let after_url = &after_url[after_url.find('"').map_or(0, |i| i + 1)..];
        let after_url = &after_url[after_url.find('"').map_or(0, |i| i + 1)..];
        deps.push(PackageDependency {
            url: url.to_string(),
            requirement: requirement(after_url.trim_start_matches([',', ' ', '\n', '\t'])),
        });
    }
    deps
}

fn requirement(text: &str) -> Option<String> {
    let text = text.trim();
    for (marker, kind) in [
        (".upToNextMajor(", "from"),
        (".upToNextMinor(", "upToNextMinor"),
        (".exact(", "exact"),
        ("from:", "from"),
        ("exact:", "exact"),
    ] {
        if let Some(rest) = text.strip_prefix(marker) {
            return first_string(rest).map(|v| format!("{kind}: {v}"));
        }
    }
    if text.contains("..<") || text.contains("...") {
        let range: String = text.split(',').next()?.split_whitespace().collect();
        return Some(range);
    }
    None
}

/// Resolved versions from `Package.resolved` (v1, v2 or v3), by normalized URL.
pub(super) fn parse_resolved(text: &str) -> Result<BTreeMap<String, String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("Package.resolved: {e}"))?;
    let pins = doc
        .get("pins")
        .or_else(|| doc.get("object").and_then(|o| o.get("pins")))
        .and_then(serde_json::Value::as_array)
        .ok_or("Package.resolved has no pins")?;
    let mut resolved = BTreeMap::new();
    for pin in pins {
        let url = pin
            .get("location")
            .or_else(|| pin.get("repositoryURL"))
            .and_then(serde_json::Value::as_str);
        let version = pin
            .get("state")
            .and_then(|s| s.get("version"))
            .and_then(serde_json::Value::as_str);
        if let (Some(url), Some(version)) = (url, version) {
            resolved.insert(normalize_url(url), version.to_string());
        }
    }
    Ok(resolved)
}

/// Tag names from `git ls-remote --tags --refs <url>` output.
pub(super) fn parse_ls_remote(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter_map(|r| r.strip_prefix("refs/tags/"))
        .map(|t| t.trim_end_matches("^{}").to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
let package = Package(
    name: "Mojentic",
    dependencies: [
        .package(url: "https://github.com/apple/swift-log.git", from: "1.15.1"),
        .package(url: "https://github.com/swiftlang/swift-docc-plugin.git", .upToNextMinor(from: "1.5.0")),
        .package(url: "https://github.com/x/pinned", exact: "2.0.0"),
        .package(url: "https://github.com/x/ranged.git", "1.0.0"..<"1.5.0"),
        .package(url: "https://github.com/x/branchy.git", branch: "main"),
        .package(path: "../Local"),
    ],
    targets: [.target(name: "Mojentic", dependencies: [.product(name: "Logging", package: "swift-log")])]
)
"#;

    #[test]
    fn manifest_dependencies_parse_with_normalized_requirements() {
        let deps = parse_package_swift(MANIFEST);
        let got: Vec<(&str, Option<&str>)> =
            deps.iter().map(|d| (d.url.as_str(), d.requirement.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("https://github.com/apple/swift-log.git", Some("from: 1.15.1")),
                (
                    "https://github.com/swiftlang/swift-docc-plugin.git",
                    Some("upToNextMinor: 1.5.0")
                ),
                ("https://github.com/x/pinned", Some("exact: 2.0.0")),
                ("https://github.com/x/ranged.git", Some("\"1.0.0\"..<\"1.5.0\"")),
                ("https://github.com/x/branchy.git", None),
            ]
        );
    }

    #[test]
    fn identities_follow_swiftpm() {
        assert_eq!(identity("https://github.com/apple/swift-log.git"), "swift-log");
        assert_eq!(identity("https://github.com/Apple/Swift-Log/"), "swift-log");
    }

    #[test]
    fn resolved_v2_and_v1_parse() {
        let v2 = r#"{"pins":[{"identity":"swift-log","location":"https://github.com/apple/swift-log.git","state":{"revision":"9c6f","version":"1.15.1"}},{"identity":"b","location":"https://github.com/x/b","state":{"branch":"main","revision":"1"}}],"version":2}"#;
        let r = parse_resolved(v2).unwrap();
        assert_eq!(r["https://github.com/apple/swift-log"], "1.15.1");
        assert_eq!(r.len(), 1, "branch pins have no version");
        let v1 = r#"{"object":{"pins":[{"package":"swift-log","repositoryURL":"https://github.com/apple/swift-log.git","state":{"version":"1.4.0"}}]},"version":1}"#;
        assert_eq!(parse_resolved(v1).unwrap()["https://github.com/apple/swift-log"], "1.4.0");
    }

    #[test]
    fn ls_remote_tags_parse() {
        let out = "abc\trefs/tags/1.15.1\ndef\trefs/tags/v1.16.0\nfed\trefs/tags/1.16.0^{}\n";
        assert_eq!(parse_ls_remote(out), ["1.15.1", "v1.16.0", "1.16.0"]);
    }
}
