//! A rate-limited, cache-aware read client for the Rust crate ecosystem.
//!
//! # What this crate reads
//!
//! Crate data lives in four places, and which one a question is asked of
//! dominates how much it costs to answer:
//!
//! | Source | Serves | Budget |
//! |---|---|---|
//! | `crates.io/api/v1` | search, crate metadata | one request per second |
//! | `index.crates.io` | every version, dependency and feature of a crate | CDN, `ETag`, built for Cargo |
//! | `static.crates.io` | rendered READMEs | CDN, `ETag`, immutable per version |
//! | `docs.rs` | build status, rustdoc JSON | its own budget |
//!
//! The API budget is the scarce one, so this client spends it only on questions
//! the API alone can answer. Versions, dependencies and features come from the
//! sparse index: one CDN request returns all of them for a crate, where the API
//! would need one request per version.
//!
//! # How requests are avoided
//!
//! Every fetch passes through four layers, in order:
//!
//! 1. a freshness check, which answers from memory at no cost;
//! 2. a coalescing gate, so concurrent callers asking the same question issue one request
//!    between them;
//! 3. conditional revalidation, so a stale-but-unchanged resource costs headers rather
//!    than a body;
//! 4. a per-origin pacer, which is what enforces the one-request-per-second policy on
//!    whatever is left.
//!
//! Parsed forms are memoized alongside the bytes they came from and survive
//! revalidation, so a crate's index document is parsed once per distinct
//! payload however many times it is read.
//!
//! # Usage
//!
//! crates.io requires a `User-Agent` that identifies the application and offers
//! a way to make contact. [`Config::new`] takes it as a required argument.
//!
//! ```no_run
//! # async fn example() -> Result<(), crates_io_client::Error> {
//! use crates_io_client::{Client, Config};
//!
//! let client = Client::new(Config::new("my-app/1.0 (https://example.com/my-app)"))?;
//!
//! // One CDN request; every version, dependency and feature comes back with it.
//! let index = client.index("serde").await?;
//! let newest = index.resolve(&Default::default(), false)?;
//! println!("{} {}", index.name(), newest.vers);
//! # Ok(())
//! # }
//! ```

mod api;
mod cache;
mod config;
mod docs;
mod error;
mod fetch;
mod gate;
mod index;
mod pacer;
mod readme;
mod version;

use std::{sync::Arc, time::Duration};

use moka::future::Cache;

pub use crate::{
    api::{
        Category as CrateCategory, CrateResponse, CrateSummary, Include, Keyword, SearchMeta,
        SearchParams, SearchResponse, Sort,
    },
    config::{Config, Ttl},
    docs::{BuildStatus, DocIndex, DocItem, Lookup, Reexport},
    error::{Category, Error, Result},
    fetch::{Origin, Stats},
    index::{CrateIndex, DependencyKind, IndexDep, IndexEntry, validate_name},
    readme::DEFAULT_MAX_CHARS as DEFAULT_README_CHARS,
    version::Selector,
};
use crate::{
    fetch::{Fetcher, Policy},
    gate::Gates,
};

/// Approximate ceiling on the parsed-documentation cache.
const DOC_INDEX_CAPACITY_BYTES: u64 = 64 * 1024 * 1024;

/// How many crate versions may have a documentation gate at once.
const DOC_GATE_CAPACITY: u64 = 256;

/// How many rustdoc documents may be expanded and parsed at the same time.
///
/// The gate above deduplicates identical requests but says nothing about
/// distinct ones. Expanding a document holds the compressed body, the expanded
/// bytes and the parsed structure at once, so without a ceiling a handful of
/// concurrent lookups across different crates add up to more memory than the
/// machine has.
const CONCURRENT_DOC_PARSES: usize = 2;

/// A client for crates.io, its sparse index, and docs.rs.
///
/// Cheap to clone behind an [`Arc`]; all caching and pacing state is shared, so
/// one client per process gets the most out of both.
#[derive(Debug)]
pub struct Client {
    fetcher: Fetcher,
    ttl: Ttl,
    doc_indexes: Cache<Arc<str>, Arc<DocIndex>>,
    doc_gates: Gates,
    doc_parses: tokio::sync::Semaphore,
    max_rustdoc_bytes: usize,
}

impl Client {
    /// Build a client.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] if the configured user agent is
    /// empty, which crates.io does not permit.
    pub fn new(config: Config) -> Result<Self> {
        Self::with_ttl(config, Ttl::default())
    }

    /// Build a client with non-default cache lifetimes.
    ///
    /// # Errors
    ///
    /// As [`Client::new`].
    pub fn with_ttl(config: Config, ttl: Ttl) -> Result<Self> {
        Ok(Self {
            fetcher: Fetcher::new(&config)?,
            ttl,
            doc_indexes: Cache::builder()
                .max_capacity(DOC_INDEX_CAPACITY_BYTES)
                .weigher(|_key: &Arc<str>, value: &Arc<DocIndex>| value.weight())
                .time_to_live(ttl.rustdoc)
                .build(),
            doc_gates: Gates::new(DOC_GATE_CAPACITY),
            doc_parses: tokio::sync::Semaphore::new(CONCURRENT_DOC_PARSES),
            max_rustdoc_bytes: config.max_rustdoc_bytes,
        })
    }

    /// Cache and traffic counters, for diagnostics.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.fetcher.stats()
    }

    /// How far the crates.io API pacing queue currently extends.
    #[must_use]
    pub fn api_backlog(&self) -> Duration {
        self.fetcher.backlog().0
    }

    /// Search crates.io.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] if the request describes no filter,
    /// or an upstream error if the request fails.
    pub async fn search(&self, params: &SearchParams) -> Result<Arc<SearchResponse>> {
        let url = params.to_url()?;
        let body =
            self.fetcher.get(&url, Policy::cached(self.ttl.search, self.ttl.negative)).await?;
        body.derive(|bytes| {
            serde_json::from_slice::<SearchResponse>(bytes)
                .map_err(|err| Error::Decode { url: url.clone(), message: err.to_string() })
        })
    }

    /// Fetch a crate's metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CrateNotFound`] if no such crate exists.
    pub async fn crate_info(&self, name: &str, include: Include) -> Result<Arc<CrateResponse>> {
        let url = api::crate_url(name, include)?;
        let body = self
            .fetcher
            .get(&url, Policy::cached(self.ttl.crate_meta, self.ttl.negative))
            .await
            .map_err(|err| missing_crate(err, name))?;
        body.derive(|bytes| {
            serde_json::from_slice::<CrateResponse>(bytes)
                .map_err(|err| Error::Decode { url: url.clone(), message: err.to_string() })
        })
    }

    /// Fetch a crate's sparse-index document: every version, with its
    /// dependencies, features and yank status.
    ///
    /// This is the cheap way to answer version and dependency questions. One
    /// request covers the whole crate, and the response revalidates against an
    /// `ETag`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CrateNotFound`] if no such crate exists.
    pub async fn index(&self, name: &str) -> Result<Arc<CrateIndex>> {
        let url = index::index_url(name);
        index::validate_name(name)?;
        let body = self
            .fetcher
            .get(&url, Policy::cached(self.ttl.index, self.ttl.negative))
            .await
            .map_err(|err| missing_crate(err, name))?;
        body.derive(|bytes| CrateIndex::parse(name, bytes))
    }

    /// Fetch a version's README, converted to Markdown.
    ///
    /// The first call spends one API request to resolve the redirect to the
    /// static CDN; the rendered document itself is immutable for a published
    /// version and is cached accordingly.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ReadmeUnavailable`] if the release has no rendered
    /// README, which is the case for anything published before crates.io began
    /// rendering them.
    pub async fn readme(&self, name: &str, version: &str, max_chars: usize) -> Result<Arc<String>> {
        let url = api::readme_url(name, version)?;
        let body = self
            .fetcher
            .get(&url, Policy::cached(self.ttl.readme, self.ttl.negative))
            .await
            .map_err(|err| missing_readme(err, name, version))?;
        // Only the conversion is memoized. Folding `max_chars` into the
        // memoized value would pin the first caller's budget onto every later
        // reader of the same cached document, for as long as it stays cached.
        let markdown = body.derive(|bytes| {
            let html = String::from_utf8_lossy(bytes);
            Ok::<_, Error>(readme::to_markdown(&html))
        })?;

        if markdown.chars().count() <= max_chars {
            return Ok(markdown);
        }
        Ok(Arc::new(readme::truncate(&markdown, max_chars)))
    }

    /// Fetch a release's docs.rs build status.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DocsUnavailable`] if docs.rs has no record of the
    /// release.
    pub async fn docs_status(&self, name: &str, version: &str) -> Result<Arc<BuildStatus>> {
        let url = docs::status_url(name, version)?;
        let body = self
            .fetcher
            .get(&url, Policy::cached(self.ttl.docs_status, self.ttl.negative))
            .await
            .map_err(|err| {
                missing_docs(err, name, version, "docs.rs has no record of this release")
            })?;
        body.derive(|bytes| {
            serde_json::from_slice::<BuildStatus>(bytes)
                .map_err(|err| Error::Decode { url: url.clone(), message: err.to_string() })
        })
    }

    /// Fetch and index a release's rustdoc JSON.
    ///
    /// This is the only source of item-level documentation: the prose attached
    /// to individual types, traits and functions. It is available only for
    /// releases docs.rs built successfully with a recent enough toolchain.
    ///
    /// # Errors
    ///
    /// Returns [`Error::DocsUnavailable`] if docs.rs has no rustdoc JSON for
    /// the release, and [`Error::BodyTooLarge`] if the document expands past
    /// the configured ceiling.
    pub async fn doc_index(&self, name: &str, version: &str) -> Result<Arc<DocIndex>> {
        let key: Arc<str> = Arc::from(format!("{name}@{version}"));
        if let Some(hit) = self.doc_indexes.get(&key).await {
            return Ok(hit);
        }

        // Coalesced here rather than relying on the HTTP layer's gate, because
        // the expensive part is downstream of the transfer: decompressing and
        // parsing several megabytes, which concurrent callers would otherwise
        // each do in full only to keep one result.
        let gate = self.doc_gates.get(&key).await;
        let _guard = gate.lock().await;
        if let Some(hit) = self.doc_indexes.get(&key).await {
            return Ok(hit);
        }

        let url = docs::rustdoc_url(name, version)?;
        // The body is not retained: its parsed form is roughly ten times its
        // compressed size, and the body cache is bounded by transferred bytes,
        // so keeping it would hold far more memory than that bound describes.
        let body =
            self.fetcher.get(&url, Policy::uncached(self.ttl.negative)).await.map_err(|err| {
                missing_docs(
                    err,
                    name,
                    version,
                    "docs.rs published no rustdoc JSON for this release, which it only generates \
                     for recent enough builds",
                )
            })?;

        // Expanding and parsing is where both the time and the memory go, and
        // it is CPU-bound, so it is admitted a couple at a time and run off the
        // async runtime rather than occupying a reactor thread.
        let _permit = self.doc_parses.acquire().await.map_err(|_| {
            Error::Internal("the documentation parser has been shut down".to_owned())
        })?;

        let limit = self.max_rustdoc_bytes;
        let owner = name.to_owned();
        let compressed = body.body.clone();
        drop(body);

        let parsed = tokio::task::spawn_blocking(move || {
            let expanded = docs::decompress_rustdoc(&owner, &compressed, limit)?;
            // Dead once expanded; releasing it keeps one fewer copy alive
            // across the parse, which is the peak.
            drop(compressed);
            DocIndex::parse(&owner, &expanded)
        })
        .await
        .map_err(|err| Error::Decode {
            url: docs::rustdoc_url(name, version).unwrap_or_default(),
            message: format!("the documentation parser did not finish: {err}"),
        })??;

        let parsed = Arc::new(parsed);
        self.doc_indexes.insert(key, Arc::clone(&parsed)).await;
        Ok(parsed)
    }

    /// The human-facing docs.rs page for a release.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidCrateName`] for an invalid name.
    pub fn docs_page_url(&self, name: &str, version: &str) -> Result<String> {
        docs::html_url(name, version)
    }
}

/// Translate a `404` on a crate-level resource into a crate-level error.
fn missing_crate(err: Error, name: &str) -> Error {
    match err {
        Error::Upstream { status: 404, .. } => Error::CrateNotFound { name: name.to_owned() },
        other => other,
    }
}

/// Translate the absence of a rendered README into a README-specific error.
///
/// The version itself may well exist: releases published before crates.io
/// rendered READMEs have none, and the static CDN reports that with a `403`
/// rather than a `404`. Reporting it as a missing *version* would be actively
/// misleading.
fn missing_readme(err: Error, name: &str, version: &str) -> Error {
    match err {
        Error::Upstream { status: 403 | 404, .. } => {
            Error::ReadmeUnavailable { name: name.to_owned(), version: version.to_owned() }
        },
        other => other,
    }
}

/// Translate a `404` from docs.rs into a documentation-specific error.
///
/// The reason is supplied by the caller because the two docs.rs endpoints mean
/// different things by `404`: the status endpoint has no record of the release
/// at all, while the rustdoc JSON endpoint may simply have no JSON for a build
/// old enough to predate it.
fn missing_docs(err: Error, name: &str, version: &str, reason: &str) -> Error {
    match err {
        Error::Upstream { status: 404, .. } => Error::DocsUnavailable {
            name: name.to_owned(),
            version: version.to_owned(),
            reason: reason.to_owned(),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_requires_a_user_agent() {
        assert!(matches!(Client::new(Config::new("   ")), Err(Error::InvalidArgument(_))));
        assert!(Client::new(Config::new("mcp-crates/0.1 (https://example.invalid)")).is_ok());
    }

    #[test]
    fn not_found_is_reported_against_the_thing_that_was_missing() {
        let upstream =
            || Error::Upstream { url: "https://crates.io/x".to_owned(), status: 404, detail: None };

        assert!(matches!(missing_crate(upstream(), "serde"), Error::CrateNotFound { .. }));
        assert!(matches!(
            missing_readme(upstream(), "serde", "0.0.0"),
            Error::ReadmeUnavailable { .. }
        ));
        assert!(matches!(
            missing_docs(upstream(), "serde", "9.9.9", "because"),
            Error::DocsUnavailable { .. }
        ));
    }

    #[test]
    fn a_release_with_no_rendered_readme_is_not_reported_as_a_missing_version() {
        // The static CDN is object storage without list permission, so it
        // answers for an object that was never stored with 403, not 404.
        let forbidden = Error::Upstream {
            url: "https://static.crates.io/x".to_owned(),
            status: 403,
            detail: None,
        };
        assert!(matches!(
            missing_readme(forbidden, "serde", "0.0.0"),
            Error::ReadmeUnavailable { .. }
        ));
    }

    #[test]
    fn errors_other_than_not_found_pass_through_unchanged() {
        let server_error =
            Error::Upstream { url: "https://crates.io/x".to_owned(), status: 503, detail: None };
        assert!(matches!(
            missing_crate(server_error, "serde"),
            Error::Upstream { status: 503, .. }
        ));
    }
}
