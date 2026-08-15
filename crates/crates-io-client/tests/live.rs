//! Tests that talk to the real crates.io and docs.rs.
//!
//! Skipped unless `CRATES_IO_LIVE_TESTS=1` is set, because they spend real
//! request budget against a shared, rate-limited service. Everything checkable
//! without the network is already covered by the unit tests; what is left is
//! the part only a live service can confirm — that these endpoints still return
//! what this crate expects them to.
//!
//! This is deliberately one test rather than a dozen. Separate test functions
//! run concurrently and would each need their own client, so each would get its
//! own pacing budget and the suite as a whole would exceed the one-request-per-
//! second policy it exists to verify. One sequential run against one client
//! paces correctly and makes the traffic counters deterministic enough to
//! assert on.
//!
//! Run with:
//!
//! ```sh
//! CRATES_IO_LIVE_TESTS=1 cargo test -p crates-io-client --test live -- --nocapture
//! ```

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crates_io_client::{Client, Config, Include, SearchParams, Selector, Ttl};

/// Identifies these tests to crates.io, as its crawler policy requires.
const USER_AGENT: &str =
    "crates-io-client-tests/0.1 (+https://github.com/zchee/mcp-crates; testing)";

#[tokio::test]
async fn the_live_registry_behaves_as_this_crate_expects() {
    if !std::env::var("CRATES_IO_LIVE_TESTS").is_ok_and(|value| value == "1") {
        eprintln!("skipping: set CRATES_IO_LIVE_TESTS=1 to run the live suite");
        return;
    }

    let client = Arc::new(Client::new(Config::new(USER_AGENT)).expect("the client builds"));

    sparse_index_describes_versions(&client).await;
    dependencies_and_features_are_reported(&client).await;
    an_unknown_crate_is_reported_as_missing(&client).await;
    crate_metadata_omits_the_per_version_payload(&client).await;
    search_finds_the_crate_it_was_given(&client).await;
    readmes_arrive_as_markdown(&client).await;
    item_documentation_is_read_from_rustdoc_json(&client).await;
    repeated_questions_cost_nothing(&client).await;
    concurrent_questions_share_one_request(&client).await;
    api_requests_are_paced(&client).await;
    a_stale_copy_is_revalidated_rather_than_refetched().await;

    let stats = client.stats();
    eprintln!(
        "live suite traffic: {} requests, {} cache hits, {} coalesced, {} bytes received",
        stats.network_requests, stats.cache_hits, stats.coalesced, stats.bytes_received
    );
}

async fn sparse_index_describes_versions(client: &Client) {
    let index = client.index("serde").await.expect("serde is indexed");

    assert_eq!(index.name(), "serde");
    assert!(index.len() > 100, "serde has a long release history, got {}", index.len());

    let latest = index.resolve(&Selector::Default, false).expect("a stable release exists");
    assert!(!latest.yanked, "the default selector must not resolve to a yanked release");
    assert!(
        latest.version().expect("valid semver").pre.is_empty(),
        "the default selector must not resolve to a pre-release"
    );
    assert!(!latest.cksum.is_empty(), "the index carries a checksum per release");

    assert!(
        index.resolve(&"^1.0".parse().expect("valid requirement"), false).is_ok(),
        "serde has a 1.x release"
    );
}

async fn dependencies_and_features_are_reported(client: &Client) {
    let index = client.index("reqwest").await.expect("reqwest is indexed");
    let release = index.resolve(&Selector::Default, false).expect("a stable release exists");

    assert!(!release.deps.is_empty(), "reqwest depends on other crates");
    assert!(!release.all_features().is_empty(), "reqwest defines features");
    assert!(
        release.deps.iter().any(|dep| dep.optional),
        "reqwest's TLS backends are optional dependencies"
    );
}

async fn an_unknown_crate_is_reported_as_missing(client: &Client) {
    let error = client
        .index("this-crate-does-not-exist-9f3a2b1c")
        .await
        .expect_err("an unpublished name has no index document");

    assert_eq!(error.kind(), "crate_not_found", "got {error}");
    assert!(!error.retryable(), "a missing crate will still be missing later");
}

async fn crate_metadata_omits_the_per_version_payload(client: &Client) {
    let info = client.crate_info("serde", Include::default()).await.expect("serde exists");

    assert_eq!(info.krate.name, "serde");
    assert!(info.krate.downloads > 0);
    assert!(info.keywords.is_some(), "keywords were requested");
    assert!(info.categories.is_some(), "categories were requested");
    assert!(
        info.krate.default_version.is_some(),
        "the default version is the one version field this endpoint reports reliably without the \
         per-version payload"
    );
}

async fn search_finds_the_crate_it_was_given(client: &Client) {
    let params = SearchParams {
        query: Some("serde".to_owned()),
        per_page: Some(5),
        ..SearchParams::default()
    };
    let response = client.search(&params).await.expect("the search runs");

    assert!(response.meta.total > 0);
    assert!(
        response.crates.iter().any(|hit| hit.name == "serde"),
        "searching for serde should find serde"
    );
}

async fn readmes_arrive_as_markdown(client: &Client) {
    let index = client.index("anyhow").await.expect("anyhow is indexed");
    let version = index.resolve(&Selector::Default, false).expect("a release exists").vers.clone();

    let readme = client.readme("anyhow", &version, 40_000).await.expect("anyhow has a README");

    assert!(readme.contains("anyhow"), "the README should mention the crate");
    assert!(!readme.contains("<p>"), "HTML should have been converted to Markdown");
    assert!(!readme.contains("<img"), "images should have been dropped");
}

async fn item_documentation_is_read_from_rustdoc_json(client: &Client) {
    let index = client.index("serde").await.expect("serde is indexed");
    let version = index.resolve(&Selector::Default, false).expect("a release exists").vers.clone();

    let docs = client.doc_index("serde", &version).await.expect("docs.rs built serde");

    // The path table alone lists roughly eighty items for serde; well past that
    // means the associated items were folded in as intended.
    assert!(docs.len() > 150, "expected methods to be indexed too, got {}", docs.len());

    let method = docs
        .lookup("Deserializer::deserialize_any")
        .found
        .expect("a trait method resolves from its type and name alone");
    assert_eq!(method.path.as_ref(), "serde::de::Deserializer::deserialize_any");
    assert!(method.docs.is_some(), "the method is documented upstream");
}

async fn repeated_questions_cost_nothing(client: &Client) {
    client.index("tokio").await.expect("tokio is indexed");
    let requests_before = client.stats().network_requests;
    let hits_before = client.stats().cache_hits;

    // Sequential, so this exercises the freshness cache rather than the
    // coalescing gate.
    for _ in 0..5 {
        client.index("tokio").await.expect("tokio is indexed");
    }

    assert_eq!(
        client.stats().network_requests,
        requests_before,
        "a fresh cache entry must not produce any traffic"
    );
    assert_eq!(client.stats().cache_hits - hits_before, 5);
}

async fn concurrent_questions_share_one_request(client: &Arc<Client>) {
    let requests_before = client.stats().network_requests;

    let mut handles = Vec::new();
    for _ in 0..8 {
        // The gate is state inside the client, so the tasks have to share one.
        let client = Arc::clone(client);
        handles.push(tokio::spawn(async move { client.index("bytes").await.map(|_| ()) }));
    }
    for handle in handles {
        handle.await.expect("the task joins").expect("bytes is indexed");
    }

    assert_eq!(
        client.stats().network_requests - requests_before,
        1,
        "eight concurrent callers should share one request"
    );
    assert!(client.stats().coalesced >= 7, "seven of the eight should have been coalesced");
}

/// The sparse index carries an `ETag`, so a stale copy should cost a
/// conditional request that transfers no body.
///
/// This needs its own client: the shared one holds the index for ten minutes,
/// which no test wants to wait out.
async fn a_stale_copy_is_revalidated_rather_than_refetched() {
    let ttl = Ttl { index: Duration::from_millis(1), ..Ttl::default() };
    let client = Client::with_ttl(Config::new(USER_AGENT), ttl).expect("the client builds");

    client.index("libc").await.expect("libc is indexed");
    let after_first = client.stats();
    assert_eq!(after_first.not_modified, 0, "the first fetch cannot be a revalidation");

    // Outlive the millisecond lifetime, so the next read finds a stale entry
    // holding a validator.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let revalidated = client.index("libc").await.expect("libc is still indexed");

    let after_second = client.stats();
    assert_eq!(after_second.not_modified, 1, "the stale copy should have been revalidated");
    assert_eq!(
        after_second.bytes_received, after_first.bytes_received,
        "a 304 transfers headers, not a body"
    );
    assert!(!revalidated.is_empty(), "the revalidated copy is still usable");
}

async fn api_requests_are_paced(client: &Client) {
    // Three crates whose metadata has not been requested yet, so nothing can be
    // served from cache. At one request per second the third cannot be issued
    // before two seconds in.
    let started = Instant::now();
    for name in ["once_cell", "rand", "regex"] {
        client.crate_info(name, Include::default()).await.expect("the crate exists");
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_secs(2),
        "three uncached API requests took {elapsed:?}, faster than one per second"
    );
}
