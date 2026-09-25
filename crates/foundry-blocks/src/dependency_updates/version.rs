//! Version parsing, ordering and classification across ecosystems.
//!
//! Every ecosystem spells versions a little differently (`v1.2.3` git tags,
//! `2026.3.post1` on PyPI, `33.0.0-jre` on Maven Central), but the questions
//! the classifier asks are the same: is this release stable, is it newer, and
//! is the move a patch, a minor or a major? This module answers them with one
//! [`Version`] type whose parsing is ecosystem-aware.

use std::cmp::Ordering;

use foundry_sdk::payload::{Ecosystem, UpdateClass};

/// How a version's qualifier ranks against a plain release of the same numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Qualifier {
    /// A pre-release (`-rc.1`, `a1`, `-M2`, `.dev3`, `-SNAPSHOT`).
    Pre,
    /// A release. Also Maven's release qualifiers (`-jre`, `.Final`).
    Release,
    /// A PyPI post-release (`.post1`): newer than the plain release.
    Post,
}

/// A parsed version.
#[derive(Debug, Clone)]
pub struct Version {
    /// The numeric release components, `[1, 2, 3]` for `1.2.3`.
    release: Vec<u64>,
    qualifier: Qualifier,
    /// The qualifier text, lowercased, for tie-breaking (`"rc.1"`, `"jre"`).
    tag: String,
    raw: String,
}

impl Version {
    /// Parse `raw` as a version of `ecosystem`. `None` when it has no leading
    /// numeric release (a branch name, a git SHA, `latest`).
    pub fn parse(ecosystem: Ecosystem, raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let mut text = trimmed;
        if let Some(rest) = text.strip_prefix(['v', 'V'])
            && rest.starts_with(|c: char| c.is_ascii_digit())
        {
            text = rest;
        }
        if ecosystem == Ecosystem::Pypi
            && let Some((epoch, rest)) = text.split_once('!')
            && epoch.chars().all(|c| c.is_ascii_digit())
        {
            text = rest;
        }

        let (release, rest) = split_release(text)?;
        let rest = match ecosystem {
            // Build metadata (`+build.5`, PEP 440 local labels) never ranks.
            Ecosystem::Cargo
            | Ecosystem::Npm
            | Ecosystem::Hex
            | Ecosystem::Swiftpm
            | Ecosystem::Pypi => rest.split('+').next().unwrap_or(""),
            Ecosystem::Maven => rest,
        };
        let tag = rest.trim_start_matches(['-', '.', '_']).to_ascii_lowercase();
        let qualifier = classify_qualifier(ecosystem, &tag);
        Some(Self {
            release,
            qualifier,
            tag,
            raw: trimmed.to_string(),
        })
    }

    /// The version as it was written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The numeric release components.
    pub fn release(&self) -> &[u64] {
        &self.release
    }

    /// A release (or post-release) rather than a pre-release.
    pub fn is_stable(&self) -> bool {
        self.qualifier != Qualifier::Pre
    }

    /// Whether `candidate` is a later release than `self`, not just a flavor
    /// of the same one.
    ///
    /// The release numbers must move (or it is a `PyPI` post-release), and on
    /// Maven the qualifier must match: `33.1.0-jre` follows `33.0.0-jre`, but
    /// `0.8.0-0.6.x-compat` is a variant of `0.8.0`, not an upgrade, and
    /// `33.1.0-android` is not an upgrade for a `-jre` user.
    pub fn is_upgrade_to(&self, candidate: &Version) -> bool {
        if candidate <= self {
            return false;
        }
        let release_moved = padded_cmp(&candidate.release, &self.release) == Ordering::Greater;
        let post_release = candidate.qualifier == Qualifier::Post;
        let same_flavor = self.qualifier != Qualifier::Release
            || candidate.qualifier != Qualifier::Release
            || candidate.tag == self.tag;
        (release_moved || post_release) && same_flavor
    }

    /// Component `index` of the release, `0` when absent.
    pub fn part(&self, index: usize) -> u64 {
        self.release.get(index).copied().unwrap_or(0)
    }

    /// How big the move from `self` to `to` is, or `None` when `to` is not newer.
    ///
    /// Semver-aware: with a `0` major, a change to the minor component is a
    /// major move; with `0.0`, any change to the patch component is too.
    pub fn class_to(&self, to: &Version) -> Option<UpdateClass> {
        if to <= self {
            return None;
        }
        let class = if to.part(0) != self.part(0) {
            UpdateClass::Major
        } else if self.part(0) == 0 {
            let minor_moved = to.part(1) != self.part(1);
            let zero_zero_patch_moved = self.part(1) == 0 && to.part(2) != self.part(2);
            if minor_moved || zero_zero_patch_moved {
                UpdateClass::Major
            } else {
                UpdateClass::Patch
            }
        } else if to.part(1) != self.part(1) {
            UpdateClass::Minor
        } else {
            UpdateClass::Patch
        };
        Some(class)
    }

    /// Whether this version is inside a hold's `max` prefix (`"1.1"` admits
    /// every `1.1.x` and below). `None` when `max` is not a version prefix.
    pub fn within_prefix(&self, max: &str) -> Option<bool> {
        let (limit, rest) = split_release(max.trim().trim_start_matches(['v', 'V']))?;
        if !rest.is_empty() {
            return None;
        }
        let own: Vec<u64> = (0..limit.len()).map(|i| self.part(i)).collect();
        Some(own <= limit)
    }
}

/// Split a leading `N(.N)*` release off `text`. `None` when there is none.
pub(crate) fn split_release(text: &str) -> Option<(Vec<u64>, &str)> {
    let bytes = text.as_bytes();
    let mut release = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            break;
        }
        release.push(text[start..i].parse().ok()?);
        // Continue only across a dot that is followed by another number.
        if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
            i += 1;
        } else {
            break;
        }
    }
    if release.is_empty() {
        return None;
    }
    Some((release, &text[i..]))
}

/// Qualifier words that mark a Maven version as a pre-release.
const MAVEN_PRE_WORDS: &[&str] = &[
    "a",
    "alpha",
    "b",
    "beta",
    "rc",
    "cr",
    "m",
    "milestone",
    "snapshot",
    "preview",
    "eap",
    "dev",
    "pre",
    "ea",
    "incubating",
    "canary",
    "nightly",
];

fn classify_qualifier(ecosystem: Ecosystem, tag: &str) -> Qualifier {
    if tag.is_empty() {
        return Qualifier::Release;
    }
    match ecosystem {
        Ecosystem::Pypi => {
            // PEP 440 post-releases: `.post1`, `-rev1`, `-r1`, or a bare `-1`.
            let is_post = tag.starts_with("post")
                || tag.starts_with("rev")
                || tag.strip_prefix('r').is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
                || tag.chars().all(|c| c.is_ascii_digit());
            if is_post {
                Qualifier::Post
            } else {
                Qualifier::Pre
            }
        }
        Ecosystem::Maven => {
            let words = tag.split(|c: char| !c.is_ascii_alphabetic()).filter(|w| !w.is_empty());
            if words.clone().any(|w| MAVEN_PRE_WORDS.contains(&w)) {
                Qualifier::Pre
            } else {
                Qualifier::Release
            }
        }
        Ecosystem::Cargo | Ecosystem::Npm | Ecosystem::Hex | Ecosystem::Swiftpm => Qualifier::Pre,
    }
}

fn padded_cmp(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    for i in 0..len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        padded_cmp(&self.release, &other.release)
            .then(self.qualifier.cmp(&other.qualifier))
            .then_with(|| natural_cmp(&self.tag, &other.tag))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// Compare qualifier text with digit runs compared as numbers (`rc.10 > rc.9`).
fn natural_cmp(left: &str, right: &str) -> Ordering {
    let mut lhs = left.chars().peekable();
    let mut rhs = right.chars().peekable();
    loop {
        let order = match (lhs.peek().copied(), rhs.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(l), Some(r)) if l.is_ascii_digit() && r.is_ascii_digit() => {
                take_number(&mut lhs).cmp(&take_number(&mut rhs))
            }
            (Some(l), Some(r)) => {
                lhs.next();
                rhs.next();
                l.cmp(&r)
            }
        };
        if order != Ordering::Equal {
            return order;
        }
    }
}

fn take_number(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> u64 {
    let mut n: u64 = 0;
    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
        n = n.saturating_mul(10).saturating_add(u64::from(c) - u64::from('0'));
        it.next();
    }
    n
}

/// Parse every stable version out of `raw`, skipping anything unparseable.
pub fn stable_versions<'a>(
    ecosystem: Ecosystem,
    raw: impl IntoIterator<Item = &'a str>,
) -> Vec<Version> {
    let mut versions: Vec<Version> = raw
        .into_iter()
        .filter_map(|v| Version::parse(ecosystem, v))
        .filter(Version::is_stable)
        .collect();
    versions.sort();
    versions.dedup();
    versions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(eco: Ecosystem, s: &str) -> Version {
        Version::parse(eco, s).unwrap_or_else(|| panic!("{s} should parse"))
    }

    fn class(eco: Ecosystem, from: &str, to: &str) -> Option<UpdateClass> {
        v(eco, from).class_to(&v(eco, to))
    }

    #[test]
    fn plain_semver_classes() {
        assert_eq!(class(Ecosystem::Cargo, "1.2.3", "1.2.4"), Some(UpdateClass::Patch));
        assert_eq!(class(Ecosystem::Cargo, "1.2.3", "1.3.0"), Some(UpdateClass::Minor));
        assert_eq!(class(Ecosystem::Cargo, "1.2.3", "2.0.0"), Some(UpdateClass::Major));
        assert_eq!(class(Ecosystem::Cargo, "1.2.3", "1.2.3"), None);
        assert_eq!(class(Ecosystem::Cargo, "1.2.3", "1.2.2"), None);
    }

    #[test]
    fn a_zero_major_minor_bump_is_major() {
        assert_eq!(class(Ecosystem::Cargo, "0.3.1", "0.4.0"), Some(UpdateClass::Major));
        assert_eq!(class(Ecosystem::Hex, "0.7.4", "0.8.0"), Some(UpdateClass::Major));
        assert_eq!(class(Ecosystem::Cargo, "0.3.1", "0.3.9"), Some(UpdateClass::Patch));
        assert_eq!(class(Ecosystem::Cargo, "0.3.1", "1.0.0"), Some(UpdateClass::Major));
    }

    #[test]
    fn a_zero_zero_patch_bump_is_major() {
        assert_eq!(class(Ecosystem::Npm, "0.0.3", "0.0.4"), Some(UpdateClass::Major));
        assert_eq!(class(Ecosystem::Npm, "0.0.3", "0.1.0"), Some(UpdateClass::Major));
    }

    #[test]
    fn short_and_long_releases_pad_with_zeros() {
        assert_eq!(v(Ecosystem::Pypi, "1.2"), v(Ecosystem::Pypi, "1.2.0"));
        assert_eq!(class(Ecosystem::Pypi, "2.6", "2.6.1"), Some(UpdateClass::Patch));
        assert_eq!(class(Ecosystem::Maven, "1.2.3.4", "1.2.3.5"), Some(UpdateClass::Patch));
    }

    #[test]
    fn calendar_versions_classify_by_their_first_component() {
        assert_eq!(class(Ecosystem::Pypi, "2025.2", "2026.3.post1"), Some(UpdateClass::Major));
        assert_eq!(class(Ecosystem::Pypi, "2026.1", "2026.3"), Some(UpdateClass::Minor));
    }

    #[test]
    fn git_tags_may_carry_a_v() {
        assert_eq!(v(Ecosystem::Swiftpm, "v1.15.1"), v(Ecosystem::Swiftpm, "1.15.1"));
        assert_eq!(v(Ecosystem::Swiftpm, "v1.15.1").as_str(), "v1.15.1");
    }

    #[test]
    fn prereleases_are_unstable_per_ecosystem() {
        assert!(!v(Ecosystem::Cargo, "1.0.0-beta.2").is_stable());
        assert!(!v(Ecosystem::Npm, "5.0.0-rc.1").is_stable());
        assert!(!v(Ecosystem::Hex, "1.8.0-rc.0").is_stable());
        assert!(!v(Ecosystem::Pypi, "2.0.0rc1").is_stable());
        assert!(!v(Ecosystem::Pypi, "2.0.0.dev3").is_stable());
        assert!(!v(Ecosystem::Pypi, "2.0.0a1").is_stable());
        assert!(!v(Ecosystem::Maven, "2.1.0-RC").is_stable());
        assert!(!v(Ecosystem::Maven, "3.0.0-M2").is_stable());
        assert!(!v(Ecosystem::Maven, "1.0.0-alpha01").is_stable());
        assert!(!v(Ecosystem::Maven, "2.2.0-Beta1").is_stable());
        assert!(!v(Ecosystem::Maven, "1.0-SNAPSHOT").is_stable());
        assert!(!v(Ecosystem::Maven, "2.4.0-dev-123").is_stable());
    }

    #[test]
    fn release_qualifiers_stay_stable() {
        assert!(v(Ecosystem::Maven, "33.0.0-jre").is_stable());
        assert!(v(Ecosystem::Maven, "5.6.15.Final").is_stable());
        assert!(v(Ecosystem::Pypi, "2026.3.post1").is_stable());
        assert!(v(Ecosystem::Cargo, "1.0.0+build.5").is_stable());
    }

    #[test]
    fn prereleases_sort_before_the_release_and_posts_after() {
        assert!(v(Ecosystem::Npm, "2.0.0-rc.1") < v(Ecosystem::Npm, "2.0.0"));
        assert!(v(Ecosystem::Npm, "2.0.0-rc.9") < v(Ecosystem::Npm, "2.0.0-rc.10"));
        assert!(v(Ecosystem::Pypi, "2026.3") < v(Ecosystem::Pypi, "2026.3.post1"));
    }

    #[test]
    fn maven_flavors_are_not_upgrades_of_each_other() {
        let jre = v(Ecosystem::Maven, "33.0.0-jre");
        assert!(jre.is_upgrade_to(&v(Ecosystem::Maven, "33.1.0-jre")));
        assert!(!jre.is_upgrade_to(&v(Ecosystem::Maven, "33.1.0-android")));
        let plain = v(Ecosystem::Maven, "0.8.0");
        assert!(!plain.is_upgrade_to(&v(Ecosystem::Maven, "0.8.0-0.6.x-compat")));
        assert!(!plain.is_upgrade_to(&v(Ecosystem::Maven, "0.9.0-0.6.x-compat")));
        assert!(plain.is_upgrade_to(&v(Ecosystem::Maven, "0.9.0")));
    }

    #[test]
    fn post_releases_are_upgrades_and_prereleases_of_newer_releases_count() {
        assert!(v(Ecosystem::Pypi, "2026.3").is_upgrade_to(&v(Ecosystem::Pypi, "2026.3.post1")));
        assert!(!v(Ecosystem::Cargo, "1.2.0").is_upgrade_to(&v(Ecosystem::Cargo, "1.2.0")));
        assert!(v(Ecosystem::Cargo, "1.2.0").is_upgrade_to(&v(Ecosystem::Cargo, "1.2.1")));
    }

    #[test]
    fn things_that_are_not_versions_do_not_parse() {
        assert!(Version::parse(Ecosystem::Swiftpm, "main").is_none());
        assert!(Version::parse(Ecosystem::Npm, "latest").is_none());
        assert!(Version::parse(Ecosystem::Swiftpm, "").is_none());
    }

    #[test]
    fn hold_prefixes() {
        let within = |ver: &str, max: &str| v(Ecosystem::Hex, ver).within_prefix(max).unwrap();
        assert!(within("1.1.9", "1.1"));
        assert!(within("1.0.0", "1.1"));
        assert!(!within("1.2.0", "1.1"));
        assert!(within("1.9.0", "1"));
        assert!(!within("2.0.0", "1"));
        assert!(within("1.1.3", "1.1.3"));
        assert!(!within("1.1.4", "1.1.3"));
        assert_eq!(v(Ecosystem::Hex, "1.0.0").within_prefix("one"), None);
    }

    #[test]
    fn stable_versions_sorts_dedupes_and_drops_prereleases() {
        let got = stable_versions(
            Ecosystem::Cargo,
            ["1.2.0", "1.10.0", "1.9.0", "2.0.0-rc.1", "junk", "1.9.0"],
        );
        let raw: Vec<&str> = got.iter().map(Version::as_str).collect();
        assert_eq!(raw, ["1.2.0", "1.9.0", "1.10.0"]);
    }
}
