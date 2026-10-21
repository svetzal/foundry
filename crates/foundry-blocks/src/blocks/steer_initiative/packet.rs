use std::path::{Component, Path, PathBuf};

use crate::gateway::ShellGateway;
use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::initiative::{
    AuditEvidence, SteeringDecision, SteeringPrecheck, SteeringSnapshot,
};
use foundry_sdk::registry::Registry;
use serde::Deserialize;

pub(super) const PACKET_BYTES: usize = 65_536;
const STAGE_ACTION_EXAMPLE: &str = r#"{"decision":"stage","campaign":{"name":"new-scoped-campaign","project":"registered initiative project","mission":"specific uncovered work","intent_refs":[],"after":[],"context_paths":[],"writable_repositories":[],"done_evidence":[{"kind":"review","statement":"observable behavior"},{"kind":"gate","command":"repository-local mechanical probe","required":true,"artifacts":["existing repository-relative probe"]}],"budget":{"max_cycles":2,"stages":{"formation_prompt_bytes":32768}},"providers":{"formation":null,"execution":null,"review":null},"agent_provider":null},"member":null}"#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SteeringResponse {
    pub action: SteeringDecision,
    #[serde(default)]
    pub audits: Vec<AuditEvidence>,
}

pub(super) fn parse_response(output: &str) -> anyhow::Result<SteeringResponse> {
    if output.len() > PACKET_BYTES {
        anyhow::bail!("steering response exceeds its byte bound");
    }
    let value = super::super::parse_agent_json(output)
        .ok_or_else(|| anyhow::anyhow!("steering agent returned no JSON decision"))?;
    Ok(serde_json::from_value(value)?)
}

pub(super) fn validate_audits(
    audits: &[AuditEvidence],
    snapshot: &SteeringSnapshot,
    repos: &[RepositorySnapshot],
) -> anyhow::Result<()> {
    for audit in audits {
        if audit.paths.is_empty() {
            anyhow::bail!("every audit classification requires checked supporting paths");
        }
        if !snapshot.initiative.intent_refs.contains(&audit.intent_ref) {
            anyhow::bail!("audit names intent outside this initiative");
        }
        let repo = repos.iter().find(|repo| repo.project == audit.project).ok_or_else(|| {
            anyhow::anyhow!("audit project is not a registered initiative project")
        })?;
        for path in &audit.paths {
            checked_evidence_path(&repo.root, path)?;
        }
    }
    Ok(())
}

pub(super) fn prompt(
    snapshot: &SteeringSnapshot,
    store: &CampaignStore,
    repos: &[RepositorySnapshot],
    requested_member: Option<&str>,
    ledger: &str,
    eligibility: &SteeringPrecheck,
    evidence_root: &Path,
) -> anyhow::Result<BoundedPrompt> {
    let initiative = &snapshot.initiative;
    let campaigns: Vec<_> = store.campaigns.iter().filter(|campaign| campaign.initiative.as_deref() == Some(&initiative.name))
        .map(|campaign| serde_json::json!({"name":campaign.name,"project":campaign.project,"mission":campaign.mission,
            "status":campaign.status,"cycles_completed":campaign.cycles_completed,"cycles_landed":campaign.cycles_landed,
            "last_objective":campaign.objective_history.last(),"escalation":campaign.open_escalation,"providers":campaign.providers}))
        .collect();
    let mut findings = Vec::new();
    let mut finding_bytes = 0;
    for finding in store.list_findings(&initiative.name)? {
        let bytes = serde_json::to_vec(&finding)?.len();
        if finding_bytes + bytes > 8192 {
            continue;
        }
        finding_bytes += bytes;
        findings.push(finding);
    }
    let repositories: Vec<_> = repos.iter().map(|repo| serde_json::json!({"project":repo.project,"path":repo.root,
        "trunk":repo.trunk.trim(),"head":repo.head.trim(),"status":compact_evidence(&repo.status,1024,"git status --porcelain --untracked-files=all"),
        "recent_commits":compact_evidence(&repo.recent,2048,"git log -8 --oneline"),
        "diff":compact_evidence(&repo.diff,2048,"git diff HEAD --")})).collect();
    let work_items: Vec<foundry_sdk::work_item::WorkItem> = serde_json::from_str(ledger)?;
    let ledger_view: Vec<_> = work_items.iter().filter(|item| item.state.is_waiting() || item.state.is_open() || item.occupies_slot())
        .map(|item| serde_json::json!({"id":item.id,"project":item.project,"state":item.state,"reason":item.reason,
            "campaign":item.campaign_name(),"source":item.source,"execution_completed":item.execution_completed,"disposition":item.disposition})).collect();
    let integration_results: Vec<_> = initiative
        .done_evidence
        .iter()
        .enumerate()
        .filter_map(|(index, _)| {
            initiative
                .integration_results
                .iter()
                .rev()
                .find(|result| result.gate_index == index)
        })
        .collect();
    let packet = serde_json::json!({"initiative":initiative.name,"outcome":initiative.outcome,"intent_refs":initiative.intent_refs,
        "members":initiative.members,
        "campaigns":campaigns,"projects":repositories,"brief":initiative.brief,
        "owner_decisions":initiative.owner_decisions,"status_notes":initiative.status_notes,"questions":initiative.open_questions,"deferred_questions":initiative.deferred_questions,"budget":initiative.budget,
        "holds":initiative.holds,"milestones":initiative.milestones,"close_proposals":initiative.close_proposals,
        "findings":findings,"requested_member":requested_member,"work_ledger":ledger_view,
        "eligibility":eligibility,
        "done_evidence":initiative.done_evidence,"integration_results":integration_results,
        "integration_skipped":snapshot.integration_skipped,
        "recovery":initiative.recovery,"completion_proposal":initiative.completion_proposal});
    bounded_prompt(&serde_json::to_string(&packet)?, evidence_root, render_prompt)
}

/// The complete steering prompt around a rendered packet. Everything except
/// the packet is fixed text, so its size is known before the packet is placed.
fn render_prompt(packet: &str) -> String {
    let contract_guidance = super::super::agent_helpers::INITIATIVE_CONTRACT_GUIDANCE;
    let test_design_guidance = super::super::agent_helpers::TEST_DESIGN_GUIDANCE;
    let text = format!(
        r#"Choose exactly one next initiative action from current intent and live repository evidence. Take one step, then reassess. You are read-only; do not modify repositories, Foundry state, or Tracker. Foundry has executed integration gates and retained bounded capture evidence in this packet. When integration_skipped is set, Foundry did not integrate this pass, for the reason given, and integration_results predate the current trunks. Inspect the registered repository paths and Foundry-owned brief yourself. All owner decisions are binding. Status notes are observations, not binding instructions; they do not answer questions or authorize scope, budget, or scheduling changes. An explicit requested_member restricts advance to that member, but never overrides audit, budget, dependencies, questions, repository ownership, or terminal state.

Before any derived work advances, audit EVERY current intent candidate: create looks for partial implementation, update compares revised intent with code, delete finds removals and references. Classify realised, partial, absent, or misplaced. Cite project-relative existing paths (a checked directory may support absence). Realised intent is proposed for owner Tracker closure and must not become redundant work. Do not infer named milestones from unrelated landings. Approved new intent becomes a candidate; discarded intent has been cancelled. Relay useful findings explicitly, and ask one question covering every applicable member when owner judgement is needed. Respect max_open_questions: when the surface is full, advance an eligible member that has no unanswered question, or escalate waiting on owner if nothing can move. Deferred questions still block their applicable members.

Foundry computed the packet's eligibility section from the store and the work ledger. Its readiness array gives exact typed blockers and dependency predicates with the evidence that satisfies each. A bare prerequisite member requires that campaign's completed outcome; a partial landed cycle never satisfies it. member:landed explicitly waits for the first verified landing. member:completed explicitly waits for completion, and member:custom-name requires that exact recorded milestone proof. Completed is the campaign outcome, not deployment or publication evidence; declare an exact custom milestone when publication is required. Realised candidate audit and completed task receipts provide their corresponding intent evidence. Conditional holds release mechanically once every exact until predicate is satisfied, including owner-declared conditional holds. Indefinite owner holds remain binding until released. Waiting for dependency evidence consumes no cycles and opens no owner question. Choose wait when no independent eligible work should proceed, and ask only for an actual owner policy choice.

Foundry computed the eligibility section from those same diagnostics. eligible lists the members that can advance now; blocked lists every other member with the reason; occupied lists the members using the max_active_members slots; dispatch_open says whether a new task can start. Advance only an eligible member. A hold on a member that already has an owner hold, or a relay to members a finding already reaches, changes nothing. When nothing should move until running work settles, choose wait and say what the initiative waits for. Use ask only for a question the owner must answer, never to wait for workers to finish. You may wait while members are eligible when the mission requires it; Foundry records your reason beside the eligible list.

{contract_guidance}

{test_design_guidance}

Return exactly one JSON object with action and audits. Action is exactly one tagged decision:
advance: {{"decision":"advance","member":"existing member or audited candidate","project":null}}
dispatch: {{"decision":"dispatch","project":"registered initiative project","objective":"one concrete task","member":null}}
hold: {{"decision":"hold","member":"member","until":["prerequisite:landed"],"reason":"reason"}}
relay: {{"decision":"relay","finding":{{full finding record from packet}},"to":["member"]}}
ask: {{"decision":"ask","question":"one question","members":["member"]}}
escalate: {{"decision":"escalate","reason":"reason"}}
recover: {{"decision":"recover","kind":"capability_unavailable|access_unavailable|environment_unavailable|missing_evidence|unmet_precondition|runner_unavailable","detail":"exact operational prerequisite"}}
wait: {{"decision":"wait","reason":"what the initiative waits for"}}
stage: {STAGE_ACTION_EXAMPLE}
done: {{"decision":"done","reviews":[{{"statement":"exact initiative review statement","affirmed":true,"evidence":["checked project-relative path or gate result id"]}}]}}
Assign a project on advance only to activate an existing intent candidate as a campaign; set dispatch.member only to activate an existing candidate as a task. Stage adds one campaign when uncovered outcome work requires it, and starts no execution. Done requires every required gate to have passed on current trunk identities and every initiative review statement affirmed from checked evidence. A done decision proposes completion for the owner's initiative complete confirmation; it never completes the initiative or closes Tracker itself. If evidence is missing, choose work, stage it, or record typed recovery with the exact prerequisite. Use recover for operational prerequisites that need rechecking, never ask the owner merely because a worker or prerequisite has not settled. Recover records no policy, grants no authority and does not rerun itself. Use existing exact finding records for relay. Audits are records {{"intent_ref":"et:interaction:id","classification":"realised|partial|absent|misplaced","project":"registered project","paths":["relative/path"],"explanation":"checked evidence"}}.

The stage example shows the complete allowed campaign declaration. Optional fields may be omitted: intent_refs, after, context_paths, writable_repositories, budget, providers and agent_provider. Provider stages omitted or null inherit initiative policy. Set the cycle limit ONLY at campaign.budget.max_cycles, with any stage limits inside campaign.budget.stages. Do not flatten max_cycles into campaign, and do not return execution state such as cycles_completed, cycles_landed, status or history. The declaration rejects unknown fields. Choose review evidence, gate evidence, or both as appropriate. A campaign gate has command, required and artifacts; it runs inside its one repository and has no host field. The campaign declaration's after names prerequisite initiative members. To stage an existing intent candidate, set action.member to its exact name; otherwise leave member null.

STEERING PACKET:
{packet}
"#
    );
    super::super::with_single_turn_discipline(&text)
}

pub(super) struct BoundedPrompt {
    pub text: String,
    pub artifact: Option<(PathBuf, String)>,
}

/// Place the packet inline when the whole rendered prompt fits
/// [`PACKET_BYTES`]; otherwise export it to a hashed artifact and place the
/// reference instead.
///
/// The decision measures the actual rendered prompt, fixed text included,
/// against the real limit. A constant margin for the fixed text went stale as
/// guidance grew: with about 10 KB of fixed text against an 8 KB margin, a
/// packet just under the margin threshold stayed inline and the steer failed
/// on the limit instead of exporting.
fn bounded_prompt(
    content: &str,
    evidence_root: &Path,
    render: impl Fn(&str) -> String,
) -> anyhow::Result<BoundedPrompt> {
    let inline = render(content);
    if inline.len() <= PACKET_BYTES {
        return Ok(BoundedPrompt {
            text: inline,
            artifact: None,
        });
    }
    let (reference, artifact) = export_packet(content, evidence_root)?;
    let text = render(&serde_json::to_string(&reference)?);
    if text.len() > PACKET_BYTES {
        anyhow::bail!(
            "steering packet exceeds {PACKET_BYTES} bytes; owner policy was not truncated"
        );
    }
    Ok(BoundedPrompt {
        text,
        artifact: Some(artifact),
    })
}

fn export_packet(
    content: &str,
    evidence_root: &Path,
) -> anyhow::Result<(serde_json::Value, (PathBuf, String))> {
    let root = evidence_root.join("initiatives").join("steering-evidence");
    std::fs::create_dir_all(&root)?;
    let path = root.join(format!("{}.json", uuid::Uuid::new_v4()));
    let hash = context_hash(content);
    std::fs::write(&path, content)?;
    let path = path.canonicalize()?;
    Ok((
        serde_json::json!({"context_artifact":{
            "path":path,"sha256":hash,"bytes":content.len(),
            "instruction":"Read this entire Foundry-exported selected context before choosing. It contains all binding owner policy, members, questions, current evidence and repository references. No prior steering prompts are included. Verify SHA-256 before using it."
        }}),
        (path, hash),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RepositorySnapshot {
    pub project: String,
    pub root: PathBuf,
    pub registry: String,
    pub trunk: String,
    pub head: String,
    pub status: String,
    pub diff: String,
    pub recent: String,
}

async fn git(shell: &dyn ShellGateway, root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = shell.run(root, "git", args, None, None).await?;
    if !output.success {
        anyhow::bail!("steering cannot inspect Git evidence: {}", output.stderr);
    }
    Ok(output.stdout)
}

pub(super) async fn repositories(
    registry: &Registry,
    projects: &[String],
    shell: &dyn ShellGateway,
) -> anyhow::Result<Vec<RepositorySnapshot>> {
    let mut snapshots = Vec::new();
    for project in projects {
        let entry = registry
            .find_project(project)
            .ok_or_else(|| anyhow::anyhow!("initiative project '{project}' is not registered"))?;
        let root = PathBuf::from(&entry.path).canonicalize()?;
        let branch = format!("refs/heads/{}", entry.branch);
        snapshots.push(RepositorySnapshot {
            project: project.clone(),
            registry: serde_json::to_string(entry)?,
            trunk: git(shell, &root, &["rev-parse", "--verify", "--end-of-options", &branch])
                .await?,
            head: git(shell, &root, &["rev-parse", "--verify", "HEAD"]).await?,
            status: git(shell, &root, &["status", "--porcelain", "--untracked-files=all"]).await?,
            diff: git(shell, &root, &["diff", "HEAD", "--"]).await?,
            recent: git(shell, &root, &["log", "-8", "--oneline", &branch, "--"]).await?,
            root,
        });
    }
    Ok(snapshots)
}

pub(super) fn checked_evidence_path(root: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    let relative = Path::new(relative);
    if relative.components().any(|component| {
        matches!(component, Component::Prefix(_) | Component::RootDir | Component::ParentDir)
    }) {
        anyhow::bail!("audit reference must be relative to its registered project");
    }
    let root = root.canonicalize()?;
    let path = root.join(relative).canonicalize()?;
    if !path.starts_with(&root) {
        anyhow::bail!("audit reference escapes its registered project");
    }
    Ok(path)
}

pub(super) fn compact_evidence(text: &str, limit: usize, command: &str) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[{} bytes omitted; inspect complete evidence with `{command}`]",
        &text[..end],
        text.len() - end
    )
}

pub(super) fn context_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_receives_contract_guidance_without_a_brief_or_findings() {
        let fixture = super::super::tests::Fixture::new();
        let mut store = CampaignStore::load(&fixture.store).unwrap();
        store
            .record_initiative_status("shared", "Verification is running", chrono::Utc::now())
            .unwrap();
        let snapshot = store.begin_steering("shared", "contract-guidance").unwrap();
        assert!(snapshot.initiative.brief.is_none());
        assert!(
            store.list_findings("shared").unwrap().is_empty(),
            "expected `store.list_findings(\"shared\").unwrap()` to be empty: {:?}",
            store.list_findings("shared").unwrap()
        );
        let eligibility = store.steering_precheck("shared", &[], &[]).unwrap();
        let prompt =
            prompt(&snapshot, &store, &[], None, "[]", &eligibility, fixture.temp.path()).unwrap();
        assert!(
            prompt
                .text
                .contains(super::super::super::agent_helpers::INITIATIVE_CONTRACT_GUIDANCE)
        );
        // Steering writes dispatch objectives and staged missions and judges
        // test evidence for done, so it applies the same test-design rule.
        assert!(prompt.text.contains(super::super::super::agent_helpers::TEST_DESIGN_GUIDANCE));
        // Steering can ask the owner and escalate, so it carries the seam
        // pre-authorization that says neither is warranted for a seam.
        for phrase in super::super::super::agent_helpers::SEAM_AUTHORIZATION_PHRASES {
            assert!(prompt.text.contains(phrase), "missing {phrase}");
        }
        assert!(prompt.text.contains("Verification is running"));
        assert!(prompt.text.contains("Status notes are observations, not binding instructions"));
    }

    #[test]
    fn staging_prompt_demonstrates_the_typed_nested_budget_and_optional_declaration_fields() {
        let fixture = super::super::tests::Fixture::new();
        let mut store = CampaignStore::load(&fixture.store).unwrap();
        let snapshot = store.begin_steering("shared", "stage-schema").unwrap();
        let eligibility = store.steering_precheck("shared", &[], &[]).unwrap();
        let prompt =
            prompt(&snapshot, &store, &[], None, "[]", &eligibility, fixture.temp.path()).unwrap();
        let example = prompt.text.lines().find_map(|line| line.strip_prefix("stage: ")).unwrap();
        let response = parse_response(&format!(r#"{{"action":{example},"audits":[]}}"#)).unwrap();
        let SteeringDecision::Stage { campaign, member } = response.action else {
            panic!("stage example must parse as staging")
        };
        assert_eq!(campaign.budget.max_cycles, 2);
        assert_eq!(campaign.budget.stages.formation_prompt_bytes, 32768);
        assert!(member.is_none());
        let value: serde_json::Value = serde_json::from_str(example).unwrap();
        for field in [
            "intent_refs",
            "after",
            "context_paths",
            "writable_repositories",
            "providers",
            "agent_provider",
        ] {
            assert!(value["campaign"].get(field).is_some(), "missing {field}");
        }
        assert!(value["campaign"].get("max_cycles").is_none());
        assert!(prompt.text.contains("campaign.budget.max_cycles"));
        assert!(prompt.text.contains("Do not flatten max_cycles"));
        assert!(prompt.text.contains("cycles_completed, cycles_landed, status or history"));
    }

    #[test]
    fn evidence_must_name_an_existing_path_inside_the_registered_checkout() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("contract.rs"), "contract").unwrap();
        assert_eq!(
            checked_evidence_path(repo.path(), "contract.rs").unwrap(),
            repo.path().join("contract.rs").canonicalize().unwrap()
        );
        assert!(checked_evidence_path(repo.path(), "missing.rs").is_err());
        assert!(checked_evidence_path(repo.path(), "../contract.rs").is_err());
        assert!(checked_evidence_path(repo.path(), "/tmp/outside.rs").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_evidence_symlink_cannot_relabel_a_sibling_checkout() {
        let repo = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        std::fs::write(sibling.path().join("contract.rs"), "contract").unwrap();
        std::os::unix::fs::symlink(sibling.path(), repo.path().join("sibling")).unwrap();
        assert!(checked_evidence_path(repo.path(), "sibling/contract.rs").is_err());
        assert!(checked_evidence_path(repo.path(), ".").is_ok());
    }

    #[test]
    fn evidence_compaction_preserves_utf8_and_directs_the_agent_to_complete_evidence() {
        let text = "éééé";
        let result = compact_evidence(text, 5, "git log -8 --oneline");
        assert!(result.starts_with("éé"));
        assert!(result.contains("4 bytes omitted"));
        assert!(result.contains("git log -8 --oneline"));
        assert_eq!(compact_evidence("complete", 20, "git log"), "complete");
    }

    #[test]
    fn oversized_policy_uses_a_hashed_selected_context_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let policy = "Binding owner decision ".repeat(6000);
        let packet = serde_json::json!({"owner_decisions":[policy],"members":["one","two"]});
        let content = serde_json::to_string(&packet).unwrap();
        let bounded = bounded_prompt(&content, directory.path(), render_prompt).unwrap();
        assert!(bounded.text.len() <= PACKET_BYTES);
        let (path, hash) = bounded.artifact.unwrap();
        let exported = std::fs::read_to_string(&path).unwrap();
        assert_eq!(context_hash(&exported), hash);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&exported).unwrap(), packet);
        assert!(bounded.text.contains("Read this entire Foundry-exported selected context"));
        assert!(bounded.text.contains(&hash));
        assert!(!bounded.text.contains(&policy));
    }

    /// A JSON packet whose compact form is exactly `bytes` long.
    fn packet_of(bytes: usize) -> String {
        let overhead = serde_json::to_string(&serde_json::json!({"owner_decisions":[""]}))
            .unwrap()
            .len();
        let content = serde_json::to_string(
            &serde_json::json!({"owner_decisions":["x".repeat(bytes - overhead)]}),
        )
        .unwrap();
        assert_eq!(content.len(), bytes);
        content
    }

    /// Regression: the export decision used a constant 8 KB margin for the
    /// fixed prompt text, which had grown past 10 KB. A packet just under the
    /// old `PACKET_BYTES - 8192` threshold stayed inline and the steer failed
    /// on the limit. mojentic-3-0 steering prompts reached 63,891 bytes on
    /// 2026-10-17, inside that window once the test-design rule was added.
    #[test]
    fn a_packet_just_under_the_old_margin_threshold_is_exported_instead_of_failing() {
        let directory = tempfile::tempdir().unwrap();
        let content = packet_of(PACKET_BYTES - 8192 - 64);
        assert!(
            render_prompt(&content).len() > PACKET_BYTES,
            "the fixture must reproduce a prompt that cannot fit inline"
        );
        let bounded = bounded_prompt(&content, directory.path(), render_prompt).unwrap();
        assert!(bounded.text.len() <= PACKET_BYTES);
        let (path, _) = bounded.artifact.expect("the packet must be exported");
        assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    }

    #[test]
    fn the_packet_stays_inline_exactly_while_the_whole_prompt_fits_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let fixed = render_prompt("").len();

        let fits = packet_of(PACKET_BYTES - fixed);
        let inline = bounded_prompt(&fits, directory.path(), render_prompt).unwrap();
        assert!(inline.artifact.is_none());
        assert_eq!(inline.text.len(), PACKET_BYTES);
        assert!(inline.text.contains(&fits));

        let one_over = packet_of(PACKET_BYTES - fixed + 1);
        let exported = bounded_prompt(&one_over, directory.path(), render_prompt).unwrap();
        assert!(exported.artifact.is_some());
        assert!(exported.text.len() <= PACKET_BYTES);
    }

    #[test]
    fn a_prompt_that_cannot_fit_even_with_an_exported_packet_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let padded = |packet: &str| format!("{}{packet}", "p".repeat(PACKET_BYTES));
        let Err(error) = bounded_prompt(&packet_of(64), directory.path(), padded) else {
            panic!("a prompt over the limit with an exported packet must be refused");
        };
        assert!(error.to_string().contains("owner policy was not truncated"), "{error}");
    }

    #[test]
    fn a_steer_requires_typed_outcome_actions_and_rejects_multiple_actions() {
        assert!(
            parse_response(r#"{"action":{"decision":"done","reason":"complete"},"audits":[]}"#)
                .is_err()
        );
        assert!(
            parse_response(r#"{"action":{"decision":"stage","name":"new"},"audits":[]}"#).is_err()
        );
        assert!(
            parse_response(r#"{"action":{"decision":"done","reviews":[]},"audits":[]}"#).is_ok()
        );
        assert!(
            parse_response(
                r#"{"action":{"decision":"ask","question":"which?"},"second_action":{}}"#
            )
            .is_err()
        );
        assert!(
            parse_response(r#"{"action":{"decision":"ask","question":"which?"},"audits":[]}"#)
                .is_ok()
        );
    }
}
