//! Tunable transport and cache limits.

use std::{path::PathBuf, time::Duration};

/// Transport, pacing and cache limits for a [`crate::Client`].
///
/// The defaults implement the crates.io crawler policy: at most one API request
/// per second, and a descriptive user agent. Only [`Config::user_agent`] has no
/// safe default, so it is a required argument to [`Config::new`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Config {
    /// Value sent in the `User-Agent` header.
    ///
    /// crates.io requires this to identify the application and to provide a way
    /// to make contact, such as a repository URL or an email address.
    pub user_agent: String,

    /// Minimum spacing between two crates.io API requests.
    ///
    /// The published crawler policy is one request per second; raising the rate
    /// beyond that risks having the client blocked.
    pub api_min_interval: Duration,

    /// Minimum spacing between two requests to the crates.io CDN hosts
    /// (`index.crates.io` and `static.crates.io`).
    ///
    /// These are static, cache-friendly origins built to serve Cargo itself, so
    /// they tolerate more traffic than the API. The client is still paced to
    /// stay a well-behaved consumer.
    pub cdn_min_interval: Duration,

    /// Minimum spacing between two docs.rs requests.
    pub docs_min_interval: Duration,

    /// Longest a request may wait behind the pacing queue before it is shed.
    pub max_queue_wait: Duration,

    /// Whole-request timeout, including the body transfer.
    pub request_timeout: Duration,

    /// Timeout for establishing a new connection.
    pub connect_timeout: Duration,

    /// Approximate ceiling, in bytes, on the shared response cache.
    pub cache_capacity_bytes: u64,

    /// Largest response body the client will buffer.
    pub max_body_bytes: usize,

    /// Largest rustdoc JSON document the client will decompress.
    ///
    /// Parsing holds roughly twice the expanded size again, and several
    /// documents can be in flight at once, so this is the main lever on how
    /// much memory documentation lookups can occupy.
    pub max_rustdoc_bytes: usize,

    /// How many times a transient failure is retried before giving up.
    pub max_retries: u32,

    /// Whether parsed documentation may be kept on disk between runs.
    ///
    /// The server is spawned per client session, so without this every session
    /// re-downloads and re-parses rustdoc JSON that cannot have changed.
    pub disk_cache: bool,

    /// Where to keep it, or `None` to use the platform's cache directory.
    ///
    /// Exists so that a test can point at a directory it owns, and so that a
    /// caller who keeps their caches somewhere specific can say so.
    pub cache_dir: Option<PathBuf>,

    /// Ceiling on the disk cache, enforced by deleting the oldest entries at
    /// startup.
    pub disk_cache_capacity_bytes: u64,

    /// How many rustdoc documents may be expanded and parsed at the same time.
    ///
    /// The coalescing gate deduplicates identical requests but says nothing
    /// about distinct ones. Expanding a document holds the compressed body, the
    /// expanded bytes and the parsed structure at once, so without a ceiling a
    /// handful of concurrent lookups across different crates add up to more
    /// memory than the machine has.
    ///
    /// Configurable rather than constant because the right value depends on how
    /// much memory the host has and how large the documents being read are,
    /// which is a deployment question rather than a library one.
    pub concurrent_doc_parses: usize,
}

impl Config {
    /// The disk store this configuration describes, or `None` when the disk
    /// cache is disabled or the platform names no cache directory.
    ///
    /// A disk cache the platform will not name a directory for is a disk cache
    /// that does not run, rather than one guessing at a path.
    pub(crate) fn disk_store(&self) -> Option<crate::disk::Store> {
        self.disk_cache
            .then(|| {
                self.cache_dir.clone().map_or_else(crate::disk::Store::discover, |root| {
                    Some(crate::disk::Store::at(root))
                })
            })
            .flatten()
    }

    /// Build a configuration with the crates.io-compliant defaults and the
    /// given user agent.
    #[must_use]
    pub fn new(user_agent: impl Into<String>) -> Self {
        Self {
            user_agent: user_agent.into(),
            api_min_interval: Duration::from_secs(1),
            cdn_min_interval: Duration::from_millis(100),
            docs_min_interval: Duration::from_millis(200),
            max_queue_wait: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            cache_capacity_bytes: 128 * 1024 * 1024,
            max_body_bytes: 16 * 1024 * 1024,
            max_rustdoc_bytes: 48 * 1024 * 1024,
            max_retries: 3,
            disk_cache: true,
            cache_dir: None,
            disk_cache_capacity_bytes: crate::disk::DEFAULT_CACHE_CAPACITY_BYTES,
            concurrent_doc_parses: DEFAULT_CONCURRENT_DOC_PARSES,
        }
    }
}

/// How many rustdoc documents may be expanded and parsed at the same time by
/// default.
///
/// Two, because the memory each parse holds is the binding constraint rather
/// than the CPU it uses, and two is what keeps a pair of concurrent lookups on
/// large crates inside a sensible footprint.
pub const DEFAULT_CONCURRENT_DOC_PARSES: usize = 2;

/// How long a given class of response stays servable without contacting the
/// origin.
///
/// Values are chosen from how quickly the underlying data can actually change.
/// Anything addressed by an exact version is immutable once published, so it is
/// held for a long time; anything reflecting live registry state is held only
/// briefly, and revalidated cheaply where the origin supplies a validator.
// Not `#[non_exhaustive]`: this exists to be constructed by a caller adjusting
// one lifetime, which the attribute would make impossible from outside.
#[derive(Clone, Copy, Debug)]
pub struct Ttl {
    /// Search results and other list endpoints.
    pub search: Duration,
    /// Crate-level metadata, which changes whenever a version is published.
    pub crate_meta: Duration,
    /// A crate's sparse-index document.
    pub index: Duration,
    /// A rendered README for one exact version. Immutable once published.
    pub readme: Duration,
    /// A docs.rs build status.
    pub docs_status: Duration,
    /// A rustdoc JSON document for one exact version. Immutable once built.
    pub rustdoc: Duration,
    /// How long a `404` is remembered, so a typo does not spend the API budget
    /// on every repeat.
    pub negative: Duration,
}

impl Default for Ttl {
    fn default() -> Self {
        Self {
            search: Duration::from_secs(300),
            crate_meta: Duration::from_secs(300),
            index: Duration::from_secs(600),
            readme: Duration::from_secs(7 * 24 * 3600),
            docs_status: Duration::from_secs(3600),
            rustdoc: Duration::from_secs(7 * 24 * 3600),
            negative: Duration::from_secs(60),
        }
    }
}
