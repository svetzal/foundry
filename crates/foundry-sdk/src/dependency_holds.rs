//! Dependency holds — the committed, per-repo record of which packages must
//! stay at or below a version, why, and until when.
//!
//! A hold exists because something outside the manifest pins a package: a
//! vendored library that requires `~> 1.1`, an upstream bug in the newest
//! release, a migration that has not been scheduled yet. The update policy
//! cannot know that, so without a hold maintenance would keep proposing the
//! same upgrade every night.
//!
//! The file is a neutral artifact named [`DEPENDENCY_HOLDS_FILE`] in the
//! repository root. Foundry *reads* it; a human writes it and commits it, so
//! every hold lives in git history. It mirrors `.supply-chain-allow.json`:
//!
//! - An active hold is respected and listed in the maintenance summary.
//! - A hold past its `expires` date lapses. The held update appears again and
//!   the summary lists the hold under "Lapsed holds — re-decide".
//! - A malformed file fails safe: no holds apply, and the classification
//!   carries a warning naming the file.
//!
//! ```json
//! {
//!   "version": 1,
//!   "holds": [
//!     {
//!       "package": "phoenix_live_view",
//!       "max": "1.1",
//!       "reason": "vendored Roost requires ~> 1.1",
//!       "expires": "2026-12-24"
//!     }
//!   ]
//! }
//! ```
//!
//! `max` is a version prefix: `"1.1"` admits every `1.1.x` and below, `"1"`
//! admits every `1.x`, and `"1.1.3"` admits up to exactly `1.1.3`. An optional
//! `ecosystem` (`cargo`, `hex`, `npm`, `pypi`, `maven`, `swiftpm`) limits the
//! hold to one ecosystem when a repository has packages of the same name in
//! two of them.

use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// The committed per-repo holds filename, read from a project's root.
pub const DEPENDENCY_HOLDS_FILE: &str = ".dependency-holds.json";

/// On-disk representation of `.dependency-holds.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependencyHolds {
    /// Format version. Bumped on schema-breaking changes.
    #[serde(default = "default_version")]
    pub version: u32,
    /// The holds.
    #[serde(default)]
    pub holds: Vec<HoldEntry>,
}

impl Default for DependencyHolds {
    fn default() -> Self {
        Self {
            version: default_version(),
            holds: Vec::new(),
        }
    }
}

const fn default_version() -> u32 {
    1
}

/// One hold: keep `package` at or below `max`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HoldEntry {
    /// The package name as its ecosystem spells it. Matched case-insensitively.
    pub package: String,
    /// The highest version prefix the package may move to (see module docs).
    pub max: String,
    /// Why the package is held. Shown in the maintenance summary and brief.
    pub reason: String,
    /// Date (`YYYY-MM-DD`) after which the hold lapses. Absent means it never
    /// lapses on its own (supported, but a date forces a re-decision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Restrict the hold to one ecosystem. Absent applies it in every one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecosystem: Option<String>,
}

/// The outcome of checking one package against the holds on a given day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldDecision {
    /// No hold names this package.
    NotHeld,
    /// An active hold: the package may move up to `max` only.
    Active {
        max: String,
        reason: String,
        expires: Option<String>,
    },
    /// The hold has lapsed: the package is free again, and the lapse must be
    /// reported so someone re-decides.
    Lapsed {
        max: String,
        reason: String,
        /// The expiry that has passed (or an unparseable date string).
        expired_on: String,
    },
}

impl DependencyHolds {
    /// Classify `package` of `ecosystem` against these holds as of `today`.
    ///
    /// An expiry today-or-later is active. An expiry in the past — or one that
    /// does not parse as `YYYY-MM-DD` — has lapsed (fail-safe: a typo frees the
    /// package and reports it rather than holding it forever). When several
    /// holds match, an active one wins over a lapsed one.
    #[must_use]
    pub fn decide(&self, ecosystem: &str, package: &str, today: NaiveDate) -> HoldDecision {
        let mut lapsed = None;
        for hold in self.holds.iter().filter(|h| h.applies_to(ecosystem, package)) {
            match hold.decide(today) {
                active @ HoldDecision::Active { .. } => return active,
                expired @ HoldDecision::Lapsed { .. } => {
                    lapsed.get_or_insert(expired);
                }
                HoldDecision::NotHeld => {}
            }
        }
        lapsed.unwrap_or(HoldDecision::NotHeld)
    }
}

impl HoldEntry {
    fn applies_to(&self, ecosystem: &str, package: &str) -> bool {
        self.package.eq_ignore_ascii_case(package)
            && self.ecosystem.as_deref().is_none_or(|e| e.eq_ignore_ascii_case(ecosystem))
    }

    fn decide(&self, today: NaiveDate) -> HoldDecision {
        match self.expires.as_deref() {
            None => HoldDecision::Active {
                max: self.max.clone(),
                reason: self.reason.clone(),
                expires: None,
            },
            Some(date) => match NaiveDate::parse_from_str(date, "%Y-%m-%d") {
                Ok(expiry) if today <= expiry => HoldDecision::Active {
                    max: self.max.clone(),
                    reason: self.reason.clone(),
                    expires: Some(date.to_string()),
                },
                Ok(_) | Err(_) => HoldDecision::Lapsed {
                    max: self.max.clone(),
                    reason: self.reason.clone(),
                    expired_on: date.to_string(),
                },
            },
        }
    }
}

/// Read `.dependency-holds.json` from `project_dir`.
///
/// A missing file is not an error: it returns no holds.
///
/// # Errors
///
/// Returns [`StoreError::Io`] when the file exists but cannot be read, and
/// [`StoreError::Parse`] when its JSON is malformed. Callers fail safe on
/// either: apply no holds and report the warning.
pub fn read_holds(project_dir: &Path) -> Result<DependencyHolds, StoreError> {
    let path = project_dir.join(DEPENDENCY_HOLDS_FILE);
    if !path.exists() {
        return Ok(DependencyHolds::default());
    }
    let contents = std::fs::read_to_string(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    serde_json::from_str(&contents).map_err(|source| StoreError::Parse { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn hold(package: &str, expires: Option<&str>, ecosystem: Option<&str>) -> HoldEntry {
        HoldEntry {
            package: package.to_string(),
            max: "1.1".to_string(),
            reason: "vendored Roost requires ~> 1.1".to_string(),
            expires: expires.map(str::to_string),
            ecosystem: ecosystem.map(str::to_string),
        }
    }

    fn holds(entries: Vec<HoldEntry>) -> DependencyHolds {
        DependencyHolds {
            version: 1,
            holds: entries,
        }
    }

    #[test]
    fn a_package_without_a_hold_is_not_held() {
        let h = holds(vec![hold("phoenix_live_view", None, None)]);
        assert_eq!(h.decide("hex", "phoenix", day("2026-09-25")), HoldDecision::NotHeld);
    }

    #[test]
    fn a_hold_is_active_through_its_expiry_day() {
        let h = holds(vec![hold("phoenix_live_view", Some("2026-12-24"), None)]);
        assert_eq!(
            h.decide("hex", "phoenix_live_view", day("2026-12-24")),
            HoldDecision::Active {
                max: "1.1".to_string(),
                reason: "vendored Roost requires ~> 1.1".to_string(),
                expires: Some("2026-12-24".to_string()),
            }
        );
    }

    #[test]
    fn a_hold_lapses_the_day_after_its_expiry() {
        let h = holds(vec![hold("phoenix_live_view", Some("2026-12-24"), None)]);
        assert!(matches!(
            h.decide("hex", "phoenix_live_view", day("2026-12-25")),
            HoldDecision::Lapsed { expired_on, .. } if expired_on == "2026-12-24"
        ));
    }

    #[test]
    fn an_unparseable_expiry_lapses_the_hold() {
        let h = holds(vec![hold("serde", Some("next spring"), None)]);
        assert!(matches!(
            h.decide("cargo", "serde", day("2026-09-25")),
            HoldDecision::Lapsed { .. }
        ));
    }

    #[test]
    fn a_hold_without_expiry_never_lapses() {
        let h = holds(vec![hold("serde", None, None)]);
        assert!(matches!(
            h.decide("cargo", "serde", day("2099-01-01")),
            HoldDecision::Active { expires: None, .. }
        ));
    }

    #[test]
    fn package_names_match_case_insensitively() {
        let h = holds(vec![hold("Django", None, None)]);
        assert!(matches!(
            h.decide("pypi", "django", day("2026-09-25")),
            HoldDecision::Active { .. }
        ));
    }

    #[test]
    fn an_ecosystem_limits_where_the_hold_applies() {
        let h = holds(vec![hold("phoenix", None, Some("hex"))]);
        assert!(matches!(
            h.decide("hex", "phoenix", day("2026-09-25")),
            HoldDecision::Active { .. }
        ));
        assert_eq!(h.decide("npm", "phoenix", day("2026-09-25")), HoldDecision::NotHeld);
    }

    #[test]
    fn an_active_hold_wins_over_a_lapsed_one() {
        let h = holds(vec![
            hold("serde", Some("2020-01-01"), None),
            hold("serde", Some("2099-01-01"), None),
        ]);
        assert!(matches!(
            h.decide("cargo", "serde", day("2026-09-25")),
            HoldDecision::Active { .. }
        ));
    }

    #[test]
    fn a_missing_file_means_no_holds() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_holds(dir.path()).unwrap(), DependencyHolds::default());
    }

    #[test]
    fn the_documented_example_parses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(DEPENDENCY_HOLDS_FILE),
            r#"{"version":1,"holds":[{"package":"phoenix_live_view","max":"1.1","reason":"vendored Roost requires ~> 1.1","expires":"2026-12-24"}]}"#,
        )
        .unwrap();
        let h = read_holds(dir.path()).unwrap();
        assert_eq!(h.holds, vec![hold("phoenix_live_view", Some("2026-12-24"), None)]);
    }

    #[test]
    fn a_malformed_file_is_an_error_for_the_caller_to_fail_safe_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEPENDENCY_HOLDS_FILE), "{ not json").unwrap();
        assert!(read_holds(dir.path()).is_err());
    }
}
