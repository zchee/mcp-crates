//! Selecting one version of a crate.

use std::{fmt, str::FromStr};

use semver::{Version, VersionReq};

use crate::error::Error;

/// How a caller asked for a version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Selector {
    /// The version a user would get from `cargo add`: the highest release that
    /// is neither yanked nor a pre-release, falling back to the highest
    /// pre-release for crates that have only ever published one.
    #[default]
    Default,
    /// One exact release.
    Exact(Version),
    /// The highest release satisfying a Cargo-style requirement such as
    /// `^1.2`, `>=1, <2` or `1.*`.
    Matching(VersionReq),
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("latest"),
            Self::Exact(version) => write!(f, "{version}"),
            Self::Matching(req) => write!(f, "{req}"),
        }
    }
}

impl FromStr for Selector {
    type Err = Error;

    /// Parse a selector.
    ///
    /// `latest`, `newest` and the empty string all mean [`Selector::Default`].
    /// A bare semver such as `1.2.3` is an exact match rather than a caret
    /// requirement, because a caller naming a full version means that version.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("latest")
            || trimmed.eq_ignore_ascii_case("newest")
        {
            return Ok(Self::Default);
        }
        if let Ok(version) = Version::parse(trimmed) {
            return Ok(Self::Exact(version));
        }
        VersionReq::parse(trimmed).map(Self::Matching).map_err(|err| Error::InvalidVersion {
            value: trimmed.to_owned(),
            reason: err.to_string(),
        })
    }
}

impl Selector {
    /// Whether a candidate version satisfies this selector.
    ///
    /// [`Selector::Default`] accepts anything; ranking between the accepted
    /// candidates is the caller's job.
    #[must_use]
    pub fn accepts(&self, version: &Version) -> bool {
        match self {
            Self::Default => true,
            Self::Exact(wanted) => wanted == version,
            // `VersionReq` excludes pre-releases unless the requirement itself
            // names one, which matches how Cargo resolves dependencies.
            Self::Matching(req) => req.matches(version),
        }
    }
}

/// Pick the best of a set of candidate versions.
///
/// Candidates are supplied as `(index, version, yanked)` triples. Ranking
/// prefers, in order: not yanked over yanked, a stable release over a
/// pre-release, and then the highest version. The pre-release preference is
/// what makes `1.0.0` win over `2.0.0-rc.1`, which is the version a user would
/// actually get from `cargo add`.
///
/// `allow_yanked` makes yanked releases *eligible*; it does not promote them.
/// A yanked release is still outranked by any non-yanked release that also
/// satisfies the selector, so a version that was withdrawn is never presented
/// as the newest one. It wins only when it is named exactly, or when nothing
/// else matches.
///
/// Returns the index of the winning candidate.
#[must_use]
pub fn best<'a, I>(candidates: I, selector: &Selector, allow_yanked: bool) -> Option<usize>
where
    I: IntoIterator<Item = (usize, &'a Version, bool)>,
{
    candidates
        .into_iter()
        .filter(|(_, version, yanked)| (allow_yanked || !*yanked) && selector.accepts(version))
        .max_by(|(_, left, left_yanked), (_, right, right_yanked)| {
            // `false < true`, so "not yanked" and "not pre-release" sort higher
            // once negated.
            (!left_yanked, left.pre.is_empty(), *left).cmp(&(
                !right_yanked,
                right.pre.is_empty(),
                *right,
            ))
        })
        .map(|(index, _, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(raw: &[(&str, bool)]) -> Vec<(Version, bool)> {
        raw.iter().map(|(v, yanked)| (Version::parse(v).expect("valid semver"), *yanked)).collect()
    }

    fn pick(raw: &[(&str, bool)], selector: &str, allow_yanked: bool) -> Option<String> {
        let parsed = versions(raw);
        let selector: Selector = selector.parse().expect("valid selector");
        let candidates = parsed.iter().enumerate().map(|(i, (v, y))| (i, v, *y));
        best(candidates, &selector, allow_yanked).map(|i| parsed[i].0.to_string())
    }

    #[test]
    fn selectors_parse_into_the_intended_shape() {
        assert_eq!(Selector::from_str("").expect("parses"), Selector::Default);
        assert_eq!(Selector::from_str("latest").expect("parses"), Selector::Default);
        assert_eq!(Selector::from_str("LATEST").expect("parses"), Selector::Default);
        assert_eq!(
            Selector::from_str("1.2.3").expect("parses"),
            Selector::Exact(Version::parse("1.2.3").expect("valid"))
        );
        assert!(matches!(Selector::from_str("^1.2").expect("parses"), Selector::Matching(_)));
        assert!(matches!(Selector::from_str("1.*").expect("parses"), Selector::Matching(_)));
        assert!(matches!(Selector::from_str("not a version"), Err(Error::InvalidVersion { .. })));
    }

    #[test]
    fn an_exact_selector_does_not_widen_into_a_caret_requirement() {
        let releases = [("1.2.3", false), ("1.2.4", false)];
        assert_eq!(pick(&releases, "1.2.3", false).as_deref(), Some("1.2.3"));
        assert_eq!(pick(&releases, "^1.2.3", false).as_deref(), Some("1.2.4"));
    }

    #[test]
    fn a_stable_release_outranks_a_higher_prerelease() {
        let releases = [("1.0.0", false), ("2.0.0-rc.1", false)];
        assert_eq!(pick(&releases, "latest", false).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn a_prerelease_wins_when_it_is_all_that_exists() {
        let releases = [("0.1.0-alpha.1", false), ("0.1.0-alpha.2", false)];
        assert_eq!(pick(&releases, "latest", false).as_deref(), Some("0.1.0-alpha.2"));
    }

    #[test]
    fn yanked_releases_are_skipped_unless_explicitly_allowed() {
        let only_yanked = [("1.0.0", true)];
        assert_eq!(pick(&only_yanked, "latest", false), None);
        assert_eq!(pick(&only_yanked, "latest", true).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn allowing_yanked_releases_does_not_promote_them_over_live_ones() {
        // 1.1.0 is both higher and eligible, but it was withdrawn, so it must
        // not be handed back as "the latest version" of the crate.
        let releases = [("1.0.0", false), ("1.1.0", true)];
        assert_eq!(pick(&releases, "latest", true).as_deref(), Some("1.0.0"));
    }

    #[test]
    fn a_requirement_matching_nothing_selects_nothing() {
        let releases = [("1.0.0", false)];
        assert_eq!(pick(&releases, "^9", false), None);
    }

    #[test]
    fn an_exact_yanked_version_is_still_reachable_when_allowed() {
        let releases = [("1.0.0", false), ("1.1.0", true)];
        assert_eq!(pick(&releases, "1.1.0", false), None);
        assert_eq!(pick(&releases, "1.1.0", true).as_deref(), Some("1.1.0"));
    }
}
