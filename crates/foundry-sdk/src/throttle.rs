use serde::{Deserialize, Serialize};

/// Controls whether an event chain performs real mutations or simulates them.
///
/// The throttle is set at invocation time and propagated through every event
/// in the chain. Events are **always** persisted and broadcast — they are
/// facts. The throttle only controls whether Mutator blocks run for real.
///
/// # Behaviour matrix
///
/// | Level    | Observers      | Mutators                          |
/// |----------|----------------|-----------------------------------|
/// | `Full`   | Execute + emit | Execute + emit (real)             |
/// | `DryRun` | Execute + emit | Simulated via `dry_run_events()`  |
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Throttle {
    /// All blocks execute and emit; the chain runs for real. Default for
    /// automated runs.
    #[default]
    Full,
    /// Observer blocks execute normally. Mutator blocks are **not** executed —
    /// they simulate success via `dry_run_events()`. Simulated events carry
    /// `dry_run: true` in their payload and are delivered, so the full chain
    /// shape is visible without changing anything.
    DryRun,
}

impl Throttle {
    /// Whether Mutator blocks run for real at this throttle level.
    ///
    /// `true` under [`Full`]; `false` under [`DryRun`], where mutations are
    /// simulated instead. Observer blocks always run regardless.
    ///
    /// [`Full`]: Throttle::Full
    /// [`DryRun`]: Throttle::DryRun
    pub fn permits_mutation(self) -> bool {
        matches!(self, Self::Full)
    }
}

impl std::fmt::Display for Throttle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => write!(f, "full"),
            Self::DryRun => write!(f, "dry_run"),
        }
    }
}

impl std::str::FromStr for Throttle {
    type Err = ThrottleParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "full" => Ok(Self::Full),
            "dry_run" => Ok(Self::DryRun),
            _ => Err(ThrottleParseError(s.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid throttle level: {0} (expected full or dry_run)")]
pub struct ThrottleParseError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_permits_mutation() {
        assert!(Throttle::Full.permits_mutation());
    }

    #[test]
    fn dry_run_does_not_permit_mutation() {
        assert!(!Throttle::DryRun.permits_mutation());
    }

    #[test]
    fn roundtrip_parse() {
        for throttle in [Throttle::Full, Throttle::DryRun] {
            let s = throttle.to_string();
            let parsed: Throttle = s.parse().unwrap();
            assert_eq!(throttle, parsed);
        }
    }

    #[test]
    fn invalid_parse() {
        assert!("bogus".parse::<Throttle>().is_err());
        assert!("audit_only".parse::<Throttle>().is_err());
    }
}
