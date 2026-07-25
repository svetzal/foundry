//! `RemediateSupplyChain` — branched remediation step of the supply-chain
//! formation (EXP-003 Phase 2, Slice 2).
//!
//! Sinks on `SupplyChainScanned`. Sits between the scan and the digest: it
//! triages every live finding by *fix availability* and emits
//! `SupplyChainRemediated`, carrying the scan through verbatim so the digest
//! still renders its findings/lapsed/accepted/not-scanned sections.
//!
//! ## Triage classifier (always)
//!
//! Every live finding is classified against the bright line the Maintenance
//! triage framework uses: a *populated* `fix_version` → mechanically
//! **fixable**; an *empty* one → a **policy call** (an exploitability judgement
//! about our usage that stays human). This is read-only and always runs.
//!
//! ## Auto-fix engine (gated dark)
//!
//! When — and only when — remediation is explicitly enabled
//! (`FOUNDRY_SUPPLY_CHAIN_REMEDIATE` truthy) *and* the throttle permits mutation
//! (`Full`), the block attempts to fix each fixable finding. The engine is
//! inert by default: with the gate off, or under `dry_run`, it is byte-for-byte
//! the classifier above (`remediated_count = 0`, no `outcomes`).
//!
//! Every fix runs the verify-and-rollback rail, all reversible (commit-only,
//! never pushed):
//! 1. refuse to touch a project whose working tree is not clean;
//! 2. apply the stack-specific fix (Cargo precise update, npm/bun lock update
//!    or manifest rewrite, or uv requirement/lock update);
//! 3. re-run the repo's own gates;
//! 4. on a passing required-gate set → commit only the touched dependency
//!    files; otherwise restore those files from HEAD.
//!
//! Committing each applied fix immediately means a later finding's rollback
//! cannot clobber an earlier success. TypeScript and Python fixers rewrite a
//! matching direct or override requirement before refreshing the lockfile;
//! transitive fixes target the audit tool's explicit `fix_package` when present.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    RemediationOutcome, SupplyChainFinding, SupplyChainRemediatedPayload, SupplyChainScannedPayload,
};
use foundry_sdk::registry::{ProjectEntry, Registry, Stack};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;

use crate::gateway::ShellGateway;

/// Triages each live supply-chain finding and, when explicitly enabled, applies
/// verified, reversible auto-fixes. Observer — it self-gates mutation on the
/// throttle (like the digest writers) rather than relying on the dry-run hook,
/// so the classifier event is always emitted and the chain always reaches the
/// digest.
pub struct RemediateSupplyChain {
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
    enabled: bool,
}

impl RemediateSupplyChain {
    pub fn new(shell: Arc<dyn ShellGateway>, registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            shell,
            enabled: remediation_enabled_from_env(),
        }
    }

    #[cfg(test)]
    fn with_enabled(
        shell: Arc<dyn ShellGateway>,
        registry: Arc<RwLock<Registry>>,
        enabled: bool,
    ) -> Self {
        Self {
            registry,
            shell,
            enabled,
        }
    }
}

/// The auto-fix engine ships dark: it acts only when this env var is truthy,
/// so it stays inert on every install until deliberately switched on — even
/// under `Full` throttle.
fn remediation_enabled_from_env() -> bool {
    std::env::var("FOUNDRY_SUPPLY_CHAIN_REMEDIATE").is_ok_and(|v| {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

impl TaskBlock for RemediateSupplyChain {
    task_block_meta! {
        name: "Remediate Supply Chain",
        kind: Observer,
        sinks_on: [SupplyChainScanned],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let scan = parse_payload!(trigger, SupplyChainScannedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let (fixable_count, no_fix_count) = classify(&scan);
        let mutate = self.enabled && throttle.permits_mutation();

        if !mutate {
            // Classifier-only path: identical whether the gate is off or the
            // throttle is dry-run. No mutation, no outcomes.
            tracing::info!(
                finding_count = scan.finding_count,
                fixable_count,
                no_fix_count,
                enabled = self.enabled,
                "supply-chain remediation triage (classifier only — mutation gated off)"
            );
            return Box::pin(async move {
                emit_remediated(&project, throttle, &scan, fixable_count, no_fix_count, vec![])
            });
        }

        // Snapshot the active project list under a short-lived read guard so the
        // lock is never held across an `.await`.
        let entries: Vec<ProjectEntry> = match super::read_registry(&self.registry) {
            Ok(guard) => guard.active_projects().into_iter().cloned().collect(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let mut outcomes = Vec::new();
            for proj in &scan.projects {
                remediate_project(proj, &entries, shell.as_ref(), &mut outcomes).await;
            }
            let applied = outcomes.iter().filter(|o| o.status == "applied").count();
            tracing::info!(
                applied,
                attempted = outcomes.len(),
                "supply-chain remediation complete"
            );
            emit_remediated(&project, throttle, &scan, fixable_count, no_fix_count, outcomes)
        })
    }
}

/// Pre-I/O routing decision for a single project's remediation attempt.
#[derive(Debug, PartialEq)]
enum ProjectRemediationPlan {
    /// No fixable findings in this project's scan; nothing to attempt.
    NothingFixable,
    /// The project has fixable findings but is not registered; skip.
    NotInRegistry,
    /// The project's stack has no auto-fix mechanism.
    NoFixer { stack: String },
    /// All pre-I/O checks pass; proceed to gate-verify-and-apply at this path.
    Proceed { path: PathBuf, stack: Stack },
}

/// Decide the pre-I/O remediation route for a single project scan.
///
/// Pure function — no filesystem or shell I/O; exercised directly in unit
/// tests. Covers: empty fixable set, missing registry entry, unsupported stack,
/// and the proceed path. Gate-file existence and tree-cleanliness are genuinely
/// I/O-bound and remain in the imperative shell (`remediate_project`).
fn plan_project_remediation(
    proj: &foundry_sdk::payload::ProjectSupplyChainScan,
    entries: &[ProjectEntry],
) -> ProjectRemediationPlan {
    let has_fixable = proj.findings.iter().any(|f| f.fix_version.is_some());
    if !has_fixable {
        return ProjectRemediationPlan::NothingFixable;
    }

    let Some(entry) = entries.iter().find(|e| e.name == proj.project) else {
        return ProjectRemediationPlan::NotInRegistry;
    };

    if !super::supply_chain_fixers::supports(&entry.stack) {
        return ProjectRemediationPlan::NoFixer {
            stack: entry.stack.to_string(),
        };
    }

    ProjectRemediationPlan::Proceed {
        path: PathBuf::from(&entry.path),
        stack: entry.stack.clone(),
    }
}

/// Count live findings split into fixable (a populated fix version) vs
/// policy-call (none).
fn classify(scan: &SupplyChainScannedPayload) -> (u64, u64) {
    let mut fixable = 0u64;
    let mut no_fix = 0u64;
    for proj in &scan.projects {
        for finding in &proj.findings {
            if finding.fix_version.is_some() {
                fixable += 1;
            } else {
                no_fix += 1;
            }
        }
    }
    (fixable, no_fix)
}

/// Attempt to remediate one project's fixable findings, appending an outcome
/// for each. Never panics; every failure mode becomes a recorded outcome.
async fn remediate_project(
    proj: &foundry_sdk::payload::ProjectSupplyChainScan,
    entries: &[ProjectEntry],
    shell: &dyn ShellGateway,
    outcomes: &mut Vec<RemediationOutcome>,
) {
    let fixable: Vec<&SupplyChainFinding> =
        proj.findings.iter().filter(|f| f.fix_version.is_some()).collect();

    // Consult the pure routing plan for all decisions that need no I/O.
    let (path, stack) = match plan_project_remediation(proj, entries) {
        ProjectRemediationPlan::NothingFixable => return,
        ProjectRemediationPlan::NotInRegistry => {
            push_each(outcomes, proj, &fixable, "skipped", Some("project not in registry"));
            return;
        }
        ProjectRemediationPlan::NoFixer { stack } => {
            let detail = format!("no auto-fix mechanism for {stack} yet");
            push_each(outcomes, proj, &fixable, "no_fixer", Some(&detail));
            return;
        }
        ProjectRemediationPlan::Proceed { path, stack } => (path, stack),
    };

    // Verify-and-rollback is mandatory; a repo with no gates cannot be verified,
    // so it is never mutated.
    let gates = match crate::gate_file::read_gates(&path) {
        Ok(g) => g,
        Err(e) => {
            let detail = format!("could not read gates: {e}");
            push_each(outcomes, proj, &fixable, "skipped", Some(&detail));
            return;
        }
    };
    if gates.is_empty() {
        push_each(outcomes, proj, &fixable, "skipped", Some("no gates to verify the fix against"));
        return;
    }

    // Never mutate a tree that already carries changes — rollback must be able
    // to revert to a known-clean HEAD without clobbering someone else's work.
    if !git_tree_clean(shell, &path).await {
        push_each(outcomes, proj, &fixable, "skipped", Some("working tree not clean"));
        return;
    }

    for f in fixable {
        let fix_version = f.fix_version.as_deref().unwrap_or_default();

        let applied = match super::supply_chain_fixers::apply_fix(shell, &path, &stack, f).await {
            Ok(applied) => applied,
            Err(failure) => {
                git_restore_files(shell, &path, &failure.files).await;
                outcomes.push(outcome(proj, f, "apply_failed", Some(&failure.detail)));
                continue;
            }
        };

        match crate::gate_runner::run_gates(&gates, &path, shell).await {
            Ok(r) if r.required_passed => {
                let target = f.fix_package.as_deref().unwrap_or(&f.package);
                let msg = format!(
                    "chore(deps): bump {} to {fix_version} for {} (supply-chain auto-fix)",
                    target, f.cve
                );
                match git_commit_files(shell, &path, &applied.files, &msg).await {
                    Ok(true) => {
                        let detail = format!("{}; verified by gates and committed", applied.detail);
                        outcomes.push(outcome(proj, f, "applied", Some(&detail)));
                    }
                    Ok(false) => {
                        git_restore_files(shell, &path, &applied.files).await;
                        outcomes.push(outcome(
                            proj,
                            f,
                            "rolled_back",
                            Some("fix verified but commit failed; reverted"),
                        ));
                    }
                    Err(e) => {
                        // Record: a gateway spawn failure while committing is a
                        // distinct fault from git itself rejecting the commit —
                        // name it explicitly rather than reusing the generic
                        // "commit failed" text.
                        git_restore_files(shell, &path, &applied.files).await;
                        let detail = format!("fix verified but {e}; reverted");
                        outcomes.push(outcome(proj, f, "rolled_back", Some(&detail)));
                    }
                }
            }
            Ok(_) => {
                git_restore_files(shell, &path, &applied.files).await;
                outcomes.push(outcome(
                    proj,
                    f,
                    "rolled_back",
                    Some("gate verification failed after the bump; reverted"),
                ));
            }
            Err(e) => {
                git_restore_files(shell, &path, &applied.files).await;
                let detail = format!("gate run errored: {e}; reverted");
                outcomes.push(outcome(proj, f, "rolled_back", Some(&detail)));
            }
        }
    }
}

async fn git_tree_clean(shell: &dyn ShellGateway, path: &Path) -> bool {
    match shell.run(path, "git", &["status", "--porcelain"], None, None).await {
        Ok(r) => r.success && r.stdout.trim().is_empty(),
        Err(_) => false,
    }
}

/// Rolls a failed auto-fix attempt back to a clean working tree.
///
/// This is the rollback path for a remediation whose gate verification
/// failed — a failed rollback is the most important thing to see in logs, so
/// both steps are best-effort but recorded via `tracing::warn!` rather than
/// silently discarded.
async fn git_restore_files(shell: &dyn ShellGateway, path: &Path, files: &[String]) {
    if files.is_empty() {
        return;
    }
    let mut reset_args = vec!["reset", "HEAD", "--"];
    reset_args.extend(files.iter().map(String::as_str));
    // Best-effort: the fix already failed gate verification; a failed
    // rollback step must not abort the formation, but must be visible.
    if let Err(e) = shell.run(path, "git", &reset_args, None, None).await {
        tracing::warn!(error = %e, files = ?files, "failed to 'git reset' files during remediation rollback");
    }

    let mut checkout_args = vec!["checkout", "--"];
    checkout_args.extend(files.iter().map(String::as_str));
    if let Err(e) = shell.run(path, "git", &checkout_args, None, None).await {
        tracing::warn!(error = %e, files = ?files, "failed to 'git checkout --' files during remediation rollback");
    }
}

/// Commit `files` with `msg`. `Ok(bool)` reports whether `git add`/`git
/// commit` themselves succeeded; `Err(String)` carries the gateway error from
/// a spawn failure, kept distinct so a caller does not misreport "git
/// returned nonzero" for a command that never ran at all.
async fn git_commit_files(
    shell: &dyn ShellGateway,
    path: &Path,
    files: &[String],
    msg: &str,
) -> Result<bool, String> {
    let mut add_args = vec!["add"];
    add_args.extend(files.iter().map(String::as_str));
    match shell.run(path, "git", &add_args, None, None).await {
        Ok(r) if !r.success => return Ok(false),
        Ok(_) => {}
        Err(e) => return Err(format!("git add failed to run: {e}")),
    }
    shell
        .run(path, "git", &["commit", "-m", msg], None, None)
        .await
        .map(|r| r.success)
        .map_err(|e| format!("git commit failed to run: {e}"))
}

fn outcome(
    proj: &foundry_sdk::payload::ProjectSupplyChainScan,
    f: &SupplyChainFinding,
    status: &str,
    detail: Option<&str>,
) -> RemediationOutcome {
    RemediationOutcome {
        project: proj.project.clone(),
        cve: f.cve.clone(),
        package: f.package.clone(),
        fix_version: f.fix_version.clone(),
        status: status.to_string(),
        detail: detail.map(str::to_string),
    }
}

fn push_each(
    outcomes: &mut Vec<RemediationOutcome>,
    proj: &foundry_sdk::payload::ProjectSupplyChainScan,
    findings: &[&SupplyChainFinding],
    status: &str,
    detail: Option<&str>,
) {
    for f in findings {
        outcomes.push(outcome(proj, f, status, detail));
    }
}

fn emit_remediated(
    project: &str,
    throttle: Throttle,
    scan: &SupplyChainScannedPayload,
    fixable_count: u64,
    no_fix_count: u64,
    outcomes: Vec<RemediationOutcome>,
) -> anyhow::Result<TaskBlockResult> {
    let remediated_count = outcomes.iter().filter(|o| o.status == "applied").count() as u64;
    let summary = if outcomes.is_empty() {
        format!(
            "Supply-chain triage: {fixable_count} auto-fixable, {no_fix_count} policy-call of {} finding(s)",
            scan.finding_count
        )
    } else {
        format!(
            "Supply-chain remediation: {remediated_count} auto-fixed, {fixable_count} fixable, {no_fix_count} policy-call of {} finding(s)",
            scan.finding_count
        )
    };
    super::emit_result(
        summary,
        EventType::SupplyChainRemediated,
        project,
        throttle,
        &SupplyChainRemediatedPayload {
            projects: scan.projects.clone(),
            project_count: scan.project_count,
            finding_count: scan.finding_count,
            affected_project_count: scan.affected_project_count,
            fixable_count,
            no_fix_count,
            remediated_count,
            outcomes,
        },
    )
}

#[cfg(test)]
mod tests {
    use foundry_sdk::payload::ProjectSupplyChainScan;
    use foundry_sdk::registry::{ProjectEntry, Stack};

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::*;

    // --- fixtures ---------------------------------------------------------

    fn finding(cve: &str, fix: Option<&str>) -> SupplyChainFinding {
        SupplyChainFinding {
            cve: cve.to_string(),
            package: "vulnerable-crate".to_string(),
            severity: Some("high".to_string()),
            version: Some("0.1.0".to_string()),
            fix_version: fix.map(str::to_string),
            fix_package: None,
        }
    }

    fn project(
        name: &str,
        stack: &str,
        findings: Vec<SupplyChainFinding>,
    ) -> ProjectSupplyChainScan {
        ProjectSupplyChainScan {
            project: name.to_string(),
            stack: stack.to_string(),
            findings,
            suppressed: vec![],
            scan_error: None,
        }
    }

    fn scanned(projects: Vec<ProjectSupplyChainScan>) -> SupplyChainScannedPayload {
        let finding_count: u64 = projects.iter().map(|p| p.findings.len() as u64).sum();
        let affected = projects.iter().filter(|p| !p.findings.is_empty()).count() as u64;
        SupplyChainScannedPayload {
            project_count: projects.len() as u64,
            finding_count,
            affected_project_count: affected,
            projects,
        }
    }

    fn trigger(p: &SupplyChainScannedPayload, throttle: Throttle) -> Event {
        Event::new(
            EventType::SupplyChainScanned,
            "system".to_string(),
            throttle,
            serde_json::to_value(p).unwrap(),
        )
    }

    fn registry_with(entries: Vec<ProjectEntry>) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: entries,
        }))
    }

    fn rust_entry(name: &str, path: &str) -> ProjectEntry {
        test_helpers::project_entry(name, path) // stack defaults to Rust
    }

    fn entry_for_stack(name: &str, path: &str, stack: Stack) -> ProjectEntry {
        let mut entry = rust_entry(name, path);
        entry.stack = stack;
        entry
    }

    /// A project dir with a single required gate so verification has something
    /// to run.
    fn project_dir_with_gate() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hone-gates.json"),
            r#"{"gates":[{"name":"test","command":"cargo test","required":true}]}"#,
        )
        .unwrap();
        dir
    }

    fn ok() -> CommandResult {
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    fn fail() -> CommandResult {
        CommandResult {
            stdout: String::new(),
            stderr: "boom".to_string(),
            exit_code: 1,
            success: false,
        }
    }

    fn remediated(result: &TaskBlockResult) -> SupplyChainRemediatedPayload {
        result.events[0].parse_payload().unwrap()
    }

    assert_block_meta!(
        RemediateSupplyChain::with_enabled(FakeShellGateway::success(), registry_with(vec![]), false),
        kind: Observer,
        sinks_on: [SupplyChainScanned],
    );

    // --- classifier (gated off) ------------------------------------------

    #[tokio::test]
    async fn disabled_classifies_without_mutating() {
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3")), finding("CVE-2", None)],
        )]);
        let block = RemediateSupplyChain::with_enabled(
            FakeShellGateway::success(),
            registry_with(vec![]),
            false,
        );
        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.fixable_count, 1);
        assert_eq!(out.no_fix_count, 1);
        assert_eq!(out.remediated_count, 0);
        assert!(out.outcomes.is_empty(), "gated-off path attempts nothing");
    }

    #[tokio::test]
    async fn enabled_but_dry_run_does_not_mutate() {
        let dir = project_dir_with_gate();
        let shell = FakeShellGateway::success();
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::DryRun)).await.unwrap();

        let out = remediated(&result);
        assert!(out.outcomes.is_empty(), "dry-run must not attempt a fix");
        assert_eq!(out.remediated_count, 0);
        assert!(shell.invocations().is_empty(), "no shell commands under dry-run");
    }

    // --- engine (enabled + Full) -----------------------------------------

    #[tokio::test]
    async fn applies_and_commits_when_gates_pass() {
        let dir = project_dir_with_gate();
        // git status (clean) → cargo update (ok) → gate (pass) → git add → git commit
        let shell = FakeShellGateway::success();
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.remediated_count, 1);
        assert_eq!(out.outcomes.len(), 1);
        assert_eq!(out.outcomes[0].status, "applied");
        assert_eq!(out.outcomes[0].cve, "CVE-1");

        // A commit happened; no rollback.
        let cmds: Vec<String> = shell.invocations().iter().map(|i| i.args.join(" ")).collect();
        assert!(cmds.iter().any(|c| c.contains("commit -m")), "applied fix is committed");
        assert!(!cmds.iter().any(|c| c.contains("checkout")), "no rollback on success");
    }

    #[tokio::test]
    async fn typescript_override_rewrite_verifies_and_commits_manifest_and_lockfile() {
        let dir = project_dir_with_gate();
        std::fs::write(
            dir.path().join("package.json"),
            "{\n  \"overrides\": { \"esbuild\": \"^0.28.0\" }\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("bun.lock"), "").unwrap();
        let shell = FakeShellGateway::success();
        let entry = entry_for_stack("alpha", dir.path().to_str().unwrap(), Stack::TypeScript);
        let block =
            RemediateSupplyChain::with_enabled(shell.clone(), registry_with(vec![entry]), true);
        let mut fix = finding("GHSA-esbuild", Some("0.28.1"));
        fix.package = "esbuild".to_string();
        let p = scanned(vec![project("alpha", "typescript", vec![fix])]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.remediated_count, 1);
        assert_eq!(out.outcomes[0].status, "applied");
        let calls = shell.invocations();
        assert!(calls.iter().any(|call| call.command == "bun"));
        let add = calls
            .iter()
            .find(|call| call.command == "git" && call.args.first().is_some_and(|arg| arg == "add"))
            .unwrap();
        assert!(add.args.contains(&"package.json".to_string()));
        assert!(add.args.contains(&"bun.lock".to_string()));
    }

    #[tokio::test]
    async fn python_requirement_rewrite_verifies_and_commits_manifest_and_lockfile() {
        let dir = project_dir_with_gate();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[project]\ndependencies = [\"chromadb>=1.5.9\"]\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("uv.lock"), "").unwrap();
        let shell = FakeShellGateway::success();
        let entry = entry_for_stack("alpha", dir.path().to_str().unwrap(), Stack::Python);
        let block =
            RemediateSupplyChain::with_enabled(shell.clone(), registry_with(vec![entry]), true);
        let mut fix = finding("PYSEC-1", Some("1.6.1"));
        fix.package = "chromadb".to_string();
        let p = scanned(vec![project("alpha", "python", vec![fix])]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.remediated_count, 1);
        assert_eq!(out.outcomes[0].status, "applied");
        let calls = shell.invocations();
        assert!(calls.iter().any(|call| call.command == "uv"));
        let add = calls
            .iter()
            .find(|call| call.command == "git" && call.args.first().is_some_and(|arg| arg == "add"))
            .unwrap();
        assert!(add.args.contains(&"pyproject.toml".to_string()));
        assert!(add.args.contains(&"uv.lock".to_string()));
    }

    #[tokio::test]
    async fn rolls_back_when_gates_fail() {
        let dir = project_dir_with_gate();
        // git status (clean) → cargo update (ok) → gate (FAIL) → git checkout
        let shell = FakeShellGateway::sequence(vec![ok(), ok(), fail(), ok()]);
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.remediated_count, 0);
        assert_eq!(out.outcomes[0].status, "rolled_back");

        let cmds: Vec<String> = shell.invocations().iter().map(|i| i.args.join(" ")).collect();
        assert!(
            cmds.iter().any(|c| c.contains("checkout")),
            "failed verify reverts the lockfile"
        );
        assert!(!cmds.iter().any(|c| c.contains("commit -m")), "nothing committed on failure");
    }

    /// Wraps `inner` but fails every `git reset`/`git checkout --` rollback
    /// command with a real `Err` (spawn failure). Used to prove a failed
    /// rollback is logged but does not abort the formation.
    struct FailingRollbackShellGateway {
        inner: std::sync::Arc<FakeShellGateway>,
    }

    impl ShellGateway for FailingRollbackShellGateway {
        fn run<'a>(
            &'a self,
            working_dir: &'a Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            timeout: Option<std::time::Duration>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + 'a>,
        > {
            let is_rollback = args.first() == Some(&"reset")
                || (args.first() == Some(&"checkout") && args.get(1) == Some(&"--"));
            if is_rollback {
                return Box::pin(async move { Err(anyhow::anyhow!("simulated spawn failure")) });
            }
            self.inner.run(working_dir, command, args, env, timeout)
        }
    }

    #[tokio::test]
    async fn rollback_failure_is_logged_but_does_not_fail_the_formation() {
        let dir = project_dir_with_gate();
        // git status (clean) → cargo update (ok) → gate (FAIL) → rollback (Err)
        let inner = FakeShellGateway::sequence(vec![ok(), ok(), fail()]);
        let shell = std::sync::Arc::new(FailingRollbackShellGateway { inner });
        let block = RemediateSupplyChain::with_enabled(
            shell,
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        assert!(result.success, "a failed rollback command must not fail the formation");
        let out = remediated(&result);
        assert_eq!(out.outcomes[0].status, "rolled_back");
    }

    /// Wraps `inner` but fails the `git add`/`git commit` step with a real
    /// `Err` (spawn failure), rather than a nonzero exit. Used to prove a
    /// gateway spawn failure while committing an already-verified fix is
    /// recorded as a distinct fault, not conflated with git itself rejecting
    /// the commit.
    struct FailingCommitShellGateway {
        inner: std::sync::Arc<FakeShellGateway>,
    }

    impl ShellGateway for FailingCommitShellGateway {
        fn run<'a>(
            &'a self,
            working_dir: &'a Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            timeout: Option<std::time::Duration>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + 'a>,
        > {
            let is_commit_step = args.first() == Some(&"add") || args.first() == Some(&"commit");
            if is_commit_step {
                return Box::pin(async move { Err(anyhow::anyhow!("simulated spawn failure")) });
            }
            self.inner.run(working_dir, command, args, env, timeout)
        }
    }

    #[tokio::test]
    async fn commit_gateway_failure_is_recorded_distinctly_and_rolled_back() {
        let dir = project_dir_with_gate();
        let inner = FakeShellGateway::success();
        let shell = std::sync::Arc::new(FailingCommitShellGateway { inner });
        let block = RemediateSupplyChain::with_enabled(
            shell,
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.remediated_count, 0);
        assert_eq!(out.outcomes[0].status, "rolled_back");
        let detail = out.outcomes[0].detail.as_deref().unwrap();
        assert!(
            detail.contains("spawn failure") || detail.contains("failed to run"),
            "detail should name the gateway failure that aborted the commit, not a generic \
             commit-rejected message: {detail}"
        );
    }

    #[tokio::test]
    async fn apply_failure_is_recorded_not_committed() {
        let dir = project_dir_with_gate();
        // git status (clean) → cargo update (FAIL, out of range)
        let shell = FakeShellGateway::sequence(vec![ok(), fail()]);
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("9.9.9"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.outcomes[0].status, "apply_failed");
        assert!(out.outcomes[0].detail.as_deref().unwrap().contains("manifest's range"));
    }

    #[tokio::test]
    async fn dirty_tree_is_skipped() {
        let dir = project_dir_with_gate();
        // git status returns dirty output
        let dirty = CommandResult {
            stdout: " M Cargo.lock".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        };
        let shell = FakeShellGateway::sequence(vec![dirty]);
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.outcomes[0].status, "skipped");
        assert!(out.outcomes[0].detail.as_deref().unwrap().contains("not clean"));
    }

    #[tokio::test]
    async fn no_gates_means_no_unverified_mutation() {
        // Project dir with no .hone-gates.json → cannot verify → skip.
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::success();
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        let p = scanned(vec![project(
            "alpha",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.outcomes[0].status, "skipped");
        assert!(out.outcomes[0].detail.as_deref().unwrap().contains("no gates"));
        assert!(shell.invocations().is_empty(), "never even checks the tree without gates");
    }

    #[tokio::test]
    async fn unsupported_stack_reports_no_fixer() {
        let dir = project_dir_with_gate();
        let mut entry = rust_entry("alpha", dir.path().to_str().unwrap());
        entry.stack = Stack::Elixir;
        let shell = FakeShellGateway::success();
        let block =
            RemediateSupplyChain::with_enabled(shell.clone(), registry_with(vec![entry]), true);
        let p = scanned(vec![project(
            "alpha",
            "elixir",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.outcomes[0].status, "no_fixer");
        assert!(shell.invocations().is_empty(), "no shell work for an unsupported stack");
    }

    #[tokio::test]
    async fn policy_call_findings_are_not_touched() {
        let dir = project_dir_with_gate();
        let shell = FakeShellGateway::success();
        let block = RemediateSupplyChain::with_enabled(
            shell.clone(),
            registry_with(vec![rust_entry("alpha", dir.path().to_str().unwrap())]),
            true,
        );
        // Only a no-fix finding → nothing fixable → no outcomes.
        let p = scanned(vec![project("alpha", "rust", vec![finding("CVE-1", None)])]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert!(out.outcomes.is_empty());
        assert!(shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn project_not_in_registry_is_skipped() {
        let shell = FakeShellGateway::success();
        let block = RemediateSupplyChain::with_enabled(shell.clone(), registry_with(vec![]), true);
        let p = scanned(vec![project(
            "ghost",
            "rust",
            vec![finding("CVE-1", Some("1.2.3"))],
        )]);

        let result = block.execute(&trigger(&p, Throttle::Full)).await.unwrap();

        let out = remediated(&result);
        assert_eq!(out.outcomes[0].status, "skipped");
        assert!(out.outcomes[0].detail.as_deref().unwrap().contains("not in registry"));
    }

    // -- plan_project_remediation pure function tests --

    #[test]
    fn plan_returns_nothing_fixable_when_all_findings_are_policy_calls() {
        let proj = project("alpha", "rust", vec![finding("CVE-1", None)]);
        let entry = rust_entry("alpha", "some/path");
        assert_eq!(
            plan_project_remediation(&proj, &[entry]),
            ProjectRemediationPlan::NothingFixable
        );
    }

    #[test]
    fn plan_returns_not_in_registry_when_project_has_no_entry() {
        let proj = project("ghost", "rust", vec![finding("CVE-1", Some("1.2.3"))]);
        assert_eq!(plan_project_remediation(&proj, &[]), ProjectRemediationPlan::NotInRegistry);
    }

    #[test]
    fn plan_returns_no_fixer_for_unsupported_stack() {
        let proj = project("alpha", "elixir", vec![finding("CVE-1", Some("1.2.3"))]);
        let mut entry = rust_entry("alpha", "some/path");
        entry.stack = Stack::Elixir;
        assert!(matches!(
            plan_project_remediation(&proj, &[entry]),
            ProjectRemediationPlan::NoFixer { .. }
        ));
    }

    #[test]
    fn plan_returns_proceed_for_rust_project_with_fixable_findings() {
        let proj = project("alpha", "rust", vec![finding("CVE-1", Some("1.2.3"))]);
        let entry = rust_entry("alpha", "some/path");
        assert_eq!(
            plan_project_remediation(&proj, &[entry]),
            ProjectRemediationPlan::Proceed {
                path: std::path::PathBuf::from("some/path"),
                stack: Stack::Rust,
            }
        );
    }

    #[test]
    fn plan_returns_proceed_for_typescript_and_python_projects() {
        for stack in [Stack::TypeScript, Stack::Python] {
            let proj = project("alpha", &stack.to_string(), vec![finding("CVE-1", Some("1.2.3"))]);
            let mut entry = rust_entry("alpha", "some/path");
            entry.stack = stack.clone();
            assert_eq!(
                plan_project_remediation(&proj, &[entry]),
                ProjectRemediationPlan::Proceed {
                    path: std::path::PathBuf::from("some/path"),
                    stack,
                }
            );
        }
    }
}
