//! Pure rendering for `foundry deps`: one project's dependency classification,
//! its maintain brief, and what the majors lane would do.

use std::fmt::Write as _;

use comfy_table::{ContentArrangement, Table};
use foundry_sdk::payload::{
    DependencyUpdatesClassifiedPayload, HeldUpdate, MajorUpgradeStatus,
    MajorUpgradesPlannedPayload, PlannedUpdate,
};

fn planned_line(u: &PlannedUpdate) -> String {
    let mut line = format!(
        "[{} {}] {} {} -> {} ({}, {})",
        u.ecosystem,
        u.manifest,
        u.package,
        u.from,
        u.to,
        u.class,
        match u.change {
            foundry_sdk::payload::ChangeKind::Lockfile => "lockfile",
            foundry_sdk::payload::ChangeKind::Manifest => "constraint edit",
        }
    );
    if let Some(id) = &u.security {
        let _ = write!(line, " security {id}");
        if u.beyond_policy {
            line.push_str(", beyond policy");
        }
        if u.beyond_hold {
            line.push_str(", overrides hold");
        }
    }
    line
}

fn held_line(h: &HeldUpdate) -> String {
    format!(
        "[{} {}] {} {} -> {} ({}): {}",
        h.ecosystem, h.manifest, h.package, h.from, h.to, h.class, h.reason
    )
}

/// Render a classification: the outdated table, then the brief's decisions.
pub fn classification(p: &DependencyUpdatesClassifiedPayload) -> String {
    let c = &p.classification;
    let b = &p.brief;
    let mut out = String::new();
    let policy_note = if b.policy_set {
        ""
    } else {
        " (not set; the default applies)"
    };
    let _ = writeln!(out, "Dependencies for {}: policy {}{policy_note}", p.project, b.policy);
    if !c.classified.is_empty() {
        let _ = writeln!(out, "Classified: {}", c.classified.join(", "));
    }
    if let Some(source) = &c.advisory_source {
        let _ = writeln!(out, "Advisories: {source}");
    }
    out.push('\n');

    if c.outdated.is_empty() {
        out.push_str("Everything classified is up to date.\n");
    } else {
        let mut table = Table::new();
        table.set_content_arrangement(ContentArrangement::Dynamic);
        table.set_header(vec![
            "Ecosystem",
            "Package",
            "Current",
            "Constraint",
            "In range",
            "Newest non-major",
            "Major",
        ]);
        for d in &c.outdated {
            let mut package = d.package.clone();
            if d.hold.is_some() {
                package.push_str(" (held)");
            }
            if !d.advisories.is_empty() {
                package.push_str(" (advisory)");
            }
            table.add_row(vec![
                format!("{} {}", d.ecosystem, d.manifest),
                package,
                d.current.clone(),
                d.requirement.clone().unwrap_or_default(),
                d.in_range.clone().unwrap_or_default(),
                d.non_major.clone().unwrap_or_default(),
                d.major.clone().unwrap_or_default(),
            ]);
        }
        let _ = writeln!(out, "{table}");
    }

    let section = |out: &mut String, title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            let _ = writeln!(out, "\n{title} ({}):", lines.len());
            for line in lines {
                let _ = writeln!(out, "- {line}");
            }
        }
    };
    section(&mut out, "Maintenance applies", b.apply.iter().map(planned_line).collect());
    section(
        &mut out,
        "Held back by policy",
        b.held_by_policy.iter().map(held_line).collect(),
    );
    section(&mut out, "Held by holds", b.held_by_hold.iter().map(held_line).collect());
    section(
        &mut out,
        "Lapsed holds — re-decide",
        c.lapsed_holds
            .iter()
            .map(|l| {
                format!("{} (max {}, expired {}): {}", l.package, l.max, l.expired_on, l.reason)
            })
            .collect(),
    );
    section(
        &mut out,
        "Not classified",
        c.unclassified.iter().map(|u| format!("{}: {}", u.scope, u.reason)).collect(),
    );
    if let Some(w) = &c.holds_warning {
        let _ = writeln!(out, "\nWarning: {w}");
    }
    out
}

/// Render the majors lane's plan.
pub fn majors_plan(p: &MajorUpgradesPlannedPayload) -> String {
    let mut out = String::new();
    if p.upgrades.is_empty() {
        out.push_str("\nMajor upgrades: none available.\n");
        return out;
    }
    let _ = writeln!(
        out,
        "\nMajor upgrades (caps: {} per project, {} per night){}:",
        p.per_project_cap,
        p.per_night_cap,
        if p.dispatch_enabled {
            ""
        } else {
            ", plan only"
        }
    );
    for m in &p.upgrades {
        let status = match (m.status, p.dispatch_enabled) {
            (MajorUpgradeStatus::Dispatch, false) => "would dispatch",
            (status, _) => status.as_str(),
        };
        let _ = write!(
            out,
            "- [{status}] {} {} -> {} ({} {})",
            m.package, m.from, m.to, m.ecosystem, m.manifest
        );
        if let Some(id) = &m.security {
            let _ = write!(out, " security {id}");
        }
        if let Some(reason) = &m.reason {
            let _ = write!(out, ": {reason}");
        }
        out.push('\n');
        let _ = writeln!(out, "    {}", m.command);
    }
    if let Some(w) = &p.history_warning {
        let _ = writeln!(out, "Warning: {w}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::payload::{
        ChainContext, ChangeKind, ClassificationPhase, DependencyBrief, DependencyClassification,
        Ecosystem, MajorUpgrade, OutdatedDependency, UnclassifiedScope, UpdateClass,
    };
    use foundry_sdk::registry::UpdatePolicy;

    fn payload() -> DependencyUpdatesClassifiedPayload {
        DependencyUpdatesClassifiedPayload {
            project: "app".to_string(),
            phase: ClassificationPhase::Review,
            workflow: None,
            success: None,
            classification: DependencyClassification {
                outdated: vec![OutdatedDependency {
                    ecosystem: Ecosystem::Npm,
                    manifest: ".".to_string(),
                    package: "zod".to_string(),
                    current: "3.22.4".to_string(),
                    requirement: Some("^3.22.0".to_string()),
                    in_range: Some("3.25.1".to_string()),
                    non_major: Some("3.25.1".to_string()),
                    major: Some("4.1.0".to_string()),
                    constraint_admits_major: false,
                    hold: None,
                    advisories: vec![],
                }],
                classified: vec!["npm (.)".to_string()],
                unclassified: vec![UnclassifiedScope {
                    scope: "pypi (.)".to_string(),
                    reason: "no uv.lock".to_string(),
                }],
                ..DependencyClassification::default()
            },
            brief: DependencyBrief {
                policy: UpdatePolicy::Minor,
                policy_set: false,
                apply: vec![PlannedUpdate {
                    ecosystem: Ecosystem::Npm,
                    manifest: ".".to_string(),
                    package: "zod".to_string(),
                    from: "3.22.4".to_string(),
                    to: "3.25.1".to_string(),
                    class: UpdateClass::Minor,
                    change: ChangeKind::Lockfile,
                    security: None,
                    beyond_policy: false,
                    beyond_hold: false,
                }],
                held_by_policy: vec![],
                held_by_hold: vec![],
                majors: vec![],
            },
            chain: ChainContext::default(),
        }
    }

    #[test]
    fn classification_shows_the_table_the_brief_and_unclassified_scopes() {
        let text = classification(&payload());
        assert!(text.contains("Dependencies for app: policy minor (not set; the default applies)"));
        assert!(text.contains("zod"));
        assert!(text.contains("4.1.0"));
        assert!(text.contains("Maintenance applies (1):"));
        assert!(text.contains("- [npm .] zod 3.22.4 -> 3.25.1 (minor, lockfile)"));
        assert!(text.contains("Not classified (1):\n- pypi (.): no uv.lock"));
    }

    #[test]
    fn a_plan_only_majors_list_says_would_dispatch_and_prints_commands() {
        let plan = MajorUpgradesPlannedPayload {
            upgrades: vec![MajorUpgrade {
                project: "app".to_string(),
                ecosystem: Ecosystem::Npm,
                manifest: ".".to_string(),
                package: "zod".to_string(),
                from: "3.22.4".to_string(),
                to: "4.1.0".to_string(),
                security: None,
                objective: "Upgrade zod ...".to_string(),
                command: "foundry task app 'Upgrade zod ...'".to_string(),
                status: MajorUpgradeStatus::Dispatch,
                reason: None,
            }],
            per_project_cap: 2,
            per_night_cap: 6,
            dispatch_enabled: false,
            review: true,
            ..MajorUpgradesPlannedPayload::default()
        };
        let text = majors_plan(&plan);
        assert!(text.contains("plan only"));
        assert!(text.contains("- [would dispatch] zod 3.22.4 -> 4.1.0 (npm .)"));
        assert!(text.contains("    foundry task app 'Upgrade zod ...'"));
    }
}
