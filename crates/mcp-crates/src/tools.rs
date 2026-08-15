//! Tool argument and result shapes.
//!
//! Results are deliberately narrower than the upstream responses they are built
//! from. A tool result is read by a language model with a finite context, so
//! every field that survives here has to earn its place: fields that repeat
//! something already present, or that no caller can act on, are dropped rather
//! than passed through.

use crates_io_client::{
    CrateCategory, CrateSummary, DependencyKind, DocItem, IndexDep, IndexEntry, Reexport, Sort,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default number of search results.
pub const DEFAULT_SEARCH_LIMIT: u32 = 10;

/// Largest page crates.io will serve.
pub const MAX_SEARCH_LIMIT: u32 = 100;

/// Default number of versions returned by `get_crate_versions`.
pub const DEFAULT_VERSION_LIMIT: u32 = 20;

/// Largest number of versions returned in one call.
pub const MAX_VERSION_LIMIT: u32 = 500;

// ---------------------------------------------------------------- search ---

/// How search results should be ordered.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Best textual match for the query. Meaningful only with a query.
    #[default]
    Relevance,
    /// All-time downloads, highest first.
    Downloads,
    /// Downloads in the last 90 days, highest first. The better popularity
    /// signal for a crate that was widely used years ago but is now dormant.
    RecentDownloads,
    /// Most recently published, newest first.
    RecentUpdates,
    /// Newest crates first.
    Newest,
    /// Crate name, A to Z.
    Alphabetical,
}

impl From<SortOrder> for Sort {
    fn from(order: SortOrder) -> Self {
        match order {
            SortOrder::Relevance => Self::Relevance,
            SortOrder::Downloads => Self::Downloads,
            SortOrder::RecentDownloads => Self::RecentDownloads,
            SortOrder::RecentUpdates => Self::RecentUpdates,
            SortOrder::Newest => Self::New,
            SortOrder::Alphabetical => Self::Alphabetical,
        }
    }
}

/// Arguments for `search_crates`.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
pub struct SearchCratesArgs {
    /// Free-text search over crate names, descriptions and keywords.
    ///
    /// At least one of `query`, `keywords` or `category` must be given.
    #[serde(default)]
    pub query: Option<String>,

    /// Restrict results to crates carrying every one of these keywords.
    #[serde(default)]
    pub keywords: Option<Vec<String>>,

    /// Restrict results to a crates.io category slug, such as
    /// `web-programming::http-server` or `asynchronous`.
    #[serde(default)]
    pub category: Option<String>,

    /// Result ordering. Defaults to `relevance`.
    #[serde(default)]
    pub sort: Option<SortOrder>,

    /// How many results to return, 1 to 100. Defaults to 10.
    #[serde(default)]
    pub limit: Option<u32>,

    /// One-based page number, for paging past the first `limit` results.
    #[serde(default)]
    pub page: Option<u32>,

    /// Include crates whose every version has been yanked. Defaults to false.
    #[serde(default)]
    pub include_yanked: Option<bool>,
}

/// One crate in a search result.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CrateHit {
    /// The crate name, as used in `Cargo.toml`.
    pub name: String,
    /// The crate's one-line description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The newest version that is not a pre-release. This is what `cargo add`
    /// would pick.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_stable_version: Option<String>,
    /// The most recently published version, which may be a pre-release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_version: Option<String>,
    /// Total downloads across every version.
    pub downloads: u64,
    /// Downloads in the last 90 days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_downloads: Option<u64>,
    /// The crate's source repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The crate's documentation URL, when it declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// When the crate was last updated, as an RFC 3339 timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Whether every version of this crate has been yanked.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub yanked: bool,
    /// Whether the query matched this crate's name exactly.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub exact_match: bool,
}

impl From<&CrateSummary> for CrateHit {
    /// Unlike the crate detail endpoint, search computes its version fields
    /// correctly without being asked for the per-version payload, so they are
    /// used directly here. `get_crate_info` takes them from the sparse index
    /// instead, and the two are not interchangeable.
    fn from(summary: &CrateSummary) -> Self {
        Self {
            name: summary.name.clone(),
            description: summary.description.clone(),
            latest_stable_version: summary.max_stable_version.clone(),
            newest_version: summary.newest_version.clone(),
            downloads: summary.downloads,
            recent_downloads: summary.recent_downloads,
            repository: summary.repository.clone(),
            documentation: summary.documentation.clone(),
            updated_at: summary.updated_at.clone(),
            yanked: summary.yanked,
            exact_match: summary.exact_match,
        }
    }
}

/// Result of `search_crates`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SearchCratesResult {
    /// How many crates match the query in total, across all pages.
    pub total_matches: u64,
    /// The page these results came from.
    pub page: u32,
    /// Whether a further page exists.
    pub has_more: bool,
    /// The matching crates, in the requested order.
    pub crates: Vec<CrateHit>,
}

// ------------------------------------------------------------ crate info ---

/// Arguments for `get_crate_info`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct GetCrateInfoArgs {
    /// The exact crate name, for example `serde` or `tokio-util`.
    pub name: String,
}

/// A crates.io category a crate belongs to.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CategoryRef {
    /// The category slug, usable as the `category` argument to `search_crates`.
    pub slug: String,
    /// The human-readable category name.
    pub name: String,
}

impl From<&CrateCategory> for CategoryRef {
    fn from(category: &CrateCategory) -> Self {
        Self { slug: category.slug.clone(), name: category.category.clone() }
    }
}

/// A summary of the crate's current release.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct LatestReleaseSummary {
    /// The version number.
    pub version: String,
    /// The minimum supported Rust version this release declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rust_version: Option<String>,
    /// The names of the Cargo features this release defines.
    pub features: Vec<String>,
    /// How many required, non-optional runtime dependencies it has.
    pub required_dependencies: usize,
    /// How many optional runtime dependencies it has.
    pub optional_dependencies: usize,
    /// The native library this release links against, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<String>,
}

impl LatestReleaseSummary {
    /// Summarize an index entry.
    #[must_use]
    pub fn from_entry(entry: &IndexEntry) -> Self {
        let runtime = || entry.deps_of_kind(DependencyKind::Normal);
        Self {
            version: entry.vers.clone(),
            rust_version: entry.rust_version.clone(),
            features: entry.all_features().keys().map(|name| (*name).to_owned()).collect(),
            required_dependencies: runtime().filter(|dep| !dep.optional).count(),
            optional_dependencies: runtime().filter(|dep| dep.optional).count(),
            links: entry.links.clone(),
        }
    }
}

/// Result of `get_crate_info`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CrateInfoResult {
    /// The crate name.
    pub name: String,
    /// The crate's one-line description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The newest version that is neither yanked nor a pre-release. This is
    /// what `cargo add` would pick.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_stable_version: Option<String>,
    /// The highest version of any kind, pre-releases included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_version: Option<String>,
    /// The version crates.io presents by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_version: Option<String>,
    /// How many versions have been published.
    pub total_versions: usize,
    /// Total downloads across every version.
    pub downloads: u64,
    /// Downloads in the last 90 days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_downloads: Option<u64>,
    /// The crate's source repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The crate's homepage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// The documentation URL the crate declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// The docs.rs page for the current release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_rs_url: Option<String>,
    /// Keywords the crate declares.
    pub keywords: Vec<String>,
    /// Categories the crate belongs to.
    pub categories: Vec<CategoryRef>,
    /// When the crate was first published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// When the crate was last updated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Whether every version has been yanked.
    pub yanked: bool,
    /// Details of the current release, from the sparse index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_release: Option<LatestReleaseSummary>,
}

// --------------------------------------------------------------- versions ---

/// Arguments for `get_crate_versions`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct GetCrateVersionsArgs {
    /// The exact crate name.
    pub name: String,

    /// Return only versions satisfying this Cargo requirement, such as `^1.2`,
    /// `>=0.4, <0.6` or `1.*`.
    #[serde(default)]
    pub requirement: Option<String>,

    /// How many versions to return, newest first. Defaults to 20.
    #[serde(default)]
    pub limit: Option<u32>,

    /// Include yanked versions. Defaults to false.
    #[serde(default)]
    pub include_yanked: Option<bool>,

    /// Include pre-releases such as `2.0.0-rc.1`. Defaults to false.
    #[serde(default)]
    pub include_prerelease: Option<bool>,
}

/// One published version.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct VersionEntry {
    /// The version number.
    pub version: String,
    /// Whether this version has been yanked.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub yanked: bool,
    /// Whether this version is a pre-release.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
    /// The minimum supported Rust version this release declares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rust_version: Option<String>,
    /// When this version was published.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// How many Cargo features it defines.
    pub features: usize,
    /// How many runtime dependencies it has, optional ones included.
    pub dependencies: usize,
}

impl From<&IndexEntry> for VersionEntry {
    fn from(entry: &IndexEntry) -> Self {
        Self {
            version: entry.vers.clone(),
            yanked: entry.yanked,
            prerelease: entry.version().is_some_and(|version| !version.pre.is_empty()),
            rust_version: entry.rust_version.clone(),
            published_at: entry.pubtime.clone(),
            features: entry.all_features().len(),
            dependencies: entry.deps_of_kind(DependencyKind::Normal).count(),
        }
    }
}

/// Result of `get_crate_versions`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CrateVersionsResult {
    /// The crate name.
    pub name: String,
    /// How many versions have ever been published, before filtering.
    pub total_versions: usize,
    /// The newest version that is neither yanked nor a pre-release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_stable_version: Option<String>,
    /// The highest version of any kind, pre-releases included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest_version: Option<String>,
    /// Whether the returned list was cut short by `limit`.
    pub truncated: bool,
    /// The matching versions, newest first.
    pub versions: Vec<VersionEntry>,
}

// ----------------------------------------------------------- dependencies ---

/// Which kinds of dependency to report.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DepKind {
    /// Dependencies linked into the crate itself.
    Normal,
    /// Dependencies of tests, examples and benchmarks.
    Dev,
    /// Dependencies of the build script.
    Build,
}

impl From<DepKind> for DependencyKind {
    fn from(kind: DepKind) -> Self {
        match kind {
            DepKind::Normal => Self::Normal,
            DepKind::Dev => Self::Dev,
            DepKind::Build => Self::Build,
        }
    }
}

/// Arguments for `get_crate_dependencies`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct GetCrateDependenciesArgs {
    /// The exact crate name.
    pub name: String,

    /// Which version's dependencies to report. Accepts an exact version such as
    /// `1.0.219`, a Cargo requirement such as `^1.0`, or `latest`. Defaults to
    /// the newest release that is neither yanked nor a pre-release.
    #[serde(default)]
    pub version: Option<String>,

    /// Which dependency kinds to include. Defaults to `normal` only.
    #[serde(default)]
    pub kinds: Option<Vec<DepKind>>,

    /// Include optional dependencies, which are pulled in only by a feature.
    /// Defaults to true.
    #[serde(default)]
    pub include_optional: Option<bool>,
}

/// One dependency edge.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct DependencyEntry {
    /// The dependency's crate name on crates.io.
    pub name: String,
    /// The name it is referred to by, when the depending crate renamed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
    /// The Cargo version requirement.
    pub requirement: String,
    /// Whether the dependency is optional, i.e. enabled by a feature.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
    /// Whether the dependency's default features are enabled.
    pub default_features: bool,
    /// Features explicitly enabled on the dependency.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    /// The platform or `cfg` expression this dependency is conditional on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The registry it comes from, when it is not crates.io.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
}

impl From<&IndexDep> for DependencyEntry {
    fn from(dep: &IndexDep) -> Self {
        // The index stores the rename in `name` and the real crate in
        // `package`; callers care most about the real crate, so the two are
        // presented the other way round.
        let (name, renamed_to) = match &dep.package {
            Some(package) => (package.clone(), Some(dep.name.clone())),
            None => (dep.name.clone(), None),
        };
        Self {
            name,
            renamed_to,
            requirement: dep.req.clone(),
            optional: dep.optional,
            default_features: dep.default_features,
            features: dep.features.clone(),
            target: dep.target.clone(),
            registry: dep.registry.clone(),
        }
    }
}

/// Result of `get_crate_dependencies`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CrateDependenciesResult {
    /// The crate name.
    pub name: String,
    /// The version the request resolved to.
    pub version: String,
    /// Whether that version has been yanked.
    pub yanked: bool,
    /// Dependencies linked into the crate itself.
    pub normal: Vec<DependencyEntry>,
    /// Dependencies of tests, examples and benchmarks.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dev: Vec<DependencyEntry>,
    /// Dependencies of the build script.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build: Vec<DependencyEntry>,
}

// --------------------------------------------------------- documentation ---

/// Arguments for `get_crate_documentation`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
pub struct GetCrateDocumentationArgs {
    /// The exact crate name.
    pub name: String,

    /// Which version to document. Accepts an exact version, a Cargo
    /// requirement, or `latest`. Defaults to the newest release that is neither
    /// yanked nor a pre-release.
    #[serde(default)]
    pub version: Option<String>,

    /// Look up one item's documentation by path, such as
    /// `serde::de::Deserializer`, or by bare name, such as `Deserializer`.
    ///
    /// This reads the rustdoc JSON docs.rs generated for the release, which is
    /// the only source of prose attached to individual types, traits and
    /// functions. Omit it to get the crate's README instead.
    #[serde(default)]
    pub item: Option<String>,

    /// Include the crate's README. Defaults to true when `item` is omitted, and
    /// false when it is given.
    #[serde(default)]
    pub include_readme: Option<bool>,

    /// Truncate the README to this many characters. Defaults to 40000.
    #[serde(default)]
    pub max_readme_chars: Option<u32>,
}

/// One documented item.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ItemDoc {
    /// The item's full path.
    pub path: String,
    /// The rustdoc item kind: `struct`, `trait`, `function`, `module`, and so on.
    pub kind: String,
    /// The item's documentation, as written in its doc comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Whether the item is deprecated.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deprecated: bool,
}

impl From<&DocItem> for ItemDoc {
    fn from(item: &DocItem) -> Self {
        Self {
            path: item.path.to_string(),
            kind: item.kind.to_string(),
            documentation: item.docs.as_ref().map(ToString::to_string),
            deprecated: item.deprecated,
        }
    }
}

/// An item this crate exposes but another crate documents.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ReexportedItem {
    /// The name this crate exposes the item under.
    pub name: String,
    /// The crate that defines and documents it. A crates.io package name is
    /// usually the same, sometimes with `-` where this has `_`.
    pub defined_in: String,
    /// The item's full path inside the crate that defines it, usable as the
    /// `item` argument once looking that crate up.
    pub path: String,
    /// The rustdoc item kind.
    pub kind: String,
}

impl From<&Reexport> for ReexportedItem {
    fn from(reexport: &Reexport) -> Self {
        Self {
            name: reexport.name.to_string(),
            defined_in: reexport.defining_crate.to_string(),
            path: reexport.path.to_string(),
            kind: reexport.kind.to_string(),
        }
    }
}

/// Result of `get_crate_documentation`.
#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct CrateDocumentationResult {
    /// The crate name.
    pub name: String,
    /// The version the request resolved to.
    pub version: String,
    /// The docs.rs page for this release.
    ///
    /// The documentation URL a crate declares for itself is reported by
    /// `get_crate_info`; repeating it here would cost an extra API request for
    /// a link the caller can already get.
    pub docs_rs_url: String,
    /// Whether docs.rs built documentation for this release.
    ///
    /// Absent when it could not be determined: an item lookup reads the
    /// rustdoc JSON, which docs.rs publishes only for recent enough builds, so
    /// its absence does not by itself mean the build failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs_rs_built: Option<bool>,
    /// The crate's README, converted to Markdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readme: Option<String>,
    /// The item the `item` argument resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<ItemDoc>,
    /// Items the `item` argument could have meant, when it did not resolve to
    /// exactly one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<ItemDoc>,
    /// Where to find the item when this crate only re-exports it. Facade
    /// crates, whose public API is mostly `pub use` of their own sub-crates,
    /// document almost nothing themselves.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reexported: Vec<ReexportedItem>,
    /// How many documented items the release has, when its rustdoc JSON was
    /// read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documented_items: Option<usize>,
    /// A note explaining anything the caller would otherwise find surprising,
    /// such as an item lookup that matched nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use crates_io_client::CrateIndex;

    use super::*;

    const INDEX: &str = r#"{"name":"demo","vers":"1.0.0","deps":[{"name":"serde","req":"^1","features":["derive"],"optional":false,"default_features":true,"target":null,"kind":"normal"},{"name":"rand","req":"^0.8","features":[],"optional":true,"default_features":true,"target":null,"kind":"normal"},{"name":"nix","req":"^0.27","features":[],"optional":false,"default_features":true,"target":"cfg(unix)","kind":"build","package":"nix-real"}],"cksum":"aa","features":{"std":[]},"features2":{"json":["dep:serde_json"]},"yanked":false,"rust_version":"1.75","links":"z","pubtime":"2026-01-02T03:04:05Z"}"#;

    fn entry() -> IndexEntry {
        let index = CrateIndex::parse("demo", INDEX.as_bytes()).expect("the fixture parses");
        index.entries()[0].clone()
    }

    #[test]
    fn a_release_summary_separates_required_from_optional_dependencies() {
        let summary = LatestReleaseSummary::from_entry(&entry());

        assert_eq!(summary.version, "1.0.0");
        assert_eq!(summary.rust_version.as_deref(), Some("1.75"));
        assert_eq!(summary.required_dependencies, 1, "the build dependency is not a runtime one");
        assert_eq!(summary.optional_dependencies, 1);
        assert_eq!(summary.features, ["json", "std"], "both feature tables are counted");
        assert_eq!(summary.links.as_deref(), Some("z"));
    }

    #[test]
    fn a_version_entry_reports_only_runtime_dependencies() {
        let entry = VersionEntry::from(&entry());

        assert_eq!(entry.version, "1.0.0");
        assert!(!entry.yanked && !entry.prerelease);
        assert_eq!(entry.published_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(entry.features, 2);
        assert_eq!(entry.dependencies, 2, "one required and one optional runtime dependency");
    }

    #[test]
    fn a_renamed_dependency_is_reported_under_its_registry_name() {
        let entry = entry();
        let build =
            entry.deps.iter().find(|dep| dep.package.is_some()).expect("the fixture has one");
        let converted = DependencyEntry::from(build);

        assert_eq!(converted.name, "nix-real", "the crate that is actually depended on");
        assert_eq!(converted.renamed_to.as_deref(), Some("nix"), "the name it is used under");
        assert_eq!(converted.target.as_deref(), Some("cfg(unix)"));
    }

    #[test]
    fn a_plain_dependency_reports_no_rename() {
        let entry = entry();
        let converted = DependencyEntry::from(&entry.deps[0]);

        assert_eq!(converted.name, "serde");
        assert_eq!(converted.renamed_to, None);
        assert_eq!(converted.features, ["derive"]);
        assert!(converted.default_features && !converted.optional);
    }

    #[test]
    fn a_prerelease_is_flagged_as_one() {
        let raw = r#"{"name":"demo","vers":"2.0.0-rc.1","deps":[],"cksum":"bb","features":{},"yanked":false}"#;
        let index = CrateIndex::parse("demo", raw.as_bytes()).expect("parses");
        let entry = VersionEntry::from(&index.entries()[0]);

        assert!(entry.prerelease);
    }
}
