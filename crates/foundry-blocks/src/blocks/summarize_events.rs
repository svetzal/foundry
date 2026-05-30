//! `SummarizeEvents` — second block of the ops-digest formation.
//!
//! Sinks on `OpsObserved`. Self-filters when `proceed` is false, emitting
//! `OpsDigestCompleted{skipped: true}` immediately. When proceeding, builds
//! a structured prompt from the lean event digests and asks the agent for a
//! markdown summary of the operational period. Falls back to a raw listing
//! when the agent is unavailable. Emits `OpsSummaryComposed`.

use std::fmt::Write as _;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{
    OpsDigestCompletedPayload, OpsEventDigest, OpsObservedPayload, OpsSummaryComposedPayload,
};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::throttle::Throttle;

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentOutcome};

use super::{AgentBlockSpec, emit_result, invoke_agent};

/// Per-call timeout for the ops-digest agent invocation.
const AGENT_TIMEOUT: Duration = Duration::from_secs(300);

/// The digest agent does not write files.
const AGENT_ACCESS: AgentAccess = AgentAccess::ReadOnly;

/// Reasoning capability — we want the agent to identify patterns and surface
/// actionable signals from the raw event stream.
const AGENT_CAPABILITY: AgentCapability = AgentCapability::Reasoning;

/// Trace label for this block's agent invocation span.
const TRACE_LABEL: &str = "summarize_events";

/// Summarises new operational events into a markdown digest by asking the
/// agent to group, highlight anomalies, and surface actionable items.
/// Short-circuits when `proceed` is false (the pressure gate was not met).
///
/// Like `SummarizeCommits`, this block does not need the project registry —
/// the trigger payload carries everything it needs.
pub struct SummarizeEvents {
    agent: Arc<dyn AgentGateway>,
}

impl SummarizeEvents {
    pub fn new(agent: Arc<dyn AgentGateway>) -> Self {
        Self { agent }
    }
}

impl TaskBlock for SummarizeEvents {
    task_block_meta! {
        name: "Summarize Events",
        kind: Observer,
        sinks_on: [OpsObserved],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let p = parse_payload!(trigger, OpsObservedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let agent = Arc::clone(&self.agent);

        Box::pin(summarize(project, throttle, p, agent))
    }
}

async fn summarize(
    project: String,
    throttle: Throttle,
    observed: OpsObservedPayload,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    // Self-filter: the pressure gate said skip — pass the signal through.
    if !observed.proceed {
        let payload = OpsDigestCompletedPayload {
            success: true,
            skipped: true,
            digest_path: None,
            event_count: observed.new_event_count,
        };
        return emit_result(
            "ops-digest skipped (proceed=false)".to_string(),
            EventType::OpsDigestCompleted,
            &project,
            throttle,
            &payload,
        );
    }

    let event_count = observed.new_event_count;
    let new_watermark = observed.new_watermark.clone();

    // Empty event list — unlikely given pressure gate, but handle it gracefully.
    if observed.events.is_empty() {
        let markdown = "No operational events in the window.\n".to_string();
        let payload = OpsSummaryComposedPayload {
            markdown,
            event_count: 0,
            new_watermark,
        };
        return emit_result(
            "ops-digest: no events to summarise".to_string(),
            EventType::OpsSummaryComposed,
            &project,
            throttle,
            &payload,
        );
    }

    let prompt = build_prompt(&observed);
    let outcome = invoke_agent(
        agent.as_ref(),
        AgentBlockSpec {
            prompt,
            working_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            access: AGENT_ACCESS,
            capability: AGENT_CAPABILITY,
            agent_file: None,
            // Proactive ops-digest formation — not part of a request chain, so
            // there is no per-request override to honour. Uses the daemon default.
            provider: None,
            timeout: AGENT_TIMEOUT,
        },
        TRACE_LABEL,
        &project,
    )
    .await;

    let markdown = match outcome {
        AgentOutcome::Success { stdout } => stdout.trim().to_string(),
        AgentOutcome::AgentFailed { stderr } => fallback_digest(&observed, Some(&stderr)),
        AgentOutcome::Unavailable { error } => fallback_digest(&observed, Some(&error)),
    };

    let payload = OpsSummaryComposedPayload {
        markdown,
        event_count,
        new_watermark,
    };

    emit_result(
        format!("Ops digest composed for {event_count} event(s)"),
        EventType::OpsSummaryComposed,
        &project,
        throttle,
        &payload,
    )
}

/// Build the agent prompt from the observed events.
fn build_prompt(observed: &OpsObservedPayload) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are rendering an operational event digest for a working software \
         developer/consultant who reads it to understand what happened in their \
         business and technical systems over the last few hours. The data below \
         is the only source of truth — do not invent events or facts not present \
         in it.\n\n",
    );
    prompt.push_str("Render a markdown digest with these rules:\n");
    prompt.push_str("- Use Canadian English spelling and punctuation throughout.\n");
    if observed.anomaly_present {
        prompt.push_str(
            "- Open with a `## ⚠ Anomalies` section listing events that require immediate \
             attention (P0 urgency, CI failures, unresolved interventions, high/critical \
             vulnerabilities, maintenance failures).\n",
        );
    }
    prompt.push_str(
        "- Then a `## {domain}` heading for each domain that has events (e.g. \
         `## Infrastructure`, `## Clients`, `## AI`, `## Engineering`), in order \
         of significance (anomalous domains first).\n",
    );
    prompt.push_str(
        "- Under each domain heading, bullet-point the events. Each bullet should \
         be ≤ 120 characters. Include the event type in backticks and the summary.\n",
    );
    prompt.push_str(
        "- After the domain sections, a `## Summary` section with 2-3 sentences \
         synthesising the overall operational picture and any recommended follow-ups.\n",
    );
    prompt.push_str("- Return only the markdown body — no preamble, no code fences.\n\n");

    prompt.push_str("## Source data\n\n");
    writeln!(
        prompt,
        "Total new events: {count}  \n\
         Anomaly present: {anomaly}\n",
        count = observed.new_event_count,
        anomaly = observed.anomaly_present,
    )
    .expect("write to String never fails");

    for event in &observed.events {
        push_event_line(&mut prompt, event);
    }
    prompt
}

fn push_event_line(out: &mut String, event: &OpsEventDigest) {
    let client_suffix = if let Some(c) = &event.client {
        format!(" [client: {c}]")
    } else {
        String::new()
    };
    let urgency = event.urgency.as_deref().unwrap_or("P2");
    let summary = event.summary.as_deref().unwrap_or("(no summary)");
    writeln!(
        out,
        "- [{urgency}] `{etype}` | {domain} | {ts} | {summary}{client}",
        urgency = urgency,
        etype = event.event_type,
        domain = event.domain,
        ts = event.occurred_at,
        summary = summary,
        client = client_suffix,
    )
    .expect("write to String never fails");
}

/// Fallback digest used when the agent cannot be invoked or fails.
fn fallback_digest(observed: &OpsObservedPayload, agent_error: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(err) = agent_error {
        writeln!(out, "> ⚠ Agent unavailable; this digest is a raw fallback. ({err})\n")
            .expect("write to String never fails");
    }
    writeln!(
        out,
        "{count} operational event(s) in window. Anomaly: {anomaly}.\n",
        count = observed.new_event_count,
        anomaly = observed.anomaly_present,
    )
    .expect("write to String never fails");
    for event in &observed.events {
        push_event_line(&mut out, event);
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::EventType;
    use foundry_core::payload::{
        OpsDigestCompletedPayload, OpsEventDigest, OpsObservedPayload, OpsSummaryComposedPayload,
    };
    use foundry_core::throttle::Throttle;

    use crate::gateway::{AgentGateway, fakes::FakeAgentGateway};

    use super::*;

    fn sample_event(id: &str, event_type: &str) -> OpsEventDigest {
        OpsEventDigest {
            id: id.to_string(),
            event_type: event_type.to_string(),
            occurred_at: "2026-05-29T12:00:00Z".to_string(),
            domain: "infrastructure".to_string(),
            urgency: Some("P1".to_string()),
            summary: Some(format!("Something happened: {id}")),
            client: None,
        }
    }

    fn observed_with_events(proceed: bool, events: Vec<OpsEventDigest>) -> OpsObservedPayload {
        let count = events.len() as u64;
        let anomaly = events.iter().any(|e| e.urgency.as_deref() == Some("P0"));
        OpsObservedPayload {
            proceed,
            new_event_count: count,
            anomaly_present: anomaly,
            new_watermark: Some("2026-05-29T12:00:00Z".to_string()),
            events,
        }
    }

    fn trigger_with(payload: &OpsObservedPayload) -> Event {
        let json = serde_json::to_value(payload).unwrap();
        Event::new(EventType::OpsObserved, "system".to_string(), Throttle::Full, json)
    }

    assert_block_meta!(
        SummarizeEvents::new(FakeAgentGateway::success() as Arc<dyn AgentGateway>),
        kind: Observer,
        sinks_on: [OpsObserved],
    );

    #[tokio::test]
    async fn skips_when_proceed_false() {
        let agent = FakeAgentGateway::success();
        let block = SummarizeEvents::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let observed = observed_with_events(false, vec![]);
        let result = block.execute(&trigger_with(&observed)).await.unwrap();
        assert_eq!(result.events[0].event_type, EventType::OpsDigestCompleted);
        let p: OpsDigestCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(p.success);
        assert!(p.skipped);
        assert_eq!(agent.invocations().len(), 0, "must not call agent when skipping");
    }

    #[tokio::test]
    async fn invokes_agent_when_proceed_true() {
        let agent = FakeAgentGateway::success_with(
            "## Infrastructure\n- ci failed\n\n## Summary\nAll good.\n",
        );
        let block = SummarizeEvents::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let observed = observed_with_events(true, vec![sample_event("e1", "ci_pipeline_failure")]);
        let result = block.execute(&trigger_with(&observed)).await.unwrap();
        assert_eq!(result.events[0].event_type, EventType::OpsSummaryComposed);
        let p: OpsSummaryComposedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(p.event_count, 1);
        assert_eq!(p.new_watermark.as_deref(), Some("2026-05-29T12:00:00Z"));
        assert!(p.markdown.contains("Infrastructure") || p.markdown.contains("ci failed"));
        assert_eq!(agent.invocations().len(), 1, "exactly one agent call");
    }

    #[tokio::test]
    async fn agent_failure_produces_fallback_digest() {
        let agent = FakeAgentGateway::failure("model overloaded");
        let block = SummarizeEvents::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let observed = observed_with_events(true, vec![sample_event("e1", "ci_pipeline_failure")]);
        let result = block.execute(&trigger_with(&observed)).await.unwrap();
        let p: OpsSummaryComposedPayload = result.events[0].parse_payload().unwrap();
        assert!(
            p.markdown.contains("Agent unavailable") || p.markdown.contains("fallback"),
            "fallback must flag the agent failure: {}",
            p.markdown
        );
        assert!(p.markdown.contains("ci_pipeline_failure"), "fallback must include event type");
    }

    #[tokio::test]
    async fn prompt_includes_event_types_and_anomaly_flag() {
        let agent = FakeAgentGateway::success_with("## Summary\nok\n");
        let block = SummarizeEvents::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let observed = observed_with_events(
            true,
            vec![
                sample_event("e1", "ci_pipeline_failure"),
                sample_event("e2", "maintenance_run_completed"),
            ],
        );
        let result = block.execute(&trigger_with(&observed)).await.unwrap();
        assert_eq!(result.events[0].event_type, EventType::OpsSummaryComposed);
        let invocations = agent.invocations();
        let prompt = &invocations[0].prompt;
        assert!(prompt.contains("ci_pipeline_failure"), "prompt must mention event types");
        assert!(prompt.contains("Canadian English"), "spelling instruction must be present");
        assert!(
            prompt.contains("do not invent events") || prompt.contains("only source of truth"),
            "no-fabrication clause must be present"
        );
    }
}
