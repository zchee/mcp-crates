//! The crates.io REST API.
//!
//! Only the read endpoints this crate needs are modelled, and each response
//! type keeps just the fields that carry information a consumer can act on.
//! Dropping the rest matters: the crate detail endpoint returns roughly 400 KiB
//! for a crate like `serde` when asked for everything, almost all of it the
//! per-version records that the sparse index supplies far more cheaply.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    error::{Error, Result},
    index::validate_name,
    version::validate_version,
};

/// Base URL of the public crates.io API.
const API_BASE: &str = "https://crates.io/api/v1/";

/// Largest page size crates.io will serve.
const MAX_PER_PAGE: u32 = 100;

/// How search results are ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Sort {
    /// Best match for the query string. Requires a query to be meaningful.
    #[default]
    Relevance,
    /// All-time download count, descending.
    Downloads,
    /// Downloads in the last 90 days, descending.
    RecentDownloads,
    /// Most recently published version, descending.
    RecentUpdates,
    /// Newest crates first.
    New,
    /// Crate name, ascending.
    Alphabetical,
}

impl Sort {
    /// The value crates.io expects in the `sort` query parameter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Relevance => "relevance",
            Self::Downloads => "downloads",
            Self::RecentDownloads => "recent-downloads",
            Self::RecentUpdates => "recent-updates",
            Self::New => "new",
            Self::Alphabetical => "alphabetical",
        }
    }
}

/// A crates.io search request.
///
/// crates.io ignores later filters once an earlier one is set: `all_keywords`
/// beats `keyword`, which beats `letter`. The precedence is the server's, and
/// is documented on the fields so that a caller is not surprised by a filter
/// that silently did nothing.
#[derive(Clone, Debug, Default)]
pub struct SearchParams {
    /// Free-text query.
    pub query: Option<String>,
    /// Result ordering.
    pub sort: Sort,
    /// One-based page number.
    pub page: Option<u32>,
    /// Results per page, capped at 100 by the server.
    pub per_page: Option<u32>,
    /// Restrict to a category slug.
    pub category: Option<String>,
    /// Restrict to crates carrying all of these keywords. Takes precedence over
    /// [`SearchParams::keyword`] and [`SearchParams::letter`].
    pub all_keywords: Vec<String>,
    /// Restrict to crates carrying this keyword. Takes precedence over
    /// [`SearchParams::letter`].
    pub keyword: Option<String>,
    /// Restrict to crates whose name starts with this letter.
    pub letter: Option<char>,
    /// Look up these exact crate names.
    pub ids: Vec<String>,
    /// Include crates whose versions have all been yanked.
    pub include_yanked: bool,
}

impl SearchParams {
    /// Build the request URL.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] if the request describes no search at
    /// all, or if pagination is out of range.
    pub fn to_url(&self) -> Result<String> {
        let mut url = Url::parse(API_BASE).and_then(|base| base.join("crates")).map_err(|err| {
            Error::InvalidArgument(format!("could not build the search URL: {err}"))
        })?;

        let describes_a_search = self.query.is_some()
            || self.category.is_some()
            || self.keyword.is_some()
            || self.letter.is_some()
            || !self.all_keywords.is_empty()
            || !self.ids.is_empty();
        if !describes_a_search {
            return Err(Error::InvalidArgument(
                "a search needs at least one of: query, category, keyword, all_keywords, letter, \
                 or ids"
                    .to_owned(),
            ));
        }

        if let Some(page) = self.page
            && page == 0
        {
            return Err(Error::InvalidArgument("page numbers start at 1".to_owned()));
        }
        if let Some(per_page) = self.per_page
            && (per_page == 0 || per_page > MAX_PER_PAGE)
        {
            return Err(Error::InvalidArgument(format!(
                "per_page must be between 1 and {MAX_PER_PAGE}"
            )));
        }

        {
            let mut query = url.query_pairs_mut();
            if let Some(text) = &self.query {
                query.append_pair("q", text);
            }
            query.append_pair("sort", self.sort.as_str());
            if let Some(page) = self.page {
                query.append_pair("page", &page.to_string());
            }
            if let Some(per_page) = self.per_page {
                query.append_pair("per_page", &per_page.to_string());
            }
            if let Some(category) = &self.category {
                query.append_pair("category", category);
            }
            if !self.all_keywords.is_empty() {
                query.append_pair("all_keywords", &self.all_keywords.join(" "));
            }
            if let Some(keyword) = &self.keyword {
                query.append_pair("keyword", keyword);
            }
            if let Some(letter) = self.letter {
                query.append_pair("letter", &letter.to_string());
            }
            for id in &self.ids {
                validate_name(id)?;
                query.append_pair("ids[]", id);
            }
            if self.include_yanked {
                query.append_pair("include_yanked", "yes");
            }
        }

        Ok(url.into())
    }
}

/// Crate-level metadata, as crates.io reports it.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct CrateSummary {
    /// The crate name.
    pub name: String,
    /// The one-line description from `Cargo.toml`.
    #[serde(default)]
    pub description: Option<String>,
    /// The highest version number, including pre-releases.
    #[serde(default)]
    pub max_version: Option<String>,
    /// The highest version that is not a pre-release.
    #[serde(default)]
    pub max_stable_version: Option<String>,
    /// The most recently published version.
    #[serde(default)]
    pub newest_version: Option<String>,
    /// The version crates.io shows by default.
    #[serde(default)]
    pub default_version: Option<String>,
    /// Total downloads across all versions.
    #[serde(default)]
    pub downloads: u64,
    /// Downloads in the last 90 days.
    #[serde(default)]
    pub recent_downloads: Option<u64>,
    /// How many versions have been published.
    #[serde(default)]
    pub num_versions: Option<u32>,
    /// The repository URL, if declared.
    #[serde(default)]
    pub repository: Option<String>,
    /// The homepage URL, if declared.
    #[serde(default)]
    pub homepage: Option<String>,
    /// The documentation URL, if declared.
    #[serde(default)]
    pub documentation: Option<String>,
    /// Keywords, when the response includes them.
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    /// Category slugs, when the response includes them.
    #[serde(default)]
    pub categories: Option<Vec<String>>,
    /// When the crate was first published.
    #[serde(default)]
    pub created_at: Option<String>,
    /// When the crate was last updated.
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Whether every version has been yanked.
    #[serde(default)]
    pub yanked: bool,
    /// Whether this result was an exact name match. Only set by search.
    #[serde(default)]
    pub exact_match: bool,
}

/// Pagination metadata attached to a search response.
#[derive(Clone, Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct SearchMeta {
    /// Total number of matching crates.
    #[serde(default)]
    pub total: u64,
    /// Query string for the next page, if there is one.
    #[serde(default)]
    pub next_page: Option<String>,
    /// Query string for the previous page, if there is one.
    #[serde(default)]
    pub prev_page: Option<String>,
}

/// A crates.io search response.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct SearchResponse {
    /// The matching crates, in the requested order.
    #[serde(default)]
    pub crates: Vec<CrateSummary>,
    /// Pagination metadata.
    #[serde(default)]
    pub meta: SearchMeta,
}

/// A keyword, with how many crates carry it.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct Keyword {
    /// The keyword itself.
    pub keyword: String,
    /// How many crates use it.
    #[serde(default)]
    pub crates_cnt: u64,
}

/// A category, with how many crates belong to it.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct Category {
    /// The human-readable category name.
    pub category: String,
    /// The URL slug.
    pub slug: String,
    /// What the category covers.
    #[serde(default)]
    pub description: Option<String>,
    /// How many crates belong to it.
    #[serde(default)]
    pub crates_cnt: u64,
}

/// A crates.io crate detail response.
#[derive(Clone, Debug, Deserialize)]
#[non_exhaustive]
pub struct CrateResponse {
    /// The crate metadata.
    #[serde(rename = "crate")]
    pub krate: CrateSummary,
    /// Keywords, when requested.
    #[serde(default)]
    pub keywords: Option<Vec<Keyword>>,
    /// Categories, when requested.
    #[serde(default)]
    pub categories: Option<Vec<Category>>,
}

/// Optional sections of the crate detail response.
///
/// The endpoint defaults to returning everything, which for a crate with a long
/// release history means hundreds of kilobytes of per-version records. Asking
/// for only what is needed is the difference between a few kilobytes and a few
/// hundred.
// Not `#[non_exhaustive]`, unlike the response types: a caller builds one of
// these to say what it wants, which the attribute would make impossible from
// outside the crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Include {
    /// Include the keyword list.
    pub keywords: bool,
    /// Include the category list.
    pub categories: bool,
    /// Include the crate's default version.
    pub default_version: bool,
    /// Include the full per-version records.
    pub versions: bool,
    /// Include download statistics.
    pub downloads: bool,
}

impl Default for Include {
    /// Everything that describes the crate itself, and nothing that the sparse
    /// index reports better.
    fn default() -> Self {
        Self {
            keywords: true,
            categories: true,
            default_version: true,
            versions: false,
            downloads: true,
        }
    }
}

impl Include {
    /// The value for the `include` query parameter.
    #[must_use]
    pub fn as_query_value(self) -> String {
        let mut parts = Vec::with_capacity(5);
        if self.keywords {
            parts.push("keywords");
        }
        if self.categories {
            parts.push("categories");
        }
        if self.default_version {
            parts.push("default_version");
        }
        if self.versions {
            parts.push("versions");
        }
        if self.downloads {
            parts.push("downloads");
        }
        parts.join(",")
    }
}

/// The URL for a crate's detail endpoint.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for a name the registry could not have
/// accepted.
pub fn crate_url(name: &str, include: Include) -> Result<String> {
    validate_name(name)?;
    let mut url = Url::parse(API_BASE)
        .and_then(|base| base.join(&format!("crates/{name}")))
        .map_err(|err| Error::InvalidArgument(format!("could not build the crate URL: {err}")))?;
    let value = include.as_query_value();
    // An empty `include` means "everything" to the server, so the parameter is
    // omitted entirely rather than sent empty.
    if !value.is_empty() {
        url.query_pairs_mut().append_pair("include", &value);
    }
    Ok(url.into())
}

/// The URL for a version's rendered README.
///
/// # Errors
///
/// Returns [`Error::InvalidCrateName`] for an invalid name, or
/// [`Error::InvalidVersion`] for a version that could not be a path segment.
pub fn readme_url(name: &str, version: &str) -> Result<String> {
    validate_name(name)?;
    validate_version(version)?;
    let url = Url::parse(API_BASE)
        .and_then(|base| base.join(&format!("crates/{name}/{version}/readme")))
        .map_err(|err| Error::InvalidArgument(format!("could not build the readme URL: {err}")))?;
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_must_describe_some_filter() {
        let empty = SearchParams::default();
        assert!(matches!(empty.to_url(), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn query_parameters_are_percent_encoded() {
        let params = SearchParams {
            query: Some("async runtime & io".to_owned()),
            ..SearchParams::default()
        };
        let url = params.to_url().expect("builds");
        assert!(url.contains("q=async+runtime+%26+io"), "{url}");
        assert!(url.starts_with("https://crates.io/api/v1/crates?"), "{url}");
    }

    #[test]
    fn pagination_bounds_are_enforced_before_the_request_is_made() {
        let base = SearchParams { query: Some("serde".to_owned()), ..SearchParams::default() };

        let zero_page = SearchParams { page: Some(0), ..base.clone() };
        assert!(matches!(zero_page.to_url(), Err(Error::InvalidArgument(_))));

        let too_many = SearchParams { per_page: Some(101), ..base.clone() };
        assert!(matches!(too_many.to_url(), Err(Error::InvalidArgument(_))));

        let ok = SearchParams { page: Some(2), per_page: Some(100), ..base };
        let url = ok.to_url().expect("builds");
        assert!(url.contains("page=2") && url.contains("per_page=100"), "{url}");
    }

    #[test]
    fn all_keywords_are_sent_as_one_space_separated_parameter() {
        let params = SearchParams {
            all_keywords: vec!["async".to_owned(), "http".to_owned()],
            ..SearchParams::default()
        };
        let url = params.to_url().expect("builds");
        assert!(url.contains("all_keywords=async+http"), "{url}");
    }

    #[test]
    fn ids_are_validated_so_a_name_cannot_smuggle_in_a_path() {
        let params =
            SearchParams { ids: vec!["../../admin".to_owned()], ..SearchParams::default() };
        assert!(matches!(params.to_url(), Err(Error::InvalidCrateName { .. })));
    }

    #[test]
    fn the_default_include_omits_the_per_version_records() {
        let include = Include::default();
        assert!(!include.versions, "per-version records come from the sparse index");
        assert_eq!(include.as_query_value(), "keywords,categories,default_version,downloads");
    }

    #[test]
    fn crate_urls_reject_names_that_could_escape_the_path() {
        assert!(matches!(
            crate_url("../secrets", Include::default()),
            Err(Error::InvalidCrateName { .. })
        ));
        let url = crate_url("serde", Include::default()).expect("builds");
        assert!(url.starts_with("https://crates.io/api/v1/crates/serde?include="), "{url}");
    }

    #[test]
    fn an_empty_include_omits_the_parameter_rather_than_sending_it_blank() {
        let nothing = Include {
            keywords: false,
            categories: false,
            default_version: false,
            versions: false,
            downloads: false,
        };
        let url = crate_url("serde", nothing).expect("builds");
        assert_eq!(url, "https://crates.io/api/v1/crates/serde");
    }
}
