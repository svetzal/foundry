//! Where a unit of work came from: the typed `source` a work item records at
//! submission.
//!
//! An item's free-text `origin` says how the work arrived in the submitter's
//! own words, and nothing parses it. [`WorkSource`] is the typed counterpart:
//! a closed set of kinds, each with the one reference that identifies the
//! dispatcher, so a reader can answer "everything this campaign dispatched" or
//! "what the nightly started" from the ledger alone, without reading the event
//! log to recover a campaign from a trace.
//!
//! The source rides the event envelope ([`crate::event::Event::source`]) from
//! the root event a submitter emits down every hop of its chain, the way
//! `trace_id` and `gather_id` do, so the ledger block that records an item
//! reads it off the event that opens the item's chain. An event or item that
//! carries no source was recorded before the field existed, or was dispatched
//! by a path that names no source; every reader treats that as "not recorded".

use std::fmt;

use serde::{Deserialize, Serialize};

/// The closed set of things that dispatch work.
///
/// Adding a kind is a code change. A source is never a free string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSourceKind {
    /// A campaign advance dispatched a cycle. The reference is the campaign
    /// name and [`WorkSource::cycle`] is the cycle number.
    Campaign,
    /// A sentinel-fired chain dispatched the item: the nightly maintenance
    /// per-project runs, the majors lane, supply-chain remediation. The
    /// reference is the sentinel name.
    Sentinel,
    /// A person dispatched it from a client. The reference is that client's
    /// hostname.
    Operator,
    /// Another work item gave rise to it: a resume child, or a release that
    /// follows a remediation. The reference is the parent item id.
    WorkItem,
}

impl WorkSourceKind {
    /// Every kind, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Campaign,
        Self::Sentinel,
        Self::Operator,
        Self::WorkItem,
    ];

    /// The serialized tag this kind is written to disk and to the wire as.
    ///
    /// Kept in lockstep with the `snake_case` serde renaming above so a caller
    /// filtering on a kind never has to round-trip through `serde_json`.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Campaign => "campaign",
            Self::Sentinel => "sentinel",
            Self::Operator => "operator",
            Self::WorkItem => "work_item",
        }
    }

    /// Parse a serialized kind tag, returning `None` for anything unknown.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.tag() == tag)
    }
}

/// What dispatched a unit of work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSource {
    /// Which sort of dispatcher.
    pub kind: WorkSourceKind,
    /// The one reference that identifies the dispatcher: the campaign name,
    /// the sentinel name, the client hostname, or the parent item id.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The campaign cycle number. Present only for [`WorkSourceKind::Campaign`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle: Option<u64>,
}

impl WorkSource {
    /// Cycle `cycle` of the campaign `name`.
    #[must_use]
    pub fn campaign(name: impl Into<String>, cycle: u64) -> Self {
        Self {
            kind: WorkSourceKind::Campaign,
            reference: name.into(),
            cycle: Some(cycle),
        }
    }

    /// A chain the sentinel `name` fired.
    #[must_use]
    pub fn sentinel(name: impl Into<String>) -> Self {
        Self {
            kind: WorkSourceKind::Sentinel,
            reference: name.into(),
            cycle: None,
        }
    }

    /// A person at the client whose hostname is `host`.
    #[must_use]
    pub fn operator(host: impl Into<String>) -> Self {
        Self {
            kind: WorkSourceKind::Operator,
            reference: host.into(),
            cycle: None,
        }
    }

    /// An item that arose from the work item `parent_id`.
    #[must_use]
    pub fn work_item(parent_id: impl Into<String>) -> Self {
        Self {
            kind: WorkSourceKind::WorkItem,
            reference: parent_id.into(),
            cycle: None,
        }
    }

    /// Whether this source is `kind` with exactly `reference`.
    ///
    /// The cycle is deliberately not part of the match: a filter on a
    /// campaign selects every cycle of it, in ledger order.
    #[must_use]
    pub fn matches(&self, kind: WorkSourceKind, reference: &str) -> bool {
        self.kind == kind && self.reference == reference
    }
}

impl fmt::Display for WorkSource {
    /// `kind:ref`, with ` cycle <n>` appended when a cycle is recorded.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind.tag(), self.reference)?;
        if let Some(cycle) = self.cycle {
            write!(f, " cycle {cycle}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_tag_matches_its_serde_representation_and_parses_back() {
        for kind in WorkSourceKind::ALL {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.tag());
            assert_eq!(WorkSourceKind::from_tag(kind.tag()), Some(kind));
        }
        assert_eq!(WorkSourceKind::from_tag("nightly"), None);
        assert_eq!(WorkSourceKind::from_tag(""), None);
        assert_eq!(WorkSourceKind::from_tag("Campaign"), None);
    }

    #[test]
    fn a_campaign_source_serializes_its_name_as_ref_and_carries_the_cycle() {
        let json = serde_json::to_value(WorkSource::campaign("tidy-cli", 3)).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "campaign", "ref": "tidy-cli", "cycle": 3}));
    }

    #[test]
    fn the_other_kinds_omit_the_cycle_key_entirely() {
        for source in [
            WorkSource::sentinel("nightly-maintenance"),
            WorkSource::operator("workbench"),
            WorkSource::work_item("wi_parent"),
        ] {
            let json = serde_json::to_value(&source).unwrap();
            assert!(json.get("cycle").is_none(), "no cycle key in {json}");
            assert_eq!(json["kind"], source.kind.tag());
            assert_eq!(json["ref"], source.reference);
        }
    }

    #[test]
    fn a_source_round_trips_through_json() {
        for source in [
            WorkSource::campaign("tidy-cli", 3),
            WorkSource::sentinel("nightly-maintenance"),
            WorkSource::operator("workbench"),
            WorkSource::work_item("wi_parent"),
        ] {
            let back: WorkSource =
                serde_json::from_value(serde_json::to_value(&source).unwrap()).unwrap();
            assert_eq!(back, source);
        }
    }

    #[test]
    fn an_unknown_kind_does_not_deserialize() {
        let result: Result<WorkSource, _> =
            serde_json::from_value(serde_json::json!({"kind": "dashboard", "ref": "x"}));
        assert!(result.is_err(), "a source is a closed enum, never a free string");
    }

    #[test]
    fn matching_is_on_kind_and_reference_but_not_cycle() {
        let source = WorkSource::campaign("tidy-cli", 3);
        assert!(source.matches(WorkSourceKind::Campaign, "tidy-cli"));
        assert!(!source.matches(WorkSourceKind::Campaign, "tidy"));
        assert!(!source.matches(WorkSourceKind::Sentinel, "tidy-cli"));
    }

    #[test]
    fn display_is_kind_colon_ref_with_the_cycle_when_recorded() {
        assert_eq!(WorkSource::campaign("tidy-cli", 3).to_string(), "campaign:tidy-cli cycle 3");
        assert_eq!(
            WorkSource::sentinel("nightly-maintenance").to_string(),
            "sentinel:nightly-maintenance"
        );
        assert_eq!(WorkSource::operator("workbench").to_string(), "operator:workbench");
        assert_eq!(WorkSource::work_item("wi_parent").to_string(), "work_item:wi_parent");
    }
}
