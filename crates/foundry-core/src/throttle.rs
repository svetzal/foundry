use serde::{Deserialize, Serialize};

/// Controls how far an event ripples through task blocks.
///
/// The throttle is set at invocation time and propagated through the event
/// chain. Task blocks check the throttle before emitting downstream events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Throttle {
    /// All task blocks emit downstream events. Default for automated runs.
    #[default]
    Full,
    /// Observation blocks emit normally. Mutation blocks suppress output events.
    AuditOnly,
    /// All blocks run read-only. No side effects, no mutation events emitted.
    DryRun,
}

impl Throttle {
    /// Whether a mutation task block should emit its output events at this throttle level.
    pub fn allows_mutation(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether any task block should execute side effects at this throttle level.
    pub fn allows_side_effects(self) -> bool {
        !matches!(self, Self::DryRun)
    }
}

impl std::fmt::Display for Throttle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::AuditOnly => write!(f, "audit_only"),
            Self::DryRun => write!(f, "dry_run"),
        }
    }
}

impl std::str::FromStr for Throttle {
    type Err = ThrottleParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Self::Full),
            "audit_only" => Ok(Self::AuditOnly),
            "dry_run" => Ok(Self::DryRun),
            _ => Err(ThrottleParseError(s.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid throttle level: {0} (expected full, audit_only, or dry_run)")]
pub struct ThrottleParseError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_allows_everything() {
        assert!(Throttle::Full.allows_mutation());
        assert!(Throttle::Full.allows_side_effects());
    }

    #[test]
    fn audit_only_suppresses_mutation() {
        assert!(!Throttle::AuditOnly.allows_mutation());
        assert!(Throttle::AuditOnly.allows_side_effects());
    }

    #[test]
    fn dry_run_suppresses_everything() {
        assert!(!Throttle::DryRun.allows_mutation());
        assert!(!Throttle::DryRun.allows_side_effects());
    }

    #[test]
    fn roundtrip_parse() {
        for throttle in [Throttle::Full, Throttle::AuditOnly, Throttle::DryRun] {
            let s = throttle.to_string();
            let parsed: Throttle = s.parse().unwrap();
            assert_eq!(throttle, parsed);
        }
    }

    #[test]
    fn invalid_parse() {
        assert!("bogus".parse::<Throttle>().is_err());
    }
}
