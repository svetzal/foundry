//! Real Git regressions. Child processes isolate global Git and Foundry path settings
//! without changing the environment of parallel workspace tests.
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

use anyhow::Result;
use foundry_sdk::gateway::{CommandResult, ShellGateway};
use foundry_sdk::payload::WorkReconcileCompletedPayload;
use foundry_sdk::task_block::TaskBlock;

use super::ReconcileWork;
use crate::blocks::test_helpers::{make_trigger, registry_with_project};

async fn git(path: &Path, args: &[&str]) -> String {
    let result = crate::shell::run(path, "git", args, None, None).await.unwrap();
    assert!(result.success, "git {args:?}: {}", result.stderr);
    result.stdout.trim_end().to_owned()
}

async fn tag_snapshot(path: &Path) -> Vec<(String, String, String)> {
    let refs = git(
        path,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/tags",
        ],
    )
    .await;
    let mut tags = Vec::new();
    for row in refs.lines() {
        let (name, oid) = row.split_once(' ').unwrap();
        tags.push((name.into(), oid.into(), git(path, &["cat-file", "-p", oid]).await));
    }
    tags
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    fetch_head: Vec<u8>,
    tags: Vec<(String, String, String)>,
    index: Vec<u8>,
    contents: Vec<u8>,
    branches: String,
    remote: String,
    worktrees: String,
    bundle: Vec<u8>,
    history: Vec<u8>,
}

async fn snapshot(path: &Path, root: &Path) -> Snapshot {
    Snapshot {
        fetch_head: std::fs::read(path.join(".git/FETCH_HEAD")).unwrap(),
        tags: tag_snapshot(path).await,
        index: std::fs::read(path.join(".git/index")).unwrap(),
        contents: std::fs::read(path.join("content")).unwrap(),
        branches: git(
            path,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads",
            ],
        )
        .await,
        remote: git(path, &["ls-remote", "origin"]).await,
        worktrees: git(path, &["worktree", "list", "--porcelain"]).await,
        bundle: std::fs::read(root.join("preserved.bundle")).unwrap(),
        history: std::fs::read(root.join("events/prior.jsonl")).unwrap(),
    }
}

/// Runs every startup command for real, pausing immediately after its fetch
/// and before preparation can resolve `FETCH_HEAD` in worktree add.
struct InterleavedShell {
    reconciler: ReconcileWork,
    root: PathBuf,
    snapshots: Mutex<Option<(Snapshot, Snapshot)>>,
}

impl ShellGateway for InterleavedShell {
    fn run<'a>(
        &'a self,
        cwd: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
        Box::pin(async move {
            let result = crate::shell::run(cwd, command, args, env, timeout).await?;
            if command == "git" && args.first() == Some(&"fetch") && result.success {
                // Startup must retain its normal behaviour. Enable the hostile
                // tag settings only for the interleaved reconciliation.
                let profile = std::env::var("RECONCILE_GIT_PROFILE").unwrap();
                let global = std::env::var("GIT_CONFIG_GLOBAL").unwrap();
                let settings = match profile.as_str() {
                    "global" => {
                        "[fetch]\nprune = true\npruneTags = true\n[remote \"origin\"]\ntagOpt = --tags\n"
                    }
                    "no-tags" => {
                        "[fetch]\nprune = true\npruneTags = true\n[remote \"origin\"]\nprune = true\npruneTags = true\ntagOpt = --no-tags\n"
                    }
                    _ => "",
                };
                std::fs::write(&global, settings).unwrap();
                if profile == "origin" {
                    git(cwd, &["config", "remote.origin.prune", "true"]).await;
                    git(cwd, &["config", "remote.origin.pruneTags", "true"]).await;
                    git(cwd, &["config", "remote.origin.tagOpt", "--tags"]).await;
                }
                // Explicit tag refspecs in origin's configuration must also
                // stay outside reconciliation's branch-only fetch.
                git(
                    cwd,
                    &[
                        "config",
                        "--add",
                        "remote.origin.fetch",
                        "+refs/tags/*:refs/tags/*",
                    ],
                )
                .await;
                let config = git(cwd, &["config", "--show-origin", "--show-scope", "--list"]).await;
                println!("effective config ({profile}):\n{config}");
                assert!(config.contains("remote.origin.fetch=+refs/tags/*:refs/tags/*"));
                if profile != "default" {
                    let scope = if profile == "origin" {
                        "local"
                    } else {
                        "global"
                    };
                    assert!(config.contains(scope));
                    assert!(config.contains("prune=true"));
                    assert!(config.contains("prunetags=true"));
                    assert!(config.contains("remote.origin.tagopt="));
                }
                let before = snapshot(cwd, &self.root).await;
                let completion = self
                    .reconciler
                    .execute(&make_trigger(
                        foundry_sdk::event::EventType::WorkReconcileStarted,
                        "system",
                        serde_json::json!({}),
                    ))
                    .await?;
                let report: WorkReconcileCompletedPayload =
                    completion.events.last().unwrap().parse_payload()?;
                assert!(report.success, "{:?}", report.errors);
                let after = snapshot(cwd, &self.root).await;
                *self.snapshots.lock().unwrap() = Some((before, after));
            }
            Ok(result)
        })
    }
}

struct Fixture {
    checkout: PathBuf,
    base: String,
    competitor: String,
    preserved: String,
    trunk: String,
    tags: Vec<(String, String, String)>,
}

async fn fixture(root: &Path) -> Fixture {
    let remote = root.join("remote.git");
    let checkout = root.join("checkout");
    let producer = root.join("producer");
    git(root, &["init", "--bare", remote.to_str().unwrap()]).await;
    git(root, &["init", "-b", "main", producer.to_str().unwrap()]).await;
    git(&producer, &["config", "user.name", "Regression"]).await;
    git(&producer, &["config", "user.email", "regression@example.test"]).await;
    std::fs::write(producer.join("content"), "base").unwrap();
    git(&producer, &["add", "content"]).await;
    git(&producer, &["commit", "-m", "base"]).await;
    let base = git(&producer, &["rev-parse", "HEAD"]).await;
    git(&producer, &["remote", "add", "origin", remote.to_str().unwrap()]).await;
    git(&producer, &["push", "origin", "main"]).await;
    git(
        root,
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    )
    .await;
    git(&checkout, &["config", "user.name", "Regression"]).await;
    git(&checkout, &["config", "user.email", "regression@example.test"]).await;
    git(&checkout, &["tag", "-a", "local-only", "-m", "local annotated object"]).await;
    git(&checkout, &["tag", "-a", "divergent", "-m", "local divergent object"]).await;
    let tags = tag_snapshot(&checkout).await;
    assert_eq!(
        tags.iter().map(|tag| tag.0.as_str()).collect::<Vec<_>>(),
        vec!["refs/tags/divergent", "refs/tags/local-only"]
    );

    // Distinct competing commits ensure replacing FETCH_HEAD cannot accidentally
    // select the intended startup source. The remote's HEAD points at competitor.
    git(&producer, &["checkout", "-b", "competitor"]).await;
    std::fs::write(producer.join("content"), "competitor").unwrap();
    git(&producer, &["commit", "-am", "competing commit"]).await;
    let competitor = git(&producer, &["rev-parse", "HEAD"]).await;
    git(&producer, &["push", "origin", "competitor"]).await;
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/competitor"]).await;
    git(&producer, &["checkout", "-b", "preserved", &base]).await;
    std::fs::write(producer.join("content"), "preserved").unwrap();
    git(&producer, &["commit", "-am", "preserved commit"]).await;
    let preserved = git(&producer, &["rev-parse", "HEAD"]).await;
    git(&producer, &["push", "origin", "preserved"]).await;
    let bundle = root.join("preserved.bundle");
    git(&producer, &["bundle", "create", bundle.to_str().unwrap(), "preserved"]).await;
    git(&producer, &["checkout", "main"]).await;
    std::fs::write(producer.join("content"), "new trunk").unwrap();
    git(&producer, &["commit", "-am", "new trunk"]).await;
    let trunk = git(&producer, &["rev-parse", "HEAD"]).await;
    git(&producer, &["tag", "-a", "remote-only", "-m", "remote annotated object"]).await;
    git(&producer, &["tag", "-a", "divergent", "-m", "remote divergent object"]).await;
    git(&producer, &["push", "origin", "main", "--tags"]).await;
    for oid in [&base, &competitor, &preserved, &trunk] {
        assert_eq!(oid.len(), 40);
    }
    assert_ne!(competitor, trunk);
    assert_ne!(competitor, preserved);
    assert_ne!(trunk, preserved);
    git(&checkout, &["update-ref", "refs/remotes/origin/stale", &base]).await;
    git(&checkout, &["update-ref", "refs/remotes/origin/competitor", &base]).await;
    // A configured tracking refresh must change competitor, but retain stale.
    assert_eq!(git(&checkout, &["rev-parse", "refs/remotes/origin/competitor"]).await, base);
    std::fs::write(checkout.join("content"), "dirty checkout retained").unwrap();
    std::fs::create_dir(root.join("events")).unwrap();
    std::fs::write(root.join("events/prior.jsonl"), b"prior event bytes\n").unwrap();
    Fixture {
        checkout,
        base,
        competitor,
        preserved,
        trunk,
        tags,
    }
}

async fn scenario(mode: &str) {
    let root = PathBuf::from(std::env::var("RECONCILE_FIXTURE_ROOT").unwrap());
    let Fixture {
        checkout,
        base,
        competitor,
        preserved,
        trunk,
        tags,
    } = fixture(&root).await;
    let bundle = root.join("preserved.bundle");
    let registry = registry_with_project("regression", checkout.to_str().unwrap());
    let entry = registry.read().unwrap().projects[0].clone();
    let shell = InterleavedShell {
        reconciler: ReconcileWork::new(
            registry,
            root.join("ledger.json"),
            root.join("worktrees"),
            root.join("events"),
            root.join("digest"),
        ),
        root: root.clone(),
        snapshots: Mutex::new(None),
    };
    let bundle_ref = format!("bundle:{}", bundle.display());
    let continuation = match mode {
        "fresh" => None,
        "remote" => Some("preserved"),
        "bundle" => Some(bundle_ref.as_str()),
        _ => unreachable!(),
    };
    let expected = if mode == "fresh" { &trunk } else { &preserved };
    let workspace =
        crate::blocks::task_workspace::prepare_task_workspace(&shell, &entry, mode, continuation)
            .await
            .unwrap();
    let head = git(&workspace.path, &["rev-parse", "HEAD"]).await;
    let branch = git(&checkout, &["rev-parse", &format!("refs/heads/{}", workspace.branch)]).await;
    println!(
        "mode={mode} base={base} competitor={competitor} preserved={preserved} trunk={trunk} HEAD={head} branch={branch}"
    );

    let (before, after) = shell.snapshots.into_inner().unwrap().unwrap();
    assert_eq!(before.tags, tags);
    println!(
        "FETCH_HEAD before={:?} after={:?}\ntags before={:#?} after={:#?}",
        String::from_utf8_lossy(&before.fetch_head),
        String::from_utf8_lossy(&after.fetch_head),
        before.tags,
        after.tags
    );
    assert_eq!(
        git(
            &checkout,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/remotes/origin"
            ]
        )
        .await,
        format!(
            "refs/remotes/origin/competitor {competitor}\nrefs/remotes/origin/main {trunk}\nrefs/remotes/origin/preserved {preserved}\nrefs/remotes/origin/stale {base}"
        )
    );
    assert!(!root.join("ledger.json").exists());
    assert!(
        head == *expected && branch == *expected && before == after,
        "reconciliation changed exact startup heads, FETCH_HEAD, tags or protected evidence"
    );
}

async fn isolated(mode: &str, name: &str) {
    if std::env::var("RECONCILE_FIXTURE_ROOT").is_ok() {
        scenario(mode).await;
        return;
    }
    let mut failures = Vec::new();
    for profile in ["default", "global", "origin", "no-tags"] {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("gitconfig");
        std::fs::write(&global, "").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("RECONCILE_FIXTURE_ROOT", root.path())
            .env("RECONCILE_GIT_PROFILE", profile)
            .env("GIT_CONFIG_GLOBAL", global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env("FOUNDRY_WORKTREES_DIR", root.path().join("worktrees"))
            .output()
            .await
            .unwrap();
        println!("{}", String::from_utf8_lossy(&output.stdout));
        if !output.status.success() {
            failures.push(format!(
                "mode={mode} profile={profile}: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[tokio::test]
async fn reconciliation_preserves_fresh_startup() {
    isolated("fresh", "blocks::work_reconcile::tests::reconciliation_preserves_fresh_startup")
        .await;
}

#[tokio::test]
async fn reconciliation_preserves_remote_continuation() {
    isolated(
        "remote",
        "blocks::work_reconcile::tests::reconciliation_preserves_remote_continuation",
    )
    .await;
}

#[tokio::test]
async fn reconciliation_preserves_bundle_continuation() {
    isolated(
        "bundle",
        "blocks::work_reconcile::tests::reconciliation_preserves_bundle_continuation",
    )
    .await;
}

async fn assert_supersession_uses_either_trunk(local_ahead: bool) {
    use foundry_sdk::work_item::{
        WorkDisposition, WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore,
        WorkLane,
    };

    for (patch_equivalent, missing_origin) in [(false, false), (true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let (checkout, base, current) =
            trunk_fixture(root.path(), local_ahead, patch_equivalent).await;
        if missing_origin {
            // Retain a stale tracking trunk that would falsely prove landing.
            git(&checkout, &["fetch", "origin"]).await;
            git(&checkout, &["update-ref", "refs/remotes/origin/main", &current]).await;
            git(&root.path().join("remote.git"), &["update-ref", "-d", "refs/heads/main"]).await;
            git(&checkout, &["config", "remote.origin.prune", "true"]).await;
        }
        let ledger = root.path().join("ledger.json");
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "regression".into(),
                objective: "preserved deliverable".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "regression".into(),
                trace_id: None,
            },
            chrono::Utc::now(),
        );
        item.state = WorkItemState::Preserved;
        item.disposition = Some(WorkDisposition {
            preservation_ref: Some("foundry-task/preserved".into()),
            verdict: None,
            landed_commit: None,
            worktree: None,
            worktree_removed: None,
        });
        WorkItemStore {
            version: 1,
            items: vec![item.clone()],
        }
        .save(&ledger)
        .unwrap();
        let block = ReconcileWork::new(
            registry_with_project("regression", checkout.to_str().unwrap()),
            ledger.clone(),
            root.path().join("worktrees"),
            root.path().join("events"),
            root.path().join("digest"),
        );
        let result = block
            .execute(&make_trigger(
                foundry_sdk::event::EventType::WorkReconcileStarted,
                "system",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        let report: WorkReconcileCompletedPayload =
            result.events.last().unwrap().parse_payload().unwrap();
        if missing_origin {
            assert_missing_origin(&report, &checkout, &ledger, &item, &current, local_ahead).await;
            continue;
        }
        assert!(report.success, "{:?}", report.errors);
        assert_eq!(report.settled_ids, vec![item.id.clone()]);
        let landed = WorkItemStore::load(&ledger).unwrap();
        assert_eq!(landed.items[0].state, WorkItemState::Landed);
        assert_eq!(
            landed.items[0].disposition.as_ref().unwrap().landed_commit.as_deref(),
            Some(current.as_str())
        );
        let proving_trunk = if local_ahead {
            "registered trunk refs/heads/main"
        } else {
            "origin trunk refs/remotes/origin/main"
        };
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.identity == item.id && f.detail.contains(proving_trunk))
        );
        assert!(report.findings.iter().any(|f| f.identity.ends_with("foundry-task/orphan")
            && f.detail.contains(proving_trunk)
            && f.detail.contains(if patch_equivalent {
                "patch-equivalent"
            } else {
                "ancestor"
            })));
        assert!(report.findings.iter().any(|f| f.detail.contains("trunks differ:")
            && f.detail.contains(&base)
            && f.detail.contains(&current)));
        assert_eq!(
            git(&checkout, &["rev-parse", "main"]).await,
            if local_ahead { current } else { base }
        );
    }
}

async fn assert_missing_origin(
    report: &WorkReconcileCompletedPayload,
    checkout: &Path,
    ledger: &Path,
    item: &foundry_sdk::work_item::WorkItem,
    current: &str,
    local_ahead: bool,
) {
    use foundry_sdk::work_item::{WorkItemState, WorkItemStore};

    assert!(report.errors.iter().any(|error| error.contains("not observed by this fetch")));
    assert_eq!(git(checkout, &["rev-parse", "refs/remotes/origin/main"]).await, current);
    let stored = WorkItemStore::load(ledger).unwrap();
    if local_ahead {
        assert_eq!(report.settled_ids, vec![item.id.clone()]);
        assert_eq!(stored.items[0].state, WorkItemState::Landed);
        assert!(report.findings.iter().any(|finding| {
            finding.identity == item.id
                && finding.detail.contains("registered trunk refs/heads/main")
        }));
    } else {
        assert!(report.settled_ids.is_empty());
        assert_eq!(stored.items[0].state, WorkItemState::Preserved);
    }
}

#[tokio::test]
async fn reconciliation_proves_supersession_with_stale_local_trunk() {
    isolated_trunk(
        false,
        "blocks::work_reconcile::tests::reconciliation_proves_supersession_with_stale_local_trunk",
    )
    .await;
}

#[tokio::test]
async fn reconciliation_proves_supersession_with_local_trunk_ahead_of_origin() {
    isolated_trunk(true, "blocks::work_reconcile::tests::reconciliation_proves_supersession_with_local_trunk_ahead_of_origin").await;
}

async fn trunk_fixture(
    root: &Path,
    local_ahead: bool,
    patch_equivalent: bool,
) -> (PathBuf, String, String) {
    let remote = root.join("remote.git");
    let producer = root.join("producer");
    let clone = root.join("clone");
    git(root, &["init", "--bare", remote.to_str().unwrap()]).await;
    git(root, &["init", "-b", "main", producer.to_str().unwrap()]).await;
    git(&producer, &["config", "user.name", "Regression"]).await;
    git(&producer, &["config", "user.email", "regression@example.test"]).await;
    std::fs::write(producer.join("base"), "base").unwrap();
    git(&producer, &["add", "base"]).await;
    git(&producer, &["commit", "-m", "base"]).await;
    let base = git(&producer, &["rev-parse", "HEAD"]).await;
    git(&producer, &["remote", "add", "origin", remote.to_str().unwrap()]).await;
    git(&producer, &["push", "origin", "main"]).await;
    git(
        root,
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    )
    .await;
    git(&producer, &["checkout", "-b", "foundry-task/preserved"]).await;
    std::fs::write(producer.join("work"), "deliverable").unwrap();
    git(&producer, &["add", "work"]).await;
    git(&producer, &["commit", "-m", "preserved work"]).await;
    let preserved = git(&producer, &["rev-parse", "HEAD"]).await;
    git(&producer, &["branch", "foundry-task/orphan"]).await;
    git(&producer, &["checkout", "main"]).await;
    if patch_equivalent {
        git(&producer, &["cherry-pick", "--no-commit", &preserved]).await;
        git(&producer, &["commit", "-m", "equivalent work"]).await;
    } else {
        git(&producer, &["merge", "--ff-only", &preserved]).await;
    }
    let current = git(&producer, &["rev-parse", "main"]).await;
    if patch_equivalent {
        assert_ne!(current, preserved);
    }
    git(
        &producer,
        &[
            "push",
            "origin",
            "foundry-task/preserved",
            "foundry-task/orphan",
        ],
    )
    .await;
    let checkout = if local_ahead {
        producer
    } else {
        git(&producer, &["push", "origin", "main"]).await;
        clone
    };
    (checkout, base, current)
}

async fn isolated_trunk(local_ahead: bool, name: &str) {
    if std::env::var_os("RECONCILE_TRUNK_CHILD").is_some() {
        assert_supersession_uses_either_trunk(local_ahead).await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("gitconfig");
    std::fs::write(&global, "").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("RECONCILE_TRUNK_CHILD", "1")
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
