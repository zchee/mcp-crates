//! The MCP server: five tools over the crates.io registry.

use std::sync::Arc;

use crates_io_client::{
    Client, CrateIndex, DependencyKind, Error, Include, IndexEntry, SearchParams, Selector,
};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};

use crate::{
    error::{invalid_argument, to_error_data},
    tools::{
        CategoryRef, CrateDependenciesResult, CrateDocumentationResult, CrateHit, CrateInfoResult,
        CrateVersionsResult, DEFAULT_SEARCH_LIMIT, DEFAULT_VERSION_LIMIT, DepKind, DependencyEntry,
        GetCrateDependenciesArgs, GetCrateDocumentationArgs, GetCrateInfoArgs,
        GetCrateVersionsArgs, ItemDoc, LatestReleaseSummary, MAX_SEARCH_LIMIT, MAX_VERSION_LIMIT,
        SearchCratesArgs, SearchCratesResult, VersionEntry,
    },
};

/// Guidance handed to the model alongside the tool list.
const INSTRUCTIONS: &str =
    "\
Read-only access to the crates.io registry: search for Rust crates and inspect their metadata, \
     versions, dependencies and documentation.

Choosing a tool:
- search_crates: find crates by topic, keyword or category when the name is unknown.
- get_crate_info: what a crate is, how popular it is, and what its current release looks like.
- get_crate_versions: the release history, optionally filtered by a Cargo version requirement.
- get_crate_dependencies: what one version of a crate depends on.
- get_crate_documentation: the crate's README, or the documentation of one specific item.

Notes:
- Crate names are exact. If a name might be wrong, search first.
- Version arguments accept an exact version (1.0.219), a Cargo requirement (^1.0), or 'latest'. \
     They default to the newest release that is neither yanked nor a pre-release.
- get_crate_documentation with an 'item' argument reads the rustdoc JSON for that release, which \
     is the only way to get the documentation of an individual type, trait or function.
- crates.io permits one request per second. Results are cached, so repeated questions about the \
     same crate are effectively free, but a sweep across many crates will be paced.";

/// The MCP server.
#[derive(Clone)]
pub struct CratesServer {
    client: Arc<Client>,
    tool_router: ToolRouter<Self>,
}

impl CratesServer {
    /// Wrap a client in the MCP tool surface.
    #[must_use]
    pub fn new(client: Arc<Client>) -> Self {
        Self { client, tool_router: Self::tool_router() }
    }

    /// Parse a caller's version argument.
    fn selector(raw: Option<&str>) -> Result<Selector, ErrorData> {
        raw.unwrap_or("latest").parse::<Selector>().map_err(|err| to_error_data(&err))
    }

    /// Resolve a selector against a crate's index.
    ///
    /// A version named exactly is honoured even if it was yanked, because the
    /// caller asked for that release specifically. A range or the default never
    /// resolves to a yanked release.
    fn resolve<'a>(
        index: &'a CrateIndex,
        selector: &Selector,
    ) -> Result<&'a IndexEntry, ErrorData> {
        let allow_yanked = matches!(selector, Selector::Exact(_));
        index.resolve(selector, allow_yanked).map_err(|err| to_error_data(&err))
    }
}

#[tool_router]
impl CratesServer {
    /// Search crates.io for crates.
    #[tool(description = "Search crates.io for Rust crates by free text, keyword or category. \
                          Use this when the crate name is unknown. Returns each crate's \
                          description, latest version, download counts and repository.")]
    async fn search_crates(
        &self,
        Parameters(args): Parameters<SearchCratesArgs>,
    ) -> Result<Json<SearchCratesResult>, ErrorData> {
        let limit = args.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
        if limit == 0 || limit > MAX_SEARCH_LIMIT {
            return Err(invalid_argument(format!(
                "limit must be between 1 and {MAX_SEARCH_LIMIT}, got {limit}"
            )));
        }
        let page = args.page.unwrap_or(1);
        if page == 0 {
            return Err(invalid_argument("page numbers start at 1"));
        }

        let params = SearchParams {
            query: args.query,
            sort: args.sort.unwrap_or_default().into(),
            page: Some(page),
            per_page: Some(limit),
            category: args.category,
            all_keywords: args.keywords.unwrap_or_default(),
            keyword: None,
            letter: None,
            ids: Vec::new(),
            include_yanked: args.include_yanked.unwrap_or(false),
        };

        let response = self.client.search(&params).await.map_err(|err| to_error_data(&err))?;

        Ok(Json(SearchCratesResult {
            total_matches: response.meta.total,
            page,
            has_more: response.meta.next_page.is_some(),
            crates: response.crates.iter().map(CrateHit::from).collect(),
        }))
    }

    /// Fetch one crate's metadata.
    #[tool(description = "Get metadata for one crate by exact name: description, links, download \
                          counts, keywords, categories, and a summary of its current release \
                          including the minimum supported Rust version and feature list.")]
    async fn get_crate_info(
        &self,
        Parameters(args): Parameters<GetCrateInfoArgs>,
    ) -> Result<Json<CrateInfoResult>, ErrorData> {
        // The API describes the crate; the index describes its releases. They
        // are different origins with independent budgets, so both are in
        // flight at once.
        let (info, index) = tokio::try_join!(
            self.client.crate_info(&args.name, Include::default()),
            self.client.index(&args.name),
        )
        .map_err(|err| to_error_data(&err))?;

        let summary = &info.krate;
        let latest = index.resolve(&Selector::Default, false).ok();
        let docs_rs_url =
            latest.and_then(|entry| self.client.docs_page_url(&summary.name, &entry.vers).ok());

        // The version fields on this API response are only computed correctly
        // when the request also asks for the full per-version records, which
        // for a crate with a long history is hundreds of kilobytes. crates.io
        // reports serde's highest stable version as absent otherwise. The
        // sparse index answers the same question accurately and for far less,
        // so it is the source here.
        Ok(Json(CrateInfoResult {
            name: summary.name.clone(),
            description: summary.description.clone(),
            latest_stable_version: latest.map(|entry| entry.vers.clone()),
            newest_version: index.descending().next().map(|entry| entry.vers.clone()),
            default_version: summary.default_version.clone(),
            total_versions: index.len(),
            downloads: summary.downloads,
            recent_downloads: summary.recent_downloads,
            repository: summary.repository.clone(),
            homepage: summary.homepage.clone(),
            documentation: summary.documentation.clone(),
            docs_rs_url,
            keywords: info
                .keywords
                .as_ref()
                .map(|keywords| keywords.iter().map(|k| k.keyword.clone()).collect())
                .or_else(|| summary.keywords.clone())
                .unwrap_or_default(),
            categories: info
                .categories
                .as_ref()
                .map(|categories| categories.iter().map(CategoryRef::from).collect())
                .unwrap_or_default(),
            created_at: summary.created_at.clone(),
            updated_at: summary.updated_at.clone(),
            yanked: summary.yanked,
            latest_release: latest.map(LatestReleaseSummary::from_entry),
        }))
    }

    /// List a crate's published versions.
    #[tool(description = "List the published versions of a crate, newest first, with yank \
                          status, minimum supported Rust version and publication date. \
                          Optionally filtered by a Cargo version requirement such as '^1.2' or \
                          '>=0.4, <0.6'.")]
    async fn get_crate_versions(
        &self,
        Parameters(args): Parameters<GetCrateVersionsArgs>,
    ) -> Result<Json<CrateVersionsResult>, ErrorData> {
        let limit = args.limit.unwrap_or(DEFAULT_VERSION_LIMIT);
        if limit == 0 || limit > MAX_VERSION_LIMIT {
            return Err(invalid_argument(format!(
                "limit must be between 1 and {MAX_VERSION_LIMIT}, got {limit}"
            )));
        }

        let requirement = match args.requirement.as_deref() {
            Some(raw) => Some(raw.parse::<semver::VersionReq>().map_err(|err| {
                invalid_argument(format!("{raw:?} is not a Cargo version requirement: {err}"))
            })?),
            None => None,
        };

        let include_yanked = args.include_yanked.unwrap_or(false);
        let include_prerelease = args.include_prerelease.unwrap_or(false);

        let index = self.client.index(&args.name).await.map_err(|err| to_error_data(&err))?;

        let matching: Vec<&IndexEntry> = index
            .descending()
            .filter(|entry| {
                let Some(version) = entry.version() else {
                    return false;
                };
                (include_yanked || !entry.yanked)
                    && (include_prerelease || version.pre.is_empty())
                    && requirement.as_ref().is_none_or(|req| req.matches(version))
            })
            .collect();

        let truncated = matching.len() > limit as usize;
        let versions =
            matching.iter().take(limit as usize).map(|entry| VersionEntry::from(*entry)).collect();

        Ok(Json(CrateVersionsResult {
            name: index.name().to_owned(),
            total_versions: index.len(),
            latest_stable_version: index
                .resolve(&Selector::Default, false)
                .ok()
                .map(|entry| entry.vers.clone()),
            newest_version: index.descending().next().map(|entry| entry.vers.clone()),
            truncated,
            versions,
        }))
    }

    /// Report one version's dependencies.
    #[tool(description = "List what one version of a crate depends on, with each dependency's \
                          version requirement, enabled features, target platform and whether it \
                          is optional. Defaults to runtime dependencies of the newest stable \
                          release.")]
    async fn get_crate_dependencies(
        &self,
        Parameters(args): Parameters<GetCrateDependenciesArgs>,
    ) -> Result<Json<CrateDependenciesResult>, ErrorData> {
        let selector = Self::selector(args.version.as_deref())?;
        let kinds = args.kinds.unwrap_or_else(|| vec![DepKind::Normal]);
        let include_optional = args.include_optional.unwrap_or(true);

        let index = self.client.index(&args.name).await.map_err(|err| to_error_data(&err))?;
        let entry = Self::resolve(&index, &selector)?;

        let collect = |kind: DepKind| -> Vec<DependencyEntry> {
            if !kinds.contains(&kind) {
                return Vec::new();
            }
            entry
                .deps_of_kind(DependencyKind::from(kind))
                .filter(|dep| include_optional || !dep.optional)
                .map(DependencyEntry::from)
                .collect()
        };

        Ok(Json(CrateDependenciesResult {
            name: index.name().to_owned(),
            version: entry.vers.clone(),
            yanked: entry.yanked,
            normal: collect(DepKind::Normal),
            dev: collect(DepKind::Dev),
            build: collect(DepKind::Build),
        }))
    }

    /// Fetch a crate's documentation.
    #[tool(description = "Get a crate's documentation: its README as Markdown, or, with the \
                          'item' argument, the doc comment of one specific type, trait, function \
                          or module read from the rustdoc JSON docs.rs generated for that \
                          release.")]
    async fn get_crate_documentation(
        &self,
        Parameters(args): Parameters<GetCrateDocumentationArgs>,
    ) -> Result<Json<CrateDocumentationResult>, ErrorData> {
        let selector = Self::selector(args.version.as_deref())?;
        let max_chars = args
            .max_readme_chars
            .map_or(crates_io_client::DEFAULT_README_CHARS, |chars| chars as usize);
        // A README is the useful default answer, but when a specific item was
        // asked for it is usually just noise around the answer.
        let want_readme = args.include_readme.unwrap_or(args.item.is_none());

        let index = self.client.index(&args.name).await.map_err(|err| to_error_data(&err))?;
        let entry = Self::resolve(&index, &selector)?;
        let name = index.name().to_owned();
        let version = entry.vers.clone();

        let docs_rs_url =
            self.client.docs_page_url(&name, &version).map_err(|err| to_error_data(&err))?;

        // The README lives on the crates.io CDN and the documentation on
        // docs.rs, so the two requests overlap rather than queue.
        let readme_task = async {
            if want_readme {
                Some(self.client.readme(&name, &version, max_chars).await)
            } else {
                None
            }
        };
        let docs_task = async {
            match args.item.as_deref() {
                Some(_) => Docs::Index(self.client.doc_index(&name, &version).await),
                // Without an item to look up, the cheap status check answers
                // "is there documentation" without downloading all of it.
                None => Docs::Status(self.client.docs_status(&name, &version).await),
            }
        };
        let (readme_result, docs_result) = tokio::join!(readme_task, docs_task);

        let mut notes: Vec<String> = Vec::new();

        // A crate with no README is normal, and not a reason to fail the call.
        let readme = match readme_result {
            Some(Ok(markdown)) => Some(markdown.as_str().to_owned()),
            Some(Err(err)) => {
                notes.push(format!("the README could not be read: {err}"));
                None
            },
            None => None,
        };

        let mut item = None;
        let mut suggestions: Vec<ItemDoc> = Vec::new();
        let mut documented_items = None;

        let docs_rs_built = match docs_result {
            Docs::Status(Ok(status)) => Some(status.doc_status),
            Docs::Status(Err(err)) => {
                notes.push(format!("docs.rs has no build for this release: {err}"));
                Some(false)
            },
            Docs::Index(Ok(doc_index)) => {
                documented_items = Some(doc_index.len());
                let query = args.item.as_deref().unwrap_or_default();
                let lookup = doc_index.lookup(query);
                item = lookup.found.map(ItemDoc::from);
                suggestions = lookup.suggestions.iter().copied().map(ItemDoc::from).collect();
                if item.is_none() {
                    notes.push(if suggestions.is_empty() {
                        format!("no item matching {query:?} is documented in this release")
                    } else {
                        format!(
                            "{query:?} matched {} items rather than one; they are listed under \
                             'suggestions'",
                            suggestions.len()
                        )
                    });
                }
                Some(true)
            },
            Docs::Index(Err(err)) => {
                notes.push(format!("item documentation is unavailable: {err}"));
                Some(false)
            },
        };

        Ok(Json(CrateDocumentationResult {
            name,
            version,
            docs_rs_url,
            docs_rs_built,
            readme,
            item,
            suggestions,
            documented_items,
            note: (!notes.is_empty()).then(|| notes.join("; ")),
        }))
    }
}

/// Which documentation request was made, so both arms can share one join.
enum Docs {
    Status(Result<Arc<crates_io_client::BuildStatus>, Error>),
    Index(Result<Arc<crates_io_client::DocIndex>, Error>),
}

// Bound to the stored router rather than the macro's default of
// `Self::tool_router()`, which would rebuild every tool's schema on each
// `tools/list` and `tools/call`.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for CratesServer {
    fn get_info(&self) -> ServerInfo {
        let mut implementation =
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        implementation.title = Some("crates.io".to_owned());
        implementation.description = Some(env!("CARGO_PKG_DESCRIPTION").to_owned());
        implementation.website_url = Some(env!("CARGO_PKG_REPOSITORY").to_owned());

        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = implementation;
        info.instructions = Some(INSTRUCTIONS.to_owned());
        info
    }
}
