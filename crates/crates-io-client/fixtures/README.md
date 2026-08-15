# Benchmark fixtures

Real documents, captured once and committed, so that a benchmark run measures
this crate's parsing and lookup code rather than the network, and so that the
differential parity suite in `src/docs.rs` and `src/index.rs` has real payloads
to compare two deserializers over. Each one is
immutable at its source — a published version's rustdoc JSON and rendered README
never change — so a captured copy stays a faithful sample indefinitely.

Captured 2026-08-16. Every fetch used a `curl` with a `User-Agent` identifying
this project, one request per second, matching the policy the client itself
follows.

| File | Source | What it is |
|---|---|---|
| `regex-1.11.1.rustdoc.json.zst` | `https://docs.rs/crate/regex/1.11.1/json` | rustdoc JSON, `format_version` 55 |
| `semver-1.0.28.rustdoc.json.zst` | `https://docs.rs/crate/semver/1.0.28/json` | rustdoc JSON, `format_version` 60 |
| `serde.index.json` | `https://index.crates.io/se/rd/serde` | sparse-index document, 316 versions |
| `tokio-1.44.2.readme.html` | `https://crates.io/api/v1/crates/tokio/1.44.2/readme` | rendered README, as `static.crates.io` serves it |

## Digests

A benchmark figure is only comparable to another one if both were taken over the
same bytes, and a parity result only means something if the two deserializers
were handed the same document. Both claims are made across commits — the
optimization record compares a measurement at `49f7569` against one at HEAD — so
the identity of these files is pinned here rather than assumed.

| File | Bytes | SHA-256 |
|---|---|---|
| `regex-1.11.1.rustdoc.json.zst` | 122 204 | `d09232dde79d8ec3e244e8239076a2168ccf1d6a176e8a918cdd4023803d17c2` |
| `semver-1.0.28.rustdoc.json.zst` | 48 435 | `b25a4ceb9896f1a9713db5a3d30a7473ae3c809cd4d2416fe414915ae44ed08e` |
| `serde.index.json` | 164 874 | `9eb6e78eab279f168b5787e4eb801765e1d5a2b40c50789e12166a616d73bbc5` |
| `tokio-1.44.2.readme.html` | 12 333 | `dac81e2088bff9621f34848f9a5fe146cc710a8a7464bbb24212bb8b685071cf` |

```sh
shasum -a 256 crates/crates-io-client/fixtures/*.zst \
              crates/crates-io-client/fixtures/serde.index.json \
              crates/crates-io-client/fixtures/tokio-1.44.2.readme.html
```

These files lived at `benches/fixtures/` until `b499047` moved them here, when
the parity suite began needing them too. The move was a relocation and nothing
else: the digests above match the ones under the old path, which is what makes a
benchmark taken before it comparable to one taken after.

Replacing a fixture means replacing a baseline. Anything measured against the old
bytes stops being a comparison at that moment, so a replacement belongs in its
own commit, with the digests here updated in the same one.

## The two rustdoc documents

They are a pair on purpose. rustdoc's JSON schema is versioned and changes
shape between releases, so a corpus of one format version proves nothing about
the others. `regex` was built at `format_version` 55 and `semver` at 60, which
brackets the range docs.rs currently serves.

They are stored exactly as docs.rs transfers them — zstd frames, not JSON — so
that the decompression step is measured on real bytes and the compression ratio
is the real one. `regex` expands from 122 KB to 1.35 MB, `semver` from 48 KB to
448 KB.

| File | Crate | `format_version` | Compressed | Expanded | Indexed items |
|---|---|---|---|---|---|
| `regex-1.11.1.rustdoc.json.zst` | `regex` 1.11.1 | 55 | 122 204 B | 1 347 778 B | 209 |
| `semver-1.0.28.rustdoc.json.zst` | `semver` 1.0.28 | 60 | 48 435 B | 448 052 B | 32 |

The item counts are low relative to the document size because rustdoc's `paths`
table mostly names *foreign* items, which the index discards. No real crate of a
size worth committing reaches the 50 000-item ceiling the lookup path is bounded
by, which is why `src/synthetic.rs` generates a document of that size in code
rather than storing one here; the benchmark and the parity suite share it.

## The other two

`serde.index.json` is stored uncompressed because that is how
`index.crates.io` serves it and how `CrateIndex::parse` receives it. 316
newline-delimited entries is the largest realistic per-line parsing workload in
the ecosystem.

`tokio-1.44.2.readme.html` was chosen for its shape rather than its size: it
opens with a wall of badge images wrapped in links and gives every heading an
injected anchor, which is exactly what the link-rewriting passes in `readme.rs`
exist to remove.
