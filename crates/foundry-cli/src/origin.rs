//! Operator context for a dispatch this CLI asks for.
//!
//! A ledger item records *how* the work reached Foundry. For work an automation
//! dispatched that is the lane itself, but for work a person asked for by hand
//! the useful fact is which machine they typed it on and, when they say so, why.
//! This module builds two things from that: the one opaque origin string the
//! daemon carries to the ledger without parsing, and the typed `operator`
//! work source naming this host, which every root event this CLI emits
//! carries. No dispatch is refused, delayed or altered because of either.

/// What the CLI reports as its host when the hostname cannot be read.
///
/// A stated fallback rather than an omission: "the lookup failed" is a more
/// honest origin than silence, which reads as "no operator was involved".
const UNKNOWN_HOST: &str = "unknown host";

/// This machine's hostname, or [`UNKNOWN_HOST`] when it cannot be read.
pub fn local_hostname() -> String {
    hostname().unwrap_or_else(|| UNKNOWN_HOST.to_string())
}

/// The typed `operator` work source for a dispatch from this client, in wire
/// form: this host is what dispatched the work.
pub fn operator_source() -> crate::proto::WorkSource {
    crate::proto::WorkSource {
        kind: foundry_sdk::work_source::WorkSourceKind::Operator.tag().to_string(),
        r#ref: local_hostname(),
        cycle: None,
    }
}

/// Build the opaque operator origin for a dispatch from this client.
///
/// A CLI dispatch always has something to say — at least which host it came
/// from — so this never yields nothing. `origin` is carried verbatim: an empty
/// `--origin` is accepted and simply adds nothing.
pub fn operator_origin(hostname: Option<&str>, origin: Option<&str>) -> String {
    let host = hostname.unwrap_or(UNKNOWN_HOST);
    match origin.filter(|text| !text.is_empty()) {
        Some(text) => format!("host {host}: {text}"),
        None => format!("host {host}"),
    }
}

/// The operator origin for this process, looking the hostname up as it goes.
pub fn local_operator_origin(origin: Option<&str>) -> String {
    operator_origin(hostname().as_deref(), origin)
}

/// This machine's hostname, or `None` when it cannot be determined.
///
/// A hostname lookup is not worth failing a dispatch over, so the fault is
/// absorbed: the caller records [`UNKNOWN_HOST`] instead.
fn hostname() -> Option<String> {
    match std::process::Command::new("hostname").output() {
        Ok(output) if output.status.success() => {
            let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if name.is_empty() { None } else { Some(name) }
        }
        Ok(output) => {
            // Best-effort: the origin is operator context, never a precondition
            // of the dispatch, so a failed lookup records the stated fallback.
            tracing::warn!(
                status = ?output.status,
                "hostname lookup failed; recording dispatch origin as '{UNKNOWN_HOST}'"
            );
            None
        }
        Err(error) => {
            // Best-effort: as above — no `hostname` binary is not a reason to
            // refuse to run the task the operator asked for.
            tracing::warn!(
                %error,
                "hostname lookup failed; recording dispatch origin as '{UNKNOWN_HOST}'"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_the_hostname_alone_when_no_origin_text_was_given() {
        assert_eq!(operator_origin(Some("workbench"), None), "host workbench");
    }

    #[test]
    fn carries_the_origin_text_verbatim_beside_the_hostname() {
        assert_eq!(
            operator_origin(Some("workbench"), Some("rerun after the flaky gate")),
            "host workbench: rerun after the flaky gate"
        );
    }

    #[test]
    fn an_empty_origin_text_is_accepted_and_adds_nothing() {
        assert_eq!(operator_origin(Some("workbench"), Some("")), "host workbench");
    }

    #[test]
    fn a_failed_hostname_lookup_still_yields_a_stated_origin() {
        assert_eq!(operator_origin(None, Some("by hand")), "host unknown host: by hand");
        assert_eq!(operator_origin(None, None), "host unknown host");
    }

    #[test]
    fn the_local_lookup_always_produces_an_origin() {
        // Whether or not this machine answers a hostname lookup, a dispatch
        // from the CLI always carries operator context — it never fails here.
        assert!(local_operator_origin(Some("by hand")).ends_with(": by hand"));
    }
}
