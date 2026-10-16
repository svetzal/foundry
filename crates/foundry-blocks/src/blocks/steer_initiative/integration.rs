//! Integration evidence is produced by commands on disposable current trunks.
use std::path::Path;
use std::time::Duration;

use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::initiative::{DoneEvidence, IntegrationGateResult};
use foundry_sdk::registry::Registry;
use foundry_sdk::work_item::WorkItemStore;
use serde::Deserialize;

use crate::gateway::ShellGateway;

const REPORT_BYTES: usize = 8192;
const RUNNER: &str = include_str!("integration_runner.py");

#[derive(Deserialize)]
struct Execution {
    passed: bool,
    exit_code: Option<i32>,
    report: String,
    #[serde(default)]
    artifacts: Vec<String>,
}

/// What integration preparation produced for one steering pass.
#[derive(Debug, Default)]
pub(super) struct Integration {
    pub results: Vec<IntegrationGateResult>,
    /// Why integration did not run, when this steer did not hold every trunk.
    pub skipped: Option<String>,
}

pub(super) async fn run(
    shell: &dyn ShellGateway,
    registry: &Registry,
    store: &CampaignStore,
    name: &str,
    ledger_path: &Path,
    store_path: &Path,
    trigger_id: &str,
) -> anyhow::Result<Integration> {
    let initiative = store.initiative_view(name)?;
    let has_gates = initiative.done_evidence.iter().any(|e| matches!(e, DoneEvidence::Gate { .. }));
    if !has_gates && initiative.completion_proposal.is_none() {
        return Ok(Integration::default());
    }
    // Pacing gives a steer every trunk or none when it starts. A steer that
    // started beside other work never touches a trunk, even when that work
    // settled before it got here: evidence against a partly current snapshot
    // would be stale on arrival, and a failed result would become a
    // regression finding.
    if let Some(skipped) = trunks_not_held(ledger_path, &initiative.projects, trigger_id)? {
        return Ok(Integration {
            results: vec![],
            skipped: Some(skipped),
        });
    }
    let root = store_path.parent().ok_or_else(|| anyhow::anyhow!("missing evidence root"))?;
    let host = shell.run(root, "hostname", &[], None, Some(Duration::from_secs(10))).await?;
    if !host.success || host.stdout.trim().is_empty() {
        anyhow::bail!("cannot identify daemon host for integration evidence");
    }
    let host = host.stdout.trim();
    let busy = active_owner(ledger_path, &initiative.projects, trigger_id)?;
    let sync =
        serde_json::json!({"mode":"sync","registry":registry,"projects":initiative.projects});
    let sync_result = if busy {
        Err(anyhow::anyhow!("integration trunks have an active execution or cleanup owner"))
    } else {
        execute(shell, root, None, &sync).await.map(|_| ())
    };
    if !has_gates {
        sync_result?;
        return Ok(Integration::default());
    }
    let repositories = super::packet::repositories(registry, &initiative.projects, shell).await?;
    let identities = super::repository_identities(&repositories);
    let snapshot = store.outcome_snapshot(name, identities)?;
    let mut results = Vec::new();
    for (index, evidence) in initiative.done_evidence.iter().enumerate() {
        let DoneEvidence::Gate {
            command,
            host: selected_host,
            artifacts,
            ..
        } = evidence
        else {
            continue;
        };
        let selected_host = selected_host.as_deref().unwrap_or(host);
        let remote = !is_local(selected_host, host);
        let id = format!("ig_{}", uuid::Uuid::new_v4());
        let spec = serde_json::json!({
            "mode":"gate", "registry":if remote {serde_json::Value::Null} else {serde_json::to_value(registry)?},
            "projects":initiative.projects,"expected":snapshot.repositories.iter().map(|r| (&r.project,r.trunk.trim())).collect::<std::collections::BTreeMap<_,_>>(),
            "command":command,"artifacts":artifacts,"id":id,
            "evidence_root":if remote {None} else {Some(root.join("initiatives").join("integration-evidence"))},
        });
        let executed = match &sync_result {
            Ok(()) => execute(shell, root, remote.then_some(selected_host), &spec).await,
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
        };
        let result = match executed {
            Ok(result) => result,
            Err(error) => Execution {
                passed: false,
                exit_code: None,
                report: error.to_string(),
                artifacts: vec![],
            },
        };
        // A command cannot turn changed trunks into fresh evidence, even when it exits zero.
        let unchanged = super::packet::repositories(registry, &initiative.projects, shell).await?
            == repositories;
        let report = if unchanged {
            result.report
        } else {
            format!("{}\nStale: daemon trunks changed during integration.", result.report)
        };
        results.push(IntegrationGateResult {
            id,
            gate_index: index,
            host: selected_host.into(),
            command: command.clone(),
            snapshot: snapshot.clone(),
            passed: result.passed && unchanged,
            exit_code: result.exit_code,
            report: super::packet::compact_evidence(
                &report,
                REPORT_BYTES,
                "foundry initiative show --json",
            ),
            artifacts: result.artifacts,
            recorded_at: chrono::Utc::now(),
        });
    }
    Ok(Integration {
        results,
        skipped: None,
    })
}

/// Whether `root` is this steer's own started root.
fn own_steer(root: &foundry_sdk::event::Event, trigger_id: &str) -> bool {
    root.id == trigger_id
        && root.event_type == foundry_sdk::event::EventType::InitiativeAdvanceRequested
}

/// Why this steer may not integrate, or `None` when its own ledger item
/// reserves every initiative project.
fn trunks_not_held(
    ledger_path: &Path,
    projects: &[String],
    trigger_id: &str,
) -> anyhow::Result<Option<String>> {
    let items = WorkItemStore::load(ledger_path)?;
    let held = items.items.iter().any(|item| {
        item.started_root.as_ref().is_some_and(|root| own_steer(root, trigger_id))
            && projects.iter().all(|project| item.reserved_projects.contains(project))
    });
    if held {
        return Ok(None);
    }
    let busy: Vec<&str> = projects
        .iter()
        .filter(|project| {
            items.items.iter().any(|item| {
                (item.is_queued() || item.occupies_slot())
                    && !item.started_root.as_ref().is_some_and(|root| own_steer(root, trigger_id))
                    && (item.project == **project
                        || (!item.reservation_if_free && item.reserved_projects.contains(project)))
            })
        })
        .map(String::as_str)
        .collect();
    Ok(Some(if busy.is_empty() {
        "integration skipped: this steer started without the initiative trunks".to_owned()
    } else {
        format!("integration skipped: other work holds initiative trunks: {}", busy.join(", "))
    }))
}

fn active_owner(ledger_path: &Path, projects: &[String], trigger_id: &str) -> anyhow::Result<bool> {
    let items = WorkItemStore::load(ledger_path)?;
    Ok(items.items.iter().any(|item| {
        item.occupies_slot()
            && !item.started_root.as_ref().is_some_and(|root| own_steer(root, trigger_id))
            && (projects.contains(&item.project)
                || item.reserved_projects.iter().any(|p| projects.contains(p)))
    }))
}

fn is_local(selected: &str, hostname: &str) -> bool {
    matches!(selected, "localhost" | "127.0.0.1" | "::1")
        || selected == hostname
        || selected == hostname.trim_end_matches(".local")
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

async fn execute(
    shell: &dyn ShellGateway,
    root: &Path,
    host: Option<&str>,
    spec: &serde_json::Value,
) -> anyhow::Result<Execution> {
    let spec = serde_json::to_string(spec)?;
    let output = if let Some(host) = host {
        if host.starts_with('-')
            || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
        {
            anyhow::bail!("invalid integration host");
        }
        let command = format!("python3 -c {} {}", quote(RUNNER), quote(&spec));
        shell
            .run(
                root,
                "ssh",
                &[
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=15",
                    "--",
                    host,
                    &command,
                ],
                None,
                Some(Duration::from_secs(2500)),
            )
            .await?
    } else {
        shell
            .run(root, "python3", &["-c", RUNNER, &spec], None, Some(Duration::from_secs(2500)))
            .await?
    };
    if !output.success {
        anyhow::bail!("integration transport exited {}: {}", output.exit_code, output.stderr);
    }
    let result: Execution = serde_json::from_str(&output.stdout)?;
    if !result.passed && spec.contains("\"mode\":\"sync\"") {
        anyhow::bail!("integration trunk preparation failed: {}", result.report);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::ProcessShellGateway;

    fn git(root: &Path, args: &[&str]) {
        let output =
            std::process::Command::new("git").current_dir(root).args(args).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn disposable_current_trunks_require_capture_logs_and_preexisting_artifacts() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let capture = bin.join("foundry");
        std::fs::write(&capture, "#!/bin/sh\nlog=$3\nshift 4\n\"$@\" >\"$log/probe.stdout.log\" 2>\"$log/probe.stderr.log\"\ncode=$?\nprintf 'sh: exit %s\\nstdout: %s\\nstderr: %s\\n' \"$code\" \"$log/probe.stdout.log\" \"$log/probe.stderr.log\"\nexit \"$code\"\n").unwrap();
        std::fs::set_permissions(&capture, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut entries = Vec::new();
        for name in ["first", "second"] {
            let root = temp.path().join(name);
            std::fs::create_dir(&root).unwrap();
            git(&root, &["init", "-b", "main"]);
            git(&root, &["config", "user.name", "Sandbox"]);
            git(&root, &["config", "user.email", "sandbox@example.invalid"]);
            std::fs::write(root.join("contract.txt"), "shared contract").unwrap();
            git(&root, &["add", "contract.txt"]);
            git(&root, &["commit", "-m", "Initial contract"]);
            let origin = temp.path().join(format!("{name}.git"));
            git(
                temp.path(),
                &[
                    "clone",
                    "--bare",
                    root.to_str().unwrap(),
                    origin.to_str().unwrap(),
                ],
            );
            git(&root, &["remote", "add", "origin", origin.to_str().unwrap()]);
            entries.push(serde_json::json!({"name":name,"path":root,"branch":"main"}));
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
        let env = [("PATH".into(), path)];
        let evidence = temp.path().join("evidence");
        for (id, command, artifact, passed, exit) in [
            (
                "passing",
                "cmp first/contract.txt second/contract.txt; printf changed > first/contract.txt",
                "first/contract.txt",
                true,
                Some(0),
            ),
            ("failing", "exit 7", "first/contract.txt", false, Some(7)),
            ("missing", "touch first/missing.txt", "first/missing.txt", false, None),
        ] {
            let spec = serde_json::json!({"mode":"gate","registry":{"projects":entries},"projects":["first","second"],"expected":{},"command":command,"artifacts":[artifact],"id":id,"evidence_root":evidence});
            let serialized = spec.to_string();
            let output = ProcessShellGateway
                .run(
                    temp.path(),
                    "python3",
                    &["-c", RUNNER, &serialized],
                    Some(&env),
                    Some(Duration::from_secs(60)),
                )
                .await
                .unwrap();
            assert!(output.success, "{}", output.stderr);
            let result: Execution = serde_json::from_str(&output.stdout).unwrap();
            assert_eq!(result.passed, passed, "{}", result.report);
            assert_eq!(result.exit_code, exit);
            if exit.is_some() {
                assert!(evidence.join(id).join("probe.stdout.log").exists());
                assert!(evidence.join(id).join("result.json").exists());
            }
            assert_eq!(
                std::fs::read_to_string(temp.path().join("first/contract.txt")).unwrap(),
                "shared contract"
            );
        }
    }

    #[test]
    fn a_steer_with_no_ledger_item_holds_no_trunk() {
        let temp = tempfile::tempdir().unwrap();
        let reason =
            trunks_not_held(&temp.path().join("ledger.json"), &["sample".into()], "evt_steer")
                .unwrap();
        assert_eq!(
            reason.as_deref(),
            Some("integration skipped: this steer started without the initiative trunks")
        );
    }

    #[test]
    fn local_host_aliases_and_ssh_quoting_preserve_arguments() {
        assert!(is_local("localhost", "Max-Headroom.local"));
        assert!(is_local("Max-Headroom", "Max-Headroom.local"));
        assert!(!is_local("mojility-ops-01", "Max-Headroom.local"));
        assert_eq!(quote("a'b $HOME\n"), "'a'\\''b $HOME\n'");
    }
}
