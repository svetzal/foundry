//! Supply-chain advisory allowlist — the committed, per-repo memory of which
//! published advisories have been reviewed and accepted (with an expiry).
//!
//! A supply-chain advisory is an *external*, time-triggered fact: it appears
//! because the world changed (a CVE was published), not because the project's
//! own code regressed. A blocking quality gate has no way to record "known,
//! accepted, waiting on upstream" and so re-fails the same advisory every
//! night. This allowlist is that missing memory.
//!
//! The file is a neutral artifact named [`SUPPLY_CHAIN_ALLOW_FILE`] checked into
//! each repository's root. Foundry *reads* it during the nightly supply-chain
//! scan; nothing here writes it — acceptance decisions are authored by a human
//! (or a future remediation tool) and land through the repo's normal commit
//! flow, so every acceptance lives in git history.
//!
//! Each entry carries an `expires` date. An acceptance past its expiry lapses:
//! the advisory resurfaces as a live finding so the decision is re-evaluated
//! rather than silently surviving forever.

use std::path::Path;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// The committed per-repo allowlist filename, read from a project's root.
pub const SUPPLY_CHAIN_ALLOW_FILE: &str = ".supply-chain-allow.json";

/// On-disk representation of `.supply-chain-allow.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SupplyChainAllowlist {
    /// Format version. Bumped on schema-breaking changes.
    #[serde(default = "default_version")]
    pub version: u32,
    /// The accepted advisories.
    #[serde(default)]
    pub allowed: Vec<AllowEntry>,
}

impl Default for SupplyChainAllowlist {
    fn default() -> Self {
        Self {
            version: default_version(),
            allowed: Vec::new(),
        }
    }
}

const fn default_version() -> u32 {
    1
}

/// A single accepted advisory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowEntry {
    /// The advisory identifier this entry suppresses (e.g. `"GHSA-gv7w-rqvm-qjhr"`
    /// or `"CVE-2026-1234"`). Matched case-insensitively against scanner output.
    pub cve: String,
    /// Why the advisory is accepted — an exploitability-for-our-usage note, an
    /// upstream-wait, or similar. Surfaced in the digest so the rationale is
    /// visible without opening the file.
    pub reason: String,
    /// Date (`YYYY-MM-DD`) after which this acceptance lapses and the advisory
    /// resurfaces for re-evaluation. Absent means it never auto-expires (an
    /// unconditional acceptance — discouraged, but supported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
}

/// The outcome of checking one advisory against the allowlist on a given day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowDecision {
    /// Not present in the allowlist — a live finding.
    NotListed,
    /// Present and still active (no expiry, or expiry today-or-later).
    Active {
        /// The acceptance rationale.
        reason: String,
    },
    /// Present but the acceptance has lapsed — treat as a live finding again,
    /// while noting the lapse so the digest can flag it for a fresh decision.
    Expired {
        /// The acceptance rationale (now lapsed).
        reason: String,
        /// The expiry date that has passed (or an unparseable date string).
        expired_on: String,
    },
}

impl SupplyChainAllowlist {
    /// Classify `cve` against this allowlist as of `today`.
    ///
    /// An entry with no `expires` is `Active` indefinitely. An entry with an
    /// expiry today-or-later is `Active`; one whose expiry has passed — or whose
    /// expiry string does not parse as `YYYY-MM-DD` — is `Expired` (fail-safe:
    /// an unparseable expiry resurfaces the advisory rather than hiding it).
    #[must_use]
    pub fn decide(&self, cve: &str, today: NaiveDate) -> AllowDecision {
        let Some(entry) = self.allowed.iter().find(|e| e.cve.eq_ignore_ascii_case(cve)) else {
            return AllowDecision::NotListed;
        };
        match entry.expires.as_deref() {
            None => AllowDecision::Active {
                reason: entry.reason.clone(),
            },
            Some(date_str) => match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                Ok(expiry) if today <= expiry => AllowDecision::Active {
                    reason: entry.reason.clone(),
                },
                Ok(_) | Err(_) => AllowDecision::Expired {
                    reason: entry.reason.clone(),
                    expired_on: date_str.to_string(),
                },
            },
        }
    }
}

/// Read `.supply-chain-allow.json` from `project_dir`.
///
/// Returns an empty allowlist when the file is absent (the common case — most
/// repos accept nothing). Returns an error only when the file exists but
/// contains malformed JSON.
///
/// # Errors
///
/// Returns [`StoreError::Io`] when the file exists but cannot be read, and
/// [`StoreError::Parse`] when the JSON is malformed. A missing file is not an
/// error — it returns an empty allowlist.
pub fn read_allowlist(project_dir: &Path) -> Result<SupplyChainAllowlist, StoreError> {
    let path = project_dir.join(SUPPLY_CHAIN_ALLOW_FILE);
    if !path.exists() {
        return Ok(SupplyChainAllowlist::default());
    }
    let contents = std::fs::read_to_string(&path).map_err(|source| StoreError::Io {
        path: path.clone(),
        source,
    })?;
    let allowlist = serde_json::from_str(&contents).map_err(|source| StoreError::Parse {
        path: path.clone(),
        source,
    })?;
    Ok(allowlist)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn allowlist(entries: Vec<AllowEntry>) -> SupplyChainAllowlist {
        SupplyChainAllowlist {
            version: 1,
            allowed: entries,
        }
    }

    fn entry(cve: &str, expires: Option<&str>) -> AllowEntry {
        AllowEntry {
            cve: cve.to_string(),
            reason: "dev-only dependency, not exploitable in our usage".to_string(),
            expires: expires.map(str::to_string),
        }
    }

    #[test]
    fn decide_not_listed_when_absent() {
        let list = allowlist(vec![entry("GHSA-aaaa", Some("2099-01-01"))]);
        assert_eq!(list.decide("CVE-2026-9999", day("2026-06-15")), AllowDecision::NotListed);
    }

    #[test]
    fn decide_active_when_expiry_in_future() {
        let list = allowlist(vec![entry("CVE-2026-1234", Some("2026-09-01"))]);
        assert!(matches!(
            list.decide("CVE-2026-1234", day("2026-06-15")),
            AllowDecision::Active { .. }
        ));
    }

    #[test]
    fn decide_active_on_expiry_day_itself() {
        // Today == expiry is still active; lapse begins the day after.
        let list = allowlist(vec![entry("CVE-2026-1234", Some("2026-06-15"))]);
        assert!(matches!(
            list.decide("CVE-2026-1234", day("2026-06-15")),
            AllowDecision::Active { .. }
        ));
    }

    #[test]
    fn decide_expired_when_expiry_in_past() {
        let list = allowlist(vec![entry("CVE-2026-1234", Some("2026-01-01"))]);
        match list.decide("CVE-2026-1234", day("2026-06-15")) {
            AllowDecision::Expired { expired_on, .. } => assert_eq!(expired_on, "2026-01-01"),
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn decide_active_when_no_expiry() {
        let list = allowlist(vec![entry("CVE-2026-1234", None)]);
        assert!(matches!(
            list.decide("CVE-2026-1234", day("2099-12-31")),
            AllowDecision::Active { .. }
        ));
    }

    #[test]
    fn decide_expired_when_expiry_unparseable() {
        // Fail-safe: a malformed expiry resurfaces the advisory rather than hiding it.
        let list = allowlist(vec![entry("CVE-2026-1234", Some("not-a-date"))]);
        assert!(matches!(
            list.decide("CVE-2026-1234", day("2026-06-15")),
            AllowDecision::Expired { .. }
        ));
    }

    #[test]
    fn decide_matches_cve_case_insensitively() {
        let list = allowlist(vec![entry("ghsa-GV7W-rqvm-qjhr", Some("2099-01-01"))]);
        assert!(matches!(
            list.decide("GHSA-gv7w-RQVM-qjhr", day("2026-06-15")),
            AllowDecision::Active { .. }
        ));
    }

    #[test]
    fn read_allowlist_missing_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let list = read_allowlist(dir.path()).unwrap();
        assert!(list.allowed.is_empty());
        assert_eq!(list.version, 1);
    }

    #[test]
    fn read_allowlist_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(SUPPLY_CHAIN_ALLOW_FILE),
            r#"{"version":1,"allowed":[{"cve":"CVE-2026-1234","reason":"dev only","expires":"2026-09-01"}]}"#,
        )
        .unwrap();
        let list = read_allowlist(dir.path()).unwrap();
        assert_eq!(list.allowed.len(), 1);
        assert_eq!(list.allowed[0].cve, "CVE-2026-1234");
        assert_eq!(list.allowed[0].expires.as_deref(), Some("2026-09-01"));
    }

    #[test]
    fn read_allowlist_malformed_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SUPPLY_CHAIN_ALLOW_FILE), "not json").unwrap();
        let err = read_allowlist(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("malformed JSON"));
    }

    #[test]
    fn allowlist_round_trips_and_omits_absent_expiry() {
        let list = allowlist(vec![entry("CVE-1", None), entry("CVE-2", Some("2027-01-01"))]);
        let json = serde_json::to_string(&list).unwrap();
        // Absent expiry must not appear on the wire for the first entry.
        let restored: SupplyChainAllowlist = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.allowed.len(), 2);
        assert!(restored.allowed[0].expires.is_none());
        assert_eq!(restored.allowed[1].expires.as_deref(), Some("2027-01-01"));
    }
}
