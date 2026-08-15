//! Benchmarks for the CPU-bound paths: decompressing, parsing and looking up.
//!
//! Everything here runs against documents captured from docs.rs,
//! `index.crates.io` and `static.crates.io`; `../fixtures/README.md` records
//! what each one is and where it came from. Nothing contacts the network, and no
//! fixture is a hand-written approximation of a real payload.
//!
//! Two of the groups run against a document generated in code rather than a
//! captured one. The lookup path is bounded at 50 000 items, and no real crate
//! whose rustdoc JSON is small enough to commit comes anywhere near that
//! ceiling — but the ceiling is exactly where the linear scans cost the most, so
//! it is the case worth measuring.
//!
//! # Running
//!
//! ```sh
//! cargo bench -p crates-io-client --bench hot_paths
//! ```
//!
//! Never concurrently with another build, and never alongside anything else:
//! a benchmark sharing the machine with a linker is measuring the linker. The
//! medians this prints are the numbers the optimization record is kept in, so a
//! run taken on a busy machine is worse than no run at all.

use std::sync::LazyLock;

use crates_io_client::{CrateIndex, DocIndex, Lookup, disk::Store, docs, readme, synthetic};

/// The allocator the server binary installs.
///
/// The library itself is allocator-agnostic, so nothing would otherwise pull
/// this in — and a benchmark measuring the system allocator would be measuring
/// something no user runs. Most of what is timed below is allocation.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Expansion ceiling handed to `decompress_rustdoc`, far above any fixture.
const DECOMPRESS_LIMIT: usize = 64 * 1024 * 1024;

fn main() {
    verify_fixtures();
    divan::main();
}

/// The item the captured-index lookups resolve to. Its bare name and its
/// two-segment suffix are both unique in `regex`'s documentation.
const REGEX_TARGET: &str = "regex::pattern::RegexSearcher";

/// The generated item the lookup benchmarks resolve to. Both its bare name and
/// its two-segment suffix are unique in the document, so each lookup terminates
/// at the step the benchmark is named for.
const SYNTHETIC_TARGET: u32 = 25_000;

/// A query no fixture contains, which is what makes it the expensive one: it
/// falls through the exact, suffix, name and re-export passes, and then scans
/// and lowercases every path in the index.
const MISS: &str = "zzqxnotpresent";

/// Check that every benchmark measures what its name says, before measuring it.
///
/// A `bare_name` benchmark that quietly resolves one step earlier — because a
/// fixture was replaced, or a generated name collided — would report a number
/// for a different code path, and nothing in the output would say so. Each
/// assertion below pins one query to the answer that makes its benchmark name
/// true; none of this runs inside a timed region.
fn verify_fixtures() {
    assert_eq!(REGEX.parse().format_version(), Some(55), "the older fixture is format_version 55");
    assert_eq!(SEMVER.parse().format_version(), Some(60), "the newer fixture is format_version 60");

    let found = |lookup: Lookup<'_>| lookup.found.map(|item| item.path().to_owned());
    assert_eq!(
        found(REGEX_INDEX.lookup("regex::builders::bytes::RegexBuilder::build")).as_deref(),
        Some("regex::builders::bytes::RegexBuilder::build")
    );
    assert_eq!(found(REGEX_INDEX.lookup("pattern::RegexSearcher")).as_deref(), Some(REGEX_TARGET));
    assert_eq!(found(REGEX_INDEX.lookup("regexsearcher")).as_deref(), Some(REGEX_TARGET));

    assert_eq!(
        SYNTHETIC_INDEX.len(),
        50_000,
        "the generated document exists to reach the item ceiling"
    );
    assert!(!SYNTHETIC_INDEX.is_truncated(), "reaching the ceiling is not the same as passing it");
    for query in [&*SYNTHETIC_EXACT, &*SYNTHETIC_SUFFIX, &*SYNTHETIC_NAME] {
        assert_eq!(found(SYNTHETIC_INDEX.lookup(query)).as_deref(), Some(SYNTHETIC_EXACT.as_str()));
    }

    for index in [&*REGEX_INDEX, &*SYNTHETIC_INDEX] {
        let miss = index.lookup(MISS);
        assert!(miss.found.is_none() && miss.suggestions.is_empty() && miss.reexported.is_empty());
    }
}

/// A captured rustdoc document, in both the form docs.rs transfers and the form
/// the parser sees.
struct Rustdoc {
    name: &'static str,
    compressed: &'static [u8],
    expanded: Vec<u8>,
}

impl Rustdoc {
    fn load(name: &'static str, compressed: &'static [u8]) -> Self {
        let expanded = docs::decompress_rustdoc(name, compressed, DECOMPRESS_LIMIT)
            .expect("the committed fixture decompresses");
        Self { name, compressed, expanded }
    }

    fn parse(&self) -> DocIndex {
        DocIndex::parse(self.name, &self.expanded).expect("the committed fixture parses")
    }
}

/// `format_version` 55, the older end of what docs.rs currently serves.
static REGEX: LazyLock<Rustdoc> = LazyLock::new(|| {
    Rustdoc::load("regex", include_bytes!("../fixtures/regex-1.11.1.rustdoc.json.zst"))
});

/// `format_version` 60, the newer end.
static SEMVER: LazyLock<Rustdoc> = LazyLock::new(|| {
    Rustdoc::load("semver", include_bytes!("../fixtures/semver-1.0.28.rustdoc.json.zst"))
});

/// The `serde` sparse-index document: 316 versions, one JSON object per line.
static SERDE_INDEX: &[u8] = include_bytes!("../fixtures/serde.index.json");

/// A rendered README, as `static.crates.io` stores it.
static TOKIO_README: &str = include_str!("../fixtures/tokio-1.44.2.readme.html");

/// A rustdoc document generated to reach the 50 000-item ceiling.
static SYNTHETIC: LazyLock<String> =
    LazyLock::new(|| synthetic::rustdoc_document(synthetic::PATHS_FOR_CEILING));

/// The captured and generated documents parsed once, for the lookup groups,
/// which are not measuring the parse.
static REGEX_INDEX: LazyLock<DocIndex> = LazyLock::new(|| REGEX.parse());
static SYNTHETIC_INDEX: LazyLock<DocIndex> = LazyLock::new(|| {
    DocIndex::parse("synth", SYNTHETIC.as_bytes()).expect("the generator is valid")
});

/// Queries built once, so that the timed region holds a lookup and nothing else.
static SYNTHETIC_EXACT: LazyLock<String> = LazyLock::new(|| {
    format!("synth::m{}::Item{SYNTHETIC_TARGET}", SYNTHETIC_TARGET / synthetic::MODULE_SIZE)
});
static SYNTHETIC_SUFFIX: LazyLock<String> = LazyLock::new(|| {
    format!("m{}::Item{SYNTHETIC_TARGET}", SYNTHETIC_TARGET / synthetic::MODULE_SIZE)
});
static SYNTHETIC_NAME: LazyLock<String> = LazyLock::new(|| format!("item{SYNTHETIC_TARGET}"));

/// A cache directory seeded with both indexes, for the group below.
static CACHE: LazyLock<Store> = LazyLock::new(|| {
    let root = std::env::temp_dir().join(format!("mcp-crates-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let store = Store::at(&root);
    store.store("regex@1.11.1", &REGEX_INDEX.to_stored()).expect("stores");
    store.store("synth@0.1.0", &SYNTHETIC_INDEX.to_stored()).expect("stores");
    store
});

/// Expanding the document docs.rs transfers.
#[divan::bench_group]
mod decompress {
    use super::{DECOMPRESS_LIMIT, REGEX, SEMVER, docs};

    #[divan::bench]
    fn regex_fv55() -> Vec<u8> {
        docs::decompress_rustdoc("regex", REGEX.compressed, DECOMPRESS_LIMIT).expect("decompresses")
    }

    #[divan::bench]
    fn semver_fv60() -> Vec<u8> {
        docs::decompress_rustdoc("semver", SEMVER.compressed, DECOMPRESS_LIMIT)
            .expect("decompresses")
    }
}

/// Turning an expanded rustdoc document into an index: the dominant cost of the
/// whole client.
#[divan::bench_group]
mod parse {
    use super::{DocIndex, REGEX, SEMVER, SYNTHETIC};

    #[divan::bench]
    fn regex_fv55() -> DocIndex {
        REGEX.parse()
    }

    #[divan::bench]
    fn semver_fv60() -> DocIndex {
        SEMVER.parse()
    }

    #[divan::bench]
    fn synthetic_50k() -> DocIndex {
        DocIndex::parse("synth", SYNTHETIC.as_bytes()).expect("parses")
    }
}

/// Resolving a query against a captured index of 209 items.
#[divan::bench_group]
mod lookup_regex {
    use super::{Lookup, MISS, REGEX_INDEX};

    #[divan::bench]
    fn exact_path() -> Lookup<'static> {
        REGEX_INDEX.lookup("regex::builders::bytes::RegexBuilder::build")
    }

    #[divan::bench]
    fn unique_suffix() -> Lookup<'static> {
        REGEX_INDEX.lookup("pattern::RegexSearcher")
    }

    #[divan::bench]
    fn bare_name() -> Lookup<'static> {
        REGEX_INDEX.lookup("regexsearcher")
    }

    #[divan::bench]
    fn fuzzy_miss() -> Lookup<'static> {
        REGEX_INDEX.lookup(MISS)
    }
}

/// The same four queries against an index at the 50 000-item ceiling, which is
/// where the linear scans and the per-item lowercasing used to hurt.
#[divan::bench_group]
mod lookup_synthetic_50k {
    use super::{Lookup, MISS, SYNTHETIC_EXACT, SYNTHETIC_INDEX, SYNTHETIC_NAME, SYNTHETIC_SUFFIX};

    #[divan::bench]
    fn exact_path() -> Lookup<'static> {
        SYNTHETIC_INDEX.lookup(&SYNTHETIC_EXACT)
    }

    #[divan::bench]
    fn unique_suffix() -> Lookup<'static> {
        SYNTHETIC_INDEX.lookup(&SYNTHETIC_SUFFIX)
    }

    #[divan::bench]
    fn bare_name() -> Lookup<'static> {
        SYNTHETIC_INDEX.lookup(&SYNTHETIC_NAME)
    }

    #[divan::bench]
    fn fuzzy_miss() -> Lookup<'static> {
        SYNTHETIC_INDEX.lookup(MISS)
    }
}

/// The two parse paths that are not rustdoc JSON.
#[divan::bench_group]
mod documents {
    use super::{CrateIndex, SERDE_INDEX, TOKIO_README, readme};

    #[divan::bench]
    fn crate_index_parse() -> CrateIndex {
        CrateIndex::parse("serde", SERDE_INDEX).expect("the committed fixture parses")
    }

    #[divan::bench]
    fn readme_to_markdown() -> String {
        readme::to_markdown(TOKIO_README)
    }
}

/// Reading an index back from the disk cache, against parsing it again.
///
/// The comparison the cache exists to win: a warm session does this, a cold one
/// does `parse` above.
#[divan::bench_group]
mod disk_cache {
    use super::{CACHE, DocIndex, REGEX_INDEX, SYNTHETIC_INDEX, docs};

    #[divan::bench]
    fn load_regex() -> DocIndex {
        let stored =
            CACHE.load::<docs::StoredIndex>("regex@1.11.1").expect("no error").expect("present");
        DocIndex::from_stored(stored)
    }

    #[divan::bench]
    fn load_synthetic_50k() -> DocIndex {
        let stored =
            CACHE.load::<docs::StoredIndex>("synth@0.1.0").expect("no error").expect("present");
        DocIndex::from_stored(stored)
    }

    #[divan::bench]
    fn store_regex() {
        CACHE.store("regex@1.11.1", &REGEX_INDEX.to_stored()).expect("stores");
    }

    #[divan::bench]
    fn store_synthetic_50k() {
        CACHE.store("synth@0.1.0", &SYNTHETIC_INDEX.to_stored()).expect("stores");
    }
}
