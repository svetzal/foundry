use foundry_sdk::initiative::derive::IntentProjection;
use foundry_sdk::initiative::{Initiative, SteeringExternalContext, SteeringRepositoryIdentity};
use foundry_sdk::registry::Registry;
use std::path::Path;

fn command(root: &Path, executable: &str, args: &[&str]) -> anyhow::Result<String> {
    let result = std::process::Command::new(executable).args(args).current_dir(root).output()?;
    if !result.status.success() {
        anyhow::bail!("cannot validate steering evidence: {executable} failed ({})", result.status);
    }
    Ok(String::from_utf8(result.stdout)?)
}
fn projection(root: &Path) -> anyhow::Result<String> {
    for path in [
        ".et/projection.json",
        ".alloy/projections/et.json",
        ".alloy/projections/epilogue.json",
    ] {
        let result = std::process::Command::new("git")
            .args(["show", &format!("HEAD:{path}")])
            .current_dir(root)
            .output()?;
        if result.status.success() {
            return Ok(serde_json::to_string(&serde_json::from_slice::<IntentProjection>(
                &result.stdout,
            )?)?);
        }
    }
    let projection = IntentProjection {
        goals: serde_json::from_str(&command(root, "et", &["list", "goals", "--all", "--json"])?)?,
        interactions: serde_json::from_str(&command(
            root,
            "et",
            &["list", "interactions", "--all", "--json"],
        )?)?,
        journeys: serde_json::from_str(&command(
            root,
            "et",
            &["list", "journeys", "--all", "--json"],
        )?)?,
    };
    Ok(serde_json::to_string(&projection)?)
}

pub fn capture(
    initiative: &Initiative,
    registry: &Registry,
) -> anyhow::Result<SteeringExternalContext> {
    let mut repositories = Vec::new();
    for project in &initiative.projects {
        let entry = registry
            .find_project(project)
            .ok_or_else(|| anyhow::anyhow!("initiative project is no longer registered"))?;
        let root = Path::new(&entry.path).canonicalize()?;
        let branch = format!("refs/heads/{}", entry.branch);
        repositories.push(SteeringRepositoryIdentity {
            project: project.clone(),
            root: root.to_string_lossy().into_owned(),
            registry: serde_json::to_string(entry)?,
            trunk: command(&root, "git", &["rev-parse", "--verify", "--end-of-options", &branch])?,
            head: command(&root, "git", &["rev-parse", "--verify", "HEAD"])?,
            status: command(&root, "git", &["status", "--porcelain", "--untracked-files=all"])?,
            diff: command(&root, "git", &["diff", "HEAD", "--"])?,
        });
    }
    Ok(SteeringExternalContext {
        repositories,
        derive_source: initiative.derive_source.clone(),
        intent: initiative
            .derive_source
            .as_ref()
            .map(|source| projection(Path::new(&source.repo)))
            .transpose()?,
        brief: initiative.brief.clone(),
        brief_content: initiative
            .brief
            .as_ref()
            .map(|brief| std::fs::read_to_string(&brief.path))
            .transpose()?,
    })
}

/// Validate saved external evidence before acquiring any store or ledger lock.
/// Synchronous for durable admission; async callers must use `spawn_blocking`.
pub fn validate_steering_external_context(
    expected: &SteeringExternalContext,
    registry: &Registry,
) -> anyhow::Result<bool> {
    // Only current external inputs are read. No initiative persistence format is consumed.
    for repo in &expected.repositories {
        let Some(entry) = registry.find_project(&repo.project) else {
            return Ok(false);
        };
        let root = Path::new(&entry.path).canonicalize()?;
        if root.to_string_lossy() != repo.root || serde_json::to_string(entry)? != repo.registry {
            return Ok(false);
        }
        let branch = format!("refs/heads/{}", entry.branch);
        if command(&root, "git", &["rev-parse", "--verify", "--end-of-options", &branch])?
            != repo.trunk
            || command(&root, "git", &["rev-parse", "--verify", "HEAD"])? != repo.head
            || command(&root, "git", &["status", "--porcelain", "--untracked-files=all"])?
                != repo.status
            || command(&root, "git", &["diff", "HEAD", "--"])? != repo.diff
        {
            return Ok(false);
        }
    }
    let intent = expected
        .derive_source
        .as_ref()
        .map(|source| projection(Path::new(&source.repo)))
        .transpose()?;
    let brief = expected
        .brief
        .as_ref()
        .map(|brief| std::fs::read_to_string(&brief.path))
        .transpose()?;
    Ok(intent == expected.intent && brief == expected.brief_content)
}
