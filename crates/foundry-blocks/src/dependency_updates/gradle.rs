//! Gradle: the version catalog `gradle/libs.versions.toml`, releases from
//! `maven-metadata.xml` on Maven Central, Google's Maven repository and the
//! Gradle plugin portal.
//!
//! A catalog has no lockfile: the version it names is the version in use. A
//! `[versions]` key shared by several libraries and plugins (`ktor`, `kotlin`)
//! moves as one, so it is classified once, under the key's name. A version
//! written inline on a library or plugin is classified under its coordinate.

use std::collections::BTreeMap;
use std::path::Path;

use foundry_sdk::payload::Ecosystem;
use toml::Value;

use super::{Declared, Scope, read_text, unclassified};

pub(super) const CATALOG: &str = "gradle/libs.versions.toml";

/// Where to find a coordinate's `maven-metadata.xml`.
const LIBRARY_REPOSITORIES: [&str; 2] = [
    "https://repo1.maven.org/maven2",
    "https://dl.google.com/dl/android/maven2",
];
const PLUGIN_REPOSITORIES: [&str; 3] = [
    "https://plugins.gradle.org/m2",
    "https://dl.google.com/dl/android/maven2",
    "https://repo1.maven.org/maven2",
];

pub(super) fn scopes(root: &Path, rel: &str) -> Vec<Scope> {
    let manifest = if rel == "." {
        CATALOG.to_string()
    } else {
        format!("{rel}/{CATALOG}")
    };
    let text = match read_text(&root.join(CATALOG)) {
        Ok(t) => t,
        Err(reason) => return vec![unclassified(Ecosystem::Maven, &manifest, reason)],
    };
    match parse_catalog(&text) {
        Ok(deps) => vec![Scope {
            ecosystem: Ecosystem::Maven,
            manifest: manifest.clone(),
            deps,
            unclassified: Vec::new(),
        }],
        Err(reason) => vec![unclassified(Ecosystem::Maven, &manifest, reason)],
    }
}

/// A version declaration: the version in use and the constraint written.
fn version_of(value: &Value) -> Option<(String, String)> {
    match value {
        Value::String(v) => Some((v.clone(), v.clone())),
        // Rich versions: `{ strictly = "[1.0,2.0)", prefer = "1.5" }`.
        Value::Table(t) => {
            let get = |k: &str| t.get(k).and_then(Value::as_str).map(str::to_string);
            let constraint = get("strictly").or_else(|| get("require"))?;
            let current = get("prefer").unwrap_or_else(|| constraint.clone());
            Some((current, constraint))
        }
        _ => None,
    }
}

/// `maven-metadata.xml` URLs for a library coordinate `group:artifact`.
fn library_urls(coordinate: &str) -> Vec<String> {
    let Some((group, artifact)) = coordinate.split_once(':') else {
        return Vec::new();
    };
    LIBRARY_REPOSITORIES
        .iter()
        .map(|repo| format!("{repo}/{}/{artifact}/maven-metadata.xml", group.replace('.', "/")))
        .collect()
}

/// `maven-metadata.xml` URLs for a plugin id's marker artifact.
fn plugin_urls(id: &str) -> Vec<String> {
    PLUGIN_REPOSITORIES
        .iter()
        .map(|repo| {
            format!("{repo}/{}/{id}.gradle.plugin/maven-metadata.xml", id.replace('.', "/"))
        })
        .collect()
}

/// A catalog entry's coordinate (or plugin id), its metadata URLs, and how its
/// version is given.
struct Entry {
    name: String,
    urls: Vec<String>,
    version_ref: Option<String>,
    version: Option<Value>,
}

fn library(alias: &str, value: &Value) -> Option<Entry> {
    match value {
        Value::String(gav) => {
            let mut parts = gav.splitn(3, ':');
            let (group, artifact) = (parts.next()?, parts.next()?);
            let coordinate = format!("{group}:{artifact}");
            Some(Entry {
                urls: library_urls(&coordinate),
                name: coordinate,
                version_ref: None,
                version: parts.next().map(|v| Value::String(v.to_string())),
            })
        }
        Value::Table(t) => {
            let coordinate = match t.get("module").and_then(Value::as_str) {
                Some(m) => m.to_string(),
                None => format!(
                    "{}:{}",
                    t.get("group").and_then(Value::as_str)?,
                    t.get("name").and_then(Value::as_str)?
                ),
            };
            let (version_ref, version) = version_field(t);
            Some(Entry {
                urls: library_urls(&coordinate),
                name: coordinate,
                version_ref,
                version,
            })
        }
        _ => {
            tracing::debug!(%alias, "unrecognised catalog library entry");
            None
        }
    }
}

fn plugin(value: &Value) -> Option<Entry> {
    match value {
        Value::String(spec) => {
            let (id, version) =
                spec.split_once(':').map_or((spec.as_str(), None), |(i, v)| (i, Some(v)));
            Some(Entry {
                urls: plugin_urls(id),
                name: id.to_string(),
                version_ref: None,
                version: version.map(|v| Value::String(v.to_string())),
            })
        }
        Value::Table(t) => {
            let id = t.get("id").and_then(Value::as_str)?;
            let (version_ref, version) = version_field(t);
            Some(Entry {
                urls: plugin_urls(id),
                name: id.to_string(),
                version_ref,
                version,
            })
        }
        _ => None,
    }
}

/// `version.ref = "key"` or `version = "1.0"` / `version = { ... }`.
fn version_field(t: &toml::Table) -> (Option<String>, Option<Value>) {
    match t.get("version") {
        Some(Value::Table(v)) if v.contains_key("ref") => {
            (v.get("ref").and_then(Value::as_str).map(str::to_string), None)
        }
        Some(v) => (None, Some(v.clone())),
        None => (None, None),
    }
}

/// One declared dependency per `[versions]` key in use and per inline version.
pub(super) fn parse_catalog(text: &str) -> Result<Vec<Declared>, String> {
    let doc: Value = toml::from_str(text).map_err(|e| format!("{CATALOG}: {e}"))?;
    let table = |name: &str| doc.get(name).and_then(Value::as_table);
    let mut entries: Vec<Entry> = Vec::new();
    for (alias, value) in table("libraries").into_iter().flatten() {
        entries.extend(library(alias, value));
    }
    // Libraries first: a shared key resolves from Maven Central when it can.
    for value in table("plugins").into_iter().flatten().map(|(_, v)| v) {
        entries.extend(plugin(value));
    }

    let versions = table("versions");
    let mut by_key: BTreeMap<String, Declared> = BTreeMap::new();
    let mut inline: BTreeMap<String, Declared> = BTreeMap::new();
    for entry in entries {
        if let Some(key) = entry.version_ref {
            let Some((current, constraint)) =
                versions.and_then(|v| v.get(&key)).and_then(version_of)
            else {
                continue;
            };
            by_key
                .entry(key.clone())
                .or_insert_with(|| Declared::new(key, Some(constraint), Some(current)))
                .lookup
                .sources
                .extend(entry.urls);
        } else if let Some((current, constraint)) = entry.version.as_ref().and_then(version_of) {
            let mut declared = Declared::new(entry.name.clone(), Some(constraint), Some(current));
            declared.lookup.sources = entry.urls;
            inline.entry(entry.name).or_insert(declared);
        }
    }
    let mut deps: Vec<Declared> = by_key.into_values().chain(inline.into_values()).collect();
    for dep in &mut deps {
        dep.lookup.sources.dedup();
    }
    Ok(deps)
}

/// Every `<version>` in a `maven-metadata.xml`.
pub(super) fn parse_metadata(body: &str) -> Result<Vec<String>, String> {
    if !body.contains("<metadata") {
        return Err("not a maven-metadata.xml document".to_string());
    }
    let mut versions = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("<version>") {
        rest = &rest[start + "<version>".len()..];
        let Some(end) = rest.find("</version>") else {
            break;
        };
        versions.push(rest[..end].trim().to_string());
        rest = &rest[end..];
    }
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG_TEXT: &str = r#"
[versions]
kotlin = "2.4.20"
ktor = "3.6.0"
unused = "1.0.0"
rich = { strictly = "[1.0,2.0)", prefer = "1.5" }

[libraries]
ktor-client-core = { module = "io.ktor:ktor-client-core", version.ref = "ktor" }
ktor-client-mock = { module = "io.ktor:ktor-client-mock", version.ref = "ktor" }
kotlin-test = { module = "org.jetbrains.kotlin:kotlin-test", version.ref = "kotlin" }
slf4j-simple = { module = "org.slf4j:slf4j-simple", version = "2.0.20" }
okio = { group = "com.squareup.okio", name = "okio", version.ref = "rich" }
guava = "com.google.guava:guava:33.0.0-jre"
bom-managed = { module = "io.ktor:ktor-bom" }

[plugins]
kotlin-multiplatform = { id = "org.jetbrains.kotlin.multiplatform", version.ref = "kotlin" }
detekt = { id = "io.gitlab.arturbosch.detekt", version = "1.23.8" }
"#;

    #[test]
    fn shared_keys_are_one_dependency_with_every_source() {
        let deps = parse_catalog(CATALOG_TEXT).unwrap();
        let ktor = deps.iter().find(|d| d.package == "ktor").unwrap();
        assert_eq!(ktor.current.as_deref(), Some("3.6.0"));
        assert_eq!(ktor.requirement.as_deref(), Some("3.6.0"));
        assert_eq!(
            ktor.lookup.sources[0],
            "https://repo1.maven.org/maven2/io/ktor/ktor-client-core/maven-metadata.xml"
        );
        let kotlin = deps.iter().find(|d| d.package == "kotlin").unwrap();
        assert!(
            kotlin.lookup.sources[0].contains("org/jetbrains/kotlin/kotlin-test"),
            "a library source comes before the plugin marker"
        );
        assert!(kotlin.lookup.sources.iter().any(|s| s.contains("plugins.gradle.org")));
        assert!(!deps.iter().any(|d| d.package == "unused"), "only keys in use are classified");
    }

    #[test]
    fn inline_versions_are_classified_by_coordinate() {
        let deps = parse_catalog(CATALOG_TEXT).unwrap();
        let names: Vec<&str> = deps.iter().map(|d| d.package.as_str()).collect();
        assert!(names.contains(&"org.slf4j:slf4j-simple"));
        assert!(names.contains(&"com.google.guava:guava"));
        assert!(names.contains(&"io.gitlab.arturbosch.detekt"));
        assert!(!names.contains(&"io.ktor:ktor-bom"), "a BOM-managed entry has no version");
        let detekt = deps.iter().find(|d| d.package == "io.gitlab.arturbosch.detekt").unwrap();
        assert!(
            detekt.lookup.sources[0]
                .starts_with("https://plugins.gradle.org/m2/io/gitlab/arturbosch/detekt/")
        );
    }

    #[test]
    fn rich_versions_use_prefer_as_current_and_strictly_as_constraint() {
        let deps = parse_catalog(CATALOG_TEXT).unwrap();
        let rich = deps.iter().find(|d| d.package == "rich").unwrap();
        assert_eq!(rich.current.as_deref(), Some("1.5"));
        assert_eq!(rich.requirement.as_deref(), Some("[1.0,2.0)"));
    }

    #[test]
    fn metadata_versions_parse() {
        let xml = "<?xml version=\"1.0\"?><metadata><versioning><latest>3.7.0</latest><versions><version>3.6.0</version><version>3.7.0</version></versions></versioning></metadata>";
        assert_eq!(parse_metadata(xml).unwrap(), ["3.6.0", "3.7.0"]);
        assert!(parse_metadata("<html>404</html>").is_err());
    }
}
