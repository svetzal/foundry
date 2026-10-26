//! How long a git command that runs repository hooks may take.
//!
//! A repository's hooks run inside the git command that triggers them: a
//! pre-push hook runs inside `git push`, a post-merge hook inside `git merge`
//! and `git pull`. Projects use a pre-push hook to run their whole validation,
//! so such a push takes as long as the project's gates do. The shell's general
//! five-minute default is meant for git housekeeping, and on 2026-10-26 it
//! killed a landing push whose hook was still validating a client frontend.
//!
//! Foundry never bypasses a hook. It gives the command the time the project's
//! own gate file says its validation needs instead: the sum of the required
//! gates' timeouts, at this host's gate-timeout scale, clamped to
//! [`GIT_HOOK_TIMEOUT_FLOOR`] and [`GIT_HOOK_TIMEOUT_CEILING`].

use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::gate_runner::{GATE_TIMEOUT_SCALE_ENV, effective_gate_timeout};

/// The least time a hook-running git command gets: the shell's general
/// default, so a project without required gates behaves exactly as it did
/// before hook timeouts were derived.
pub(crate) const GIT_HOOK_TIMEOUT_FLOOR: Duration = crate::shell::DEFAULT_TIMEOUT;

/// The most time a hook-running git command gets, whatever the gate file says.
///
/// Gate files contain mistakes. A client repository had a gate with
/// `"timeout": 300000`, meant as milliseconds and read as seconds, and an
/// uncapped sum would let a hung hook hold Finalize Task for 83 hours. Two
/// hours is above the largest required-gate sum Foundry runs legitimately at
/// scale 1.0 (its own gate file sums to 90 minutes), so a working hook is never
/// cut short by the cap. A hung one costs one lane at most two hours per
/// command.
pub(crate) const GIT_HOOK_TIMEOUT_CEILING: Duration = Duration::from_secs(2 * 60 * 60);

/// Where a [`GitHookTimeout`]'s limit came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Basis {
    /// The limit is the sum of the required gate timeouts.
    GateSum,
    /// The sum was below [`GIT_HOOK_TIMEOUT_FLOOR`], so the floor applies.
    Floor,
    /// The sum was above [`GIT_HOOK_TIMEOUT_CEILING`], so the ceiling applies.
    Ceiling,
    /// The gate file exists but could not be read; the floor applies.
    UnreadableGateFile,
}

/// The time limit for one hook-running git command, and how it was derived.
///
/// Its [`Display`](fmt::Display) says how the limit was derived, in a form a
/// timeout message can quote.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GitHookTimeout {
    /// What the command runs under.
    pub(crate) limit: Duration,
    basis: Basis,
    /// How many required gates the gate file declares.
    required_gates: usize,
    /// Their timeouts summed, after the host scale, before clamping.
    gate_sum: Duration,
    /// The host's gate-timeout scale.
    scale: f64,
}

impl GitHookTimeout {
    /// Derive the limit from the required gates' own timeouts (`None` for a
    /// gate that sets none) and the host's gate-timeout scale.
    ///
    /// Each timeout is the one the gate runner would use
    /// ([`effective_gate_timeout`]); they are summed because a pre-push hook
    /// runs a project's validation steps one after another.
    pub(crate) fn derive(
        required_gate_timeouts: impl IntoIterator<Item = Option<Duration>>,
        scale: f64,
    ) -> Self {
        let mut required_gates = 0;
        let mut gate_sum = Duration::ZERO;
        for own in required_gate_timeouts {
            required_gates += 1;
            gate_sum = gate_sum.saturating_add(effective_gate_timeout(own, scale));
        }
        let (limit, basis) = if gate_sum < GIT_HOOK_TIMEOUT_FLOOR {
            (GIT_HOOK_TIMEOUT_FLOOR, Basis::Floor)
        } else if gate_sum > GIT_HOOK_TIMEOUT_CEILING {
            (GIT_HOOK_TIMEOUT_CEILING, Basis::Ceiling)
        } else {
            (gate_sum, Basis::GateSum)
        };
        Self {
            limit,
            basis,
            required_gates,
            gate_sum,
            scale,
        }
    }

    /// The limit for a tree whose gate file could not be read: the floor.
    fn unreadable_gate_file(scale: f64) -> Self {
        Self {
            basis: Basis::UnreadableGateFile,
            ..Self::derive([], scale)
        }
    }

    /// The limit for a git command that runs in `dir`, from the
    /// `.hone-gates.json` there. A missing gate file declares no required
    /// gates; an unreadable one is logged and gets the floor.
    pub(crate) fn for_tree(dir: &Path) -> Self {
        let scale = crate::gate_runner::host_gate_timeout_scale();
        match crate::gate_file::read_gates(dir) {
            Ok(gates) => Self::derive(
                gates.iter().filter(|gate| gate.required).map(|gate| gate.timeout),
                scale,
            ),
            Err(error) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %error,
                    floor_secs = GIT_HOOK_TIMEOUT_FLOOR.as_secs(),
                    "could not read the gate file to derive a git hook timeout; using the floor"
                );
                Self::unreadable_gate_file(scale)
            }
        }
    }

    fn gates_phrase(&self) -> String {
        let scale = if (self.scale - 1.0).abs() > f64::EPSILON {
            format!(" (scaled by {GATE_TIMEOUT_SCALE_ENV}={})", self.scale)
        } else {
            String::new()
        };
        format!(
            "the {} required gate timeout(s) in .hone-gates.json{scale}",
            self.required_gates
        )
    }
}

impl fmt::Display for GitHookTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let floor = human(GIT_HOOK_TIMEOUT_FLOOR);
        match self.basis {
            Basis::GateSum => write!(f, "the sum of {}", self.gates_phrase()),
            Basis::Floor if self.required_gates == 0 => write!(
                f,
                "the {floor} floor, because .hone-gates.json is absent or declares no required gates"
            ),
            Basis::Floor => write!(
                f,
                "the {floor} floor, because {} sum to only {}",
                self.gates_phrase(),
                human(self.gate_sum)
            ),
            Basis::Ceiling => write!(
                f,
                "the {} ceiling, because {} sum to {}",
                human(GIT_HOOK_TIMEOUT_CEILING),
                self.gates_phrase(),
                human(self.gate_sum)
            ),
            Basis::UnreadableGateFile => {
                write!(f, "the {floor} floor, because .hone-gates.json could not be read")
            }
        }
    }
}

/// A git command that was killed at its [`GitHookTimeout`] while still
/// running, most likely inside a repository hook.
///
/// Callers find it with `anyhow::Error::downcast_ref` to tell an operational
/// timeout from an ordinary git failure.
#[derive(Debug)]
pub(crate) struct GitCommandTimedOut {
    /// The git arguments, joined: `push origin main`.
    pub(crate) command: String,
    pub(crate) timeout: GitHookTimeout,
}

impl fmt::Display for GitCommandTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "git {} was still running when it reached its {} limit, so the repository's \
             git hook (pre-push, for a push) was most likely still running. The limit is {}. \
             Hooks are never bypassed: if the hook needs longer, raise the required gate \
             timeouts in .hone-gates.json or {GATE_TIMEOUT_SCALE_ENV} on this host",
            self.command,
            human(self.timeout.limit),
            self.timeout
        )
    }
}

impl std::error::Error for GitCommandTimedOut {}

/// Whether `error` is the shell gateway's timeout: a command killed because it
/// was still running at its limit.
///
/// The gateway builds that error from `tokio::time::timeout`, so the chain
/// holds an [`Elapsed`](tokio::time::error::Elapsed); any other failure (spawn,
/// wait) does not.
pub(crate) fn is_shell_timeout(error: &anyhow::Error) -> bool {
    error.chain().any(<dyn std::error::Error>::is::<tokio::time::error::Elapsed>)
}

/// A duration as hours, minutes and seconds, largest unit first, zero parts
/// left out: `2h`, `1h 40m`, `5m`, `312s`.
fn human(duration: Duration) -> String {
    let secs = duration.as_secs() + u64::from(duration.subsec_nanos() >= 500_000_000);
    let (hours, minutes, seconds) = (secs / 3600, secs % 3600 / 60, secs % 60);
    if hours == 0 && minutes == 0 {
        return format!("{seconds}s");
    }
    if hours == 0 && seconds != 0 {
        return format!("{secs}s");
    }
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 {
        parts.push(format!("{seconds}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate_runner::DEFAULT_GATE_TIMEOUT;

    fn secs(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn the_limit_is_the_sum_of_the_required_gate_timeouts() {
        let timeout = GitHookTimeout::derive([Some(secs(400)), None, Some(secs(2_400))], 1.0);

        assert_eq!(timeout.limit, secs(400) + DEFAULT_GATE_TIMEOUT + secs(2_400));
        assert_eq!(
            timeout.to_string(),
            "the sum of the 3 required gate timeout(s) in .hone-gates.json"
        );
    }

    #[test]
    fn a_sum_below_the_shell_default_gets_the_floor() {
        let timeout = GitHookTimeout::derive([Some(secs(60)), Some(secs(90))], 1.0);

        assert_eq!(GIT_HOOK_TIMEOUT_FLOOR, secs(300));
        assert_eq!(timeout.limit, GIT_HOOK_TIMEOUT_FLOOR);
        assert_eq!(
            timeout.to_string(),
            "the 5m floor, because the 2 required gate timeout(s) in .hone-gates.json sum to only \
             150s"
        );
    }

    #[test]
    fn no_required_gates_behaves_exactly_as_the_shell_default() {
        let timeout = GitHookTimeout::derive([], 1.0);

        assert_eq!(timeout.limit, crate::shell::DEFAULT_TIMEOUT);
        assert!(timeout.to_string().contains("absent or declares no required gates"));
    }

    /// The client gate file that motivated the cap: 300000 meant as
    /// milliseconds, read as seconds.
    #[test]
    fn a_mistaken_gate_timeout_is_capped_at_the_ceiling() {
        let timeout = GitHookTimeout::derive([Some(secs(300_000)), None], 1.0);

        assert_eq!(timeout.limit, GIT_HOOK_TIMEOUT_CEILING);
        assert_eq!(
            timeout.to_string(),
            "the 2h ceiling, because the 2 required gate timeout(s) in .hone-gates.json sum to \
             83h 30m"
        );
    }

    #[test]
    fn an_absurd_gate_timeout_saturates_rather_than_panicking() {
        let timeout = GitHookTimeout::derive([Some(Duration::MAX), Some(Duration::MAX)], 2.0);

        assert_eq!(timeout.limit, GIT_HOOK_TIMEOUT_CEILING);
    }

    #[test]
    fn the_host_scale_stretches_every_gate_and_is_named() {
        let timeout = GitHookTimeout::derive([Some(secs(300)), None], 2.0);

        assert_eq!(timeout.limit, secs(600) + DEFAULT_GATE_TIMEOUT * 2);
        assert_eq!(
            timeout.to_string(),
            "the sum of the 2 required gate timeout(s) in .hone-gates.json (scaled by \
             FOUNDRY_GATE_TIMEOUT_SCALE=2)"
        );
    }

    fn write_gates(dir: &Path, gates: &serde_json::Value) {
        std::fs::write(dir.join(".hone-gates.json"), gates.to_string()).unwrap();
    }

    #[test]
    fn a_tree_counts_only_its_required_gates() {
        let dir = tempfile::tempdir().unwrap();
        write_gates(
            dir.path(),
            &serde_json::json!({"gates": [
                {"name": "lint", "command": "true", "required": true, "timeout": 200},
                {"name": "slow", "command": "true", "required": false, "timeout": 9000},
                {"name": "test", "command": "true", "required": true, "timeout": 500}
            ]}),
        );

        let timeout = GitHookTimeout::for_tree(dir.path());

        let scale = crate::gate_runner::host_gate_timeout_scale();
        assert_eq!(timeout, GitHookTimeout::derive([Some(secs(200)), Some(secs(500))], scale));
    }

    #[test]
    fn a_tree_without_a_gate_file_gets_the_floor() {
        let dir = tempfile::tempdir().unwrap();

        let timeout = GitHookTimeout::for_tree(dir.path());

        assert_eq!(timeout.limit, GIT_HOOK_TIMEOUT_FLOOR);
        assert_eq!(timeout.required_gates, 0);
    }

    #[test]
    fn a_tree_with_an_unreadable_gate_file_gets_the_floor_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hone-gates.json"), "not json").unwrap();

        let timeout = GitHookTimeout::for_tree(dir.path());

        assert_eq!(timeout.limit, GIT_HOOK_TIMEOUT_FLOOR);
        assert_eq!(timeout.to_string(), "the 5m floor, because .hone-gates.json could not be read");
    }

    #[test]
    fn a_timeout_message_names_the_command_the_limit_and_its_derivation() {
        let timed_out = GitCommandTimedOut {
            command: "push origin main".to_string(),
            timeout: GitHookTimeout::derive([Some(secs(1_200)), Some(secs(1_800))], 1.0),
        };

        let message = timed_out.to_string();

        assert!(
            message.starts_with(
                "git push origin main was still running when it reached its 50m limit"
            ),
            "{message}"
        );
        assert!(message.contains("git hook (pre-push, for a push) was most likely still running"));
        assert!(
            message.contains(
                "The limit is the sum of the 2 required gate timeout(s) in .hone-gates.json"
            ),
            "{message}"
        );
        assert!(message.contains("Hooks are never bypassed"), "{message}");
    }

    /// The detection must work on the error the real gateway builds, through
    /// the trait object every block holds.
    #[tokio::test]
    async fn the_real_shell_gateway_timeout_is_recognised() {
        let shell: &dyn crate::gateway::ShellGateway = &crate::gateway::ProcessShellGateway;
        let dir = std::env::temp_dir();

        let timed_out = shell
            .run(&dir, "sleep", &["60"], None, Some(Duration::from_millis(50)))
            .await
            .unwrap_err();
        let spawn_failed = shell
            .run(&dir, "this_command_does_not_exist_foundry", &[], None, None)
            .await
            .unwrap_err();

        assert!(is_shell_timeout(&timed_out), "{timed_out:#}");
        assert!(!is_shell_timeout(&spawn_failed), "{spawn_failed:#}");
    }

    #[test]
    fn durations_read_as_hours_minutes_and_seconds() {
        assert_eq!(human(secs(300)), "5m");
        assert_eq!(human(secs(312)), "312s");
        assert_eq!(human(secs(45)), "45s");
        assert_eq!(human(secs(7_200)), "2h");
        assert_eq!(human(secs(6_000)), "1h 40m");
        assert_eq!(human(secs(3_725)), "1h 2m 5s");
    }
}
