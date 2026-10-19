//! What a failed release left in the registered checkout.
//!
//! A release runs in the registered checkout itself, so an agent that stops
//! part way leaves its edits and local commits there. Foundry reports them and
//! never cleans them: the checkout may hold another session's work, and
//! destroying files it does not own is worse than leaving them.
//!
//! The parsing, diffing and wording are pure; [`probe`] and [`snapshot`] are
//! the thin shell over `git`.

use std::path::{Path, PathBuf};

use crate::gateway::ShellGateway;

/// Paths named in the failure summary before the rest are counted.
const LISTED_PATHS: usize = 10;

/// The uncommitted state of a checkout at one moment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct CheckoutState {
    /// Paths that differ from HEAD, relative to the repository root, in the
    /// order `git status` lists them. A rename contributes both its paths.
    pub dirty: Vec<String>,
    /// Commits on HEAD that its upstream lacks; `None` when unknown, for
    /// example a branch with no upstream.
    pub unpushed: Option<u32>,
}

/// What changed in a checkout between the start and the end of a release.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Leftovers {
    /// Paths dirty now that were clean when the release started.
    pub left: Vec<String>,
    /// Paths that were already dirty when the release started and still are.
    pub already_dirty: Vec<String>,
    /// Commits not pushed now.
    pub unpushed: Option<u32>,
    /// Commits that were already not pushed when the release started.
    pub unpushed_before: Option<u32>,
}

/// Parse `git status --porcelain=v1 -z` output into the paths it names.
///
/// Each entry is `XY PATH`, NUL-terminated. A rename or copy (`R` or `C` in
/// either column) is followed by one more NUL-terminated field, the original
/// path. Malformed entries are skipped.
pub(super) fn parse_porcelain_z(output: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut fields = output.split('\0');
    while let Some(entry) = fields.next() {
        let Some((status, path)) = entry.split_at_checked(2) else {
            continue;
        };
        let Some(path) = path.strip_prefix(' ') else {
            continue;
        };
        push_unique(&mut paths, path);
        if status.contains(['R', 'C'])
            && let Some(original) = fields.next()
        {
            push_unique(&mut paths, original);
        }
    }
    paths
}

fn push_unique(paths: &mut Vec<String>, path: &str) {
    if !path.is_empty() && !paths.iter().any(|p| p == path) {
        paths.push(path.to_string());
    }
}

/// Separate what the release left behind from what was there before it.
pub(super) fn leftovers(before: &CheckoutState, after: &CheckoutState) -> Leftovers {
    let (already_dirty, left) =
        after.dirty.iter().cloned().partition(|path| before.dirty.contains(path));
    Leftovers {
        left,
        already_dirty,
        unpushed: after.unpushed,
        unpushed_before: before.unpushed,
    }
}

/// The sentences a failure summary gains, or `None` when the release left
/// nothing to report.
pub(super) fn describe(checkout: &Path, leftovers: &Leftovers) -> Option<String> {
    let mut sentences = Vec::new();
    if !leftovers.left.is_empty() {
        sentences.push(format!(
            "Left uncommitted in {}: {}.",
            checkout.display(),
            path_list(&leftovers.left)
        ));
    }
    if !leftovers.already_dirty.is_empty() {
        sentences.push(format!(
            "Already uncommitted before the release: {}.",
            path_list(&leftovers.already_dirty)
        ));
    }
    if let Some(count) = leftovers.unpushed.filter(|&n| n > 0) {
        let noun = if count == 1 { "commit" } else { "commits" };
        let already = leftovers
            .unpushed_before
            .filter(|&n| n > 0)
            .map(|before| format!(" ({} already before the release)", before.min(count)))
            .unwrap_or_default();
        sentences.push(format!("{count} local {noun} not pushed{already}."));
    }
    (!sentences.is_empty()).then(|| sentences.join(" "))
}

fn path_list(paths: &[String]) -> String {
    let shown = paths
        .iter()
        .take(LISTED_PATHS)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let rest = paths.len().saturating_sub(LISTED_PATHS);
    let more = if rest > 0 {
        format!(" and {rest} more")
    } else {
        String::new()
    };
    let noun = if paths.len() == 1 { "file" } else { "files" };
    format!("{shown}{more} ({} {noun})", paths.len())
}

/// Append `sentences` to `summary` as further sentences.
pub(super) fn append_to_summary(summary: &str, sentences: &str) -> String {
    let summary = summary.trim_end();
    if summary.is_empty() {
        sentences.to_string()
    } else if summary.ends_with(['.', '!', '?']) {
        format!("{summary} {sentences}")
    } else {
        format!("{summary}. {sentences}")
    }
}

/// The root of the git work tree containing `dir`, or `None` when `dir` is not
/// inside one or git cannot say.
///
/// A registered path may be a subdirectory of a monorepo, so this asks git
/// rather than looking for `dir/.git`.
pub(super) async fn probe(dir: &Path, shell: &dyn ShellGateway) -> Option<PathBuf> {
    match shell.run(dir, "git", &["rev-parse", "--show-toplevel"], None, None).await {
        Ok(result) if result.success && !result.stdout.trim().is_empty() => {
            Some(PathBuf::from(result.stdout.trim()))
        }
        Ok(result) => {
            tracing::warn!(
                dir = %dir.display(),
                stderr = %result.stderr.trim(),
                "release checkout is not a git work tree; it will not be synced and leftovers will not be reported"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                dir = %dir.display(),
                %error,
                "could not run git in the release checkout; it will not be synced and leftovers will not be reported"
            );
            None
        }
    }
}

/// Read the uncommitted state of the work tree containing `dir`, or `None`
/// when `git status` fails.
pub(super) async fn snapshot(dir: &Path, shell: &dyn ShellGateway) -> Option<CheckoutState> {
    let status = match shell.run(dir, "git", &["status", "--porcelain=v1", "-z"], None, None).await
    {
        Ok(result) if result.success => result.stdout,
        Ok(result) => {
            tracing::warn!(stderr = %result.stderr.trim(), "git status failed in the release checkout");
            return None;
        }
        Err(error) => {
            tracing::warn!(%error, "could not run git status in the release checkout");
            return None;
        }
    };
    Some(CheckoutState {
        dirty: parse_porcelain_z(&status),
        unpushed: unpushed_commits(dir, shell).await,
    })
}

/// Commits on HEAD that its upstream lacks.
async fn unpushed_commits(dir: &Path, shell: &dyn ShellGateway) -> Option<u32> {
    match shell
        .run(dir, "git", &["rev-list", "--count", "@{upstream}..HEAD"], None, None)
        .await
    {
        Ok(result) if result.success => result.stdout.trim().parse().ok(),
        // Best-effort: a branch with no upstream has no unpushed count to
        // report, and the uncommitted paths are still reported without it.
        Ok(result) => {
            tracing::debug!(stderr = %result.stderr.trim(), "no upstream to count unpushed commits against");
            None
        }
        Err(error) => {
            tracing::warn!(%error, "could not count unpushed commits in the release checkout");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dirty: &[&str], unpushed: Option<u32>) -> CheckoutState {
        CheckoutState {
            dirty: dirty.iter().map(ToString::to_string).collect(),
            unpushed,
        }
    }

    #[test]
    fn porcelain_lists_modified_untracked_and_both_sides_of_a_rename() {
        let output = "R  new name.txt\0old.txt\0 M sub/x\0?? dd/\0";
        assert_eq!(parse_porcelain_z(output), ["new name.txt", "old.txt", "sub/x", "dd/"]);
    }

    #[test]
    fn porcelain_of_a_clean_tree_is_empty() {
        assert_eq!(parse_porcelain_z(""), [] as [std::string::String; 0]);
    }

    #[test]
    fn porcelain_skips_malformed_entries() {
        assert_eq!(parse_porcelain_z("M\0 M ok.rs\0"), ["ok.rs"]);
    }

    #[test]
    fn leftovers_separate_new_dirt_from_dirt_that_was_already_there() {
        let before = state(&["notes.md"], Some(0));
        let after = state(&["mix.exs", "notes.md", "CHANGELOG.md"], Some(1));

        let found = leftovers(&before, &after);

        assert_eq!(found.left, ["mix.exs", "CHANGELOG.md"]);
        assert_eq!(found.already_dirty, ["notes.md"]);
        assert_eq!(found.unpushed, Some(1));
    }

    #[test]
    fn describe_names_the_files_and_unpushed_commits() {
        let found = leftovers(&state(&[], Some(0)), &state(&["mix.exs", "CHANGELOG.md"], Some(1)));

        let sentence = describe(Path::new("/work/mojentic-unify"), &found).expect("something left");

        assert_eq!(
            sentence,
            "Left uncommitted in /work/mojentic-unify: mix.exs, CHANGELOG.md (2 files). \
             1 local commit not pushed."
        );
    }

    #[test]
    fn describe_reports_pre_existing_dirt_and_commits_as_pre_existing() {
        let found = leftovers(&state(&["notes.md"], Some(2)), &state(&["notes.md"], Some(3)));

        let sentence = describe(Path::new("/w"), &found).expect("something to say");

        assert!(!sentence.contains("Left uncommitted"), "{sentence}");
        assert_eq!(
            sentence,
            "Already uncommitted before the release: notes.md (1 file). \
             3 local commits not pushed (2 already before the release)."
        );
    }

    #[test]
    fn describe_caps_the_listed_paths() {
        let paths: Vec<String> = (1..=13).map(|n| format!("f{n}")).collect();
        let found = Leftovers {
            left: paths,
            ..Leftovers::default()
        };

        let sentence = describe(Path::new("/w"), &found).expect("something left");

        assert!(sentence.contains("f10 and 3 more (13 files)."), "{sentence}");
        assert!(!sentence.contains("f11"), "{sentence}");
    }

    #[test]
    fn describe_says_nothing_when_the_release_left_nothing() {
        let found = leftovers(&state(&[], None), &state(&[], Some(0)));
        assert_eq!(describe(Path::new("/w"), &found), None);
    }

    #[test]
    fn append_to_summary_starts_a_new_sentence() {
        assert_eq!(append_to_summary("timed out", "Left x."), "timed out. Left x.");
        assert_eq!(append_to_summary("Timed out.", "Left x."), "Timed out. Left x.");
        assert_eq!(append_to_summary("", "Left x."), "Left x.");
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// The mojentic-ex shape against real git: the registered path is a
    /// subdirectory of a monorepo, and an unstaged edit's porcelain entry
    /// starts with a space that must survive the shell.
    #[tokio::test]
    async fn a_monorepo_subdirectory_reports_its_edits_relative_to_the_root() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q", "-b", "main"]);
        git(root.path(), &["config", "user.email", "test@example.com"]);
        git(root.path(), &["config", "user.name", "Test"]);
        let project = root.path().join("mojentic-ex");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("mix.exs"), "@version \"2.1.0\"\n").unwrap();
        git(root.path(), &["add", "-A"]);
        git(root.path(), &["commit", "-q", "-m", "init"]);
        let shell = crate::gateway::ProcessShellGateway;

        let checkout = probe(&project, &shell).await.expect("inside a work tree");
        let before = snapshot(&project, &shell).await.expect("git status runs");
        std::fs::write(project.join("mix.exs"), "@version \"3.0.0\"\n").unwrap();
        std::fs::write(project.join("CHANGELOG.md"), "## [3.0.0]\n").unwrap();
        let after = snapshot(&project, &shell).await.expect("git status runs");

        assert_eq!(checkout.canonicalize().unwrap(), root.path().canonicalize().unwrap());
        assert!(before.dirty.is_empty(), "{before:?}");
        assert_eq!(before.unpushed, None, "no upstream to count against");
        assert_eq!(
            leftovers(&before, &after).left,
            ["mojentic-ex/mix.exs", "mojentic-ex/CHANGELOG.md"]
        );
    }
}
