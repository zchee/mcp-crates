//! The disk cache, exercised through the client that uses it.
//!
//! Nothing here contacts the network, and that is the point: every test seeds a
//! cache directory itself and then asserts that a client asked for the same
//! release answers from it without spending a request. A test that needed the
//! network could not tell "the cache worked" from "the fetch was fast".

use std::{fs, path::PathBuf};

use crates_io_client::{
    Client, Config, DocIndex,
    disk::{Store, StoredBody, body_key},
    docs,
};

/// Larger than the fixture expands to.
const LIMIT: usize = 64 * 1024 * 1024;

/// The release the fixtures describe, and the key the cache stores it under.
const NAME: &str = "regex";
const VERSION: &str = "1.11.1";

/// A directory that removes itself, so a failing test leaves nothing behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mcp-crates-cache-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the temporary directory is creatable");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The committed `regex` document, parsed.
fn fixture() -> DocIndex {
    const COMPRESSED: &[u8] = include_bytes!("../fixtures/regex-1.11.1.rustdoc.json.zst");
    let expanded = docs::decompress_rustdoc(NAME, COMPRESSED, LIMIT).expect("decompresses");
    DocIndex::parse(NAME, &expanded).expect("parses")
}

fn config(cache_dir: Option<&std::path::Path>, disk_cache: bool) -> Config {
    let mut config = Config::new("mcp-crates-test/0.1 (https://example.invalid)");
    config.disk_cache = disk_cache;
    config.cache_dir = cache_dir.map(PathBuf::from);
    config
}

#[tokio::test]
async fn a_seeded_cache_answers_without_spending_a_request() {
    let dir = TempDir::new("seeded");
    let parsed = fixture();

    // Seeded directly rather than by a previous fetch, so the assertion below
    // is about the read path alone.
    Store::at(&dir.0)
        .store(&format!("{NAME}@{VERSION}"), &parsed.to_stored())
        .expect("the cache is writable");

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    let loaded = client.doc_index(NAME, VERSION).await.expect("answers from the cache");

    assert_eq!(*loaded, parsed, "the cached index is the one that was stored");
    let stats = client.stats();
    assert_eq!(stats.network_requests, 0, "nothing should have gone on the wire");
    assert_eq!(stats.disk_hits, 1);
    assert_eq!(stats.disk_writes, 0, "a hit writes nothing back");
}

#[tokio::test]
async fn a_second_client_sharing_the_directory_reads_what_the_first_left() {
    let dir = TempDir::new("shared");
    let parsed = fixture();
    let key = format!("{NAME}@{VERSION}");
    Store::at(&dir.0).store(&key, &parsed.to_stored()).expect("writable");

    // Two clients, as two sessions of the server would be: separate in-memory
    // caches, one directory between them.
    for attempt in 0..2 {
        let client = Client::new(config(Some(&dir.0), true)).expect("builds");
        let loaded = client.doc_index(NAME, VERSION).await.expect("answers");
        assert_eq!(*loaded, parsed, "attempt {attempt}");
        assert_eq!(client.stats().network_requests, 0, "attempt {attempt}");
    }
}

#[tokio::test]
async fn the_in_memory_cache_answers_the_second_time_without_touching_the_disk() {
    let dir = TempDir::new("memoized");
    Store::at(&dir.0)
        .store(&format!("{NAME}@{VERSION}"), &fixture().to_stored())
        .expect("writable");

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    client.doc_index(NAME, VERSION).await.expect("answers");
    client.doc_index(NAME, VERSION).await.expect("answers");

    assert_eq!(client.stats().disk_hits, 1, "the second call is served from memory");
}

/// Whether the tests that fall through to the network may run.
///
/// A cache *miss* ends in a fetch by definition, so the three tests below
/// cannot observe a miss without one. They are gated like the live suite rather
/// than mocked, and what they would have asserted about the miss itself is
/// covered without a network by the unit tests in `disk.rs`.
fn live_tests_enabled() -> bool {
    std::env::var_os("CRATES_IO_LIVE_TESTS").is_some()
}

#[tokio::test]
async fn a_disabled_cache_does_not_read_what_is_sitting_there() {
    if !live_tests_enabled() {
        return;
    }
    let dir = TempDir::new("disabled");
    Store::at(&dir.0)
        .store(&format!("{NAME}@{VERSION}"), &fixture().to_stored())
        .expect("writable");

    let client = Client::new(config(Some(&dir.0), false)).expect("builds");
    client.doc_index(NAME, VERSION).await.expect("falls through to the network");

    let stats = client.stats();
    assert_eq!(stats.disk_hits, 0, "the file should not have been read");
    assert_eq!(stats.disk_writes, 0, "nor written");
    assert!(stats.network_requests > 0, "the answer must have come from the wire");
}

#[tokio::test]
async fn a_damaged_entry_is_discarded_and_replaced_by_a_good_one() {
    if !live_tests_enabled() {
        return;
    }
    let dir = TempDir::new("damaged");
    let key = format!("{NAME}@{VERSION}");
    Store::at(&dir.0).store(&key, &fixture().to_stored()).expect("writable");

    // A bit flipped in the middle of the compressed body: the shape a torn
    // write or a bad sector leaves behind.
    let path = dir.0.join(format!("{key}.mcpc"));
    let mut bytes = fs::read(&path).expect("readable");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0b0001_0000;
    fs::write(&path, &bytes).expect("writable");

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    client.doc_index(NAME, VERSION).await.expect("refetches");

    let stats = client.stats();
    assert_eq!(stats.disk_hits, 0, "a damaged entry is not a hit");
    assert!(stats.network_requests > 0, "it must have been refetched");
    // Rewritten by the refetch, and readable again — the damaged bytes are
    // gone rather than merely ignored.
    assert_eq!(stats.disk_writes, 1);
    assert!(Store::at(&dir.0).load::<docs::StoredIndex>(&key).expect("no error").is_some());
}

#[tokio::test]
async fn an_entry_for_another_release_is_not_offered_for_this_one() {
    if !live_tests_enabled() {
        return;
    }
    let dir = TempDir::new("wrong-key");
    Store::at(&dir.0).store(&format!("{NAME}@9.9.9"), &fixture().to_stored()).expect("writable");

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    client.doc_index(NAME, VERSION).await.expect("fetches");

    assert_eq!(client.stats().disk_hits, 0, "the key includes the version for a reason");
}

#[tokio::test]
async fn a_request_that_could_never_be_made_is_not_answered_from_a_cache() {
    // Validation runs before either cache, so a version that could not be put
    // in a URL is refused rather than served from a file whose name happens to
    // be spellable.
    let dir = TempDir::new("invalid");
    let store = Store::at(&dir.0);
    store.store(&format!("{NAME}@.."), &fixture().to_stored()).expect("writable");

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    let outcome = client.doc_index(NAME, "..").await;

    assert!(outcome.is_err(), "an unusable version is an error, not a cache hit");
    assert_eq!(client.stats().disk_hits, 0);
    assert_eq!(client.stats().network_requests, 0, "and it costs no request either");
}

#[test]
fn a_stored_index_is_smaller_than_the_document_it_was_built_from() {
    // Not a performance nicety: the cache is capped by total size, so an
    // artifact larger than the compressed JSON it saves refetching would make
    // the cap hold fewer crates than simply keeping the downloads would.
    const COMPRESSED: &[u8] = include_bytes!("../fixtures/regex-1.11.1.rustdoc.json.zst");
    let dir = TempDir::new("size");
    let store = Store::at(&dir.0);
    let key = format!("{NAME}@{VERSION}");
    store.store(&key, &fixture().to_stored()).expect("writable");

    let stored = fs::metadata(dir.0.join(format!("{key}.mcpc"))).expect("exists").len();
    let expanded =
        docs::decompress_rustdoc(NAME, COMPRESSED, LIMIT).expect("decompresses").len() as u64;

    eprintln!(
        "rustdoc JSON: {} B compressed, {expanded} B expanded; stored index: {stored} B",
        COMPRESSED.len()
    );
    assert!(
        stored < COMPRESSED.len() as u64,
        "the cache entry ({stored} B) is larger than the download it replaces ({} B)",
        COMPRESSED.len()
    );
}

/// The captured `serde` sparse-index document and the URL it came from.
const INDEX_URL: &str = "https://index.crates.io/se/rd/serde";
const INDEX_BODY: &[u8] = include_bytes!("../fixtures/serde.index.json");

/// Seed a sparse-index body as a previous process would have left it.
fn seed_index(store: &Store, age_secs: u64, fresh_for_secs: u64, etag: Option<&str>) {
    store
        .store(
            &body_key(INDEX_URL),
            &StoredBody {
                body: INDEX_BODY.to_vec(),
                status: 200,
                etag: etag.map(ToOwned::to_owned),
                last_modified: None,
                final_url: INDEX_URL.to_owned(),
                stored_at_unix: StoredBody::now_unix() - age_secs,
                fresh_for_secs,
            },
        )
        .expect("the cache is writable");
}

#[tokio::test]
async fn a_fresh_index_body_resolves_a_version_without_a_request() {
    // The whole point of artifact 4b: a new process turns a crate name into a
    // version without asking anyone, because the last one wrote down what the
    // CDN said and how long it said it for.
    let dir = TempDir::new("index-fresh");
    seed_index(&Store::at(&dir.0), 0, 600, Some("\"abc\""));

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    let index = client.index("serde").await.expect("answers from the cache");

    assert_eq!(index.name(), "serde");
    assert_eq!(index.len(), 316, "the whole document was restored, not a prefix");
    let stats = client.stats();
    assert_eq!(stats.network_requests, 0, "nothing should have gone on the wire");
    assert_eq!(stats.disk_hits, 1);
}

#[tokio::test]
async fn two_clients_sharing_a_directory_both_answer_without_a_request() {
    let dir = TempDir::new("index-shared");
    seed_index(&Store::at(&dir.0), 0, 600, Some("\"abc\""));

    for attempt in 0..2 {
        let client = Client::new(config(Some(&dir.0), true)).expect("builds");
        let index = client.index("serde").await.expect("answers");
        assert_eq!(index.len(), 316, "attempt {attempt}");
        assert_eq!(client.stats().network_requests, 0, "attempt {attempt}");
    }
}

#[tokio::test]
async fn an_index_body_is_not_read_when_the_cache_is_disabled() {
    let dir = TempDir::new("index-disabled");
    seed_index(&Store::at(&dir.0), 0, 600, Some("\"abc\""));

    // No network here, so the call cannot succeed; the counter is the point.
    let client = Client::new(config(Some(&dir.0), false)).expect("builds");
    let _ = client.index("serde").await;
    assert_eq!(client.stats().disk_hits, 0, "the file should not have been read");
}

#[tokio::test]
async fn a_damaged_index_body_is_discarded_rather_than_served() {
    let dir = TempDir::new("index-damaged");
    let store = Store::at(&dir.0);
    seed_index(&store, 0, 600, Some("\"abc\""));

    let path = dir.0.join(format!("{}.mcpc", body_key(INDEX_URL)));
    let mut damaged = fs::read(&path).expect("readable");
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0b0001_0000;
    fs::write(&path, &damaged).expect("writable");

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    let _ = client.index("serde").await;

    assert_eq!(client.stats().disk_hits, 0, "a damaged body is not a hit");
    // Deleted on rejection. If this machine has network access the refetch will
    // have written a good file back in its place, which is the right outcome
    // too — what must not survive is the damaged copy.
    if path.exists() {
        assert_ne!(fs::read(&path).expect("readable"), damaged, "the damaged copy survived");
        assert!(
            Store::at(&dir.0).load::<StoredBody>(&body_key(INDEX_URL)).expect("no error").is_some()
        );
    }
}

#[tokio::test]
async fn an_expired_index_body_is_not_served_as_if_it_were_fresh() {
    // Expired is not the same as useless — it still carries the validator, so
    // the request it causes is conditional. What must not happen is it being
    // served without asking at all.
    let dir = TempDir::new("index-expired");
    seed_index(&Store::at(&dir.0), 6_000, 600, Some("\"abc\""));

    let client = Client::new(config(Some(&dir.0), true)).expect("builds");
    let _ = client.index("serde").await;

    assert_eq!(client.stats().disk_hits, 0, "an expired body is not a hit");
}

#[test]
fn a_stored_body_is_rejected_the_same_way_a_stored_index_is() {
    // The corrupt matrix, on the second artifact kind. The guards live in one
    // place, but "the same guards apply" is worth asserting rather than
    // assuming.
    let dir = TempDir::new("body-corrupt");
    let store = Store::at(&dir.0);
    let key = body_key(INDEX_URL);
    seed_index(&store, 0, 600, Some("\"abc\""));

    let path = dir.0.join(format!("{key}.mcpc"));
    let good = fs::read(&path).expect("readable");

    let cases: [(&str, Vec<u8>); 5] = [
        ("shorter than a header", good[..8].to_vec()),
        ("not one of ours", b"some other file entirely".to_vec()),
        ("a different schema", {
            let mut bytes = good.clone();
            bytes[8] = bytes[8].wrapping_add(1);
            bytes
        }),
        ("a different bitcode format", {
            let mut bytes = good.clone();
            bytes[12] = bytes[12].wrapping_add(1);
            bytes
        }),
        ("a truncated body", good[..good.len() - 4].to_vec()),
    ];

    for (label, bytes) in cases {
        fs::write(&path, &bytes).expect("writable");
        assert!(store.load::<StoredBody>(&key).expect("no error").is_none(), "{label}");
        assert!(!path.exists(), "{label}: the unusable file should have been removed");
    }
}
