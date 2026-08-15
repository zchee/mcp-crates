//! The crates.io sparse index.
//!
//! The index is the highest-leverage source this client has. One request to
//! `index.crates.io` returns every published version of a crate together with
//! that version's full dependency list, feature table, yank status and minimum
//! supported Rust version. Answering the same questions through the REST API
//! would cost one request per version, against a one-per-second budget.
//!
//! The index is also served as static objects behind a CDN with `ETag` and
//! `Cache-Control`, so a repeat read is either free or a conditional request
//! that transfers no body. It is the same endpoint Cargo itself hammers.

use std::collections::BTreeMap;

use semver::Version;
use serde::Deserialize;

use crate::error::{Error, Result};

/// Longest crate name crates.io accepts.
const MAX_NAME_LEN: usize = 64;

/// What kind of dependency an entry describes.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum DependencyKind {
    /// A regular dependency, linked into the crate itself.
    #[default]
    Normal,
    /// A dependency of the crate's tests, examples and benchmarks.
    Dev,
    /// A dependency of the crate's build script.
    Build,
}

impl DependencyKind {
    /// The lowercase name Cargo uses for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
        }
    }
}

/// One dependency of one published version.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct IndexDep {
    /// The name this dependency is known by inside the depending crate. When
    /// [`IndexDep::package`] is set this is the rename, not the registry name.
    pub name: String,
    /// The Cargo version requirement.
    pub req: String,
    /// Features explicitly enabled on the dependency.
    #[serde(default)]
    pub features: Vec<String>,
    /// Whether the dependency is optional.
    #[serde(default)]
    pub optional: bool,
    /// Whether the dependency's default features are enabled.
    #[serde(default = "default_true")]
    pub default_features: bool,
    /// The `cfg` expression or target triple this dependency applies to.
    #[serde(default)]
    pub target: Option<String>,
    /// Whether this is a normal, dev or build dependency.
    ///
    /// Absent in the oldest index entries, which predate the field and are all
    /// normal dependencies.
    #[serde(default)]
    pub kind: DependencyKind,
    /// The registry the dependency comes from, when it is not crates.io.
    #[serde(default)]
    pub registry: Option<String>,
    /// The registry name of the dependency, when it was renamed.
    #[serde(default)]
    pub package: Option<String>,
}

impl IndexDep {
    /// The name the dependency is published under.
    #[must_use]
    pub fn registry_name(&self) -> &str {
        self.package.as_deref().unwrap_or(&self.name)
    }
}

const fn default_true() -> bool {
    true
}

/// One published version of a crate, as the index describes it.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct IndexEntry {
    /// The crate name.
    pub name: String,
    /// The version number, as published.
    pub vers: String,
    /// Every dependency of this version.
    #[serde(default)]
    pub deps: Vec<IndexDep>,
    /// SHA-256 checksum of the `.crate` file, hex encoded.
    #[serde(default)]
    pub cksum: String,
    /// The feature table.
    #[serde(default)]
    pub features: BTreeMap<String, Vec<String>>,
    /// Features that reference optional dependencies with the `dep:` or
    /// `pkg?/feat` syntax, split out for older Cargo versions.
    #[serde(default)]
    pub features2: Option<BTreeMap<String, Vec<String>>>,
    /// Whether this version has been yanked.
    #[serde(default)]
    pub yanked: bool,
    /// The native library this version links against, if any.
    #[serde(default)]
    pub links: Option<String>,
    /// The minimum Rust version this release declares.
    #[serde(default)]
    pub rust_version: Option<String>,
    /// Publication timestamp, a crates.io extension to the index format.
    #[serde(default)]
    pub pubtime: Option<String>,
    /// Index schema version of this entry.
    #[serde(default)]
    pub v: Option<u32>,

    /// [`IndexEntry::vers`] parsed once, at load time.
    #[serde(skip)]
    parsed: Option<Version>,
}

impl IndexEntry {
    /// The parsed version, or `None` for the handful of historical releases
    /// whose version string is not valid semver.
    #[must_use]
    pub fn version(&self) -> Option<&Version> {
        self.parsed.as_ref()
    }

    /// The complete feature table, with the `features2` split rejoined.
    ///
    /// Cargo treats the two tables as one; keeping them apart would make a
    /// crate look like it is missing features that it does in fact have.
    #[must_use]
    pub fn all_features(&self) -> BTreeMap<&str, &[String]> {
        let mut merged: BTreeMap<&str, &[String]> =
            self.features.iter().map(|(name, values)| (name.as_str(), values.as_slice())).collect();
        if let Some(extra) = &self.features2 {
            for (name, values) in extra {
                merged.insert(name.as_str(), values.as_slice());
            }
        }
        merged
    }

    /// Dependencies of the given kind.
    pub fn deps_of_kind(&self, kind: DependencyKind) -> impl Iterator<Item = &IndexDep> {
        self.deps.iter().filter(move |dep| dep.kind == kind)
    }
}

/// Every published version of one crate.
#[derive(Debug)]
pub struct CrateIndex {
    name: String,
    entries: Box<[IndexEntry]>,
    /// Indices into [`CrateIndex::entries`], ascending by semver.
    ///
    /// The index file is in publication order, which is not version order once
    /// a crate backports a patch to an older line. Sorting once here means
    /// every later lookup is already ordered.
    by_version: Box<[usize]>,
}

impl CrateIndex {
    /// Parse a sparse-index document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Decode`] if a line is not a valid index entry.
    pub fn parse(name: &str, body: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(body).map_err(|err| Error::Decode {
            url: index_url(name),
            message: format!("the index document is not valid UTF-8: {err}"),
        })?;

        let mut entries: Vec<IndexEntry> = Vec::with_capacity(text.lines().count());
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut entry: IndexEntry =
                serde_json::from_str(line).map_err(|err| Error::Decode {
                    url: index_url(name),
                    message: format!("line {} is not a valid index entry: {err}", number + 1),
                })?;
            entry.parsed = Version::parse(&entry.vers).ok();
            entries.push(entry);
        }

        if entries.is_empty() {
            return Err(Error::Decode {
                url: index_url(name),
                message: "the index document contains no versions".to_owned(),
            });
        }

        let mut by_version: Vec<usize> = (0..entries.len()).collect();
        by_version.sort_by(|&left, &right| {
            match (entries[left].version(), entries[right].version()) {
                (Some(a), Some(b)) => a.cmp(b),
                // Versions that are not valid semver cannot be ordered
                // against ones that are, so they are parked at the bottom of
                // the ascending order. That puts them last in `descending`,
                // which is what callers asking for the newest release read.
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => left.cmp(&right),
            }
        });

        Ok(Self {
            name: entries[0].name.clone(),
            entries: entries.into_boxed_slice(),
            by_version: by_version.into_boxed_slice(),
        })
    }

    /// The crate's name, as the index spells it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Every version, in publication order.
    #[must_use]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// Every version, ascending by semver.
    pub fn ascending(&self) -> impl DoubleEndedIterator<Item = &IndexEntry> {
        self.by_version.iter().map(|&index| &self.entries[index])
    }

    /// Every version, descending by semver: newest first.
    pub fn descending(&self) -> impl Iterator<Item = &IndexEntry> {
        self.ascending().rev()
    }

    /// How many versions have been published.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the crate has no versions. Never true for a document that parsed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve a selector against this crate's versions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VersionNotFound`] when nothing matches.
    pub fn resolve(
        &self,
        selector: &crate::version::Selector,
        allow_yanked: bool,
    ) -> Result<&IndexEntry> {
        let candidates = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| entry.version().map(|v| (i, v, entry.yanked)));
        crate::version::best(candidates, selector, allow_yanked)
            .map(|index| &self.entries[index])
            .ok_or_else(|| Error::VersionNotFound {
                name: self.name.clone(),
                selector: selector.to_string(),
            })
    }
}

/// Check a crate name against the crates.io naming rules.
///
/// Beyond rejecting names the registry could not have accepted, this is what
/// keeps a caller-supplied name from being spliced into a URL path as anything
/// other than a single path segment.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an empty, over-long, or
/// non-conforming name.
pub fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid { Ok(()) } else { Err(Error::InvalidCrateName { name: name.to_owned() }) }
}

/// The sparse-index path for a crate, following Cargo's directory layout.
///
/// Names are bucketed by length: one and two character names live under `1/`
/// and `2/`, three character names under `3/<first letter>/`, and everything
/// else under `<first two>/<next two>/`. Lookups are lowercase.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] if the name is not one the registry
/// could have accepted.
pub fn index_path(name: &str) -> Result<String> {
    validate_name(name)?;
    let lower = name.to_ascii_lowercase();
    let path = match lower.len() {
        1 => format!("1/{lower}"),
        2 => format!("2/{lower}"),
        3 => format!("3/{}/{lower}", &lower[..1]),
        _ => format!("{}/{}/{lower}", &lower[..2], &lower[2..4]),
    };
    Ok(path)
}

/// The full sparse-index URL for a crate.
///
/// Falls back to a descriptive placeholder for an invalid name so that it stays
/// usable inside error messages.
#[must_use]
pub fn index_url(name: &str) -> String {
    match index_path(name) {
        Ok(path) => format!("https://index.crates.io/{path}"),
        Err(_) => format!("<invalid crate name {name:?}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = concat!(
        r#"{"name":"demo","vers":"0.1.0","deps":[],"cksum":"aa","features":{},"yanked":false}"#,
        "\n",
        r#"{"name":"demo","vers":"1.0.0","deps":[{"name":"serde","req":"^1","features":["derive"],"optional":false,"default_features":true,"target":null,"kind":"normal"},{"name":"tempfile","req":"^3","features":[],"optional":false,"default_features":true,"target":null,"kind":"dev"}],"cksum":"bb","features":{"std":[]},"features2":{"json":["dep:serde_json"]},"yanked":false,"rust_version":"1.75"}"#,
        "\n",
        "\n",
        r#"{"name":"demo","vers":"0.9.0","deps":[],"cksum":"cc","features":{},"yanked":true}"#,
        "\n",
        r#"{"name":"demo","vers":"2.0.0-rc.1","deps":[],"cksum":"dd","features":{},"yanked":false}"#,
    );

    fn sample() -> CrateIndex {
        CrateIndex::parse("demo", SAMPLE.as_bytes()).expect("parses")
    }

    #[test]
    fn index_paths_follow_cargos_bucketing_rules() {
        assert_eq!(index_path("a").expect("valid"), "1/a");
        assert_eq!(index_path("id").expect("valid"), "2/id");
        assert_eq!(index_path("log").expect("valid"), "3/l/log");
        assert_eq!(index_path("serde").expect("valid"), "se/rd/serde");
        assert_eq!(index_path("tokio-util").expect("valid"), "to/ki/tokio-util");
        assert_eq!(index_path("SERDE").expect("valid"), "se/rd/serde", "lookups are lowercase");
    }

    #[test]
    fn names_that_could_escape_the_path_are_rejected() {
        for bad in ["", "../../etc/passwd", "a/b", "sé rde", "a%2e", ".", "..", &"x".repeat(65)] {
            assert!(
                matches!(index_path(bad), Err(Error::InvalidCrateName { .. })),
                "{bad:?} should have been rejected"
            );
        }
    }

    #[test]
    fn parsing_skips_blank_lines_and_records_every_version() {
        let index = sample();
        assert_eq!(index.name(), "demo");
        assert_eq!(index.len(), 4);
    }

    #[test]
    fn versions_are_ordered_by_semver_not_publication_order() {
        let index = sample();
        let ordered: Vec<&str> = index.ascending().map(|entry| entry.vers.as_str()).collect();
        assert_eq!(ordered, ["0.1.0", "0.9.0", "1.0.0", "2.0.0-rc.1"]);

        let newest = index.descending().next().expect("some version");
        assert_eq!(newest.vers, "2.0.0-rc.1");
    }

    #[test]
    fn resolving_the_default_selector_prefers_the_newest_stable_release() {
        let index = sample();
        let selected = index.resolve(&crate::version::Selector::Default, false).expect("resolves");
        assert_eq!(selected.vers, "1.0.0", "a release candidate is not the default version");
    }

    #[test]
    fn resolving_skips_yanked_versions_unless_allowed() {
        let index = sample();
        let req = "^0.9".parse().expect("valid selector");
        assert!(index.resolve(&req, false).is_err());
        assert_eq!(index.resolve(&req, true).expect("resolves").vers, "0.9.0");
    }

    #[test]
    fn dependencies_carry_their_kind_and_feature_selection() {
        let index = sample();
        let release =
            index.entries().iter().find(|entry| entry.vers == "1.0.0").expect("the release exists");

        let normal: Vec<&str> =
            release.deps_of_kind(DependencyKind::Normal).map(|dep| dep.name.as_str()).collect();
        assert_eq!(normal, ["serde"]);

        let dev: Vec<&str> =
            release.deps_of_kind(DependencyKind::Dev).map(|dep| dep.name.as_str()).collect();
        assert_eq!(dev, ["tempfile"]);

        let serde = &release.deps[0];
        assert_eq!(serde.features, ["derive"]);
        assert!(serde.default_features);
        assert_eq!(serde.registry_name(), "serde");
    }

    #[test]
    fn the_split_feature_tables_are_presented_as_one() {
        let index = sample();
        let release =
            index.entries().iter().find(|entry| entry.vers == "1.0.0").expect("the release exists");
        let features = release.all_features();
        assert_eq!(features.keys().copied().collect::<Vec<_>>(), ["json", "std"]);
    }

    #[test]
    fn an_entry_without_a_kind_field_is_treated_as_a_normal_dependency() {
        let legacy = r#"{"name":"old","vers":"0.1.0","deps":[{"name":"libc","req":"*","features":[],"optional":false,"default_features":true,"target":null}],"cksum":"ee","features":{},"yanked":false}"#;
        let index = CrateIndex::parse("old", legacy.as_bytes()).expect("parses");
        assert_eq!(index.entries()[0].deps[0].kind, DependencyKind::Normal);
    }

    #[test]
    fn an_empty_document_is_a_decode_error_rather_than_an_empty_crate() {
        assert!(matches!(CrateIndex::parse("demo", b""), Err(Error::Decode { .. })));
    }
}
