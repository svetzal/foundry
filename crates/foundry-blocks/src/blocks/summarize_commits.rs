//! `SummarizeCommits` — second block of the daily commit-digest formation.
//!
//! Sinks on `CommitsObserved`. Short-circuits when the window holds zero
//! commits and writes a "nothing today" stub; otherwise builds a structured
//! prompt naming every commit's SHA prefix / subject / author and asks the
//! agent for a markdown digest. Emits `CommitSummaryCompleted`.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    CommitInfo, CommitSummaryCompletedPayload, CommitsObservedPayload, ProjectCommits,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;

use crate::gateway::{AgentAccess, AgentGateway, AgentOutcome, ModelTier, ReasoningEffort};

use super::{AgentBlockSpec, emit_result, invoke_agent};

/// Per-call timeout for the digest agent invocation. The prompt is large
/// but cheap — five minutes is generous and bounds a hung agent.
const AGENT_TIMEOUT: Duration = Duration::from_secs(300);

/// The digest agent does not write files; `ReadOnly` is the right access tier.
const AGENT_ACCESS: AgentAccess = AgentAccess::ReadOnly;

/// Reasoning capability — we want the agent to spot patterns and call out
/// risky changes, not just template-fill.
const AGENT_TIER: ModelTier = ModelTier::Deep;
const AGENT_EFFORT: ReasoningEffort = ReasoningEffort::High;

/// Trace label that shows up in `tracing` spans for this block's agent call.
const TRACE_LABEL: &str = "summarize_commits";

/// SHA prefix length used in the prompt and (downstream) in the rendered
/// digest. Seven chars is git's de-facto short-SHA convention.
const SHA_PREFIX_LEN: usize = 7;

/// Renders the day's commits into a human-friendly markdown digest by
/// asking the agent to group, summarise, and call out anything risky.
/// Short-circuits when there is nothing to summarise.
///
/// Unlike most agent-driven blocks, `SummarizeCommits` does not need the
/// project registry — the trigger payload carries everything it needs about
/// each project (name, branch, commits). The constructor therefore takes
/// only the agent gateway, sidestepping the otherwise-shared
/// `agent_block_new!` macro.
pub struct SummarizeCommits {
    agent: Arc<dyn AgentGateway>,
}

impl SummarizeCommits {
    pub fn new(agent: Arc<dyn AgentGateway>) -> Self {
        Self { agent }
    }
}

impl TaskBlock for SummarizeCommits {
    task_block_meta! {
        name: "Summarize Commits",
        kind: Observer,
        sinks_on: [CommitsObserved],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let p = parse_payload!(trigger, CommitsObservedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let agent = Arc::clone(&self.agent);

        Box::pin(summarize(project, throttle, p, agent))
    }
}

async fn summarize(
    project: String,
    throttle: Throttle,
    observed: CommitsObservedPayload,
    agent: Arc<dyn AgentGateway>,
) -> anyhow::Result<TaskBlockResult> {
    let project_count = observed.project_count();
    let total_commits = observed.total_commits();

    // Empty-day short-circuit: skip the agent call, emit a stub digest.
    // Absence is a fact too — the downstream writer still produces a file.
    if total_commits == 0 {
        let markdown = format!(
            "No commits across {project_count} project{plural} in the last \
             {hours} hours.\n",
            plural = if project_count == 1 { "" } else { "s" },
            hours = observed.window_hours,
        );
        let payload = CommitSummaryCompletedPayload {
            markdown,
            project_count,
            total_commits,
        };
        return emit_result(
            "empty-day short-circuit (no agent call)".to_string(),
            EventType::CommitSummaryCompleted,
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
            // Agent runs over the digest text itself, not against any one
            // project's checkout — use the daemon's process working dir so
            // the agent has no filesystem coupling to a registry entry.
            working_dir: std::env::current_dir().unwrap_or_else(|_| ".".into()),
            access: AGENT_ACCESS,
            tier: AGENT_TIER,
            effort: AGENT_EFFORT,
            agent_file: None,
            // Proactive digest formation — not part of a request chain, so there
            // is no per-request override to honour. Uses the daemon default.
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

    let payload = CommitSummaryCompletedPayload {
        markdown,
        project_count,
        total_commits,
    };

    emit_result(
        format!("Digest composed for {total_commits} commits across {project_count} projects"),
        EventType::CommitSummaryCompleted,
        &project,
        throttle,
        &payload,
    )
}

/// Build the agent prompt. Encodes the four user-confirmed prompt instructions:
/// Canadian English spelling, ## heading per project, bullets ≤ 100 chars
/// ending with the 7-char SHA in parens, one-line synthesis per project,
/// ⚠ Heads-up section at the top for anything that looks risky. Forbids
/// fabricating commits.
fn build_prompt(observed: &CommitsObservedPayload) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are rendering a daily commit digest for a working software \
         developer who reads it each evening. The data below is the only \
         source of truth — do not invent commits, projects, or facts not \
         present in it.\n\n",
    );
    prompt.push_str("Render a markdown digest with these rules:\n");
    prompt.push_str("- Use Canadian English spelling and punctuation throughout.\n");
    prompt.push_str(
        "- If anything in the data looks risky (touches CI, security, \
         release machinery, secrets, infrastructure), open the digest with \
         a `## ⚠ Heads-up` section listing those items.\n",
    );
    prompt.push_str(
        "- Then a `## {project name}` heading for every project (in input \
         order), each followed by a short bullet list — one bullet per \
         commit, ≤ 100 characters, ending with the first 7 characters of \
         the SHA in parentheses.\n",
    );
    prompt.push_str(
        "- After each project's bullets, add a one-line synthesis of what \
         shipped in that project (1-2 sentences max).\n",
    );
    prompt.push_str(
        "- For projects whose data carries an `error` field, render the \
         error inline under the project heading and skip the bullets/synthesis.\n",
    );
    prompt.push_str("- Return only the markdown body — no preamble, no code fences.\n\n");

    prompt.push_str("## Source data\n\n");
    for project in &observed.projects {
        push_project_block(&mut prompt, project);
    }
    prompt
}

fn push_project_block(out: &mut String, project: &ProjectCommits) {
    writeln!(out, "### {}", project.name).expect("write to String never fails");
    writeln!(out, "- branch: `{}`", project.branch).expect("write to String never fails");
    if let Some(err) = &project.error {
        writeln!(out, "- error: {err}\n").expect("write to String never fails");
        return;
    }
    if project.commits.is_empty() {
        out.push_str("- (no commits in window)\n\n");
        return;
    }
    for commit in &project.commits {
        push_commit_line(out, commit);
    }
    out.push('\n');
}

fn push_commit_line(out: &mut String, commit: &CommitInfo) {
    // Carry the full SHA in source data so the agent can render the 7-char
    // prefix in its output without truncation ambiguity. `SHA_PREFIX_LEN`
    // remains the documented prefix length used in the fallback path.
    writeln!(
        out,
        "- `{sha}` — {subject} ({author}, {ts})",
        sha = commit.sha,
        subject = commit.subject,
        author = commit.author,
        ts = commit.timestamp,
    )
    .expect("write to String never fails");
}

/// Fallback digest used when the agent cannot be invoked or fails. Returns a
/// minimal "raw evidence" rendering of the input data so the chain still
/// produces a useful artefact instead of a blank file.
fn fallback_digest(observed: &CommitsObservedPayload, agent_error: Option<&str>) -> String {
    let mut out = String::new();
    if let Some(err) = agent_error {
        writeln!(out, "> ⚠ Agent unavailable; this digest is a raw fallback. ({err})\n")
            .expect("write to String never fails");
    }
    writeln!(
        out,
        "{} commits across {} projects in the last {} hours.\n",
        observed.total_commits(),
        observed.project_count(),
        observed.window_hours,
    )
    .expect("write to String never fails");
    for project in &observed.projects {
        writeln!(out, "## {}", project.name).expect("write to String never fails");
        if let Some(err) = &project.error {
            writeln!(out, "- error: {err}\n").expect("write to String never fails");
            continue;
        }
        if project.commits.is_empty() {
            out.push_str("- no commits in window\n\n");
            continue;
        }
        for commit in &project.commits {
            let sha = commit.sha.chars().take(SHA_PREFIX_LEN).collect::<String>();
            writeln!(out, "- `{sha}` {} — {}", commit.subject, commit.author)
                .expect("write to String never fails");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::gateway::fakes::FakeAgentGateway;

    use super::*;

    fn sample_commit(sha: &str, subject: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            author: "Stacey".to_string(),
            timestamp: "2026-05-28T16:00:00Z".to_string(),
            subject: subject.to_string(),
        }
    }

    fn observed_with_commits() -> CommitsObservedPayload {
        CommitsObservedPayload {
            window_hours: 24,
            projects: vec![ProjectCommits {
                name: "alpha".to_string(),
                branch: "main".to_string(),
                commits: vec![sample_commit("aaaaaaa1111", "first thing")],
                error: None,
            }],
        }
    }

    fn observed_empty() -> CommitsObservedPayload {
        CommitsObservedPayload {
            window_hours: 24,
            projects: vec![ProjectCommits {
                name: "alpha".to_string(),
                branch: "main".to_string(),
                commits: vec![],
                error: None,
            }],
        }
    }

    fn trigger_with(payload: &CommitsObservedPayload) -> Event {
        let json = serde_json::to_value(payload).unwrap();
        Event::new(EventType::CommitsObserved, "system".to_string(), Throttle::Full, json)
    }

    assert_block_meta!(
        SummarizeCommits::new(FakeAgentGateway::success() as Arc<dyn AgentGateway>),
        kind: Observer,
        sinks_on: [CommitsObserved],
    );

    // -----------------------------------------------------------------
    // Empty-day short-circuit
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn empty_observed_payload_short_circuits_and_does_not_call_agent() {
        let agent = FakeAgentGateway::success();
        let block = SummarizeCommits::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let result = block.execute(&trigger_with(&observed_empty())).await.unwrap();
        let payload: CommitSummaryCompletedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(payload.total_commits, 0);
        assert!(payload.markdown.contains("No commits"));
        assert!(payload.markdown.contains("24 hours"));
        assert_eq!(agent.invocations().len(), 0, "empty day must skip the agent call");
    }

    #[tokio::test]
    async fn empty_observed_singular_project_uses_singular_noun() {
        let agent = FakeAgentGateway::success();
        let block = SummarizeCommits::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let payload = CommitsObservedPayload {
            window_hours: 12,
            projects: vec![ProjectCommits {
                name: "only".to_string(),
                branch: "main".to_string(),
                commits: vec![],
                error: None,
            }],
        };
        let result = block.execute(&trigger_with(&payload)).await.unwrap();
        let out: CommitSummaryCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(out.markdown.contains("1 project"));
        assert!(!out.markdown.contains("1 projects"), "should pluralise correctly");
    }

    // -----------------------------------------------------------------
    // Non-empty path — agent gets a prompt with every SHA & project name
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn non_empty_observed_invokes_agent_with_every_commit_in_prompt() {
        let agent = FakeAgentGateway::success_with("## Alpha\n- first thing (aaaaaaa)\n");
        let block = SummarizeCommits::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let result = block.execute(&trigger_with(&observed_with_commits())).await.unwrap();

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1, "exactly one agent call");
        let prompt = &invocations[0].prompt;
        assert!(prompt.contains("alpha"), "project name must appear in prompt");
        assert!(prompt.contains("aaaaaaa1111"), "full SHA must be in prompt");
        assert!(prompt.contains("first thing"), "subject must be in prompt");
        assert!(prompt.contains("Canadian English"), "spelling instruction must be in prompt");
        assert!(
            prompt.contains("Heads-up"),
            "risky-changes section instruction must be in prompt",
        );
        assert!(
            prompt.contains("do not invent commits"),
            "no-fabrication clause must be in prompt",
        );

        let payload: CommitSummaryCompletedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(payload.total_commits, 1);
        assert_eq!(payload.project_count, 1);
        assert_eq!(payload.markdown, "## Alpha\n- first thing (aaaaaaa)");
    }

    #[tokio::test]
    async fn agent_failure_produces_fallback_digest_marked_as_unavailable() {
        let agent = FakeAgentGateway::failure("model overloaded");
        let block = SummarizeCommits::new(Arc::clone(&agent) as Arc<dyn AgentGateway>);
        let result = block.execute(&trigger_with(&observed_with_commits())).await.unwrap();
        let payload: CommitSummaryCompletedPayload = result.events[0].parse_payload().unwrap();
        assert!(
            payload.markdown.contains("Agent unavailable") || payload.markdown.contains("fallback"),
            "fallback should flag the agent failure: got {}",
            payload.markdown
        );
        assert!(payload.markdown.contains("aaaaaaa"), "fallback must include the SHA");
    }
}
