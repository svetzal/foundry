//! Ends agent sessions the previous `foundryd` process was stopped during.
//!
//! An agent session is a child process of the daemon. When the daemon stops,
//! the child dies with it, but nothing writes the session's
//! `agent_session_ended`. Consumers that rebuild session state from the event
//! log then show the session as running forever. On 2026-09-30 two restarts on
//! ops-01 left a task session and a campaign session in that state.
//!
//! On start, before the daemon accepts work, foundryd looks back
//! [`LOOKBACK_DAYS`] for `agent_session_started` events with no
//! `agent_session_ended` for the same `session_id`. Nothing can be running at
//! that point, so each such session is dead. For each, it records an
//! `agent_session_ended` with status `interrupted` and the error
//! `daemon restarted`, on the original session's project and trace.
//!
//! The ends it records are themselves in the log, so a second start finds
//! every session already ended and records nothing.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::AgentSessionEndedPayload;
use foundry_sdk::throttle::Throttle;

use super::RuntimeContext;
use super::recovery::LOOKBACK_DAYS;
use super::work_ledger::RESTART_REASON;

/// The session id an agent-session lifecycle event names, if its payload has
/// one.
fn session_id(event: &Event) -> Option<&str> {
    event.payload.get("session_id").and_then(serde_json::Value::as_str)
}

/// `agent_session_started` events at or after `since` with no
/// `agent_session_ended` for the same session id, in log order.
pub(crate) fn find_unended_sessions(events: &[Event], since: DateTime<Utc>) -> Vec<&Event> {
    let ended: HashSet<&str> = events
        .iter()
        .filter(|e| e.event_type == EventType::AgentSessionEnded)
        .filter_map(session_id)
        .collect();
    events
        .iter()
        .filter(|e| e.event_type == EventType::AgentSessionStarted && e.occurred_at >= since)
        .filter(|e| session_id(e).is_some_and(|id| !ended.contains(id)))
        .collect()
}

/// The `agent_session_ended` that closes `started`, a session the daemon was
/// stopped during, as of `ended_at`.
///
/// Returns `None` when `started` names no session id: there is nothing a
/// consumer could match the end to.
pub(crate) fn interrupted_end(started: &Event, ended_at: DateTime<Utc>) -> Option<Event> {
    let id = session_id(started)?;
    let payload = AgentSessionEndedPayload::interrupted(id, &ended_at.to_rfc3339(), RESTART_REASON);
    #[allow(
        clippy::expect_used,
        reason = "AgentSessionEndedPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    let payload = Event::serialize_payload(&payload)
        .expect("AgentSessionEndedPayload is infallibly serializable");
    Some(
        Event::new(EventType::AgentSessionEnded, started.project.clone(), Throttle::Full, payload)
            .with_trace_id(started.trace_id.clone())
            .with_causation_id(Some(started.id.clone())),
    )
}

/// End every agent session in the last [`LOOKBACK_DAYS`] of the event log at
/// `events_dir` that has no end, recording each end as a root event.
///
/// Called on daemon start, before any new dispatch. Unparseable log lines are
/// skipped by the reader, so a damaged log never keeps the daemon from
/// starting.
pub(crate) async fn end_interrupted_sessions(ctx: &RuntimeContext, events_dir: &Path) {
    let started_at = Utc::now();
    let since = started_at - Duration::days(LOOKBACK_DAYS);
    let events = foundry_blocks::blocks::triage_core::read_events_jsonl(events_dir, since);
    for started in find_unended_sessions(&events, since) {
        let Some(end) = interrupted_end(started, started_at) else {
            continue;
        };
        tracing::warn!(
            session_id = session_id(started).unwrap_or_default(),
            project = %started.project,
            session_started_at = %started.occurred_at,
            "ending an agent session foundryd was stopped during"
        );
        // The engine persists and broadcasts a root event like any other; no
        // block sinks on `agent_session_ended`, so nothing else runs.
        let _recorded = ctx.engine.process(end).await;
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, h, m, 0).unwrap()
    }

    fn lifecycle(ty: EventType, session: &str, when: DateTime<Utc>) -> Event {
        let mut e = Event::new(
            ty,
            "bedrock".to_string(),
            Throttle::Full,
            serde_json::json!({ "session_id": session }),
        )
        .with_trace_id(Some("t1".to_string()));
        e.occurred_at = when;
        e
    }

    #[test]
    fn a_started_session_with_no_end_is_found() {
        let events = vec![lifecycle(EventType::AgentSessionStarted, "s1", at(2, 6))];
        let found = find_unended_sessions(&events, at(0, 0));
        assert_eq!(found.len(), 1);
        assert_eq!(session_id(found[0]), Some("s1"));
    }

    #[test]
    fn an_ended_session_is_left_alone() {
        let events = vec![
            lifecycle(EventType::AgentSessionStarted, "s1", at(2, 6)),
            lifecycle(EventType::AgentSessionEnded, "s1", at(2, 7)),
        ];
        assert!(find_unended_sessions(&events, at(0, 0)).is_empty());
    }

    #[test]
    fn a_session_started_before_the_lookback_is_left_alone() {
        let events = vec![lifecycle(EventType::AgentSessionStarted, "s1", at(2, 6))];
        assert!(find_unended_sessions(&events, at(3, 0)).is_empty());
    }

    #[test]
    fn a_started_event_with_no_session_id_is_ignored() {
        let mut started = lifecycle(EventType::AgentSessionStarted, "s1", at(2, 6));
        started.payload = serde_json::json!({});
        assert!(find_unended_sessions(&[started], at(0, 0)).is_empty());
    }

    #[test]
    fn the_interrupted_end_carries_the_session_ids_and_closes_it() {
        let started = lifecycle(EventType::AgentSessionStarted, "s1", at(2, 6));
        let end = interrupted_end(&started, at(2, 8)).unwrap();

        assert_eq!(end.event_type, EventType::AgentSessionEnded);
        assert_eq!(end.project, "bedrock");
        assert_eq!(end.trace_id.as_deref(), Some("t1"));
        assert_eq!(end.causation_id.as_deref(), Some(started.id.as_str()));
        let payload: AgentSessionEndedPayload = end.parse_payload().unwrap();
        assert_eq!(payload.session_id, "s1");
        assert_eq!(payload.status, AgentSessionEndedPayload::STATUS_INTERRUPTED);
        assert_eq!(payload.error.as_deref(), Some(RESTART_REASON));
        assert_eq!(payload.ended_at, at(2, 8).to_rfc3339());

        // Once recorded, the same session is no longer found.
        assert!(find_unended_sessions(&[started, end], at(0, 0)).is_empty());
    }
}
