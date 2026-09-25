//! The maintain brief: what maintenance may do to a project's dependencies.
//!
//! [`build`] is pure. It takes the classification and the project's policy
//! and decides, for every outdated dependency, whether the move is applied,
//! held by the policy, held by a hold, or left to the majors lane. Security
//! fixes override the ceiling:
//!
//! - A fix that is a patch or minor move is applied even when the policy
//!   would not allow it (`beyond_policy`).
//! - A fix that is a major move goes to the majors lane, whatever the policy.
//! - A hold is respected when a fixed release exists inside it; otherwise the
//!   fix overrides the hold (`beyond_hold`), and the brief says so.
//!
//! [`render`] turns the brief into the instructions the maintain agent gets.

use std::fmt::Write as _;

use foundry_sdk::payload::{
    Advisory, ChangeKind, DependencyBrief, DependencyClassification, Ecosystem, HeldUpdate,
    OutdatedDependency, PlannedUpdate, TransitiveAdvisory, UpdateClass,
};
use foundry_sdk::registry::UpdatePolicy;

use super::version::Version;

fn parse(ecosystem: Ecosystem, raw: &str) -> Option<Version> {
    Version::parse(ecosystem, raw)
}

/// `a > b`, reading both as versions of `ecosystem`.
fn newer(ecosystem: Ecosystem, a: &str, b: &str) -> bool {
    matches!((parse(ecosystem, a), parse(ecosystem, b)), (Some(x), Some(y)) if x > y)
}

fn class(ecosystem: Ecosystem, from: &str, to: &str) -> UpdateClass {
    match (parse(ecosystem, from), parse(ecosystem, to)) {
        (Some(f), Some(t)) => f.class_to(&t).unwrap_or(UpdateClass::Patch),
        // An unknown installed version (a transitive finding without one):
        // treat the move as the smallest it can be; the fix version is the
        // floor the agent must reach.
        _ => UpdateClass::Patch,
    }
}

fn within_hold(ecosystem: Ecosystem, version: &str, max: &str) -> bool {
    parse(ecosystem, version).and_then(|v| v.within_prefix(max)).unwrap_or(true)
}

/// The version the policy alone would move `dep` to.
fn policy_target(policy: UpdatePolicy, dep: &OutdatedDependency) -> Option<String> {
    match policy {
        UpdatePolicy::Patch => dep.in_range.clone(),
        UpdatePolicy::Minor | UpdatePolicy::Major => {
            dep.non_major.clone().or_else(|| dep.in_range.clone())
        }
    }
}

/// The advisory with the highest fix version (the one that decides the move).
fn deciding_advisory(dep: &OutdatedDependency) -> Option<(&Advisory, String)> {
    dep.advisories
        .iter()
        .filter_map(|a| a.fix_version.clone().map(|f| (a, f)))
        .max_by(|(_, x), (_, y)| parse(dep.ecosystem, x).cmp(&parse(dep.ecosystem, y)))
}

fn planned(dep: &OutdatedDependency, to: String) -> PlannedUpdate {
    let change = match &dep.in_range {
        Some(in_range) if !newer(dep.ecosystem, &to, in_range) => ChangeKind::Lockfile,
        _ => ChangeKind::Manifest,
    };
    PlannedUpdate {
        ecosystem: dep.ecosystem,
        manifest: dep.manifest.clone(),
        package: dep.package.clone(),
        class: class(dep.ecosystem, &dep.current, &to),
        from: dep.current.clone(),
        to,
        change,
        security: None,
        beyond_policy: false,
        beyond_hold: false,
    }
}

fn held(dep: &OutdatedDependency, to: &str, reason: String) -> HeldUpdate {
    HeldUpdate {
        ecosystem: dep.ecosystem,
        manifest: dep.manifest.clone(),
        package: dep.package.clone(),
        from: dep.current.clone(),
        to: to.to_string(),
        class: class(dep.ecosystem, &dep.current, to),
        reason,
    }
}

/// Decide the brief for one project.
pub fn build(
    policy: Option<UpdatePolicy>,
    classification: &DependencyClassification,
) -> DependencyBrief {
    let effective = policy.unwrap_or(UpdatePolicy::DEFAULT);
    let mut brief = DependencyBrief {
        policy: effective,
        policy_set: policy.is_some(),
        apply: Vec::new(),
        held_by_policy: Vec::new(),
        held_by_hold: Vec::new(),
        majors: Vec::new(),
    };
    for dep in &classification.outdated {
        decide(effective, dep, &mut brief);
    }
    for transitive in &classification.transitive_advisories {
        decide_transitive(transitive, &mut brief);
    }
    brief
}

fn decide(policy: UpdatePolicy, dep: &OutdatedDependency, brief: &mut DependencyBrief) {
    let eco = dep.ecosystem;
    let by_policy = policy_target(policy, dep);
    let mut target = by_policy.clone();

    if policy == UpdatePolicy::Patch
        && let Some(non_major) = &dep.non_major
        && target.as_ref().is_none_or(|t| newer(eco, non_major, t))
    {
        brief.held_by_policy.push(held(
            dep,
            non_major,
            "policy is patch: this needs a constraint change".to_string(),
        ));
    }

    // Holds cap the policy's target and the majors lane.
    let mut hold_blocked_major = false;
    if let Some(hold) = &dep.hold {
        let blocked_major = dep.major.as_ref().filter(|m| !within_hold(eco, m, &hold.max));
        let blocked_target = target.as_ref().filter(|t| !within_hold(eco, t, &hold.max));
        if let Some(blocked) = blocked_major.or(blocked_target) {
            let stale = if within_hold(eco, &dep.current, &hold.max) {
                String::new()
            } else {
                format!(
                    " (stale hold: locked {} is above cap {}, re-decide)",
                    dep.current, hold.max
                )
            };
            brief.held_by_hold.push(held(
                dep,
                blocked,
                format!("held at {}: {}{stale}", hold.max, hold.reason),
            ));
        }
        hold_blocked_major = blocked_major.is_some();
        if blocked_target.is_some() {
            target = hold
                .newest_within
                .clone()
                .filter(|w| target.as_ref().is_some_and(|t| !newer(eco, w, t)));
        }
    }

    // Security overrides the ceiling.
    let mut security_major = false;
    let mut update = None;
    if let Some((advisory, fix)) = deciding_advisory(dep) {
        let covered = target.as_ref().is_some_and(|t| !newer(eco, &fix, t));
        if covered {
            let mut u = planned(dep, target.clone().unwrap_or_default());
            u.security = Some(advisory.id.clone());
            update = Some(u);
        } else if class(eco, &dep.current, &fix) == UpdateClass::Major {
            security_major = true;
            let to = match (&dep.major, policy) {
                (Some(major), UpdatePolicy::Major) if !newer(eco, &fix, major) => major.clone(),
                _ => fix.clone(),
            };
            let mut u = planned(dep, to);
            u.security = Some(advisory.id.clone());
            u.beyond_hold = dep.hold.as_ref().is_some_and(|h| !within_hold(eco, &u.to, &h.max));
            brief.majors.push(u);
        } else {
            let inside_hold = dep
                .hold
                .as_ref()
                .and_then(|h| h.newest_within.clone())
                .filter(|w| !newer(eco, &fix, w));
            let to = inside_hold.unwrap_or_else(|| fix.clone());
            let mut u = planned(dep, to);
            u.security = Some(advisory.id.clone());
            u.beyond_policy = by_policy.as_ref().is_none_or(|p| newer(eco, &u.to, p));
            u.beyond_hold = dep.hold.as_ref().is_some_and(|h| !within_hold(eco, &u.to, &h.max));
            update = Some(u);
        }
    }

    let update =
        update.or_else(|| target.filter(|t| newer(eco, t, &dep.current)).map(|t| planned(dep, t)));
    // Never a downgrade, whatever a hold or an advisory says.
    if let Some(u) = update.filter(|u| newer(eco, &u.to, &dep.current)) {
        brief.apply.push(u);
    }

    if let Some(major) = &dep.major
        && !security_major
        && !hold_blocked_major
    {
        let mut u = planned(dep, major.clone());
        u.class = UpdateClass::Major;
        brief.majors.push(u);
    }
}

fn decide_transitive(t: &TransitiveAdvisory, brief: &mut DependencyBrief) {
    let Some(fix) = t.advisory.fix_version.clone() else {
        return; // No fixed release: a policy call, not a move.
    };
    let from = t.version.clone().unwrap_or_else(|| "?".to_string());
    if t.version.as_ref().is_some_and(|v| !newer(t.ecosystem, &fix, v)) {
        return;
    }
    let move_class = class(t.ecosystem, &from, &fix);
    let update = PlannedUpdate {
        ecosystem: t.ecosystem,
        manifest: ".".to_string(),
        package: t.package.clone(),
        from,
        to: fix,
        class: move_class,
        change: ChangeKind::Lockfile,
        security: Some(t.advisory.id.clone()),
        beyond_policy: false,
        beyond_hold: false,
    };
    if move_class == UpdateClass::Major {
        brief.majors.push(update);
    } else {
        brief.apply.push(update);
    }
}

fn describe(u: &PlannedUpdate) -> String {
    let mut line = format!(
        "[{} {}] {} {} -> {} ({}",
        u.ecosystem, u.manifest, u.package, u.from, u.to, u.class
    );
    match u.change {
        ChangeKind::Lockfile => line.push_str(", lockfile only"),
        ChangeKind::Manifest => line.push_str(", edit the constraint to admit it"),
    }
    line.push(')');
    if let Some(id) = &u.security {
        let _ = write!(line, " — security fix for {id}");
        if u.beyond_policy {
            line.push_str(", beyond the policy ceiling");
        }
        if u.beyond_hold {
            line.push_str(", overrides a hold (no fixed release inside it)");
        }
    }
    line
}

fn describe_held(h: &HeldUpdate) -> String {
    format!(
        "[{} {}] {} {} -> {} ({}): {}",
        h.ecosystem, h.manifest, h.package, h.from, h.to, h.class, h.reason
    )
}

/// The maintain agent's instructions for the dependency part of its work.
pub fn render(brief: &DependencyBrief, classification: &DependencyClassification) -> String {
    let mut out = String::new();
    let policy_note = if brief.policy_set {
        "set for this project"
    } else {
        "not set; the default applies"
    };
    let _ = writeln!(out, "Dependency update policy: {} ({policy_note}).", brief.policy);
    out.push('\n');

    if brief.apply.is_empty() {
        out.push_str(
            "No dependency updates are planned for this project tonight. Do not change any \
             dependency version, constraint or lockfile entry.\n",
        );
    } else {
        out.push_str("Apply exactly these dependency updates, and no others:\n");
        for u in &brief.apply {
            let _ = writeln!(out, "- {}", describe(u));
        }
        out.push_str(
            "\nRules for these updates:\n\
             - Move each listed package to the listed version. Use targeted commands (for \
             example `cargo update -p <pkg> --precise <version>`, `mix deps.update <pkg>`, \
             `npm install <pkg>@<version>`, `uv lock --upgrade-package <pkg>==<version>`), \
             not blanket upgrades that could move other packages.\n\
             - Where an entry says to edit the constraint, change it only as far as needed to \
             admit the listed version, in the constraint's existing style.\n\
             - Do not change any other dependency version or constraint. Transitive \
             dependencies may move only as a side effect of the listed changes.\n\
             - Never take a major upgrade in this session. Major upgrades run separately.\n\
             - If a listed update breaks a quality gate and you cannot fix the breakage, \
             revert that one update and say which, and why, in your final report.\n",
        );
    }

    if !brief.held_by_policy.is_empty() || !brief.held_by_hold.is_empty() {
        out.push_str("\nHeld back (do not apply):\n");
        for h in brief.held_by_policy.iter().chain(&brief.held_by_hold) {
            let _ = writeln!(out, "- {}", describe_held(h));
        }
    }
    if !brief.majors.is_empty() {
        out.push_str("\nMajor upgrades available (handled as separate tasks; do not apply):\n");
        for u in &brief.majors {
            let _ = writeln!(out, "- {}", describe(u));
        }
    }
    if !classification.stale_holds.is_empty() {
        out.push_str(
            "\nStale holds (the locked version is already above the cap; do not downgrade):\n",
        );
        for h in &classification.stale_holds {
            let _ = writeln!(
                out,
                "- [{} {}] {}: stale hold: locked {} is above cap {}, re-decide ({})",
                h.ecosystem, h.manifest, h.package, h.locked, h.max, h.reason
            );
        }
    }
    if !classification.vendored.is_empty() {
        let _ = writeln!(
            out,
            "\nVendored, updated upstream (do not change their dependencies): {}",
            classification.vendored.join(", ")
        );
    }
    if !classification.unclassified.is_empty() {
        out.push_str(
            "\nNot classified (Foundry could not check these; do not update them, and report \
             anything you notice):\n",
        );
        for u in &classification.unclassified {
            let _ = writeln!(out, "- {}: {}", u.scope, u.reason);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::payload::AppliedHold;

    fn dep(package: &str, current: &str) -> OutdatedDependency {
        OutdatedDependency {
            ecosystem: Ecosystem::Hex,
            manifest: ".".to_string(),
            package: package.to_string(),
            current: current.to_string(),
            requirement: None,
            in_range: None,
            non_major: None,
            major: None,
            constraint_admits_major: false,
            hold: None,
            advisories: Vec::new(),
        }
    }

    fn with(
        mut d: OutdatedDependency,
        in_range: Option<&str>,
        non_major: Option<&str>,
        major: Option<&str>,
    ) -> OutdatedDependency {
        d.in_range = in_range.map(str::to_string);
        d.non_major = non_major.map(str::to_string);
        d.major = major.map(str::to_string);
        d
    }

    fn classification(outdated: Vec<OutdatedDependency>) -> DependencyClassification {
        DependencyClassification {
            outdated,
            ..DependencyClassification::default()
        }
    }

    fn advisory(id: &str, fix: &str) -> Advisory {
        Advisory {
            id: id.to_string(),
            fix_version: Some(fix.to_string()),
            severity: None,
        }
    }

    fn targets(updates: &[PlannedUpdate]) -> Vec<(&str, &str, ChangeKind)> {
        updates.iter().map(|u| (u.package.as_str(), u.to.as_str(), u.change)).collect()
    }

    #[test]
    fn patch_policy_applies_lockfile_moves_only_and_holds_the_rest() {
        let c = classification(vec![with(
            dep("phoenix", "1.8.1"),
            Some("1.8.3"),
            Some("1.9.0"),
            Some("2.0.0"),
        )]);
        let b = build(Some(UpdatePolicy::Patch), &c);
        assert_eq!(targets(&b.apply), [("phoenix", "1.8.3", ChangeKind::Lockfile)]);
        assert_eq!(b.held_by_policy.len(), 1);
        assert_eq!(b.held_by_policy[0].to, "1.9.0");
        assert_eq!(b.held_by_policy[0].class, UpdateClass::Minor);
        assert_eq!(targets(&b.majors), [("phoenix", "2.0.0", ChangeKind::Manifest)]);
    }

    #[test]
    fn minor_policy_widens_to_the_newest_non_major() {
        let c = classification(vec![with(
            dep("phoenix", "1.8.1"),
            Some("1.8.3"),
            Some("1.9.0"),
            None,
        )]);
        let b = build(Some(UpdatePolicy::Minor), &c);
        assert_eq!(targets(&b.apply), [("phoenix", "1.9.0", ChangeKind::Manifest)]);
        assert!(b.held_by_policy.is_empty());
        assert_eq!(b.apply[0].class, UpdateClass::Minor);
    }

    #[test]
    fn an_unset_policy_behaves_as_minor_and_says_so() {
        let c = classification(vec![with(
            dep("jason", "1.4.4"),
            Some("1.4.5"),
            Some("1.4.5"),
            None,
        )]);
        let b = build(None, &c);
        assert_eq!(b.policy, UpdatePolicy::Minor);
        assert!(!b.policy_set);
        assert_eq!(targets(&b.apply), [("jason", "1.4.5", ChangeKind::Lockfile)]);
    }

    #[test]
    fn majors_never_enter_the_apply_list() {
        let c = classification(vec![with(dep("req", "0.7.4"), None, None, Some("0.8.0"))]);
        for policy in UpdatePolicy::ALL {
            let b = build(Some(policy), &c);
            assert!(b.apply.is_empty(), "{policy}");
            assert_eq!(targets(&b.majors), [("req", "0.8.0", ChangeKind::Manifest)]);
        }
    }

    #[test]
    fn an_active_hold_caps_the_target_and_blocks_the_major() {
        let mut d =
            with(dep("phoenix_live_view", "1.0.9"), Some("1.0.12"), Some("1.2.0"), Some("2.0.0"));
        d.hold = Some(AppliedHold {
            max: "1.1".to_string(),
            reason: "vendored Roost requires ~> 1.1".to_string(),
            expires: Some("2026-12-24".to_string()),
            newest_within: Some("1.1.4".to_string()),
        });
        let b = build(Some(UpdatePolicy::Major), &classification(vec![d]));
        assert_eq!(targets(&b.apply), [("phoenix_live_view", "1.1.4", ChangeKind::Manifest)]);
        assert!(b.majors.is_empty(), "the hold blocks the major");
        assert_eq!(b.held_by_hold.len(), 1);
        assert_eq!(b.held_by_hold[0].to, "2.0.0");
        assert!(b.held_by_hold[0].reason.contains("held at 1.1: vendored Roost"));
    }

    #[test]
    fn a_security_minor_overrides_a_patch_ceiling() {
        let mut d = with(dep("plug", "1.15.0"), Some("1.15.3"), Some("1.16.1"), None);
        d.advisories = vec![advisory("GHSA-plug", "1.16.0")];
        let b = build(Some(UpdatePolicy::Patch), &classification(vec![d]));
        assert_eq!(targets(&b.apply), [("plug", "1.16.0", ChangeKind::Manifest)]);
        assert_eq!(b.apply[0].security.as_deref(), Some("GHSA-plug"));
        assert!(b.apply[0].beyond_policy);
    }

    #[test]
    fn a_security_fix_inside_the_policy_is_marked_not_forced() {
        let mut d = with(dep("plug", "1.15.0"), Some("1.15.3"), Some("1.16.1"), None);
        d.advisories = vec![advisory("GHSA-plug", "1.15.2")];
        let b = build(Some(UpdatePolicy::Minor), &classification(vec![d]));
        assert_eq!(targets(&b.apply), [("plug", "1.16.1", ChangeKind::Manifest)]);
        assert_eq!(b.apply[0].security.as_deref(), Some("GHSA-plug"));
        assert!(!b.apply[0].beyond_policy);
    }

    #[test]
    fn a_security_major_goes_to_the_majors_lane_even_for_patch_projects() {
        let mut d = with(dep("jose", "0.9.0"), None, None, Some("1.2.0"));
        d.advisories = vec![advisory("CVE-jose", "1.0.1")];
        let b = build(Some(UpdatePolicy::Patch), &classification(vec![d]));
        assert!(b.apply.is_empty());
        assert_eq!(targets(&b.majors), [("jose", "1.0.1", ChangeKind::Manifest)]);
        assert_eq!(b.majors[0].security.as_deref(), Some("CVE-jose"));
    }

    #[test]
    fn a_security_major_for_a_major_project_targets_the_newest_major() {
        let mut d = with(dep("jose", "0.9.0"), None, None, Some("1.2.0"));
        d.advisories = vec![advisory("CVE-jose", "1.0.1")];
        let b = build(Some(UpdatePolicy::Major), &classification(vec![d]));
        assert_eq!(targets(&b.majors), [("jose", "1.2.0", ChangeKind::Manifest)]);
        assert_eq!(b.majors.len(), 1, "one major per package");
    }

    #[test]
    fn a_hold_is_respected_when_a_fix_exists_inside_it() {
        let mut d = with(dep("lv", "1.0.9"), Some("1.0.12"), Some("1.2.0"), None);
        d.hold = Some(AppliedHold {
            max: "1.1".to_string(),
            reason: "roost".to_string(),
            expires: None,
            newest_within: Some("1.1.4".to_string()),
        });
        d.advisories = vec![advisory("GHSA-lv", "1.1.2")];
        let b = build(Some(UpdatePolicy::Minor), &classification(vec![d]));
        assert_eq!(targets(&b.apply), [("lv", "1.1.4", ChangeKind::Manifest)]);
        assert!(!b.apply[0].beyond_hold);
    }

    #[test]
    fn a_fix_beyond_the_hold_overrides_it_and_says_so() {
        let mut d = with(dep("lv", "1.0.9"), Some("1.0.12"), Some("1.2.0"), None);
        d.hold = Some(AppliedHold {
            max: "1.1".to_string(),
            reason: "roost".to_string(),
            expires: None,
            newest_within: Some("1.1.4".to_string()),
        });
        d.advisories = vec![advisory("GHSA-lv", "1.2.0")];
        let b = build(Some(UpdatePolicy::Minor), &classification(vec![d]));
        assert_eq!(targets(&b.apply), [("lv", "1.2.0", ChangeKind::Manifest)]);
        assert!(b.apply[0].beyond_hold);
        assert!(render(&b, &DependencyClassification::default()).contains("overrides a hold"));
    }

    #[test]
    fn transitive_advisories_become_lockfile_moves_or_security_majors() {
        let c = DependencyClassification {
            transitive_advisories: vec![
                TransitiveAdvisory {
                    ecosystem: Ecosystem::Npm,
                    package: "braces".to_string(),
                    version: Some("3.0.2".to_string()),
                    advisory: advisory("GHSA-braces", "3.0.3"),
                },
                TransitiveAdvisory {
                    ecosystem: Ecosystem::Npm,
                    package: "semver".to_string(),
                    version: Some("5.7.1".to_string()),
                    advisory: advisory("GHSA-semver", "7.5.2"),
                },
                TransitiveAdvisory {
                    ecosystem: Ecosystem::Npm,
                    package: "nofix".to_string(),
                    version: Some("1.0.0".to_string()),
                    advisory: Advisory {
                        id: "GHSA-none".to_string(),
                        fix_version: None,
                        severity: None,
                    },
                },
            ],
            ..DependencyClassification::default()
        };
        let b = build(Some(UpdatePolicy::Patch), &c);
        assert_eq!(targets(&b.apply), [("braces", "3.0.3", ChangeKind::Lockfile)]);
        assert_eq!(targets(&b.majors), [("semver", "7.5.2", ChangeKind::Lockfile)]);
    }

    #[test]
    fn the_rendered_brief_lists_exactly_what_to_apply() {
        let c = classification(vec![with(
            dep("phoenix", "1.8.1"),
            Some("1.8.3"),
            Some("1.9.0"),
            Some("2.0.0"),
        )]);
        let b = build(Some(UpdatePolicy::Patch), &c);
        let text = render(&b, &c);
        assert!(text.contains("Dependency update policy: patch (set for this project)."));
        assert!(text.contains("Apply exactly these dependency updates, and no others:"));
        assert!(text.contains("- [hex .] phoenix 1.8.1 -> 1.8.3 (patch, lockfile only)"));
        assert!(text.contains("Held back (do not apply):"));
        assert!(text.contains("policy is patch"));
        assert!(
            text.contains("Major upgrades available (handled as separate tasks; do not apply):")
        );
        assert!(text.contains("Never take a major upgrade"));
    }

    #[test]
    fn an_empty_brief_forbids_dependency_changes_and_lists_unclassified_scopes() {
        let c = DependencyClassification {
            unclassified: vec![foundry_sdk::payload::UnclassifiedScope {
                scope: "npm (.)".to_string(),
                reason: "bun.lockb is binary".to_string(),
            }],
            ..DependencyClassification::default()
        };
        let text = render(&build(None, &c), &c);
        assert!(text.contains("(not set; the default applies)"));
        assert!(text.contains("No dependency updates are planned"));
        assert!(text.contains("- npm (.): bun.lockb is binary"));
    }
}
