//! Dependency update classification, the maintain brief, and the majors lane.
//!
//! These types are structured data on purpose. The classifier decides what is
//! outdated and by how much; the brief decides what maintenance may apply; the
//! majors lane decides which breaking upgrades become tasks. None of them is
//! prose that an agent or a later block has to re-derive.

use serde::{Deserialize, Serialize};

use super::context::ChainContext;
use crate::registry::UpdatePolicy;

/// A package ecosystem the classifier understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ecosystem {
    /// Rust crates (`Cargo.toml` + `Cargo.lock`, crates.io).
    Cargo,
    /// Elixir packages (`mix.exs` + `mix.lock`, hex.pm).
    Hex,
    /// JavaScript packages (`package.json` + `package-lock.json` or `bun.lock`, npm).
    Npm,
    /// Python packages (`pyproject.toml` + `uv.lock`, `PyPI`).
    Pypi,
    /// Gradle version catalog entries (`gradle/libs.versions.toml`, Maven Central and the Gradle plugin portal).
    Maven,
    /// Swift packages (`Package.swift` + `Package.resolved`, git tags).
    Swiftpm,
}

impl Ecosystem {
    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Hex => "hex",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Maven => "maven",
            Self::Swiftpm => "swiftpm",
        }
    }
}

impl std::fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How big a version move is.
///
/// Semver-aware: in a `0.x` version a minor bump is breaking, so it is a
/// [`Major`](Self::Major); in a `0.0.x` version every bump is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateClass {
    Patch,
    Minor,
    Major,
}

impl UpdateClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::Minor => "minor",
            Self::Major => "major",
        }
    }
}

impl std::fmt::Display for UpdateClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An active hold that applies to a dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedHold {
    /// The version prefix the package may move up to.
    pub max: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// The newest release inside the hold that is newer than the current
    /// version. `None` when the package is already at the top of the hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_within: Option<String>,
}

/// A published advisory against a package, from the latest supply-chain scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advisory {
    /// Advisory identifier (CVE, GHSA, RUSTSEC, ...).
    pub id: String,
    /// The earliest release that fixes it. `None` when no fix exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

/// A direct dependency that has at least one newer stable release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutdatedDependency {
    pub ecosystem: Ecosystem,
    /// Where the dependency is declared, relative to the repository root
    /// (`"."` for the root, `"apps/bedrock"` for a nested Mix project).
    pub manifest: String,
    pub package: String,
    /// The locked (installed) version.
    pub current: String,
    /// The manifest constraint as written, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<String>,
    /// The newest non-major release the current constraint admits: a lockfile
    /// move. `None` when nothing newer is admitted or the constraint is not
    /// understood.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_range: Option<String>,
    /// The newest release that is not a major upgrade from `current`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_major: Option<String>,
    /// The newest release, when it is a major upgrade from `current`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub major: Option<String>,
    /// The constraint admits a major upgrade (an open `>=` constraint, for
    /// example), so a blanket lockfile refresh would take one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub constraint_admits_major: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<AppliedHold>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<Advisory>,
}

/// A vulnerable package that is not a direct dependency: it moves through the
/// lockfile, or through the direct dependency that pulls it in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitiveAdvisory {
    pub ecosystem: Ecosystem,
    pub package: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub advisory: Advisory,
}

/// A part of the repository the classifier could not classify, and why.
///
/// Never read as "nothing outdated": the summary reports each one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnclassifiedScope {
    /// What was not classified: an ecosystem and location (`"npm (.)"`), a
    /// single package (`"hex (apps/bedrock) phoenix"`), or a whole stack.
    pub scope: String,
    pub reason: String,
}

/// A hold whose expiry has passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LapsedHold {
    pub package: String,
    pub max: String,
    pub reason: String,
    pub expired_on: String,
}

/// Everything the classifier found for one project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyClassification {
    /// Outdated direct dependencies.
    #[serde(default)]
    pub outdated: Vec<OutdatedDependency>,
    /// The scopes that were classified (`"cargo (.)"`, `"hex (apps/bedrock)"`).
    #[serde(default)]
    pub classified: Vec<String>,
    /// The scopes that were not, with reasons.
    #[serde(default)]
    pub unclassified: Vec<UnclassifiedScope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lapsed_holds: Vec<LapsedHold>,
    /// Set when `.dependency-holds.json` could not be read: no holds applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holds_warning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transitive_advisories: Vec<TransitiveAdvisory>,
    /// Where advisory data came from (for example the date of the supply-chain
    /// scan), or why there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory_source: Option<String>,
    /// Vendored scopes left out of classification: their dependencies are
    /// updated upstream, in the vendored project's own repository. The audit
    /// still scans them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vendored: Vec<String>,
    /// Holds whose cap is below the version already locked. They never
    /// produce a downgrade; they need a new decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale_holds: Vec<StaleHold>,
    /// The commit the classification read (short SHA), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Set when the checkout was behind its remote, so the classification
    /// may describe old lockfiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_warning: Option<String>,
}

/// A hold whose cap is already below the locked version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleHold {
    pub ecosystem: Ecosystem,
    pub manifest: String,
    pub package: String,
    /// The version locked now.
    pub locked: String,
    /// The hold's cap.
    pub max: String,
    pub reason: String,
}

/// Whether an update moves only the lockfile or needs a manifest edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// The current constraint already admits the target.
    Lockfile,
    /// The constraint must be widened (or the pinned version edited).
    Manifest,
}

/// One dependency move the brief plans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedUpdate {
    pub ecosystem: Ecosystem,
    pub manifest: String,
    pub package: String,
    pub from: String,
    pub to: String,
    pub class: UpdateClass,
    pub change: ChangeKind,
    /// The advisory this move fixes, when it is a security fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
    /// A security fix took this move past the project's policy ceiling.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub beyond_policy: bool,
    /// A security fix took this move past an active hold, because no fixed
    /// release exists inside the hold.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub beyond_hold: bool,
}

/// An available move the brief does not plan, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldUpdate {
    pub ecosystem: Ecosystem,
    pub manifest: String,
    pub package: String,
    pub from: String,
    pub to: String,
    pub class: UpdateClass,
    pub reason: String,
}

/// What maintenance may do to one project's dependencies tonight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyBrief {
    /// The ceiling in force (the default when none is set).
    pub policy: UpdatePolicy,
    /// `false` when the project has no policy and the default applied.
    pub policy_set: bool,
    /// Moves maintenance must apply — and the only ones it may apply.
    #[serde(default)]
    pub apply: Vec<PlannedUpdate>,
    /// Moves above the policy ceiling (a `patch` project's constraint edits).
    #[serde(default)]
    pub held_by_policy: Vec<HeldUpdate>,
    /// Moves an active hold blocks.
    #[serde(default)]
    pub held_by_hold: Vec<HeldUpdate>,
    /// Major upgrades. Never applied by maintain: each is a task (major
    /// policy, or a security fix) or a proposal (minor and patch policies).
    #[serde(default)]
    pub majors: Vec<PlannedUpdate>,
}

/// Which point in the run a classification describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClassificationPhase {
    /// Before the maintain agent runs: the brief it receives.
    Before,
    /// After maintenance completed: what is still outdated.
    After,
    /// An on-demand `dependency_review_requested`: nothing is applied.
    Review,
}

/// Payload for `DependencyUpdatesClassified`.
///
/// In the `before` phase it also carries the gate-resolution fields forward so
/// `Execute Maintain` has everything it had before this step existed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyUpdatesClassifiedPayload {
    pub project: String,
    pub phase: ClassificationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    pub classification: DependencyClassification,
    pub brief: DependencyBrief,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `DependencyReviewRequested` (all fields optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DependencyReviewRequestedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Preview the brief and majors lane under this policy instead of the
    /// registered one. Changes nothing in the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<UpdatePolicy>,
}

/// What the majors lane decided for one major upgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MajorUpgradeStatus {
    /// Dispatched (or, under `dry_run` and in a review, would be dispatched)
    /// as its own `foundry task`.
    Dispatch,
    /// A task for the same project, package and target is in flight or left
    /// preserved work; not dispatched again.
    Deduped,
    /// Over the per-project or per-night cap; run it by hand or wait.
    Overflow,
    /// The project's policy is `minor` or `patch`: proposed, not dispatched.
    Proposed,
    /// Not dispatched because maintenance for the project did not succeed.
    Deferred,
}

impl MajorUpgradeStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch",
            Self::Deduped => "deduped",
            Self::Overflow => "overflow",
            Self::Proposed => "proposed",
            Self::Deferred => "deferred",
        }
    }
}

/// One major upgrade and what the lane decided for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MajorUpgrade {
    pub project: String,
    pub ecosystem: Ecosystem,
    pub manifest: String,
    pub package: String,
    pub from: String,
    pub to: String,
    /// The advisory that makes this a security upgrade, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<String>,
    /// The task objective, exactly as dispatched.
    pub objective: String,
    /// The `foundry task` command that runs it by hand.
    pub command: String,
    pub status: MajorUpgradeStatus,
    /// Why it was deduped, overflowed or deferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One project's policy as the majors lane saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectPolicy {
    pub project: String,
    pub policy: UpdatePolicy,
    pub policy_set: bool,
}

/// Payload for `MajorUpgradesPlanned`.
///
/// In the nightly run it carries the `MaintenanceSummaryRequested` fields
/// forward, so `Generate Summary` (which sinks on this event) still has the
/// trace locations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MajorUpgradesPlannedPayload {
    #[serde(default)]
    pub upgrades: Vec<MajorUpgrade>,
    #[serde(default)]
    pub per_project_cap: u32,
    #[serde(default)]
    pub per_night_cap: u32,
    /// `true` when the dispatches will actually run (nightly, full throttle).
    #[serde(default)]
    pub dispatch_enabled: bool,
    /// `true` for an on-demand review of one project; the nightly summary
    /// ignores these.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review: bool,
    /// Set when prior task history could not be read, so dedupe was blind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_warning: Option<String>,
    #[serde(default)]
    pub project_trace_ids: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub skipped_projects: Vec<String>,
    #[serde(default)]
    pub total_duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_event_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_and_ecosystems_are_lowercase_on_the_wire() {
        assert_eq!(serde_json::to_string(&UpdateClass::Major).unwrap(), r#""major""#);
        assert_eq!(serde_json::to_string(&Ecosystem::Swiftpm).unwrap(), r#""swiftpm""#);
        assert_eq!(serde_json::to_string(&ChangeKind::Lockfile).unwrap(), r#""lockfile""#);
        assert_eq!(serde_json::to_string(&MajorUpgradeStatus::Deduped).unwrap(), r#""deduped""#);
    }

    #[test]
    fn classes_order_by_size() {
        assert!(UpdateClass::Patch < UpdateClass::Minor);
        assert!(UpdateClass::Minor < UpdateClass::Major);
    }

    #[test]
    fn an_outdated_dependency_omits_empty_fields() {
        let dep = OutdatedDependency {
            ecosystem: Ecosystem::Cargo,
            manifest: ".".to_string(),
            package: "serde".to_string(),
            current: "1.0.100".to_string(),
            requirement: Some("1".to_string()),
            in_range: Some("1.0.228".to_string()),
            non_major: Some("1.0.228".to_string()),
            major: None,
            constraint_admits_major: false,
            hold: None,
            advisories: vec![],
        };
        let json = serde_json::to_value(&dep).unwrap();
        assert!(json.get("major").is_none());
        assert!(json.get("constraint_admits_major").is_none());
        assert!(json.get("advisories").is_none());
        let back: OutdatedDependency = serde_json::from_value(json).unwrap();
        assert_eq!(back, dep);
    }

    #[test]
    fn the_classified_payload_flattens_chain_context() {
        let payload = DependencyUpdatesClassifiedPayload {
            project: "p".to_string(),
            phase: ClassificationPhase::Before,
            workflow: Some("maintain".to_string()),
            success: None,
            classification: DependencyClassification::default(),
            brief: DependencyBrief {
                policy: UpdatePolicy::Minor,
                policy_set: false,
                apply: vec![],
                held_by_policy: vec![],
                held_by_hold: vec![],
                majors: vec![],
            },
            chain: ChainContext {
                gates: Some(serde_json::json!([{"name": "fmt"}])),
                ..ChainContext::default()
            },
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["workflow"], "maintain");
        assert_eq!(json["phase"], "before");
        assert_eq!(json["gates"][0]["name"], "fmt");
    }
}
