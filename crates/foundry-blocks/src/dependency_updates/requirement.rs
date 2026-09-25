//! Manifest constraints, per ecosystem.
//!
//! A constraint decides which releases a lockfile refresh may pick without a
//! manifest edit. Each ecosystem has its own operators, and the classifier has
//! to read them correctly or it will call a constraint edit a lockfile move:
//!
//! | Ecosystem | Operators |
//! | --- | --- |
//! | Cargo | bare and `^` (caret), `~`, `=`, `<`, `<=`, `>`, `>=`, `*` wildcards, comma-joined |
//! | Hex | `~>`, `==`, `!=`, `<`, `<=`, `>`, `>=`, joined by `and` / `or` |
//! | npm | `^`, `~`, x-ranges, hyphen ranges, comparators, `||` |
//! | PyPI | PEP 440: `~=`, `==` (with `.*`), `!=`, `<`, `<=`, `>`, `>=`, `===` |
//! | Gradle catalog | exact versions, `1.+` prefixes, Maven ranges `[1.0,2.0)` |
//! | SwiftPM | `from:` / `upToNextMajor`, `upToNextMinor`, `exact:`, `a..<b`, `a...b` |
//!
//! [`Requirement::parse`] returns `None` for a constraint it does not
//! understand (a git URL, a path, a workspace reference). The caller then
//! treats the lockfile move as unknown instead of guessing.

use std::cmp::Ordering;

use foundry_sdk::payload::Ecosystem;

use super::version::{Version, split_release};

/// One comparison against a release.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cmp {
    Any,
    Ge(Vec<u64>),
    Gt(Vec<u64>),
    Le(Vec<u64>),
    Lt(Vec<u64>),
    /// Same release numbers (qualifiers ignored).
    Eq(Vec<u64>),
    Ne(Vec<u64>),
    /// The release does not start with this prefix (PEP 440 `!=1.2.*`).
    NotPrefix(Vec<u64>),
}

/// A parsed constraint: any alternative whose comparisons all hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    alternatives: Vec<Vec<Cmp>>,
}

impl Requirement {
    /// Parse `text` as a constraint of `ecosystem`.
    pub fn parse(ecosystem: Ecosystem, text: &str) -> Option<Self> {
        let alternatives = match ecosystem {
            Ecosystem::Cargo => vec![cargo(text)?],
            Ecosystem::Hex => hex(text)?,
            Ecosystem::Npm => npm(text)?,
            Ecosystem::Pypi => vec![pep440(text)?],
            Ecosystem::Maven => vec![maven(text)?],
            Ecosystem::Swiftpm => vec![swiftpm(text)?],
        };
        Some(Self { alternatives })
    }

    /// Whether `version` satisfies the constraint.
    pub fn admits(&self, version: &Version) -> bool {
        let release = version.release();
        self.alternatives.iter().any(|all| all.iter().all(|c| holds(c, release)))
    }
}

fn cmp(a: &[u64], b: &[u64]) -> Ordering {
    let len = a.len().max(b.len());
    (0..len)
        .map(|i| a.get(i).copied().unwrap_or(0).cmp(&b.get(i).copied().unwrap_or(0)))
        .find(|o| *o != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn holds(c: &Cmp, v: &[u64]) -> bool {
    match c {
        Cmp::Any => true,
        Cmp::Ge(b) => cmp(v, b) != Ordering::Less,
        Cmp::Gt(b) => cmp(v, b) == Ordering::Greater,
        Cmp::Le(b) => cmp(v, b) != Ordering::Greater,
        Cmp::Lt(b) => cmp(v, b) == Ordering::Less,
        Cmp::Eq(b) => cmp(v, b) == Ordering::Equal,
        Cmp::Ne(b) => cmp(v, b) != Ordering::Equal,
        Cmp::NotPrefix(p) => {
            !p.iter().enumerate().all(|(i, n)| v.get(i).copied().unwrap_or(0) == *n)
        }
    }
}

/// A partially written version: `1`, `1.2`, `1.2.3`, with `*`/`x` wildcards
/// ending it early. Qualifiers are dropped: candidates are stable releases.
fn partial(text: &str) -> Option<Vec<u64>> {
    let text = text.trim().trim_start_matches(['v', 'V', '=']).trim();
    if text.is_empty() || matches!(text, "*" | "x" | "X") {
        return Some(Vec::new());
    }
    let mut parts = Vec::new();
    for piece in text.split('.') {
        if matches!(piece, "*" | "x" | "X" | "+") {
            break;
        }
        // A qualifier component (`2026.3.post1`, `1.0.Final`) ends the release.
        if !parts.is_empty() && !piece.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        let (numbers, rest) = split_release(piece)?;
        parts.push(*numbers.first()?);
        if !rest.is_empty() {
            // `3-beta.1`, `3+build`: the number counts, the rest does not.
            break;
        }
    }
    Some(parts)
}

/// `prefix` with its last component incremented: the exclusive upper bound of
/// a partial version (`1.2` → `1.3`, `1` → `2`).
fn bump_last(prefix: &[u64]) -> Vec<u64> {
    let mut next = prefix.to_vec();
    if let Some(last) = next.last_mut() {
        *last += 1;
    }
    next
}

/// Everything a partial version covers (`1.2` → `>=1.2.0, <1.3.0`); a full
/// version is an exact match.
fn x_range(parts: &[u64], full_len: usize) -> Vec<Cmp> {
    if parts.is_empty() {
        vec![Cmp::Any]
    } else if parts.len() >= full_len {
        vec![Cmp::Eq(parts.to_vec())]
    } else {
        vec![Cmp::Ge(parts.to_vec()), Cmp::Lt(bump_last(parts))]
    }
}

/// Caret semantics shared by Cargo and npm: allow changes that do not modify
/// the left-most non-zero component.
fn caret(parts: &[u64]) -> Vec<Cmp> {
    let major = parts.first().copied().unwrap_or(0);
    let upper = match (parts.get(1), parts.get(2)) {
        _ if parts.is_empty() => return vec![Cmp::Any],
        _ if major > 0 => vec![major + 1],
        (None, _) => vec![1],
        (Some(&0), None) => vec![0, 1],
        (Some(&minor), _) if minor > 0 => vec![0, minor + 1],
        (Some(_), Some(&patch)) => vec![0, 0, patch + 1],
        (Some(_), None) => vec![0, 1],
    };
    vec![Cmp::Ge(parts.to_vec()), Cmp::Lt(upper)]
}

/// Tilde semantics shared by Cargo and npm: patch-level changes if a minor is
/// given, minor-level changes if not.
fn tilde(parts: &[u64]) -> Vec<Cmp> {
    match parts {
        [] => vec![Cmp::Any],
        [major] => vec![Cmp::Ge(vec![*major]), Cmp::Lt(vec![major + 1])],
        [major, minor, ..] => vec![Cmp::Ge(parts.to_vec()), Cmp::Lt(vec![*major, minor + 1])],
    }
}

/// An operator applied to a partial version, with the npm/Cargo meaning of
/// partials (`>1.2` is `>=1.3.0`, `<=1.2` is `<1.3.0`).
fn comparator(op: &str, parts: &[u64], full_len: usize) -> Option<Vec<Cmp>> {
    let full = parts.len() >= full_len;
    let cmps = match op {
        ">=" => vec![Cmp::Ge(parts.to_vec())],
        "<" => vec![Cmp::Lt(parts.to_vec())],
        ">" if full => vec![Cmp::Gt(parts.to_vec())],
        ">" => vec![Cmp::Ge(bump_last(parts))],
        "<=" if full => vec![Cmp::Le(parts.to_vec())],
        "<=" => vec![Cmp::Lt(bump_last(parts))],
        "=" | "" => x_range(parts, full_len),
        "^" => caret(parts),
        "~" | "~>" => tilde(parts),
        _ => return None,
    };
    if parts.is_empty() && !matches!(op, "=" | "" | "^" | "~" | "~>") {
        return None;
    }
    Some(cmps)
}

/// Split a leading operator off a comparator.
fn split_op<'a>(text: &'a str, ops: &[&'a str]) -> (&'a str, &'a str) {
    let text = text.trim();
    ops.iter()
        .find_map(|op| text.strip_prefix(op).map(|rest| (*op, rest.trim())))
        .unwrap_or(("", text))
}

fn cargo(text: &str) -> Option<Vec<Cmp>> {
    let mut all = Vec::new();
    for piece in text.split(',') {
        let (op, version) = split_op(piece, &[">=", "<=", ">", "<", "=", "^", "~"]);
        let parts = partial(version)?;
        // A bare Cargo version is a caret requirement.
        let op = if op.is_empty() && !version.contains(['*', 'x', 'X']) {
            "^"
        } else {
            op
        };
        all.extend(comparator(op, &parts, 3)?);
    }
    Some(all)
}

fn hex(text: &str) -> Option<Vec<Vec<Cmp>>> {
    text.split(" or ")
        .map(|alternative| {
            let mut all = Vec::new();
            for piece in alternative.split(" and ") {
                let (op, version) = split_op(piece, &["~>", "==", "!=", ">=", "<=", ">", "<"]);
                let parts = partial(version)?;
                if parts.is_empty() {
                    return None;
                }
                match op {
                    // `~> 1.2` allows `< 2.0.0`; `~> 1.2.3` allows `< 1.3.0`.
                    "~>" if parts.len() >= 3 => {
                        all.push(Cmp::Ge(parts.clone()));
                        all.push(Cmp::Lt(vec![parts[0], parts[1] + 1]));
                    }
                    "~>" => {
                        all.push(Cmp::Ge(parts.clone()));
                        all.push(Cmp::Lt(vec![parts[0] + 1]));
                    }
                    "==" | "" => all.push(Cmp::Eq(parts)),
                    "!=" => all.push(Cmp::Ne(parts)),
                    ">=" => all.push(Cmp::Ge(parts)),
                    "<=" => all.push(Cmp::Le(parts)),
                    ">" => all.push(Cmp::Gt(parts)),
                    "<" => all.push(Cmp::Lt(parts)),
                    _ => return None,
                }
            }
            Some(all)
        })
        .collect()
}

/// npm specs that are not version ranges: git, paths, tarballs, aliases, tags.
pub(crate) fn npm_is_not_a_range(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "file:",
        "link:",
        "git",
        "http:",
        "https:",
        "workspace:",
        "npm:",
        "github:",
        "portal:",
        "patch:",
        "catalog:",
    ];
    PREFIXES.iter().any(|p| text.starts_with(p)) || text.contains('/')
}

fn npm(text: &str) -> Option<Vec<Vec<Cmp>>> {
    let text = text.trim();
    if text.is_empty() || text == "latest" {
        return Some(vec![vec![Cmp::Any]]);
    }
    if npm_is_not_a_range(text) {
        return None;
    }
    text.split("||").map(npm_range).collect()
}

fn npm_range(range: &str) -> Option<Vec<Cmp>> {
    let range = range.trim();
    if let Some((low, high)) = range.split_once(" - ") {
        let low = partial(low)?;
        let high = partial(high)?;
        let mut all = vec![Cmp::Ge(low)];
        if high.len() >= 3 {
            all.push(Cmp::Le(high));
        } else if !high.is_empty() {
            all.push(Cmp::Lt(bump_last(&high)));
        }
        return Some(all);
    }
    // Join operators written apart from their version (`>= 1.2`).
    let mut tokens: Vec<String> = Vec::new();
    for token in range.split_whitespace() {
        match tokens.last_mut() {
            Some(prev) if prev.chars().all(|c| "<>=~^".contains(c)) => prev.push_str(token),
            _ => tokens.push(token.to_string()),
        }
    }
    if tokens.is_empty() {
        return Some(vec![Cmp::Any]);
    }
    let mut all = Vec::new();
    for token in &tokens {
        let (op, version) = split_op(token, &[">=", "<=", ">", "<", "=", "^", "~>", "~"]);
        all.extend(comparator(op, &partial(version)?, 3)?);
    }
    Some(all)
}

fn pep440(text: &str) -> Option<Vec<Cmp>> {
    let text = text.trim();
    if text.is_empty() {
        return Some(vec![Cmp::Any]);
    }
    let mut all = Vec::new();
    for piece in text.split(',') {
        let (op, version) = split_op(piece, &["~=", "===", "==", "!=", "<=", ">=", "<", ">"]);
        let wildcard = version.ends_with(".*");
        let parts = partial(version.trim_end_matches(".*"))?;
        if parts.is_empty() {
            return None;
        }
        match (op, wildcard) {
            ("~=", _) if parts.len() >= 2 => {
                let prefix = &parts[..parts.len() - 1];
                all.push(Cmp::Ge(parts.clone()));
                all.push(Cmp::Lt(bump_last(prefix)));
            }
            ("==", true) => {
                all.push(Cmp::Ge(parts.clone()));
                all.push(Cmp::Lt(bump_last(&parts)));
            }
            ("!=", true) => all.push(Cmp::NotPrefix(parts)),
            ("==" | "===", false) => all.push(Cmp::Eq(parts)),
            ("!=", false) => all.push(Cmp::Ne(parts)),
            (">=", false) => all.push(Cmp::Ge(parts)),
            ("<=", false) => all.push(Cmp::Le(parts)),
            (">", false) => all.push(Cmp::Gt(parts)),
            ("<", false) => all.push(Cmp::Lt(parts)),
            _ => return None,
        }
    }
    Some(all)
}

fn maven(text: &str) -> Option<Vec<Cmp>> {
    let text = text.trim();
    if text == "+" {
        return Some(vec![Cmp::Any]);
    }
    if let Some(prefix) = text.strip_suffix(".+") {
        let parts = partial(prefix)?;
        return Some(vec![Cmp::Ge(parts.clone()), Cmp::Lt(bump_last(&parts))]);
    }
    let (Some(open), Some(close)) = (text.chars().next(), text.chars().last()) else {
        return None;
    };
    if matches!(open, '[' | '(' | ']') && matches!(close, ']' | ')' | '[') {
        let inner = &text[1..text.len() - 1];
        let Some((low, high)) = inner.split_once(',') else {
            // `[1.0]`: exactly that version.
            return Some(vec![Cmp::Eq(partial(inner)?)]);
        };
        if high.contains(',') {
            return None;
        }
        let mut all = Vec::new();
        if !low.trim().is_empty() {
            let low = partial(low)?;
            all.push(if open == '[' {
                Cmp::Ge(low)
            } else {
                Cmp::Gt(low)
            });
        }
        if !high.trim().is_empty() {
            let high = partial(high)?;
            all.push(if close == ']' {
                Cmp::Le(high)
            } else {
                Cmp::Lt(high)
            });
        }
        return Some(all);
    }
    // A plain catalog version is a single required version.
    Version::parse(Ecosystem::Maven, text)?;
    Some(vec![Cmp::Eq(partial(text)?)])
}

fn swiftpm(text: &str) -> Option<Vec<Cmp>> {
    let text = text.trim();
    let unquote = |s: &str| s.trim().trim_matches('"').to_string();
    if let Some((low, high)) = text.split_once("..<") {
        return Some(vec![
            Cmp::Ge(partial(&unquote(low))?),
            Cmp::Lt(partial(&unquote(high))?),
        ]);
    }
    if let Some((low, high)) = text.split_once("...") {
        return Some(vec![
            Cmp::Ge(partial(&unquote(low))?),
            Cmp::Le(partial(&unquote(high))?),
        ]);
    }
    let (kind, version) = text.split_once(':')?;
    let parts = partial(&unquote(version))?;
    let major = *parts.first()?;
    match kind.trim() {
        "from" | "upToNextMajor" => Some(vec![Cmp::Ge(parts), Cmp::Lt(vec![major + 1])]),
        "upToNextMinor" => {
            let minor = parts.get(1).copied().unwrap_or(0);
            Some(vec![Cmp::Ge(parts), Cmp::Lt(vec![major, minor + 1])])
        }
        "exact" => Some(vec![Cmp::Eq(parts)]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admits(eco: Ecosystem, req: &str, version: &str) -> bool {
        let r = Requirement::parse(eco, req).unwrap_or_else(|| panic!("{req} should parse"));
        r.admits(&Version::parse(eco, version).unwrap())
    }

    // --- Cargo ---

    #[test]
    fn cargo_bare_versions_are_caret() {
        assert!(admits(Ecosystem::Cargo, "1.2.3", "1.9.0"));
        assert!(!admits(Ecosystem::Cargo, "1.2.3", "2.0.0"));
        assert!(!admits(Ecosystem::Cargo, "1.2.3", "1.2.2"));
        assert!(admits(Ecosystem::Cargo, "1", "1.99.0"));
    }

    #[test]
    fn cargo_caret_on_zero_versions() {
        assert!(admits(Ecosystem::Cargo, "^0.2.3", "0.2.9"));
        assert!(!admits(Ecosystem::Cargo, "^0.2.3", "0.3.0"));
        assert!(admits(Ecosystem::Cargo, "^0.0.3", "0.0.3"));
        assert!(!admits(Ecosystem::Cargo, "^0.0.3", "0.0.4"));
        assert!(admits(Ecosystem::Cargo, "0.14", "0.14.7"));
        assert!(!admits(Ecosystem::Cargo, "0.14", "0.15.0"));
        assert!(!admits(Ecosystem::Cargo, "^0", "1.0.0"));
        assert!(!admits(Ecosystem::Cargo, "^0.0", "0.1.0"));
    }

    #[test]
    fn cargo_tilde_wildcards_and_comparators() {
        assert!(admits(Ecosystem::Cargo, "~1.2.3", "1.2.9"));
        assert!(!admits(Ecosystem::Cargo, "~1.2.3", "1.3.0"));
        assert!(admits(Ecosystem::Cargo, "~1", "1.9.0"));
        assert!(admits(Ecosystem::Cargo, "1.*", "1.9.0"));
        assert!(!admits(Ecosystem::Cargo, "1.2.*", "1.3.0"));
        assert!(admits(Ecosystem::Cargo, "*", "9.0.0"));
        assert!(admits(Ecosystem::Cargo, ">=1.2, <1.5", "1.4.9"));
        assert!(!admits(Ecosystem::Cargo, ">=1.2, <1.5", "1.5.0"));
        assert!(admits(Ecosystem::Cargo, "=1.2.3", "1.2.3"));
        assert!(!admits(Ecosystem::Cargo, "=1.2.3", "1.2.4"));
        assert!(admits(Ecosystem::Cargo, ">=0.5", "3.0.0"));
    }

    // --- Hex ---

    #[test]
    fn elixir_pessimistic_operator() {
        assert!(admits(Ecosystem::Hex, "~> 1.4", "1.9.0"));
        assert!(!admits(Ecosystem::Hex, "~> 1.4", "2.0.0"));
        assert!(admits(Ecosystem::Hex, "~> 1.4.2", "1.4.9"));
        assert!(!admits(Ecosystem::Hex, "~> 1.4.2", "1.5.0"));
        assert!(!admits(Ecosystem::Hex, "~> 1.4.2", "1.4.1"));
        assert!(admits(Ecosystem::Hex, "~> 0.7", "0.9.0"), "~> 0.7 is < 1.0.0");
        assert!(!admits(Ecosystem::Hex, "~> 0.7.4", "0.8.0"));
    }

    #[test]
    fn elixir_and_or_and_comparisons() {
        assert!(admits(Ecosystem::Hex, ">= 1.0.0 and < 2.0.0", "1.5.0"));
        assert!(!admits(Ecosystem::Hex, ">= 1.0.0 and < 2.0.0", "2.0.0"));
        assert!(admits(Ecosystem::Hex, "~> 1.1 or ~> 2.0", "2.3.0"));
        assert!(admits(Ecosystem::Hex, "== 1.2.3", "1.2.3"));
        assert!(!admits(Ecosystem::Hex, "!= 1.2.3", "1.2.3"));
    }

    // --- npm ---

    #[test]
    fn npm_caret_and_tilde() {
        assert!(admits(Ecosystem::Npm, "^1.2.3", "1.9.9"));
        assert!(!admits(Ecosystem::Npm, "^1.2.3", "2.0.0"));
        assert!(admits(Ecosystem::Npm, "^0.2.3", "0.2.9"));
        assert!(!admits(Ecosystem::Npm, "^0.2.3", "0.3.0"));
        assert!(!admits(Ecosystem::Npm, "^0.0.3", "0.0.4"));
        assert!(admits(Ecosystem::Npm, "^1.x", "1.8.0"));
        assert!(admits(Ecosystem::Npm, "~1.2.3", "1.2.9"));
        assert!(!admits(Ecosystem::Npm, "~1.2.3", "1.3.0"));
        assert!(admits(Ecosystem::Npm, "~1", "1.5.0"));
    }

    #[test]
    fn npm_x_ranges_hyphens_and_unions() {
        assert!(admits(Ecosystem::Npm, "1.2.x", "1.2.7"));
        assert!(!admits(Ecosystem::Npm, "1.2", "1.3.0"));
        assert!(admits(Ecosystem::Npm, "1.2.3", "1.2.3"));
        assert!(!admits(Ecosystem::Npm, "1.2.3", "1.2.4"), "a bare full npm version is exact");
        assert!(admits(Ecosystem::Npm, "1.2.3 - 2.3", "2.3.9"));
        assert!(!admits(Ecosystem::Npm, "1.2.3 - 2.3", "2.4.0"));
        assert!(admits(Ecosystem::Npm, "^1.0.0 || ^2.0.0", "2.5.0"));
        assert!(admits(Ecosystem::Npm, ">= 1.2 < 3", "2.9.0"));
        assert!(!admits(Ecosystem::Npm, ">= 1.2 < 3", "3.0.0"));
        assert!(admits(Ecosystem::Npm, "*", "5.0.0"));
        assert!(admits(Ecosystem::Npm, "", "5.0.0"));
    }

    #[test]
    fn npm_non_range_specs_are_not_understood() {
        for spec in [
            "file:../lib",
            "github:user/repo",
            "user/repo",
            "workspace:*",
            "npm:other@1",
            "git+https://x",
        ] {
            assert!(Requirement::parse(Ecosystem::Npm, spec).is_none(), "{spec}");
        }
    }

    // --- PyPI ---

    #[test]
    fn pep440_open_and_compatible_release() {
        assert!(
            admits(Ecosystem::Pypi, ">=0.116.0", "1.8.0"),
            "an open lower bound admits majors"
        );
        assert!(admits(Ecosystem::Pypi, "~=1.4.5", "1.4.9"));
        assert!(!admits(Ecosystem::Pypi, "~=1.4.5", "1.5.0"));
        assert!(admits(Ecosystem::Pypi, "~=1.4", "1.9"));
        assert!(!admits(Ecosystem::Pypi, "~=1.4", "2.0"));
        assert!(admits(Ecosystem::Pypi, ">=2.0,<3", "2.9.1"));
        assert!(!admits(Ecosystem::Pypi, ">=2.0,<3", "3.0.0"));
    }

    #[test]
    fn pep440_lower_bounds_may_carry_a_post_release() {
        assert!(admits(Ecosystem::Pypi, ">=2026.3.post1", "2026.4"));
        assert!(admits(Ecosystem::Pypi, ">=2.0rc1", "2.0"));
    }

    #[test]
    fn pep440_equality_and_wildcards() {
        assert!(admits(Ecosystem::Pypi, "==1.2", "1.2.0"));
        assert!(admits(Ecosystem::Pypi, "==1.2.*", "1.2.9"));
        assert!(!admits(Ecosystem::Pypi, "==1.2.*", "1.3.0"));
        assert!(!admits(Ecosystem::Pypi, ">=1.0,!=1.2.*", "1.2.5"));
        assert!(admits(Ecosystem::Pypi, ">=1.0,!=1.2.*", "1.3.0"));
        assert!(admits(Ecosystem::Pypi, "", "4.0"));
    }

    // --- Gradle / Maven ---

    #[test]
    fn a_catalog_version_is_exact() {
        assert!(admits(Ecosystem::Maven, "3.6.0", "3.6.0"));
        assert!(!admits(Ecosystem::Maven, "3.6.0", "3.6.1"));
        assert!(admits(Ecosystem::Maven, "33.0.0-jre", "33.0.0-jre"));
    }

    #[test]
    fn gradle_prefixes_and_maven_ranges() {
        assert!(admits(Ecosystem::Maven, "1.+", "1.9.0"));
        assert!(!admits(Ecosystem::Maven, "1.2.+", "1.3.0"));
        assert!(admits(Ecosystem::Maven, "[1.0,2.0)", "1.9.9"));
        assert!(!admits(Ecosystem::Maven, "[1.0,2.0)", "2.0.0"));
        assert!(admits(Ecosystem::Maven, "[1.0,)", "9.0"));
        assert!(!admits(Ecosystem::Maven, "(1.0,2.0]", "1.0"));
        assert!(admits(Ecosystem::Maven, "(1.0,2.0]", "2.0"));
    }

    // --- SwiftPM ---

    #[test]
    fn swiftpm_requirements() {
        assert!(admits(Ecosystem::Swiftpm, "from: 1.15.1", "1.99.0"));
        assert!(!admits(Ecosystem::Swiftpm, "from: 1.15.1", "2.0.0"));
        assert!(
            admits(Ecosystem::Swiftpm, "from: 0.3.0", "0.9.0"),
            "SwiftPM's upToNextMajor is literal on 0.x"
        );
        assert!(admits(Ecosystem::Swiftpm, "upToNextMinor: 1.2.0", "1.2.9"));
        assert!(!admits(Ecosystem::Swiftpm, "upToNextMinor: 1.2.0", "1.3.0"));
        assert!(admits(Ecosystem::Swiftpm, "exact: 1.2.0", "1.2.0"));
        assert!(!admits(Ecosystem::Swiftpm, "exact: 1.2.0", "1.2.1"));
        assert!(admits(Ecosystem::Swiftpm, "\"1.0.0\"..<\"1.5.0\"", "1.4.0"));
        assert!(!admits(Ecosystem::Swiftpm, "\"1.0.0\"..<\"1.5.0\"", "1.5.0"));
        assert!(admits(Ecosystem::Swiftpm, "\"1.0.0\"...\"1.5.0\"", "1.5.0"));
        assert!(Requirement::parse(Ecosystem::Swiftpm, "branch: main").is_none());
    }
}
