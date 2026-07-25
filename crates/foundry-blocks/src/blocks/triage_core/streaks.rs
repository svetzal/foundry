use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::PreflightCompletedPayload;
use foundry_sdk::triage::{Decision, FailureClass, FailureVerdict, InfraIncident};

use super::RawFailure;
use super::classify::{classify, decision_for};
use super::correlate::correlate;

/// One gate occurrence record used while computing streak lengths.
struct GateOccurrence {
    occurred_at: DateTime<Utc>,
    passed: bool,
}

/// Compute per-(project, gate) consecutive failure streaks from historical events.
///
/// Reads `PreflightCompleted` events from the lookback window, groups gate
/// results by `(project, gate.name)`, sorts by `occurred_at` descending, and
/// counts consecutive failures from the most recent occurrence until a pass or
/// the lookback boundary.
///
/// Returns a map of `(project, gate) => streak_length` alongside a count of
/// in-window `PreflightCompleted` events whose payload could not be parsed
/// and were therefore skipped (Absorb: one malformed event must not prevent
/// streak computation for the rest of the history; the caller surfaces the
/// count in the emitted digest payload instead).
pub fn streaks(events: &[Event], lookback_days: u32) -> (HashMap<(String, String), u32>, u64) {
    let cutoff = Utc::now() - Duration::days(i64::from(lookback_days));

    let mut gate_history: HashMap<(String, String), Vec<GateOccurrence>> = HashMap::new();
    let mut unparsed_events = 0u64;

    for event in events {
        if event.event_type != EventType::PreflightCompleted {
            continue;
        }
        if event.occurred_at < cutoff {
            continue;
        }
        let payload = match event.parse_payload::<PreflightCompletedPayload>() {
            Ok(payload) => payload,
            Err(err) => {
                // Best-effort: a malformed PreflightCompleted payload must not
                // fail streak computation for the rest of the history; it is
                // skipped and counted so the caller can surface it.
                tracing::warn!(
                    event_id = %event.id,
                    error = %err,
                    "triage: failed to parse PreflightCompleted payload while computing streaks, skipping"
                );
                unparsed_events += 1;
                continue;
            }
        };
        for result in &payload.results {
            let key = (payload.project.clone(), result.name.clone());
            gate_history.entry(key).or_default().push(GateOccurrence {
                occurred_at: event.occurred_at,
                passed: result.passed || result.fix_applied,
            });
        }
    }

    let mut result = HashMap::new();

    for (key, mut occurrences) in gate_history {
        // Sort descending so we walk from newest to oldest.
        occurrences.sort_by_key(|o| std::cmp::Reverse(o.occurred_at));

        let mut streak: u32 = 0;
        for occ in &occurrences {
            if occ.passed {
                break;
            }
            streak += 1;
        }

        if streak > 0 {
            result.insert(key, streak);
        }
    }

    (result, unparsed_events)
}

/// Build the full set of verdicts applying: correlation, classification,
/// decision boundary, streak override, and benign reclassification.
///
/// 1. Correlate failures → (incidents, remaining)
/// 2. Compute streaks over `all_events`
/// 3. For each remaining failure: classify → base decision → apply overrides
/// 4. Return (verdicts, incidents, `unparsed_events`) — `unparsed_events` counts
///    `PreflightCompleted` events in the streak lookback window whose payload
///    could not be parsed (see `streaks`).
pub fn build_verdicts(
    raw_failures: Vec<RawFailure>,
    all_events: &[Event],
    streak_lookback_days: u32,
) -> (Vec<FailureVerdict>, Vec<InfraIncident>, u64) {
    let (incidents, remaining) = correlate(raw_failures);
    let (streak_map, unparsed_events) = streaks(all_events, streak_lookback_days);

    let mut verdicts = Vec::new();

    for failure in remaining {
        let mut class = classify(&failure);
        let key = (failure.project.clone(), failure.gate_name.clone());
        let streak = streak_map.get(&key).copied().unwrap_or(0);

        // Streak override: N≥3 consecutive failures → ChronicDeadlock + Escalate
        if streak >= 3 {
            class = FailureClass::ChronicDeadlock;
        }

        let mut decision = decision_for(&class);

        // TriageRejectedNoise always maps to ReclassifyBenign (redundant but explicit)
        if class == FailureClass::TriageRejectedNoise {
            decision = Decision::ReclassifyBenign;
        }

        // Attach proposed_command for AutoFixable failures
        let proposed_command = if decision == Decision::AutoFixable {
            failure.fix_command.clone()
        } else {
            None
        };

        let evidence = failure.gate_output.chars().take(500).collect::<String>();

        verdicts.push(FailureVerdict {
            project: failure.project,
            gate: failure.gate_name,
            class,
            decision,
            evidence,
            proposed_command,
        });
    }

    (verdicts, incidents, unparsed_events)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::triage::{Decision, FailureClass};

    use super::*;

    fn make_preflight_event(project: &str, gate: &str, passed: bool, fix_applied: bool) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "workflow": "maintain",
                "all_passed": passed,
                "required_passed": passed,
                "results": [{
                    "name": gate,
                    "command": "cargo fmt --check",
                    "passed": passed,
                    "required": true,
                    "output": "some output",
                    "exit_code": i32::from(!passed),
                    "fix_applied": fix_applied,
                }]
            }),
        )
    }

    #[test]
    fn streaks_detects_consecutive_failures() {
        let events: Vec<Event> = (0..4)
            .map(|i| {
                let mut e = make_preflight_event("alpha", "fmt", false, false);
                e.occurred_at = Utc::now() - Duration::hours(i64::from(i) + 1);
                e
            })
            .collect();

        let (result, unparsed) = streaks(&events, 30);
        let streak = result.get(&("alpha".to_string(), "fmt".to_string())).copied().unwrap_or(0);
        assert_eq!(streak, 4);
        assert_eq!(unparsed, 0);
    }

    #[test]
    fn streaks_resets_on_pass() {
        let mut events: Vec<Event> = Vec::new();

        for i in 0..2 {
            let mut e = make_preflight_event("alpha", "test", false, false);
            e.occurred_at = Utc::now() - Duration::hours(i64::from(i) + 1);
            events.push(e);
        }
        let mut pass = make_preflight_event("alpha", "test", true, false);
        pass.occurred_at = Utc::now() - Duration::hours(3);
        events.push(pass);
        let mut old_fail = make_preflight_event("alpha", "test", false, false);
        old_fail.occurred_at = Utc::now() - Duration::hours(4);
        events.push(old_fail);

        let (result, unparsed) = streaks(&events, 30);
        let streak = result.get(&("alpha".to_string(), "test".to_string())).copied().unwrap_or(0);
        assert_eq!(streak, 2);
        assert_eq!(unparsed, 0);
    }

    #[test]
    fn streaks_counts_unparsed_events_and_skips_them() {
        let mut events: Vec<Event> = Vec::new();
        let mut good = make_preflight_event("alpha", "fmt", false, false);
        good.occurred_at = Utc::now() - Duration::hours(1);
        events.push(good);

        let mut malformed = Event::new(
            EventType::PreflightCompleted,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({ "not": "a valid preflight payload shape" }),
        );
        malformed.occurred_at = Utc::now() - Duration::hours(1);
        events.push(malformed);

        let (result, unparsed) = streaks(&events, 30);
        let streak = result.get(&("alpha".to_string(), "fmt".to_string())).copied().unwrap_or(0);
        assert_eq!(streak, 1, "only the well-formed event should count toward the streak");
        assert_eq!(unparsed, 1);
    }

    #[test]
    fn build_verdicts_streak_override_escalates_at_n_ge_3() {
        let now = Utc::now();
        let window_start = now - Duration::hours(5);
        let window_end = now;

        let mut all_events: Vec<Event> = Vec::new();
        let mut raw_failures: Vec<RawFailure> = Vec::new();

        for i in 0..4_u32 {
            let mut e = make_preflight_event("alpha", "test", false, false);
            e.occurred_at = now - Duration::hours(i64::from(i) + 1);
            all_events.push(e.clone());
            raw_failures.push(RawFailure {
                project: "alpha".to_string(),
                gate_name: "test".to_string(),
                gate_output: "test failed".to_string(),
                exit_code: Some(1),
                fix_command: None,
                occurred_at: e.occurred_at,
            });
        }

        let recent: Vec<RawFailure> = raw_failures
            .into_iter()
            .filter(|f| f.occurred_at >= window_start && f.occurred_at <= window_end)
            .take(1)
            .collect();

        let (verdicts, _incidents, unparsed) = build_verdicts(recent, &all_events, 30);

        assert_eq!(verdicts.len(), 1);
        assert_eq!(verdicts[0].class, FailureClass::ChronicDeadlock);
        assert_eq!(verdicts[0].decision, Decision::Escalate);
        assert_eq!(unparsed, 0);
    }
}
