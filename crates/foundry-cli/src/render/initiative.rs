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
}
