//! Deterministic dependency update classification.
//!
//! Before the maintain agent runs, Foundry works out — in code, not in a
//! prompt — which of a project's direct dependencies have newer releases, and
//! how big each move is. Each ecosystem contributes two pure pieces (a parser
//! for its manifest and lockfile, a parser for its registry's answer) and the
//! shared core does the rest:
//!
//! - [`version`] orders versions and classifies a move as patch, minor or
//!   major, treating a `0.x` minor bump as major.
//! - [`requirement`] reads each ecosystem's constraint operators, so a
//!   lockfile move is never mistaken for a constraint edit.
//! - [`assess`] turns one dependency and its published releases into an
//!   [`OutdatedDependency`].
//! - [`brief`] applies the project's update policy, holds and advisories to
//!   decide what maintenance may apply.
//!
//! The imperative shell is [`classify`]: it reads the files, asks a
//! [`VersionSource`] for releases, and records anything it could not classify
//! as an [`UnclassifiedScope`] instead of pretending nothing is outdated.

pub mod brief;
mod cargo;
mod gradle;
mod hex;
pub mod majors;
mod npm;
mod python;
pub mod requirement;
mod swiftpm;
pub mod version;

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use foundry_sdk::dependency_holds::{DependencyHolds, HoldDecision};
use foundry_sdk::payload::{
    Advisory, AppliedHold, DependencyClassification, Ecosystem, LapsedHold, OutdatedDependency,
    SupplyChainFinding, TransitiveAdvisory, UnclassifiedScope, UpdateClass,
};
use foundry_sdk::registry::Stack;

use crate::gateway::ShellGateway;

use self::requirement::Requirement;
use self::version::{Version, stable_versions};

/// How a dependency's releases are looked up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lookup {
    /// Where to look: a git URL (SwiftPM) or `maven-metadata.xml` URLs
    /// (Gradle). Empty for ecosystems with one registry.
    pub sources: Vec<String>,
}

/// A direct dependency as its manifest and lockfile describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declared {
    /// The name its registry knows it by.
    pub package: String,
    /// The manifest constraint, when one is written.
    pub requirement: Option<String>,
    /// The locked version. `None` when the lockfile does not have it.
    pub current: Option<String>,
    pub lookup: Lookup,
}

impl Declared {
    pub(crate) fn new(
        package: String,
        requirement: Option<String>,
        current: Option<String>,
    ) -> Self {
        Self {
            package,
            requirement,
            current,
            lookup: Lookup::default(),
        }
    }
}

/// The declared dependencies of one manifest location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub ecosystem: Ecosystem,
    /// Location relative to the repository root.
    pub manifest: String,
    pub deps: Vec<Declared>,
    pub unclassified: Vec<UnclassifiedScope>,
}

impl Scope {
    fn label(&self) -> String {
        scope_label(self.ecosystem, &self.manifest)
    }
}

fn scope_label(ecosystem: Ecosystem, manifest: &str) -> String {
    format!("{ecosystem} ({manifest})")
}

/// A scope that could not be read at all.
pub(crate) fn unclassified(ecosystem: Ecosystem, manifest: &str, reason: String) -> Scope {
    Scope {
        ecosystem,
        manifest: manifest.to_string(),
        deps: Vec::new(),
        unclassified: vec![UnclassifiedScope {
            scope: scope_label(ecosystem, manifest),
            reason,
        }],
    }
}

pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| {
        let name = path
            .file_name()
            .map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().to_string());
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("{name} not found")
        } else {
            format!("{name}: {e}")
        }
    })
}

/// Every scope in the repository, by ecosystem manifest present at the root
/// (and every Mix project, found the way the audit finds them).
pub fn discover(root: &Path, stack: &Stack) -> Vec<Scope> {
    let mut scopes = Vec::new();
    if root.join("Cargo.toml").is_file() {
        scopes.extend(cargo::scopes(root));
    }
    if root.join("mix.exs").is_file() || !crate::scanner::mix_projects(root).is_empty() {
        scopes.extend(hex::scopes(root));
    }
    if root.join("package.json").is_file() {
        scopes.extend(npm::scopes(root));
    }
    if root.join("pyproject.toml").is_file() {
        scopes.extend(python::scopes(root));
    } else if root.join("requirements.txt").is_file() {
        scopes.push(unclassified(
            Ecosystem::Pypi,
            ".",
            "requirements.txt without pyproject.toml and uv.lock is not supported".to_string(),
        ));
    }
    if root.join(gradle::CATALOG).is_file() {
        scopes.extend(gradle::scopes(root));
    } else if *stack == Stack::Kotlin {
        scopes.push(unclassified(
            Ecosystem::Maven,
            gradle::CATALOG,
            "no Gradle version catalog; only gradle/libs.versions.toml is classified".to_string(),
        ));
    }
    if root.join("Package.swift").is_file() {
        scopes.extend(swiftpm::scopes(root));
    }
    scopes
}

/// The locked version of every direct dependency, keyed
/// `"<ecosystem> <manifest> <package>"`.
///
/// Used to check what a blanket lockfile refresh actually moved.
pub fn locked_direct_versions(
    root: &Path,
    stack: &Stack,
) -> std::collections::BTreeMap<String, (Ecosystem, String)> {
    discover(root, stack)
        .into_iter()
        .flat_map(|scope| {
            let ecosystem = scope.ecosystem;
            let manifest = scope.manifest;
            scope.deps.into_iter().filter_map(move |d| {
                let current = d.current?;
                Some((format!("{ecosystem} {manifest} {}", d.package), (ecosystem, current)))
            })
        })
        .collect()
}

/// The direct dependencies that moved by a major between two
/// [`locked_direct_versions`] snapshots, as `"<key> <from> -> <to>"`.
pub fn major_moves(
    before: &std::collections::BTreeMap<String, (Ecosystem, String)>,
    after: &std::collections::BTreeMap<String, (Ecosystem, String)>,
) -> Vec<String> {
    after
        .iter()
        .filter_map(|(key, (ecosystem, to))| {
            let (_, from) = before.get(key)?;
            let class =
                Version::parse(*ecosystem, from)?.class_to(&Version::parse(*ecosystem, to)?)?;
            (class == UpdateClass::Major).then(|| format!("{key} {from} -> {to}"))
        })
        .collect()
}

/// The stack's own ecosystem, used for advisories on transitive packages.
pub fn stack_ecosystem(stack: &Stack) -> Option<Ecosystem> {
    match stack {
        Stack::Rust => Some(Ecosystem::Cargo),
        Stack::Elixir => Some(Ecosystem::Hex),
        Stack::TypeScript => Some(Ecosystem::Npm),
        Stack::Python => Some(Ecosystem::Pypi),
        Stack::Kotlin => Some(Ecosystem::Maven),
        Stack::Swift => Some(Ecosystem::Swiftpm),
        Stack::Cpp => None,
    }
}

/// Where releases come from. The production source is [`RegistryVersionSource`].
pub trait VersionSource: Send + Sync {
    /// Every published version of `dep` (unfiltered). `current` lets a source
    /// skip a large download when the newest release is the one installed.
    fn versions<'a>(
        &'a self,
        ecosystem: Ecosystem,
        dep: &'a Declared,
        current: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>>;
}

/// Advisory findings for one project, from the latest supply-chain scan.
#[derive(Debug, Clone, Default)]
pub struct Advisories {
    pub findings: Vec<SupplyChainFinding>,
    /// Where they came from (or why there are none), for the summary.
    pub source: Option<String>,
}

/// Compare package names the way `ecosystem` does.
fn same_package(ecosystem: Ecosystem, a: &str, b: &str) -> bool {
    if ecosystem == Ecosystem::Pypi {
        python::normalize(a) == python::normalize(b)
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

/// Classify one project: discover its scopes, look up releases, apply holds
/// and attach advisories.
pub async fn classify(
    root: &Path,
    stack: &Stack,
    holds: Result<DependencyHolds, String>,
    advisories: Advisories,
    source: &dyn VersionSource,
    today: NaiveDate,
) -> DependencyClassification {
    let mut out = DependencyClassification {
        advisory_source: advisories.source.clone(),
        ..DependencyClassification::default()
    };
    let holds = match holds {
        Ok(h) => h,
        Err(warning) => {
            out.holds_warning = Some(warning);
            DependencyHolds::default()
        }
    };

    let scopes = discover(root, stack);
    if scopes.is_empty() {
        out.unclassified.push(UnclassifiedScope {
            scope: stack.to_string(),
            reason: match stack {
                Stack::Cpp => "no dependency classifier for C++".to_string(),
                _ => "no supported manifest found at the repository root".to_string(),
            },
        });
    }

    let mut declared_names: Vec<(Ecosystem, String)> = Vec::new();
    for scope in &scopes {
        if scope.unclassified.is_empty() {
            out.classified.push(scope.label());
        }
        out.unclassified.extend(scope.unclassified.iter().cloned());
        for dep in &scope.deps {
            declared_names.push((scope.ecosystem, dep.package.clone()));
            let Some(current_raw) = dep.current.as_deref() else {
                out.unclassified.push(UnclassifiedScope {
                    scope: format!("{} {}", scope.label(), dep.package),
                    reason: "declared but not in the lockfile".to_string(),
                });
                continue;
            };
            let Some(current) = Version::parse(scope.ecosystem, current_raw) else {
                out.unclassified.push(UnclassifiedScope {
                    scope: format!("{} {}", scope.label(), dep.package),
                    reason: format!("locked version '{current_raw}' is not a version"),
                });
                continue;
            };
            let published = match source.versions(scope.ecosystem, dep, current_raw).await {
                Ok(v) => v,
                Err(reason) => {
                    out.unclassified.push(UnclassifiedScope {
                        scope: format!("{} {}", scope.label(), dep.package),
                        reason,
                    });
                    continue;
                }
            };
            let hold = holds.decide(scope.ecosystem.as_str(), &dep.package, today);
            if let HoldDecision::Lapsed {
                max,
                reason,
                expired_on,
            } = &hold
                && !out
                    .lapsed_holds
                    .iter()
                    .any(|l| same_package(scope.ecosystem, &l.package, &dep.package))
            {
                out.lapsed_holds.push(LapsedHold {
                    package: dep.package.clone(),
                    max: max.clone(),
                    reason: reason.clone(),
                    expired_on: expired_on.clone(),
                });
            }
            let versions = stable_versions(scope.ecosystem, published.iter().map(String::as_str));
            match assess(scope.ecosystem, &scope.manifest, dep, &current, &versions, &hold) {
                Ok(Some(outdated)) => out.outdated.push(outdated),
                Ok(None) => {}
                Err(warning) => {
                    out.holds_warning.get_or_insert(warning);
                }
            }
        }
    }

    attach_advisories(&mut out, &declared_names, stack_ecosystem(stack), &advisories.findings);
    out
}

/// One dependency against its published stable releases (sorted ascending).
///
/// `Ok(None)` when it is up to date. `Err` only when an active hold's `max`
/// is not a version prefix; the hold is then ignored and reported.
pub fn assess(
    ecosystem: Ecosystem,
    manifest: &str,
    dep: &Declared,
    current: &Version,
    versions: &[Version],
    hold: &HoldDecision,
) -> Result<Option<OutdatedDependency>, String> {
    let newer: Vec<&Version> = versions.iter().filter(|v| current.is_upgrade_to(v)).collect();
    let Some(latest) = newer.last() else {
        return Ok(None);
    };
    let is_major = |v: &Version| current.class_to(v) == Some(UpdateClass::Major);
    let requirement = dep.requirement.as_deref().and_then(|r| Requirement::parse(ecosystem, r));

    let non_major = newer.iter().rev().find(|v| !is_major(v));
    let in_range = requirement
        .as_ref()
        .and_then(|r| newer.iter().rev().find(|v| !is_major(v) && r.admits(v)));
    let constraint_admits_major = requirement
        .as_ref()
        .is_some_and(|r| newer.iter().any(|v| is_major(v) && r.admits(v)));

    let mut warning = None;
    let hold = match hold {
        HoldDecision::Active {
            max,
            reason,
            expires,
        } => {
            if newer.first().and_then(|v| v.within_prefix(max)).is_none() {
                warning = Some(format!(
                    "hold on {} has max '{max}', which is not a version prefix; hold ignored",
                    dep.package
                ));
                None
            } else {
                let newest_within = newer.iter().rev().find(|v| v.within_prefix(max) == Some(true));
                Some(AppliedHold {
                    max: max.clone(),
                    reason: reason.clone(),
                    expires: expires.clone(),
                    newest_within: newest_within.map(|v| v.as_str().to_string()),
                })
            }
        }
        HoldDecision::NotHeld | HoldDecision::Lapsed { .. } => None,
    };

    let outdated = OutdatedDependency {
        ecosystem,
        manifest: manifest.to_string(),
        package: dep.package.clone(),
        current: current.as_str().to_string(),
        requirement: dep.requirement.clone(),
        in_range: in_range.map(|v| v.as_str().to_string()),
        non_major: non_major.map(|v| v.as_str().to_string()),
        major: is_major(latest).then(|| latest.as_str().to_string()),
        constraint_admits_major,
        hold,
        advisories: Vec::new(),
    };
    match warning {
        Some(w) => Err(w),
        None => Ok(Some(outdated)),
    }
}

/// Attach each advisory to the direct dependency it names, or record it as a
/// transitive advisory. Findings a newer install already fixes are dropped.
fn attach_advisories(
    out: &mut DependencyClassification,
    declared: &[(Ecosystem, String)],
    primary: Option<Ecosystem>,
    findings: &[SupplyChainFinding],
) {
    for finding in findings {
        let advisory = Advisory {
            id: finding.cve.clone(),
            fix_version: finding.fix_version.clone(),
            severity: finding.severity.clone(),
        };
        if let Some(dep) = out
            .outdated
            .iter_mut()
            .find(|d| same_package(d.ecosystem, &d.package, &finding.package))
        {
            let already_fixed = match (
                Version::parse(dep.ecosystem, &dep.current),
                finding.fix_version.as_deref().and_then(|f| Version::parse(dep.ecosystem, f)),
            ) {
                (Some(current), Some(fix)) => current >= fix,
                _ => false,
            };
            if !already_fixed && !dep.advisories.contains(&advisory) {
                dep.advisories.push(advisory);
            }
            continue;
        }
        let direct = declared.iter().any(|(eco, name)| same_package(*eco, name, &finding.package));
        if direct {
            // A direct dependency with nothing newer: no release fixes it yet.
            continue;
        }
        if let Some(ecosystem) = primary {
            out.transitive_advisories.push(TransitiveAdvisory {
                ecosystem,
                package: finding.package.clone(),
                version: finding.version.clone(),
                advisory,
            });
        }
    }
}

/// Registry lookups over `curl` and `git`, cached for the daemon's lifetime.
///
/// Uses the shell rather than an HTTP client so the lookups run through the
/// same gateway (and the same fakes) as every other external call.
pub struct RegistryVersionSource {
    shell: Arc<dyn ShellGateway>,
    workdir: std::path::PathBuf,
    cache: Mutex<HashMap<CacheKey, (Instant, Vec<String>)>>,
}

/// A registry answer's cache key: the ecosystem and the lookup it answered.
type CacheKey = (Ecosystem, String);

/// How long a registry answer is reused: long enough to cover a whole night's
/// before-and-after classification of every project.
const CACHE_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

impl RegistryVersionSource {
    pub fn new(shell: Arc<dyn ShellGateway>) -> Self {
        Self {
            shell,
            workdir: std::env::temp_dir(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached(&self, key: &CacheKey) -> Option<Vec<String>> {
        let cache = self.cache.lock().ok()?;
        cache
            .get(key)
            .filter(|(at, _)| at.elapsed() < CACHE_TTL)
            .map(|(_, v)| v.clone())
    }

    fn remember(&self, key: CacheKey, versions: &[String]) {
        match self.cache.lock() {
            Ok(mut cache) => {
                cache.insert(key, (Instant::now(), versions.to_vec()));
            }
            Err(e) => {
                // Best-effort: the cache only saves repeat downloads; a
                // poisoned lock means the next lookup fetches again.
                tracing::warn!(error = %e, "registry cache lock poisoned; not caching");
            }
        }
    }

    async fn curl(&self, url: &str, accept: Option<&str>) -> Result<String, String> {
        let mut args = vec![
            "-fsSL",
            "--compressed",
            "--max-time",
            "90",
            "--retry",
            "3",
            "--retry-delay",
            "20",
        ];
        if let Some(accept) = accept {
            args.push("-H");
            args.push(accept);
        }
        args.push(url);
        let result = self
            .shell
            .run(&self.workdir, "curl", &args, None, Some(FETCH_TIMEOUT))
            .await
            .map_err(|e| format!("curl {url}: {e}"))?;
        if result.success {
            Ok(result.stdout)
        } else {
            Err(format!("curl {url} failed ({}): {}", result.exit_code, result.stderr.trim()))
        }
    }

    async fn fetch(
        &self,
        ecosystem: Ecosystem,
        dep: &Declared,
        current: &str,
    ) -> Result<Vec<String>, String> {
        match ecosystem {
            Ecosystem::Cargo => {
                let url = format!("https://index.crates.io/{}", cargo::index_path(&dep.package));
                cargo::parse_index(&self.curl(&url, None).await?)
            }
            Ecosystem::Hex => {
                let url = format!("https://hex.pm/api/packages/{}", dep.package);
                hex::parse_releases(&self.curl(&url, Some("Accept: application/json")).await?)
            }
            Ecosystem::Npm => {
                let base =
                    format!("https://registry.npmjs.org/{}", npm::registry_path(&dep.package));
                let latest = npm::parse_latest(&self.curl(&format!("{base}/latest"), None).await?)?;
                if latest == current {
                    return Ok(vec![latest]);
                }
                let accept = "Accept: application/vnd.npm.install-v1+json";
                npm::parse_packument(&self.curl(&base, Some(accept)).await?)
            }
            Ecosystem::Pypi => {
                let url = format!("https://pypi.org/pypi/{}/json", python::normalize(&dep.package));
                python::parse_releases(&self.curl(&url, None).await?)
            }
            Ecosystem::Maven => {
                let mut errors = Vec::new();
                for url in &dep.lookup.sources {
                    match self.curl(url, None).await.and_then(|b| gradle::parse_metadata(&b)) {
                        Ok(v) => return Ok(v),
                        Err(e) => errors.push(e),
                    }
                }
                Err(format!("no repository answered: {}", errors.join("; ")))
            }
            Ecosystem::Swiftpm => {
                let url = dep.lookup.sources.first().ok_or("no repository URL")?;
                let env = [("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())];
                let result = self
                    .shell
                    .run(
                        &self.workdir,
                        "git",
                        &["ls-remote", "--tags", "--refs", url],
                        Some(&env),
                        Some(FETCH_TIMEOUT),
                    )
                    .await
                    .map_err(|e| format!("git ls-remote {url}: {e}"))?;
                if result.success {
                    Ok(swiftpm::parse_ls_remote(&result.stdout))
                } else {
                    Err(format!("git ls-remote {url} failed: {}", result.stderr.trim()))
                }
            }
        }
    }
}

impl VersionSource for RegistryVersionSource {
    fn versions<'a>(
        &'a self,
        ecosystem: Ecosystem,
        dep: &'a Declared,
        current: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
        Box::pin(async move {
            let key = (ecosystem, format!("{}|{}", dep.package, dep.lookup.sources.join(",")));
            if let Some(hit) = self.cached(&key) {
                return Ok(hit);
            }
            let versions = self.fetch(ecosystem, dep, current).await?;
            self.remember(key, &versions);
            Ok(versions)
        })
    }
}

/// In-memory release sources for tests.
#[cfg(any(test, feature = "test-support"))]
pub mod fakes {
    use super::{Declared, Ecosystem, Future, HashMap, Pin, VersionSource};

    /// Versions by package name; a missing package is a lookup failure.
    pub struct FakeVersionSource(pub HashMap<String, Vec<String>>);

    impl FakeVersionSource {
        /// A source that knows these packages and versions.
        pub fn with(entries: &[(&str, &[&str])]) -> Self {
            Self(
                entries
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), v.iter().map(|s| (*s).to_string()).collect()))
                    .collect(),
            )
        }
    }

    impl VersionSource for FakeVersionSource {
        fn versions<'a>(
            &'a self,
            _ecosystem: Ecosystem,
            dep: &'a Declared,
            _current: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, String>> + Send + 'a>> {
            let result = self
                .0
                .get(&dep.package)
                .cloned()
                .ok_or_else(|| format!("registry has no {}", dep.package));
            Box::pin(async move { result })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::FakeVersionSource;
    use super::*;
    use foundry_sdk::dependency_holds::HoldEntry;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 9, 25).unwrap()
    }

    fn versions(eco: Ecosystem, raw: &[&str]) -> Vec<Version> {
        stable_versions(eco, raw.iter().copied())
    }

    fn assess_one(
        eco: Ecosystem,
        req: Option<&str>,
        current: &str,
        raw: &[&str],
        hold: &HoldDecision,
    ) -> Option<OutdatedDependency> {
        let dep =
            Declared::new("pkg".to_string(), req.map(str::to_string), Some(current.to_string()));
        let current = Version::parse(eco, current).unwrap();
        assess(eco, ".", &dep, &current, &versions(eco, raw), hold).unwrap()
    }

    #[test]
    fn an_up_to_date_dependency_is_not_outdated() {
        assert!(
            assess_one(
                Ecosystem::Cargo,
                Some("1"),
                "1.2.0",
                &["1.0.0", "1.2.0"],
                &HoldDecision::NotHeld
            )
            .is_none()
        );
    }

    #[test]
    fn caret_admits_minors_so_they_are_in_range() {
        let d = assess_one(
            Ecosystem::Cargo,
            Some("1.2"),
            "1.2.0",
            &["1.2.0", "1.3.1", "1.4.0", "2.0.0", "2.1.0-rc.1"],
            &HoldDecision::NotHeld,
        )
        .unwrap();
        assert_eq!(d.in_range.as_deref(), Some("1.4.0"));
        assert_eq!(d.non_major.as_deref(), Some("1.4.0"));
        assert_eq!(d.major.as_deref(), Some("2.0.0"));
        assert!(!d.constraint_admits_major);
    }

    #[test]
    fn a_pessimistic_patch_constraint_leaves_minors_out_of_range() {
        let d = assess_one(
            Ecosystem::Hex,
            Some("~> 1.8.1"),
            "1.8.1",
            &["1.8.1", "1.8.3", "1.9.0"],
            &HoldDecision::NotHeld,
        )
        .unwrap();
        assert_eq!(d.in_range.as_deref(), Some("1.8.3"));
        assert_eq!(d.non_major.as_deref(), Some("1.9.0"));
        assert_eq!(d.major, None);
    }

    #[test]
    fn a_zero_major_minor_is_a_major() {
        let d = assess_one(
            Ecosystem::Hex,
            Some("~> 0.7"),
            "0.7.4",
            &["0.7.4", "0.7.5", "0.8.0"],
            &HoldDecision::NotHeld,
        )
        .unwrap();
        assert_eq!(d.non_major.as_deref(), Some("0.7.5"));
        assert_eq!(
            d.in_range.as_deref(),
            Some("0.7.5"),
            "~> 0.7 admits 0.8.0, but that is a major"
        );
        assert_eq!(d.major.as_deref(), Some("0.8.0"));
        assert!(d.constraint_admits_major);
    }

    #[test]
    fn an_open_python_constraint_admits_a_major() {
        let d = assess_one(
            Ecosystem::Pypi,
            Some(">=0.116.0"),
            "1.8.0",
            &["1.8.0", "1.9.0", "2.0.0"],
            &HoldDecision::NotHeld,
        )
        .unwrap();
        assert_eq!(d.in_range.as_deref(), Some("1.9.0"));
        assert!(d.constraint_admits_major);
    }

    #[test]
    fn an_unknown_constraint_has_no_in_range_move() {
        let d = assess_one(
            Ecosystem::Npm,
            Some("github:x/y"),
            "1.0.0",
            &["1.0.0", "1.1.0"],
            &HoldDecision::NotHeld,
        )
        .unwrap();
        assert_eq!(d.in_range, None);
        assert_eq!(d.non_major.as_deref(), Some("1.1.0"));
    }

    #[test]
    fn an_active_hold_records_the_newest_release_inside_it() {
        let hold = HoldDecision::Active {
            max: "1.1".to_string(),
            reason: "roost".to_string(),
            expires: None,
        };
        let d = assess_one(
            Ecosystem::Hex,
            Some("~> 1.0"),
            "1.0.9",
            &["1.0.9", "1.1.4", "1.2.0"],
            &hold,
        )
        .unwrap();
        let applied = d.hold.unwrap();
        assert_eq!(applied.newest_within.as_deref(), Some("1.1.4"));
    }

    #[test]
    fn a_hold_whose_max_is_not_a_version_is_reported() {
        let hold = HoldDecision::Active {
            max: "latest".to_string(),
            reason: "x".to_string(),
            expires: None,
        };
        let dep = Declared::new("pkg".to_string(), None, Some("1.0.0".to_string()));
        let current = Version::parse(Ecosystem::Npm, "1.0.0").unwrap();
        let err = assess(
            Ecosystem::Npm,
            ".",
            &dep,
            &current,
            &versions(Ecosystem::Npm, &["1.0.0", "1.1.0"]),
            &hold,
        )
        .unwrap_err();
        assert!(err.contains("not a version prefix"), "{err}");
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn cargo_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname='app'\n[dependencies]\nserde='1'\nrand='0.8'\nlocal={path='x'}\n",
        );
        write(
            dir.path(),
            "Cargo.lock",
            "version=4\n[[package]]\nname='serde'\nversion='1.0.100'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n[[package]]\nname='rand'\nversion='0.8.5'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n",
        );
        dir
    }

    #[tokio::test]
    async fn classify_reports_outdated_dependencies_per_scope() {
        let repo = cargo_repo();
        let source = FakeVersionSource::with(&[
            ("serde", &["1.0.100", "1.0.228"]),
            ("rand", &["0.8.5", "0.9.2"]),
        ]);
        let c = classify(
            repo.path(),
            &Stack::Rust,
            Ok(DependencyHolds::default()),
            Advisories::default(),
            &source,
            day(),
        )
        .await;
        assert_eq!(c.classified, ["cargo (.)"]);
        assert!(c.unclassified.is_empty(), "{:?}", c.unclassified);
        let rand = c.outdated.iter().find(|d| d.package == "rand").unwrap();
        assert_eq!(rand.major.as_deref(), Some("0.9.2"));
        assert_eq!(rand.non_major, None);
        let serde = c.outdated.iter().find(|d| d.package == "serde").unwrap();
        assert_eq!(serde.in_range.as_deref(), Some("1.0.228"));
    }

    #[tokio::test]
    async fn a_failed_lookup_is_unclassified_not_up_to_date() {
        let repo = cargo_repo();
        let source = FakeVersionSource::with(&[("serde", &["1.0.100"])]);
        let c = classify(
            repo.path(),
            &Stack::Rust,
            Ok(DependencyHolds::default()),
            Advisories::default(),
            &source,
            day(),
        )
        .await;
        assert_eq!(c.unclassified.len(), 1);
        assert_eq!(c.unclassified[0].scope, "cargo (.) rand");
        assert!(c.unclassified[0].reason.contains("registry has no rand"));
    }

    #[tokio::test]
    async fn a_stack_without_a_classifier_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "CMakeLists.txt", "project(x)");
        let source = FakeVersionSource::with(&[]);
        let c = classify(
            dir.path(),
            &Stack::Cpp,
            Ok(DependencyHolds::default()),
            Advisories::default(),
            &source,
            day(),
        )
        .await;
        assert!(c.outdated.is_empty());
        assert_eq!(c.unclassified[0].scope, "cpp");
        assert!(c.unclassified[0].reason.contains("no dependency classifier"));
    }

    #[tokio::test]
    async fn a_kotlin_project_without_a_catalog_is_unclassified() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "build.gradle.kts", "plugins {}");
        let source = FakeVersionSource::with(&[]);
        let c = classify(
            dir.path(),
            &Stack::Kotlin,
            Ok(DependencyHolds::default()),
            Advisories::default(),
            &source,
            day(),
        )
        .await;
        assert_eq!(c.unclassified[0].scope, "maven (gradle/libs.versions.toml)");
    }

    #[tokio::test]
    async fn malformed_holds_fail_safe_with_a_warning() {
        let repo = cargo_repo();
        let source =
            FakeVersionSource::with(&[("serde", &["1.0.100", "1.0.228"]), ("rand", &["0.8.5"])]);
        let c = classify(
            repo.path(),
            &Stack::Rust,
            Err("bad json".to_string()),
            Advisories::default(),
            &source,
            day(),
        )
        .await;
        assert_eq!(c.holds_warning.as_deref(), Some("bad json"));
        assert_eq!(c.outdated.len(), 1);
    }

    #[tokio::test]
    async fn lapsed_holds_are_reported_and_no_longer_apply() {
        let repo = cargo_repo();
        let source =
            FakeVersionSource::with(&[("serde", &["1.0.100"]), ("rand", &["0.8.5", "0.9.2"])]);
        let holds = DependencyHolds {
            version: 1,
            holds: vec![HoldEntry {
                package: "rand".to_string(),
                max: "0.8".to_string(),
                reason: "waiting on rand_core".to_string(),
                expires: Some("2026-09-01".to_string()),
                ecosystem: None,
            }],
        };
        let c =
            classify(repo.path(), &Stack::Rust, Ok(holds), Advisories::default(), &source, day())
                .await;
        assert_eq!(c.lapsed_holds.len(), 1);
        assert_eq!(c.lapsed_holds[0].expired_on, "2026-09-01");
        let rand = c.outdated.iter().find(|d| d.package == "rand").unwrap();
        assert!(rand.hold.is_none());
    }

    #[test]
    fn major_moves_compare_locked_snapshots() {
        let repo = cargo_repo();
        let before = locked_direct_versions(repo.path(), &Stack::Rust);
        assert_eq!(before["cargo . rand"], (Ecosystem::Cargo, "0.8.5".to_string()));
        let mut after = before.clone();
        after.insert("cargo . rand".to_string(), (Ecosystem::Cargo, "0.9.2".to_string()));
        after.insert("cargo . serde".to_string(), (Ecosystem::Cargo, "1.0.228".to_string()));
        assert_eq!(major_moves(&before, &after), ["cargo . rand 0.8.5 -> 0.9.2"]);
        assert!(major_moves(&before, &before).is_empty());
    }

    #[tokio::test]
    async fn advisories_attach_to_direct_dependencies_or_are_transitive() {
        let repo = cargo_repo();
        let source =
            FakeVersionSource::with(&[("serde", &["1.0.100", "1.0.228"]), ("rand", &["0.8.5"])]);
        let finding = |package: &str, fix: &str| SupplyChainFinding {
            cve: format!("RUSTSEC-{package}"),
            package: package.to_string(),
            version: Some("0.1.0".to_string()),
            fix_version: Some(fix.to_string()),
            ..SupplyChainFinding::default()
        };
        let advisories = Advisories {
            findings: vec![
                finding("serde", "1.0.200"),
                finding("idna", "1.0.0"),
                finding("serde", "1.0.50"),
            ],
            source: Some("supply-chain scan 2026-09-25".to_string()),
        };
        let c = classify(
            repo.path(),
            &Stack::Rust,
            Ok(DependencyHolds::default()),
            advisories,
            &source,
            day(),
        )
        .await;
        let serde = c.outdated.iter().find(|d| d.package == "serde").unwrap();
        assert_eq!(serde.advisories.len(), 1, "a fix at or below the installed version is dropped");
        assert_eq!(serde.advisories[0].fix_version.as_deref(), Some("1.0.200"));
        assert_eq!(c.transitive_advisories.len(), 1);
        assert_eq!(c.transitive_advisories[0].package, "idna");
        assert_eq!(c.transitive_advisories[0].ecosystem, Ecosystem::Cargo);
        assert_eq!(c.advisory_source.as_deref(), Some("supply-chain scan 2026-09-25"));
    }
}
