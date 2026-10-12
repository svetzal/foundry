//! Human and JSON output from the same typed initiative response.

use std::fmt::Write;

use crate::proto::{Finding, Initiative};

pub fn advance(name: &str, response: &crate::proto::AdvanceInitiativeResponse) -> String {
    if let Some(in_flight) = &response.in_flight {
        format!(
            "Initiative '{name}' already has a steer {} ({}). Nothing dispatched.\n",
            in_flight.event_id, in_flight.state
        )
    } else {
        format!(
            "Requested steer for initiative '{name}'. Event: {}\nInspect with: foundry initiative show {name}\n",
            response.event_id
        )
    }
}

pub fn owner_action(name: &str, event_id: &str) -> String {
    if event_id.is_empty() {
        format!("Recorded owner action for initiative '{name}'.\n")
    } else {
        format!("Recorded owner action for initiative '{name}'. Event: {event_id}\n")
    }
}

pub fn detail(initiative: &Initiative) -> String {
    let mut out = format!(
        "Initiative '{}' ({})\nOutcome: {}\n",
        initiative.name, initiative.status, initiative.outcome
    );
    if !initiative.intent_refs.is_empty() {
        let _ = writeln!(out, "Intent: {}", initiative.intent_refs.join(", "));
    }
    let _ = writeln!(out, "Projects: {}", initiative.projects.join(", "));
    if let Some(brief) = &initiative.brief {
        let _ = writeln!(out, "Brief: {} (sha256 {})", brief.path, brief.hash);
    }
    if let Some(budget) = &initiative.budget {
        let _ = writeln!(
            out,
            "Budget: {} cycles, {} active members, {} open questions",
            budget.max_cycles, budget.max_active_members, budget.max_open_questions
        );
    }
    if let Some(providers) = &initiative.providers {
        let _ = writeln!(
            out,
            "Providers: formation {}, execution {}, review {}",
            providers.formation.as_deref().unwrap_or("inherited"),
            providers.execution.as_deref().unwrap_or("inherited"),
            providers.review.as_deref().unwrap_or("inherited")
        );
    }
    if !initiative.done_evidence.is_empty() {
        out.push_str("Done evidence:\n");
        for evidence in &initiative.done_evidence {
            let statement = if evidence.kind == "gate" {
                &evidence.command
            } else {
                &evidence.statement
            };
            let _ = writeln!(out, "  {}: {statement}", evidence.kind);
        }
    }
    render_steering(&mut out, initiative);
    render_outcome(&mut out, initiative);
    out.push_str("\nMembers:\nNAME  PROJECT  STATUS  CYCLES USED/LANDED  LAST VERDICT\n");
    for member in &initiative.members {
        if let Some(campaign) = &member.campaign {
            let _ = writeln!(
                out,
                "{}  {}  {}  {}/{}  {}",
                member.name,
                campaign.project,
                campaign.status,
                campaign.cycles_completed,
                campaign.cycles_landed,
                member.last_verdict.as_deref().unwrap_or("-")
            );
        } else {
            let _ = writeln!(out, "{}  -  {}  -  -", member.name, member.kind);
        }
        if let Some(description) = &member.description {
            let _ = writeln!(out, "  {description}");
        }
        if let Some(intent) = &member.intent_ref {
            let _ = writeln!(out, "  Intent: {intent}");
        }
        for gap in &member.reviewer_gaps {
            let _ = writeln!(out, "  Review gap: {gap}");
        }
        if !member.after.is_empty() {
            let _ = writeln!(out, "  After: {}", member.after.join(", "));
        }
    }
    render_questions(&mut out, initiative);
    if !initiative.owner_decisions.is_empty() {
        out.push_str("\nOwner decisions:\n");
        for decision in &initiative.owner_decisions {
            let _ = writeln!(
                out,
                "{}: {} (members: {})",
                decision.authorized_by,
                decision.decision,
                decision.members.join(", ")
            );
        }
    }
    out
}

pub fn completion(initiative: &Initiative) -> String {
    let mut out = format!("Initiative '{}' is {}.\n", initiative.name, initiative.status);
    for receipt in &initiative.completion_receipts {
        let _ = writeln!(
            out,
            "Tracker reality verified: {} ({})",
            receipt.intent_ref, receipt.completed_at
        );
    }
    out
}

fn render_outcome(out: &mut String, initiative: &Initiative) {
    for result in &initiative.integration_results {
        let _ = writeln!(
            out,
            "Integration gate {}: {} on {} ({})",
            result.gate_index + 1,
            if result.passed { "passed" } else { "failed" },
            result.host,
            result.recorded_at
        );
        let _ = writeln!(out, "  Command: {}", result.command);
        let _ = writeln!(out, "  Capture: {}", result.report);
        if !result.artifacts.is_empty() {
            let _ = writeln!(out, "  Artifacts: {}", result.artifacts.join(", "));
        }
    }
    if let Some(proposal) = &initiative.completion_proposal {
        let _ = writeln!(out, "Outcome review: {} ({})", proposal.id, proposal.proposed_at);
        for review in &proposal.reviews {
            let _ = writeln!(
                out,
                "  {}: {}",
                if review.affirmed {
                    "affirmed"
                } else {
                    "not affirmed"
                },
                review.statement
            );
            let _ = writeln!(out, "  Evidence: {}", review.evidence.join(", "));
        }
        if initiative.status != "completed" && initiative.status != "cancelled" {
            if let Some(confirmation) = &initiative.completion_confirmation {
                let _ = writeln!(
                    out,
                    "Owner confirmation: {} ({})",
                    confirmation.reason, confirmation.confirmed_at
                );
                if let Some(error) = &confirmation.last_error {
                    let _ = writeln!(out, "Completion held: {error}");
                }
                let _ = writeln!(
                    out,
                    "Retry: foundry initiative complete {} --reason '<confirmation>'",
                    initiative.name
                );
            } else {
                out.push_str("Outcome awaits owner confirmation.\n");
                let _ = writeln!(
                    out,
                    "Confirm: foundry initiative complete {} --reason '<verified outcome>'",
                    initiative.name
                );
            }
        }
    }
    for receipt in &initiative.completion_receipts {
        let _ = writeln!(
            out,
            "Tracker reality verified: {} ({})",
            receipt.intent_ref, receipt.completed_at
        );
    }
}

fn render_steering(out: &mut String, initiative: &Initiative) {
    let _ = writeln!(
        out,
        "Automatic steering: {}",
        if initiative.steering_enabled {
            "enabled"
        } else {
            "awaiting first advance"
        }
    );
    if let Some(source) = &initiative.derive_source {
        let _ = writeln!(out, "Tracker source: {} in {}", source.from, source.repo);
    }
    for hold in &initiative.holds {
        let until = if hold.until.is_empty() {
            "owner release".into()
        } else {
            hold.until.join(", ")
        };
        let _ = writeln!(out, "Hold: {} until {} ({})", hold.member, until, hold.reason);
    }
    if let Some(steer) = initiative.steering_history.last() {
        let _ = writeln!(out, "Last steer: {} ({})", steer.id, steer.recorded_at);
        let _ = writeln!(out, "Decision: {}", steer.decision_json);
    }
    for proposal in &initiative.close_proposals {
        let _ = writeln!(
            out,
            "Tracker close proposed: {} (evidence: {})",
            proposal.intent_ref,
            proposal.evidence.join(", ")
        );
    }
}

fn render_questions(out: &mut String, initiative: &Initiative) {
    out.push_str("\nOpen questions:\n");
    if initiative.open_questions.is_empty() {
        out.push_str("(none)\n");
    }
    for question in &initiative.open_questions {
        let _ = writeln!(
            out,
            "{} (members: {}; opened {})",
            question.question,
            question.members.join(", "),
            question.opened_at
        );
        let _ = write!(
            out,
            "  next: foundry initiative decide {} --decision '<answer>'",
            initiative.name
        );
        for member in &question.members {
            let _ = write!(out, " --member {member}");
        }
        let _ = writeln!(out);
    }
}

pub fn list(initiatives: &[Initiative]) -> String {
    if initiatives.is_empty() {
        return "No initiatives configured.\n".into();
    }
    let mut out = "NAME  STATUS  MEMBERS  OUTCOME\n".to_string();
    for initiative in initiatives {
        let _ = writeln!(
            out,
            "{}  {}  {}  {}",
            initiative.name,
            initiative.status,
            initiative.members.len(),
            initiative.outcome
        );
    }
    out
}

pub fn findings(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "No findings recorded.\n".into();
    }
    let mut out = String::new();
    for finding in findings {
        let _ = writeln!(
            out,
            "{}  {}  {}  to: {}\n  {}",
            finding.id,
            finding.initiative,
            finding.kind,
            finding.to.join(", "),
            finding.text
        );
        if !finding.refs.is_empty() {
            let _ = writeln!(out, "  Refs: {}", finding.refs.join(", "));
        }
        for consumption in &finding.consumed_by {
            let _ =
                writeln!(out, "  Consumed by {} in {}", consumption.member, consumption.work_item);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_question_names_all_applicable_members_and_its_answer_command() {
        let initiative = Initiative {
            name: "shared".into(),
            outcome: "A shared result".into(),
            open_questions: vec![crate::proto::InitiativeQuestion {
                question: "Which contract?".into(),
                members: vec!["a".into(), "b".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let rendered = detail(&initiative);
        assert_eq!(rendered.matches("Which contract?").count(), 1);
        assert!(
            rendered
                .contains("initiative decide shared --decision '<answer>' --member a --member b")
        );
    }

    #[test]
    fn outcome_rendering_names_the_gate_failure_owner_step_and_partial_tracker_receipts() {
        let initiative = Initiative {
            name: "shared".into(),
            status: "active".into(),
            integration_results: vec![crate::proto::InitiativeIntegrationResult {
                gate_index: 1,
                host: "build-host".into(),
                command: "./check-contract".into(),
                report: "capture/contract.md".into(),
                passed: false,
                ..Default::default()
            }],
            completion_proposal: Some(crate::proto::InitiativeCompletionProposal {
                id: "done_1".into(),
                reviews: vec![crate::proto::InitiativeOutcomeReview {
                    statement: "The owner can use the contract".into(),
                    affirmed: true,
                    evidence: vec!["capture/contract.md".into()],
                }],
                ..Default::default()
            }),
            completion_confirmation: Some(crate::proto::InitiativeCompletionConfirmation {
                reason: "Owner inspected the contract".into(),
                last_error: Some("Tracker unavailable".into()),
                ..Default::default()
            }),
            completion_receipts: vec![crate::proto::InitiativeEtCloseReceipt {
                intent_ref: "et:interaction:contract".into(),
                completed_at: "now".into(),
                proposal_id: "done_1".into(),
            }],
            ..Default::default()
        };
        let out = detail(&initiative);
        assert!(out.contains("Integration gate 2: failed on build-host"));
        assert!(out.contains("Capture: capture/contract.md"));
        assert!(out.contains("affirmed: The owner can use the contract"));
        assert!(out.contains("Completion held: Tracker unavailable"));
        assert!(out.contains("Retry: foundry initiative complete shared"));
        assert!(out.contains("Tracker reality verified: et:interaction:contract"));
        assert!(!completion(&initiative).contains("Completed initiative"));
    }

    #[test]
    fn reviewed_outcome_explicitly_awaits_owner_confirmation() {
        let initiative = Initiative {
            name: "shared".into(),
            status: "active".into(),
            completion_proposal: Some(crate::proto::InitiativeCompletionProposal::default()),
            ..Default::default()
        };
        assert!(detail(&initiative).contains("Outcome awaits owner confirmation."));
        assert!(detail(&initiative).contains("Confirm: foundry initiative complete shared"));
    }
}
