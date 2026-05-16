//! Task-local span context for in-process span propagation across
//! subprocess spawn sites.
//!
//! Set by the engine before invoking a block's `execute`; read by
//! `shell.rs` and `agent_stream.rs` to inject `TRACEPARENT` into
//! spawned commands.
//!
//! Items here are wired up incrementally over Phase 5. After Task 5.5
//! the tokio-flavored `inject_traceparent` and `SpanContext`/`SPAN_CONTEXT`
//! have production consumers; `inject_traceparent_std` and `from_event`
//! remain staged for future migrations (release/install chains and event
//! replay) and carry targeted `allow(dead_code)` attributes.

use foundry_core::event::Event;

/// The current block's span context.
#[derive(Debug, Clone)]
pub struct SpanContext {
    /// 32-char lowercase hex.
    pub trace_id: String,
    /// 16-char lowercase hex — the block's own `span_id`.
    pub span_id: String,
}

impl SpanContext {
    /// Build a W3C Trace Context `traceparent` header value:
    /// `00-<trace_id>-<span_id>-01`.
    #[must_use]
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }
}

tokio::task_local! {
    /// Set by the engine for the duration of a block's `execute` call.
    pub static SPAN_CONTEXT: SpanContext;
}

/// Inject `TRACEPARENT` into a `Command` if a span context is active in the
/// current tokio task. When no context is active, explicitly removes any
/// inherited `TRACEPARENT` from the parent process environment so the child
/// starts clean.
pub fn inject_traceparent(cmd: &mut tokio::process::Command) {
    if SPAN_CONTEXT
        .try_with(|ctx| {
            cmd.env("TRACEPARENT", ctx.traceparent());
        })
        .is_err()
    {
        cmd.env_remove("TRACEPARENT");
    }
}

/// Variant for `std::process::Command` (legacy callsites in subprocess
/// migration). Prefer migrating to `tokio::process::Command`.
///
/// Like `inject_traceparent`, explicitly removes any inherited `TRACEPARENT`
/// from the parent process environment when no context is active.
#[allow(dead_code)]
pub fn inject_traceparent_std(cmd: &mut std::process::Command) {
    if SPAN_CONTEXT
        .try_with(|ctx| {
            cmd.env("TRACEPARENT", ctx.traceparent());
        })
        .is_err()
    {
        cmd.env_remove("TRACEPARENT");
    }
}

/// Extract a `SpanContext` from an Event's span fields. Returns `None` if any
/// required field is missing.
#[allow(dead_code)]
#[must_use]
pub fn from_event(event: &Event) -> Option<SpanContext> {
    let trace_id = event.trace_id.clone()?;
    let span_id = event.span_id.clone()?;
    Some(SpanContext { trace_id, span_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_format_matches_w3c() {
        let ctx = SpanContext {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };
        assert_eq!(ctx.traceparent(), "00-0123456789abcdef0123456789abcdef-fedcba9876543210-01");
    }

    #[tokio::test]
    async fn inject_traceparent_within_scope_sets_env() {
        let ctx = SpanContext {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };
        let expected = ctx.traceparent();
        SPAN_CONTEXT
            .scope(ctx, async move {
                let mut cmd = tokio::process::Command::new("true");
                inject_traceparent(&mut cmd);
                // tokio::process::Command exposes env via as_std()
                let env: Vec<(String, String)> = cmd
                    .as_std()
                    .get_envs()
                    .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
                    .collect();
                assert!(env.iter().any(|(k, v)| k == "TRACEPARENT" && v == &expected));
            })
            .await;
    }

    #[tokio::test]
    async fn inject_traceparent_outside_scope_strips_inherited() {
        // No SPAN_CONTEXT::scope around this — inject_traceparent should
        // issue an env_remove so the child does not inherit any TRACEPARENT
        // that happens to live in the parent process environment.
        // `get_envs()` represents a removal directive as `(key, None)`, which
        // is what we want to see — no non-None value for TRACEPARENT.
        let mut cmd = tokio::process::Command::new("true");
        inject_traceparent(&mut cmd);
        assert!(
            cmd.as_std()
                .get_envs()
                .all(|(k, v)| { k.to_str() != Some("TRACEPARENT") || v.is_none() })
        );
    }
}
