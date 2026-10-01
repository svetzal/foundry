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
                    "global" => "[fetch]\npruneTags = true\n[remote \"origin\"]\ntagOpt = --tags\n",
                    "no-tags" => {
                        "[fetch]\npruneTags = true\n[remote \"origin\"]\npruneTags = true\ntagOpt = --no-tags\n"
                    }
                    _ => "",
                };
                std::fs::write(&global, settings).unwrap();
                if profile == "origin" {
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
    // A configured tracking refresh must change competitor, and prune stale.
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
            "refs/remotes/origin/competitor {competitor}\nrefs/remotes/origin/main {trunk}\nrefs/remotes/origin/preserved {preserved}"
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
