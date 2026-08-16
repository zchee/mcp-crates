//! Cache-aware, rate-limited HTTP fetching.
//!
//! Every byte this crate reads from the network passes through [`Fetcher::get`],
//! which layers four independent savings on top of a plain request:
//!
//! 1. **Freshness.** A cached response inside its lifetime is returned without touching
//!    the network at all, so it costs nothing against the request budget.
//! 2. **Coalescing.** Concurrent callers asking for the same URL are funnelled through
//!    one gate, so a burst of tool calls issues a single request.
//! 3. **Revalidation.** A stale response that carries an `ETag` or `Last-Modified` is
//!    refreshed with a conditional request, so the common `304` answer transfers headers
//!    instead of a body.
//! 4. **Pacing.** Whatever survives the layers above is emitted through a per-origin
//!    [`Pacer`], which is what actually enforces the crates.io one-request-per-second
//!    policy.
//!
//! Redirects are followed manually rather than by `reqwest`, for two reasons:
//! each hop must be paced and counted like any other request, and each hop must
//! be checked against the allowed host list so that a redirect cannot steer the
//! client at an arbitrary origin.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::BytesMut;
use moka::future::Cache;
use reqwest::{
    StatusCode,
    header::{
        CACHE_CONTROL, ETAG, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED, LOCATION,
        RETRY_AFTER,
    },
};
use url::Url;

use crate::{
    cache::CachedBody,
    config::Config,
    disk::{Store as DiskStore, StoredBody, body_key},
    error::{Error, Result},
    gate::Gates,
    pacer::Pacer,
};

/// Most redirect hops the client will follow for one logical request.
const MAX_REDIRECTS: u8 = 5;

/// Longest error detail retained from an upstream error body.
const MAX_DETAIL_LEN: usize = 240;

/// An upstream host this client is permitted to contact.
///
/// Each origin has its own pacing budget, because the three differ by orders of
/// magnitude in how much traffic they are built to absorb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Origin {
    /// The crates.io REST API, limited to one request per second by policy.
    Api,
    /// The crates.io static CDN hosts: the sparse index and rendered READMEs.
    Cdn,
    /// docs.rs, used for build status and rustdoc JSON.
    Docs,
}

impl Origin {
    /// Classify a host, or return `None` if the client must not contact it.
    #[must_use]
    fn classify(host: &str) -> Option<Self> {
        match host {
            "crates.io" => Some(Self::Api),
            "index.crates.io" | "static.crates.io" => Some(Self::Cdn),
            "docs.rs" | "static.docs.rs" => Some(Self::Docs),
            _ => None,
        }
    }

    /// The host label used in diagnostics.
    #[must_use]
    const fn label(self) -> &'static str {
        match self {
            Self::Api => "crates.io",
            Self::Cdn => "crates.io CDN",
            Self::Docs => "docs.rs",
        }
    }
}

/// How one request should interact with the shared cache.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct Policy {
    /// How long a successful response may be served without revalidation.
    pub ttl: Duration,
    /// How long a `404` is remembered.
    pub negative_ttl: Duration,
    /// Whether the body is kept in the shared cache.
    ///
    /// Set to `false` for payloads that are large and only useful once parsed;
    /// the caller caches the parsed projection instead.
    pub store: bool,
}

impl Policy {
    /// A cached policy with the given lifetime.
    #[must_use]
    pub fn cached(ttl: Duration, negative_ttl: Duration) -> Self {
        Self { ttl, negative_ttl, store: true }
    }

    /// Fetch without retaining the body.
    ///
    /// For a payload that is only useful once parsed and whose parsed form is
    /// far larger than the bytes it came from: keeping the body would also pin
    /// the projection memoized beside it, and the body cache is bounded by
    /// transferred bytes, which would then understate what is being held.
    #[must_use]
    pub fn uncached(negative_ttl: Duration) -> Self {
        Self { ttl: Duration::ZERO, negative_ttl, store: false }
    }
}

/// Counters describing how well the caching layers are working.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Responses served from cache without any network traffic.
    pub cache_hits: u64,
    /// Requests that found a fresh entry after waiting behind another caller,
    /// i.e. requests saved by coalescing.
    pub coalesced: u64,
    /// Requests actually put on the wire, including redirect hops.
    pub network_requests: u64,
    /// Conditional requests answered with `304 Not Modified`.
    pub not_modified: u64,
    /// Response body bytes received.
    pub bytes_received: u64,
    /// Requests shed because the pacing queue was saturated.
    pub shed: u64,
    /// Transient failures that were retried.
    pub retries: u64,
    /// Documentation indexes read back from the disk cache instead of fetched.
    pub disk_hits: u64,
    /// Documentation indexes written to the disk cache.
    pub disk_writes: u64,
}

#[derive(Debug, Default)]
struct Counters {
    cache_hits: AtomicU64,
    coalesced: AtomicU64,
    network_requests: AtomicU64,
    not_modified: AtomicU64,
    bytes_received: AtomicU64,
    shed: AtomicU64,
    retries: AtomicU64,
    disk_hits: AtomicU64,
    disk_writes: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> Stats {
        Stats {
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            coalesced: self.coalesced.load(Ordering::Relaxed),
            network_requests: self.network_requests.load(Ordering::Relaxed),
            not_modified: self.not_modified.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            shed: self.shed.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            disk_writes: self.disk_writes.load(Ordering::Relaxed),
        }
    }
}

/// Outcome of a single trip to the network.
enum Wire {
    /// A body was transferred.
    Body(CachedBody),
    /// The origin confirmed the cached copy is still current, and said how long
    /// the refreshed copy may be served for.
    NotModified(Duration),
}

/// The shared HTTP layer.
#[derive(Debug)]
pub struct Fetcher {
    client: reqwest::Client,
    bodies: Cache<Arc<str>, Arc<CachedBody>>,
    gates: Gates,
    api: Pacer,
    cdn: Pacer,
    docs: Pacer,
    counters: Counters,
    jitter: AtomicU64,
    max_body_bytes: usize,
    max_retries: u32,
    /// Where a persistable body survives between runs, when it may.
    disk: Option<DiskStore>,
}

impl Fetcher {
    /// Build a fetcher from a configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the user agent is empty or the underlying HTTP
    /// client cannot be constructed.
    pub fn new(config: &Config) -> Result<Self> {
        if config.user_agent.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "the user agent must not be empty: crates.io requires a header that identifies \
                 the application and offers a way to make contact"
                    .to_owned(),
            ));
        }

        let client = reqwest::Client::builder()
            .user_agent(config.user_agent.clone())
            // Redirects are handled by this module so that every hop is paced
            // and host-checked.
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .timeout(config.request_timeout)
            .connect_timeout(config.connect_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            .build()
            .map_err(|source| Error::Network { url: "<client construction>".to_owned(), source })?;

        Ok(Self {
            client,
            bodies: Cache::builder()
                .max_capacity(config.cache_capacity_bytes)
                .weigher(|_key: &Arc<str>, value: &Arc<CachedBody>| value.weight())
                .build(),
            gates: Gates::new(4096),
            api: Pacer::new("crates.io", config.api_min_interval, config.max_queue_wait),
            cdn: Pacer::new("crates.io CDN", config.cdn_min_interval, config.max_queue_wait),
            docs: Pacer::new("docs.rs", config.docs_min_interval, config.max_queue_wait),
            counters: Counters::default(),
            jitter: AtomicU64::new(0x9E37_79B9_7F4A_7C15),
            max_body_bytes: config.max_body_bytes,
            max_retries: config.max_retries,
            disk: config.disk_store(),
        })
    }

    /// A snapshot of the cache and traffic counters.
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.counters.snapshot()
    }

    /// How far each origin's pacing queue currently extends.
    #[must_use]
    pub fn backlog(&self) -> (Duration, Duration, Duration) {
        (self.api.backlog(), self.cdn.backlog(), self.docs.backlog())
    }

    fn pacer(&self, origin: Origin) -> &Pacer {
        match origin {
            Origin::Api => &self.api,
            Origin::Cdn => &self.cdn,
            Origin::Docs => &self.docs,
        }
    }

    /// Fetch a URL, consulting and populating the shared cache.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Upstream`] for any non-success status, including a
    /// cached `404`, and [`Error::Network`] for transport failures that
    /// survived the retry budget.
    pub async fn get(&self, url: &str, policy: Policy) -> Result<Arc<CachedBody>> {
        let key: Arc<str> = Arc::from(url);

        if let Some(hit) = self.bodies.get(&key).await
            && hit.is_fresh()
        {
            self.counters.cache_hits.fetch_add(1, Ordering::Relaxed);
            return Self::interpret(hit);
        }

        // Serialize concurrent callers for this URL so a burst costs one
        // request rather than one per caller.
        let gate = self.gates.get(&key).await;
        let _guard = gate.lock().await;

        let mut stale = self.bodies.get(&key).await;
        if let Some(hit) = &stale
            && hit.is_fresh()
        {
            // Another caller refreshed this entry while we waited.
            self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
            return Self::interpret(Arc::clone(hit));
        }

        // The third tier, consulted only when memory holds nothing at all and
        // only for the one kind of body worth keeping between runs. It sits
        // inside the gate, so concurrent callers still cost one read rather
        // than one each.
        if stale.is_none()
            && let Some(restored) = self.load_from_disk(url).await
        {
            if restored.is_fresh() {
                self.counters.disk_hits.fetch_add(1, Ordering::Relaxed);
                self.bodies.insert(Arc::clone(&key), Arc::clone(&restored)).await;
                return Self::interpret(restored);
            }
            // Expired, but it still carries the validator the origin issued, so
            // the request below becomes a conditional one. That is most of the
            // saving: a `304` transfers headers instead of a document.
            stale = Some(restored);
        }

        let validators = stale.as_deref().filter(|e| e.status < 300 && e.has_validator());
        let entry = match self.fetch_with_retry(url, validators, policy).await? {
            Wire::Body(body) => Arc::new(body),
            Wire::NotModified(ttl) => {
                self.counters.not_modified.fetch_add(1, Ordering::Relaxed);
                let previous = stale.as_deref().ok_or_else(|| Error::Upstream {
                    url: url.to_owned(),
                    status: 304,
                    detail: Some("the origin sent 304 without a cached copy to refresh".to_owned()),
                })?;
                Arc::new(previous.revalidated(ttl))
            },
        };

        // The origin matters for what counts as absence, and after redirects it
        // is the origin that served the body, not the one first asked.
        let served_by = Url::parse(&entry.final_url).ok().and_then(|url| classify(&url).ok());
        let absent = served_by.is_some_and(|origin| is_absence(entry.status, origin));
        let cacheable = (policy.store && entry.status < 300) || absent;
        if cacheable {
            self.bodies.insert(Arc::clone(&key), Arc::clone(&entry)).await;
        }
        // Only successes are persisted. An absence is remembered in memory for
        // a minute so a typo does not spend the budget twice, and that is a
        // within-session courtesy rather than a fact worth carrying to the next
        // run — a crate that did not exist an hour ago may exist now. Keeping
        // this to successes also means nothing read back off disk can be an
        // error body, so the provenance argument in `extract_detail` never has
        // to reason about bytes that came from a file.
        if entry.status < 300 {
            self.store_to_disk(url, &entry).await;
        }
        Self::interpret(entry)
    }

    /// Read a persistable body back, or `None` if there is nothing usable.
    async fn load_from_disk(&self, url: &str) -> Option<Arc<CachedBody>> {
        let store = self.disk.clone()?;
        if !persistable(url) {
            return None;
        }
        let key = body_key(url);
        let stored = tokio::task::spawn_blocking(move || store.load::<StoredBody>(&key))
            .await
            .ok()?
            .ok()??;

        Some(Arc::new(CachedBody::new(
            bytes::Bytes::from(stored.body.clone()),
            stored.status,
            stored.etag.clone().map(Into::into),
            stored.last_modified.clone().map(Into::into),
            stored.final_url.clone().into_boxed_str(),
            stored.remaining_freshness(),
        )))
    }

    /// Keep a persistable body for the next run.
    ///
    /// Awaited rather than detached: a session that answered and exited without
    /// the write landing would leave the cache empty exactly when sessions are
    /// short, which is the case it exists for. The cost is an encode of a body
    /// that was just transferred over the network.
    async fn store_to_disk(&self, url: &str, entry: &CachedBody) {
        let Some(store) = self.disk.clone() else { return };
        if !persistable(url) {
            return;
        }
        let key = body_key(url);
        let record = StoredBody {
            body: entry.body.to_vec(),
            status: entry.status,
            etag: entry.etag.as_ref().map(ToString::to_string),
            last_modified: entry.last_modified.as_ref().map(ToString::to_string),
            final_url: entry.final_url.to_string(),
            stored_at_unix: StoredBody::now_unix(),
            fresh_for_secs: entry.fresh_for().as_secs(),
        };
        match tokio::task::spawn_blocking(move || store.store(&key, &record)).await {
            Ok(Ok(())) => {
                self.counters.disk_writes.fetch_add(1, Ordering::Relaxed);
            },
            Ok(Err(err)) => tracing::debug!(%err, url, "could not cache the response body"),
            Err(err) => tracing::debug!(%err, url, "the response cache writer did not finish"),
        }
    }

    /// Turn a cached entry into a result, mapping non-success statuses to errors.
    fn interpret(entry: Arc<CachedBody>) -> Result<Arc<CachedBody>> {
        if (200..300).contains(&entry.status) {
            return Ok(entry);
        }
        Err(Error::Upstream {
            url: entry.final_url.to_string(),
            status: entry.status,
            detail: extract_detail(&entry.body),
        })
    }

    /// Issue a request, retrying transient failures with decorrelated backoff.
    async fn fetch_with_retry(
        &self,
        url: &str,
        validators: Option<&CachedBody>,
        policy: Policy,
    ) -> Result<Wire> {
        let mut attempt = 0;
        loop {
            match self.fetch_once(url, validators, policy).await {
                Ok(outcome) => return Ok(outcome),
                Err(err) if err.is_transient() && attempt < self.max_retries => {
                    attempt += 1;
                    self.counters.retries.fetch_add(1, Ordering::Relaxed);
                    let delay = self.backoff(attempt);
                    tracing::debug!(
                        url,
                        attempt,
                        delay_ms = delay.as_millis(),
                        error = %err,
                        "retrying transient upstream failure"
                    );
                    tokio::time::sleep(delay).await;
                },
                Err(err) => {
                    if matches!(err, Error::RateLimitQueueFull { .. }) {
                        self.counters.shed.fetch_add(1, Ordering::Relaxed);
                    }
                    return Err(err);
                },
            }
        }
    }

    /// Exponential backoff with decorrelating jitter.
    ///
    /// The jitter is drawn from a counter mixed with a large odd constant
    /// instead of a random source: it only needs to keep concurrent retries
    /// from re-colliding, which does not require true randomness.
    fn backoff(&self, attempt: u32) -> Duration {
        let base_ms = 200_u64 << attempt.min(4);
        let mixed = self.jitter.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let spread = (mixed >> 33) % (base_ms / 2 + 1);
        Duration::from_millis(base_ms + spread)
    }

    /// One logical request, following redirects manually.
    async fn fetch_once(
        &self,
        url: &str,
        validators: Option<&CachedBody>,
        policy: Policy,
    ) -> Result<Wire> {
        let mut current = Url::parse(url).map_err(|err| Error::UnsupportedUrl {
            url: url.to_owned(),
            reason: err.to_string(),
        })?;
        let mut hops = 0_u8;

        loop {
            let origin = classify(&current)?;
            self.pacer(origin).acquire().await?;

            let mut request = self.client.get(current.clone());
            // A validator only means anything to the URL that issued it, which
            // is where the cached body came from — not necessarily where the
            // request started. A README is requested from the API and served
            // from the CDN, so sending its `ETag` to the API would revalidate
            // nothing and the CDN hop would re-transfer the whole body.
            if let Some(cached) = validators
                && validator_applies(&current, cached)
            {
                if let Some(etag) = &cached.etag {
                    request = request.header(IF_NONE_MATCH, if_none_match(etag));
                } else if let Some(modified) = &cached.last_modified {
                    request = request.header(IF_MODIFIED_SINCE, modified.as_ref());
                }
            }

            let response = request
                .send()
                .await
                .map_err(|source| Error::Network { url: current.to_string(), source })?;
            self.counters.network_requests.fetch_add(1, Ordering::Relaxed);

            let status = response.status();
            if status == StatusCode::NOT_MODIFIED {
                // A 304 carries its own caching directives, and they govern
                // the refreshed copy just as they would a transferred one.
                // Reusing the configured lifetime here instead would let a
                // revalidation grant an origin that asked for no caching at
                // all a full lifetime.
                return Ok(Wire::NotModified(effective_ttl(policy.ttl, response.headers())));
            }

            if status.is_redirection() {
                hops += 1;
                if hops > MAX_REDIRECTS {
                    return Err(Error::UnsupportedUrl {
                        url: url.to_owned(),
                        reason: format!("more than {MAX_REDIRECTS} redirects"),
                    });
                }
                current = follow_redirect(&current, response.headers(), status.as_u16())?;
                continue;
            }

            if status.as_u16() == 429 || status.is_server_error() {
                let backoff = retry_after(response.headers());
                if let Some(backoff) = backoff {
                    self.pacer(origin).penalize(backoff);
                    tracing::warn!(
                        host = origin.label(),
                        status = status.as_u16(),
                        backoff_ms = backoff.as_millis(),
                        "upstream asked the client to slow down"
                    );
                }
                let body = self.read_body(response, &current).await.unwrap_or_default();
                return Err(Error::Upstream {
                    url: current.to_string(),
                    status: status.as_u16(),
                    detail: extract_detail(&body),
                });
            }

            let headers = response.headers().clone();
            let final_url = current.to_string();
            let body = self.read_body(response, &current).await?;
            self.counters.bytes_received.fetch_add(body.len() as u64, Ordering::Relaxed);

            let ttl = if is_absence(status.as_u16(), origin) {
                policy.negative_ttl
            } else {
                effective_ttl(policy.ttl, &headers)
            };

            return Ok(Wire::Body(CachedBody::new(
                body.freeze(),
                status.as_u16(),
                header_string(&headers, ETAG),
                header_string(&headers, LAST_MODIFIED),
                final_url.into_boxed_str(),
                ttl,
            )));
        }
    }

    /// Read a response body, refusing to buffer more than the configured limit.
    async fn read_body(&self, response: reqwest::Response, url: &Url) -> Result<BytesMut> {
        let limit = self.max_body_bytes;
        if let Some(declared) = response.content_length()
            && declared > limit as u64
        {
            return Err(Error::BodyTooLarge { url: url.to_string(), limit });
        }

        let hint = response.content_length().unwrap_or(16 * 1024).min(limit as u64);
        let mut buffer = BytesMut::with_capacity(usize::try_from(hint).unwrap_or(16 * 1024));
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|source| Error::Network { url: url.to_string(), source })?
        {
            // Content-Length is absent or pre-decompression when the transfer is
            // compressed, so the ceiling is also enforced as bytes arrive.
            if buffer.len() + chunk.len() > limit {
                return Err(Error::BodyTooLarge { url: url.to_string(), limit });
            }
            buffer.extend_from_slice(&chunk);
        }
        Ok(buffer)
    }
}

/// Resolve the next hop of a redirect.
///
/// The `Location` is resolved against the URL that issued it, so a relative
/// target lands where the origin meant. The result is not trusted further than
/// that: [`classify`] checks it before it is requested, on this hop as on every
/// other.
fn follow_redirect(current: &Url, headers: &HeaderMap, status: u16) -> Result<Url> {
    let location =
        headers.get(LOCATION).and_then(|value| value.to_str().ok()).ok_or_else(|| {
            Error::UnsupportedUrl {
                url: current.to_string(),
                reason: format!("HTTP {status} without a usable Location header"),
            }
        })?;
    current.join(location).map_err(|err| Error::UnsupportedUrl {
        url: current.to_string(),
        reason: format!("unusable Location {location:?}: {err}"),
    })
}

/// Whether a cached entry's validator applies to the URL about to be requested.
///
/// A validator means something only to the URL that issued it, which is where
/// the cached body came from rather than where the request began. They differ
/// whenever a redirect is involved.
fn validator_applies(current: &Url, cached: &CachedBody) -> bool {
    current.as_str() == cached.final_url.as_ref()
}

/// Build an `If-None-Match` value from a cached entity tag.
///
/// A tag arrives weak (`W/"x"`) whenever the crates.io CDN compressed the
/// response it came from, and that origin will not match its own weak form:
/// handing `W/"x"` back returns the whole body again, while `"x"` returns
/// `304`, with or without compression negotiated. Offering both as a list does
/// not help either, so the weakness marker is dropped.
///
/// This is sound rather than a trick. `If-None-Match` is defined to compare
/// weakly, under which `W/"x"` and `"x"` are the same entity tag, so nothing is
/// claimed that the cached tag did not already assert. A changed entity gets a
/// changed tag regardless, so no stale copy can be validated by this.
fn if_none_match(etag: &str) -> &str {
    etag.strip_prefix("W/").unwrap_or(etag)
}

/// Whether a status means "this resource is not there", and so is worth
/// remembering briefly.
///
/// `404` is the obvious one. `403` counts only on the CDN, which is object
/// storage without list permission and answers a request for an object that was
/// never stored with `403` rather than `404` — the case for any release
/// published before rendered READMEs existed. A `403` from the API means
/// something else entirely, most likely that this client has been refused, and
/// treating that as absence would bury it.
const fn is_absence(status: u16, origin: Origin) -> bool {
    matches!(status, 404) || (status == 403 && matches!(origin, Origin::Cdn))
}

/// Whether a URL's body may be kept on disk between runs.
///
/// The sparse index and nothing else. It is the one body large enough to be
/// worth keeping and cheap enough to revalidate — a conditional request costs
/// headers where a transfer costs a document — and it is the request every
/// documentation lookup makes first, to turn a crate name into a version.
///
/// Everything else stays in memory on purpose. Search results and crate
/// metadata reflect registry state that moves; a rendered README is immutable
/// but is already reached through the API and would need its redirect cached to
/// save anything; rustdoc JSON is kept in its far more useful parsed form by
/// the layer above this one.
fn persistable(url: &str) -> bool {
    Url::parse(url)
        .is_ok_and(|url| url.scheme() == "https" && url.host_str() == Some("index.crates.io"))
}

/// Classify a URL's host, rejecting anything outside the allowed set.
///
/// Applied to every hop, this is what stops a redirect from steering the client
/// at an unrelated origin.
fn classify(url: &Url) -> Result<Origin> {
    if url.scheme() != "https" {
        return Err(Error::UnsupportedUrl {
            url: url.to_string(),
            reason: format!("scheme {:?} is not https", url.scheme()),
        });
    }
    // A non-default port on an allowed host is still not an endpoint any of
    // these services publishes, so it is refused rather than reasoned about.
    if let Some(port) = url.port() {
        return Err(Error::UnsupportedUrl {
            url: url.to_string(),
            reason: format!("port {port} is not the default https port"),
        });
    }
    let host = url.host_str().unwrap_or_default();
    Origin::classify(host).ok_or_else(|| Error::UnsupportedUrl {
        url: url.to_string(),
        reason: format!("host {host:?} is not a crates.io or docs.rs origin"),
    })
}

/// Copy a header into an owned string, if present and valid UTF-8.
fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<Box<str>> {
    headers.get(name)?.to_str().ok().map(Box::from)
}

/// Clamp the configured lifetime by whatever the origin permits.
///
/// The client never serves a response for longer than the origin allows, but it
/// also does not extend its own lifetime just because the origin offered more.
fn effective_ttl(configured: Duration, headers: &HeaderMap) -> Duration {
    let Some(control) = headers.get(CACHE_CONTROL).and_then(|value| value.to_str().ok()) else {
        return configured;
    };
    for directive in control.split(',') {
        let directive = directive.trim();
        if directive.eq_ignore_ascii_case("no-store") || directive.eq_ignore_ascii_case("no-cache")
        {
            return Duration::ZERO;
        }
        if let Some(seconds) =
            directive.strip_prefix("max-age=").or_else(|| directive.strip_prefix("max-age ="))
            && let Ok(seconds) = seconds.trim().parse::<u64>()
        {
            return configured.min(Duration::from_secs(seconds));
        }
    }
    configured
}

/// Parse a `Retry-After` delay expressed in seconds.
///
/// The HTTP-date form is accepted by the spec but not emitted by these origins;
/// an unparsable value falls back to a conservative pause rather than none.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?;
    match raw.trim().parse::<u64>() {
        Ok(seconds) => Some(Duration::from_secs(seconds.min(300))),
        Err(_) => Some(Duration::from_secs(5)),
    }
}

/// Pull a human-readable message out of an upstream error body.
///
/// crates.io answers with `{"errors":[{"detail":"..."}]}`; other origins answer
/// with text or HTML, which is truncated rather than surfaced whole.
fn extract_detail(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    // The crates.io error shape. This branch is the third site the provenance
    // argument in [`is_markup`] rests on, alongside that rule and the trim
    // below it: what makes the result safe to report unframed is that crates.io
    // composes these strings. Widening what is read out of an error body — a
    // field that echoes something a publisher chose, rather than a message
    // crates.io wrote — is the change that would break it, and the other two
    // sites are worth re-reading before making it.
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body)
        && let Some(errors) = value.get("errors").and_then(serde_json::Value::as_array)
    {
        let joined = errors
            .iter()
            .filter_map(|entry| entry.get("detail").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("; ");
        if !joined.is_empty() {
            return Some(truncate(&joined));
        }
    }
    let text = String::from_utf8_lossy(body);
    // Tidiness only: a leading byte-order mark would otherwise ride along into
    // a message a human reads. Nothing safety-relevant rests on this set being
    // exhaustive, and that is structural rather than incidental — none of the
    // characters below is alphanumeric or `<`, so trimming cannot delete either
    // of the two things `is_markup` looks for, and trimming only from the ends
    // cannot reorder what remains. Both offsets shift together and the
    // comparison is unchanged. Adding a letter or `<` to this set is the one
    // edit that would break that, and would need `is_markup` reconsidered.
    let trimmed = text.trim_matches(|character: char| {
        character.is_whitespace()
            || character.is_control()
            || matches!(character, '\u{feff}' | '\u{200b}' | '\u{00ad}')
    });
    if trimmed.is_empty() || is_markup(trimmed) {
        return None;
    }
    Some(truncate(trimmed))
}

/// Whether a body is a markup document rather than a message.
///
/// Dropping markup is what keeps an error detail free of anything a crate
/// author wrote, and consumers rely on that: the detail reaches a caller
/// unframed, on the grounds that only the registry itself composes these
/// strings. The two responses that could carry publisher-adjacent bytes are
/// both markup — the CDN answers a missing README object with an XML error
/// document, docs.rs with an HTML page — so this is the reason those grounds
/// hold. Salvaging text out of markup here would make the channel
/// publisher-reachable, and that decision would have to be revisited.
///
/// Asking whether a `<` comes before any letter or digit, rather than whether
/// the text starts with one, is what makes it hold. A prefix test is defeated
/// by any invisible character in front of the tag, and there is no way to
/// enumerate those without dragging in a Unicode character-category table:
/// `str::trim` sees only the White_Space property, `char::is_control` only the
/// Cc range, and a byte-order mark, a soft hyphen and the bidi controls are all
/// Cf. Ordering sidesteps the whole question.
///
/// Both misclassifications are known and neither is a hole. A body whose markup
/// is preceded by prose — `404 Not Found\n<html>`, which some proxies emit —
/// reads as a message and its markup reaches the caller. That is acceptable
/// because shape was never the property being protected: those bytes are still
/// composed by the registry, its CDN or an intermediary, and no publisher
/// artifact reaches this path whatever the body looks like. In the other
/// direction a `<` with no letter or digit anywhere reads as markup and the
/// detail is dropped, which is the right default for something unrecognised.
fn is_markup(text: &str) -> bool {
    let Some(angle) = text.find('<') else {
        return false;
    };
    text.find(char::is_alphanumeric).is_none_or(|word| angle < word)
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_DETAIL_LEN {
        return text.to_owned();
    }
    let mut end = MAX_DETAIL_LEN;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn only_the_sparse_index_is_kept_between_runs() {
        // The confinement rule, written where a reader looking for it will
        // find it. Widening this set is the change that would need the
        // reasoning in `persistable` revisited, not a passing thought.
        for kept in [
            "https://index.crates.io/se/rd/serde",
            "https://index.crates.io/1/a",
            "https://index.crates.io/to/ki/tokio-util",
        ] {
            assert!(persistable(kept), "{kept} is a sparse-index document");
        }
        for dropped in [
            "https://crates.io/api/v1/crates/serde",
            "https://crates.io/api/v1/crates?q=serde",
            "https://static.crates.io/readmes/serde/serde-1.0.219.html",
            "https://docs.rs/crate/serde/1.0.219/json",
            "https://docs.rs/crate/serde/1.0.219/status.json",
            "http://index.crates.io/se/rd/serde",
            "https://index.crates.io.evil.example/se/rd/serde",
            "not a url at all",
        ] {
            assert!(!persistable(dropped), "{dropped} must not be written to disk");
        }
    }

    #[test]
    fn a_record_carries_the_remainder_of_its_lifetime_across_a_process_boundary() {
        // What a second process has to be able to work out: not "was this
        // fresh when it was written" but "how much of what the origin granted
        // is left now".
        let record = |age: u64, granted: u64| StoredBody {
            body: b"{}".to_vec(),
            status: 200,
            etag: Some("\"abc\"".to_owned()),
            last_modified: None,
            final_url: "https://index.crates.io/se/rd/serde".to_owned(),
            stored_at_unix: StoredBody::now_unix() - age,
            fresh_for_secs: granted,
        };

        // Ranges, not equalities: the stamp has one-second granularity, so a
        // record written "now" can already read as a second old.
        let just_now = record(0, 600).remaining_freshness().as_secs();
        assert!((599..=600).contains(&just_now), "written just now, got {just_now}");
        let partly = record(100, 600).remaining_freshness().as_secs();
        assert!((499..=500).contains(&partly), "most of a ten-minute life left, got {partly}");
        assert_eq!(record(600, 600).remaining_freshness(), Duration::ZERO, "exactly used up");
        assert_eq!(record(6_000, 600).remaining_freshness(), Duration::ZERO, "long expired");
    }

    #[test]
    fn a_clock_that_moved_backwards_expires_an_entry_rather_than_extending_it() {
        // The only direction a clock jump may push this. Believing a record
        // written "in the future" would serve a body for longer than the origin
        // allowed, which is the one thing the freshness rule may never do.
        let from_the_future = StoredBody {
            body: b"{}".to_vec(),
            status: 200,
            etag: None,
            last_modified: None,
            final_url: "https://index.crates.io/se/rd/serde".to_owned(),
            stored_at_unix: StoredBody::now_unix() + 86_400,
            fresh_for_secs: 600,
        };
        assert_eq!(from_the_future.remaining_freshness(), Duration::ZERO);
    }

    #[test]
    fn a_restored_entry_keeps_the_url_its_validator_belongs_to() {
        // `validator_applies` compares the hop being requested against the URL
        // that served the body. A record that lost `final_url` would offer an
        // `ETag` to an origin that never issued it.
        let stored = StoredBody {
            body: b"{}".to_vec(),
            status: 200,
            etag: Some("W/\"abc\"".to_owned()),
            last_modified: None,
            final_url: "https://index.crates.io/se/rd/serde".to_owned(),
            stored_at_unix: StoredBody::now_unix(),
            fresh_for_secs: 600,
        };
        let restored = CachedBody::new(
            bytes::Bytes::from(stored.body.clone()),
            stored.status,
            stored.etag.clone().map(Into::into),
            stored.last_modified.clone().map(Into::into),
            stored.final_url.clone().into_boxed_str(),
            stored.remaining_freshness(),
        );

        let same = Url::parse("https://index.crates.io/se/rd/serde").expect("parses");
        let other = Url::parse("https://index.crates.io/se/rd/serde_json").expect("parses");
        assert!(validator_applies(&same, &restored));
        assert!(!validator_applies(&other, &restored));
        // And the weakness marker survives storage, so the request-time
        // stripping keeps behaving as it always did.
        assert_eq!(if_none_match(restored.etag.as_deref().expect("stored")), "\"abc\"");
    }

    #[test]
    fn two_urls_never_share_a_cache_key() {
        let keys: Vec<String> = [
            "https://index.crates.io/se/rd/serde",
            "https://index.crates.io/se/rd/serde_json",
            "https://index.crates.io/3/l/log",
            "https://index.crates.io/1/a",
        ]
        .iter()
        .map(|url| crate::disk::body_key(url))
        .collect();

        let mut unique = keys.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), keys.len(), "keys collided: {keys:?}");
        assert_eq!(
            crate::disk::body_key("https://index.crates.io/se/rd/serde"),
            keys[0],
            "the same URL must produce the same name in every process"
        );
        // Namespaced away from the documentation artifacts, which are keyed
        // `name@version`.
        assert!(keys.iter().all(|key| key.starts_with("body-")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(reqwest::header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), value.parse().expect("valid header value"));
        }
        map
    }

    fn fetcher_with_body_limit(limit: usize) -> Fetcher {
        let mut config = Config::new("test/1.0 (+https://example.invalid)");
        config.max_body_bytes = limit;
        Fetcher::new(&config).expect("the fetcher builds")
    }

    fn response_of(len: usize) -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder().status(200).body(vec![b'x'; len]).expect("a valid response"),
        )
    }

    #[tokio::test]
    async fn a_body_within_the_ceiling_is_read_whole() {
        let fetcher = fetcher_with_body_limit(1024);
        let url = Url::parse("https://crates.io/x").expect("valid url");

        let body = fetcher.read_body(response_of(1024), &url).await.expect("it fits exactly");
        assert_eq!(body.len(), 1024, "a body at the ceiling is not over it");
    }

    #[tokio::test]
    async fn a_body_over_the_ceiling_is_refused() {
        let fetcher = fetcher_with_body_limit(1024);
        let url = Url::parse("https://crates.io/x").expect("valid url");

        let refused = fetcher.read_body(response_of(1025), &url).await;
        assert!(matches!(refused, Err(Error::BodyTooLarge { limit: 1024, .. })), "{refused:?}");
    }

    #[tokio::test]
    async fn a_body_larger_than_it_claims_to_be_is_still_refused() {
        // The declared length is only a first check. A response that understates
        // its size, or omits the length because the transfer is compressed, has
        // to be stopped as the bytes arrive instead.
        let fetcher = fetcher_with_body_limit(1024);
        let url = Url::parse("https://crates.io/x").expect("valid url");

        let understated = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .header(reqwest::header::CONTENT_LENGTH, "8")
                .body(vec![b'x'; 4096])
                .expect("a valid response"),
        );

        let refused = fetcher.read_body(understated, &url).await;
        assert!(matches!(refused, Err(Error::BodyTooLarge { limit: 1024, .. })), "{refused:?}");
    }

    #[test]
    fn a_missing_cdn_object_is_absence_but_a_refusal_from_the_api_is_not() {
        // The CDN is object storage without list permission, so 403 is how it
        // reports an object that was never stored. A 403 from the API means
        // this client has been refused, which must not be filed as "not there".
        assert!(is_absence(403, Origin::Cdn));
        assert!(!is_absence(403, Origin::Api));
        assert!(!is_absence(403, Origin::Docs));

        for origin in [Origin::Api, Origin::Cdn, Origin::Docs] {
            assert!(is_absence(404, origin), "404 is absence everywhere");
            assert!(!is_absence(500, origin));
            assert!(!is_absence(200, origin));
        }
    }

    #[test]
    fn backoff_grows_with_each_attempt_and_stays_bounded() {
        // Building a fetcher makes no requests, so this needs no network.
        let fetcher = Fetcher::new(&Config::new("test/1.0 (+https://example.invalid)"))
            .expect("the fetcher builds");

        // Base delay is 200ms doubled per attempt and capped at the fifth, with
        // up to half the base added as jitter.
        for (attempt, base_ms) in [(1_u32, 400_u64), (2, 800), (3, 1600), (4, 3200)] {
            let delay = fetcher.backoff(attempt).as_millis() as u64;
            assert!(
                (base_ms..=base_ms + base_ms / 2).contains(&delay),
                "attempt {attempt} produced {delay}ms, outside {base_ms}..={}",
                base_ms + base_ms / 2
            );
        }

        // The shift is clamped, so a runaway attempt count cannot overflow it.
        let capped = fetcher.backoff(64).as_millis() as u64;
        assert!((3200..=4800).contains(&capped), "an extreme attempt gave {capped}ms");
    }

    #[test]
    fn concurrent_retries_do_not_all_wait_the_same_amount() {
        // Identical delays would re-collide the requests that just failed
        // together, which is the whole point of the jitter.
        let fetcher = Fetcher::new(&Config::new("test/1.0 (+https://example.invalid)"))
            .expect("the fetcher builds");

        let delays: std::collections::HashSet<u128> =
            (0..16).map(|_| fetcher.backoff(2).as_millis()).collect();
        assert!(delays.len() > 1, "every retry waited exactly {delays:?}");
    }

    #[test]
    fn an_empty_user_agent_is_refused_because_crates_io_requires_one() {
        assert!(matches!(Fetcher::new(&Config::new("  ")), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn only_crates_io_and_docs_rs_origins_are_reachable() {
        let cases = [
            ("https://crates.io/api/v1/crates", Some(Origin::Api)),
            ("https://index.crates.io/se/rd/serde", Some(Origin::Cdn)),
            ("https://static.crates.io/readmes/serde/serde-1.0.0.html", Some(Origin::Cdn)),
            ("https://docs.rs/crate/serde/1.0.0/json", Some(Origin::Docs)),
            ("https://evil.invalid/steal", None),
            // A lookalike host must not be accepted by a suffix match.
            ("https://crates.io.evil.invalid/", None),
            // The host of a URL carrying userinfo is what follows the `@`.
            ("https://crates.io@evil.invalid/steal", None),
            // A Unicode homograph is normalized to punycode, which no longer
            // matches.
            ("https://\u{0441}rates.io/steal", None),
            // Nothing here is published on a non-default port.
            ("https://crates.io:8443/steal", None),
            ("https://[::1]/steal", None),
        ];
        for (raw, expected) in cases {
            let url = Url::parse(raw).expect("valid url");
            assert_eq!(classify(&url).ok(), expected, "{raw}");
        }
    }

    #[test]
    fn plaintext_urls_are_refused() {
        let url = Url::parse("http://crates.io/api/v1/crates").expect("valid url");
        assert!(matches!(classify(&url), Err(Error::UnsupportedUrl { .. })));
    }

    #[test]
    fn upstream_max_age_shortens_but_never_extends_the_configured_ttl() {
        let configured = Duration::from_secs(600);

        let shorter = headers(&[(CACHE_CONTROL, "public, max-age=60")]);
        assert_eq!(effective_ttl(configured, &shorter), Duration::from_secs(60));

        let longer = headers(&[(CACHE_CONTROL, "public,max-age=604800")]);
        assert_eq!(effective_ttl(configured, &longer), configured);

        let forbidden = headers(&[(CACHE_CONTROL, "no-store")]);
        assert_eq!(effective_ttl(configured, &forbidden), Duration::ZERO);

        assert_eq!(effective_ttl(configured, &HeaderMap::new()), configured);
    }

    fn cached_from(final_url: &str, etag: &str) -> CachedBody {
        CachedBody::new(
            bytes::Bytes::from_static(b"body"),
            200,
            Some(Box::from(etag)),
            None,
            Box::from(final_url),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn a_validator_applies_only_to_the_url_that_issued_it() {
        // A README is asked of the API and served by the CDN, so its entity tag
        // belongs to the CDN URL. Sending it to the API would revalidate
        // nothing and the CDN hop would re-transfer the whole body.
        let cached =
            cached_from("https://static.crates.io/readmes/anyhow/anyhow-1.0.0.html", r#""abc""#);

        let asked = Url::parse("https://crates.io/api/v1/crates/anyhow/1.0.0/readme").expect("url");
        assert!(!validator_applies(&asked, &cached), "the first hop did not serve this body");

        let served =
            Url::parse("https://static.crates.io/readmes/anyhow/anyhow-1.0.0.html").expect("url");
        assert!(validator_applies(&served, &cached));
    }

    #[test]
    fn a_validator_applies_at_the_first_hop_when_nothing_redirected() {
        let url = "https://index.crates.io/se/rd/serde";
        let cached = cached_from(url, r#""abc""#);
        assert!(validator_applies(&Url::parse(url).expect("url"), &cached));
    }

    #[test]
    fn a_relative_location_resolves_against_the_url_that_issued_it() {
        let current =
            Url::parse("https://crates.io/api/v1/crates/serde/1.0.0/readme").expect("url");
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, "/readmes/serde.html".parse().expect("header"));

        let next = follow_redirect(&current, &headers, 302).expect("resolves");
        assert_eq!(next.as_str(), "https://crates.io/readmes/serde.html");
    }

    #[test]
    fn a_redirect_off_the_allowed_hosts_is_refused_on_the_hop_that_would_follow_it() {
        let current =
            Url::parse("https://crates.io/api/v1/crates/serde/1.0.0/readme").expect("url");
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, "https://evil.invalid/steal".parse().expect("header"));

        // Resolution itself is permissive; the host check is what refuses it,
        // and it runs on every hop rather than only the first.
        let next = follow_redirect(&current, &headers, 302).expect("resolves");
        assert!(matches!(classify(&next), Err(Error::UnsupportedUrl { .. })));
    }

    #[test]
    fn a_redirect_without_a_location_is_an_error_rather_than_a_silent_stop() {
        let current = Url::parse("https://crates.io/x").expect("url");
        let refused = follow_redirect(&current, &HeaderMap::new(), 302);
        assert!(matches!(refused, Err(Error::UnsupportedUrl { .. })), "{refused:?}");
    }

    #[test]
    fn a_weak_entity_tag_is_sent_without_its_weakness_marker() {
        // The CDN weakens a tag when it compresses the response, then declines
        // to match the weak form it just issued. Dropping the marker is what
        // makes revalidation of a compressed response actually return 304.
        assert_eq!(if_none_match(r#"W/"abc""#), r#""abc""#);
        assert_eq!(if_none_match(r#""abc""#), r#""abc""#);
    }

    #[test]
    fn retry_after_is_parsed_and_bounded() {
        assert_eq!(retry_after(&headers(&[(RETRY_AFTER, "12")])), Some(Duration::from_secs(12)));
        assert_eq!(
            retry_after(&headers(&[(RETRY_AFTER, "99999")])),
            Some(Duration::from_secs(300)),
            "an absurd delay is capped rather than obeyed"
        );
        assert_eq!(
            retry_after(&headers(&[(RETRY_AFTER, "Wed, 21 Oct 2026 07:28:00 GMT")])),
            Some(Duration::from_secs(5)),
            "an HTTP-date falls back to a conservative pause"
        );
        assert_eq!(retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn error_detail_comes_from_the_crates_io_error_shape() {
        let body = br#"{"errors":[{"detail":"Not Found"},{"detail":"and more"}]}"#;
        assert_eq!(extract_detail(body), Some("Not Found; and more".to_owned()));

        assert_eq!(extract_detail(b""), None);
        assert_eq!(extract_detail(b"plain failure"), Some("plain failure".to_owned()));
    }

    #[test]
    fn a_markup_error_page_yields_no_detail_at_all() {
        // Load-bearing rather than tidiness. The detail reaches a caller
        // unframed because only the registry composes these strings, and the
        // responses that could carry anything a crate author wrote are exactly
        // these two: the CDN answers a missing README object with XML, docs.rs
        // with HTML. Making either of these return text would put
        // publisher-influenced bytes into an unframed channel.
        let cdn_error =
            br#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>AccessDenied</Code></Error>"#;
        assert_eq!(extract_detail(cdn_error), None);

        let docs_error = b"<!DOCTYPE html>\n<html><body>not found</body></html>";
        assert_eq!(extract_detail(docs_error), None);

        assert_eq!(extract_detail(b"   \n  <html>"), None, "leading whitespace does not evade it");

        // Every one of these is an invisible character that `str::trim` leaves
        // in place, so a prefix test would see the character rather than the
        // `<` and pass the markup straight through. They span three different
        // reasons for being invisible, which is why the guard asks about
        // ordering instead of enumerating them.
        for prefix in [
            "\u{feff}", // byte-order mark
            "\u{200b}", // zero-width space
            "\u{00ad}", // soft hyphen
            "\u{202e}", // right-to-left override
            "\u{2066}", // left-to-right isolate
            "\u{2060}\u{feff} ",
        ] {
            let body = format!("{prefix}<html>fail</html>");
            assert_eq!(extract_detail(body.as_bytes()), None, "{prefix:?} must not evade it");
        }
    }

    #[test]
    fn markup_introduced_by_prose_is_let_through_on_purpose() {
        // Some proxies put a status line ahead of the document. This reads as a
        // message and its markup reaches the caller, which is deliberate: the
        // reason a detail needs no framing is that the registry, its CDN or an
        // intermediary composed the bytes, not what shape they arrived in.
        // Anyone tempted to close this should know they are trading a
        // cosmetic improvement for the enumeration problem this rule escaped.
        let proxied = b"404 Not Found\n<html><body>gone</body></html>";
        assert!(extract_detail(proxied).is_some());
    }

    #[test]
    fn a_message_that_merely_contains_an_angle_bracket_is_still_a_message() {
        // The ordering rule has to keep prose that happens to use `<`, or an
        // upstream complaint about a version bound would vanish.
        assert_eq!(
            extract_detail(b"version must be < 2.0"),
            Some("version must be < 2.0".to_owned())
        );
        assert_eq!(
            extract_detail("\u{feff}rate limited".as_bytes()),
            Some("rate limited".to_owned()),
            "an invisible prefix on a real message must not discard it"
        );
    }

    #[test]
    fn long_details_are_truncated_on_a_character_boundary() {
        let body = "\u{3042}".repeat(200);
        let detail = extract_detail(body.as_bytes()).expect("some detail");
        assert!(detail.ends_with("..."));
        assert!(detail.len() <= MAX_DETAIL_LEN + 3);
    }
}
