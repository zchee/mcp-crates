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
//! Never concurrently with another build: a benchmark sharing the machine with
//! a linker is measuring the linker. Comparisons across a change are made
//! against a stored baseline rather than by copying numbers between documents,
//! which is what keeps a claimed improvement checkable:
//!
//! ```sh
//! cargo bench -p crates-io-client -- --save-baseline before
//! # ... make the change ...
//! cargo bench -p crates-io-client -- --baseline before
//! ```

use std::{sync::LazyLock, time::Duration};

use crates_io_client::{CrateIndex, DocIndex, Lookup, docs, readme, synthetic};
use criterion::{Criterion, measurement::WallTime};

/// The allocator the server binary installs.
///
/// The library itself is allocator-agnostic, so nothing would otherwise pull
/// this in — and a benchmark measuring the system allocator would be measuring
/// something no user runs. Most of what is timed below is allocation.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Expansion ceiling handed to `decompress_rustdoc`, far above any fixture.
const DECOMPRESS_LIMIT: usize = 64 * 1024 * 1024;

/// Long enough for criterion to fit its default 100 samples around the slowest
/// benchmarks here, which are tens of milliseconds each. Left at the default,
/// criterion would warn on every run and shrink the sample it reasons from.
const SLOW_MEASUREMENT: Duration = Duration::from_secs(12);

fn main() {
    verify_fixtures();

    let mut criterion = Criterion::default().configure_from_args();
    decompress(&mut criterion);
    parse(&mut criterion);
    lookup_regex(&mut criterion);
    lookup_synthetic_50k(&mut criterion);
    documents(&mut criterion);
    criterion.final_summary();
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

/// Expanding the document docs.rs transfers.
fn decompress(criterion: &mut Criterion<WallTime>) {
    let mut group = criterion.benchmark_group("decompress");
    group.bench_function("regex_fv55", |bencher| {
        bencher.iter(|| {
            docs::decompress_rustdoc("regex", REGEX.compressed, DECOMPRESS_LIMIT)
                .expect("decompresses")
        });
    });
    group.bench_function("semver_fv60", |bencher| {
        bencher.iter(|| {
            docs::decompress_rustdoc("semver", SEMVER.compressed, DECOMPRESS_LIMIT)
                .expect("decompresses")
        });
    });
    group.finish();
}

/// Turning an expanded rustdoc document into an index: the dominant cost of the
/// whole client.
fn parse(criterion: &mut Criterion<WallTime>) {
    let mut group = criterion.benchmark_group("parse");
    group.measurement_time(SLOW_MEASUREMENT);
    group.bench_function("regex_fv55", |bencher| bencher.iter(|| REGEX.parse()));
    group.bench_function("semver_fv60", |bencher| bencher.iter(|| SEMVER.parse()));
    group.bench_function("synthetic_50k", |bencher| {
        bencher.iter(|| DocIndex::parse("synth", SYNTHETIC.as_bytes()).expect("parses"));
    });
    group.finish();
}

/// Resolving a query against a captured index of 209 items.
fn lookup_regex(criterion: &mut Criterion<WallTime>) {
    let mut group = criterion.benchmark_group("lookup_regex");
    group.bench_function("exact_path", |bencher| {
        bencher.iter(|| REGEX_INDEX.lookup("regex::builders::bytes::RegexBuilder::build"));
    });
    group.bench_function("unique_suffix", |bencher| {
        bencher.iter(|| REGEX_INDEX.lookup("pattern::RegexSearcher"));
    });
    group.bench_function("bare_name", |bencher| {
        bencher.iter(|| REGEX_INDEX.lookup("regexsearcher"))
    });
    group.bench_function("fuzzy_miss", |bencher| bencher.iter(|| REGEX_INDEX.lookup(MISS)));
    group.finish();
}

/// The same four queries against an index at the 50 000-item ceiling, which is
/// where the linear scans and the per-item lowercasing actually hurt.
fn lookup_synthetic_50k(criterion: &mut Criterion<WallTime>) {
    let mut group = criterion.benchmark_group("lookup_synthetic_50k");
    group.measurement_time(SLOW_MEASUREMENT);
    group.bench_function("exact_path", |bencher| {
        bencher.iter(|| SYNTHETIC_INDEX.lookup(&SYNTHETIC_EXACT));
    });
    group.bench_function("unique_suffix", |bencher| {
        bencher.iter(|| SYNTHETIC_INDEX.lookup(&SYNTHETIC_SUFFIX));
    });
    group.bench_function("bare_name", |bencher| {
        bencher.iter(|| SYNTHETIC_INDEX.lookup(&SYNTHETIC_NAME));
    });
    group.bench_function("fuzzy_miss", |bencher| bencher.iter(|| SYNTHETIC_INDEX.lookup(MISS)));
    group.finish();
}

/// The two parse paths that are not rustdoc JSON.
fn documents(criterion: &mut Criterion<WallTime>) {
    let mut group = criterion.benchmark_group("documents");
    group.bench_function("crate_index_parse", |bencher| {
        bencher.iter(|| CrateIndex::parse("serde", SERDE_INDEX).expect("the fixture parses"));
    });
    group.bench_function("readme_to_markdown", |bencher| {
        bencher.iter(|| readme::to_markdown(TOKIO_README));
    });
    group.finish();
}
