use std::path::Path;

use crate::gateway::ShellGateway;

/// Verify the tag points at HEAD when the agent succeeded and the project is a git repo.
///
/// Returns `(success, summary)`. Skips the check when:
/// - the agent did not succeed (`cli_success == false`)
/// - no tag was extracted (`new_tag.is_none()`)
/// - the project path has no `.git` directory (test environment)
pub(super) async fn check_tag_at_head(
    cli_success: bool,
    new_tag: Option<&str>,
    cli_summary: String,
    project_dir: &Path,
    shell: &dyn ShellGateway,
) -> (bool, String) {
    if !cli_success {
        return (false, cli_summary);
    }
    let Some(tag) = new_tag else {
        return (true, cli_summary);
    };
    if !project_dir.join(".git").exists() {
        // Not a git repo (test environment) — skip verification.
        return (true, cli_summary);
    }
    match verify_tag_at_head(project_dir, tag, shell).await {
        Ok(true) => (true, cli_summary),
        Ok(false) => {
            tracing::error!(
                tag = %tag,
                "tag does not point at HEAD; release may have tagged the wrong commit"
            );
            (
                false,
                format!(
                    "Tag {tag} does not point at HEAD; \
                     the release may have tagged the wrong commit"
                ),
            )
        }
        Err(err) => {
            tracing::error!(tag = %tag, error = %err, "could not verify tag position");
            (false, format!("Could not verify tag {tag} position: {err}"))
        }
    }
}

/// Verify that `tag` points at the same commit as HEAD in `project_dir`.
///
/// Returns `Ok(true)` when the tag and HEAD resolve to the same commit,
/// `Ok(false)` when they differ or when the tag does not exist, and
/// `Err` on unexpected shell failures.
async fn verify_tag_at_head(
    project_dir: &Path,
    tag: &str,
    shell: &dyn ShellGateway,
) -> anyhow::Result<bool> {
    // Resolve the commit that the tag points to.
    let tag_ref = format!("{tag}^{{commit}}");
    let tag_result = shell.run(project_dir, "git", &["rev-parse", &tag_ref], None, None).await?;

    if !tag_result.success {
        // Tag does not exist or cannot be resolved.
        tracing::warn!(tag = %tag, stderr = %tag_result.stderr, "git rev-parse tag failed");
        return Ok(false);
    }

    let tag_commit = tag_result.stdout.trim().to_string();

    // Resolve HEAD.
    let head_result = shell.run(project_dir, "git", &["rev-parse", "HEAD"], None, None).await?;

    if !head_result.success {
        anyhow::bail!("git rev-parse HEAD failed: {}", head_result.stderr);
    }

    let head_commit = head_result.stdout.trim().to_string();

    Ok(tag_commit == head_commit)
}

/// Scan output words for a semver tag of the form `v<major>.<minor>.<patch>`.
pub(super) fn extract_version_tag(output: &str) -> Option<String> {
    for word in output.split_whitespace() {
        // Strip trailing punctuation before matching.
        let w = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.');
        if w.starts_with('v')
            && w.len() > 1
            && w[1..].split('.').count() == 3
            && w[1..].split('.').all(|part| part.chars().all(char::is_numeric))
        {
            return Some(w.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_version_tag_finds_semver() {
        let output = "Release complete! Tagged as v1.2.3 and pushed.";
        assert_eq!(extract_version_tag(output), Some("v1.2.3".to_string()));
    }

    #[test]
    fn extract_version_tag_returns_none_when_absent() {
        assert_eq!(extract_version_tag("No version info here."), None);
    }

    #[test]
    fn extract_version_tag_ignores_non_semver() {
        assert_eq!(extract_version_tag("version v1.2 released"), None);
    }
}
