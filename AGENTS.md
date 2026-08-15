# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Workspace

Two crates: `crates/crates-io-client` (rate-limited, cache-aware HTTP/parse
layer) and `crates/mcp-crates` (the MCP stdio server over it). Toolchain is
pinned to stable via `rust-toolchain.toml`; `rustfmt.toml` deliberately
contains only options stable rustfmt enforces — do not add nightly-only
options back.

## Invariants that are easy to break

- **Crawler policy is a floor, not a setting.** One crates.io API request per
  second and an identifying `User-Agent` are required by crates.io.
  `POLICY_MIN_API_INTERVAL_MS` in `crates/mcp-crates/src/main.rs` clamps
  upward only; `Fetcher::new` rejects an empty UA. Never make either
  configurable downward.
- **Hot JSON paths use sonic-rs; serde_json stays for two jobs**: the
  error-detail probe in `fetch.rs` (part of a documented provenance argument —
  read the comments at `extract_detail`/`is_markup` before touching it) and as
  the dev-dependency referee in the differential parity tests. Any change to
  deserialization must keep the parity suite at zero divergence.
- **`unsafe_code = "forbid"` is workspace-wide.** `bitcode` is used through
  its serde integration with `default-features = false` because its derive
  macro would bring `unsafe` into the build. Keep it that way.
- **`[profile.release]` sets `panic = "abort"` and `strip`; the
  `build-override` carve-out is load-bearing**: macOS cannot dlopen a stripped
  proc-macro dylib ("mis-aligned LINKEDIT string pool"), so removing the
  override silently breaks every release build.
- **Disk-cache artifacts carry a schema-version header.** If you change any
  stored shape (`DocIndex`, persisted index bodies), bump
  `cache_schema_version` — old files must be rejected, never reinterpreted.

## Benchmarks and fixtures

- Benches use divan (`crates/crates-io-client/benches/hot_paths.rs`,
  `harness = false`). Never run a bench concurrently with any other build or
  heavy process; compare medians from an idle machine, or use paired
  back-to-back runs with the `fastest` column as a contention control.
- Fixtures in `crates/crates-io-client/fixtures/` are shared by benches and
  the parity suite, and are SHA-256-pinned in `fixtures/README.md`.
  **Replacing a fixture retires every number measured against the old bytes**
  — do it in its own commit and update the digests alongside.
- Performance claims live in `.omc/research/optimization-baseline.md` and
  `optimization-results.md` (never committed). New measurements are compared
  against the recorded baseline, not against remembered numbers.

## Testing

- Table tests as a name→case map; test names read as sentences
  (`a_zero_readme_budget_is_rejected_rather_than_returning_a_marker`).
- No mock services. Network-dependent tests are gated behind
  `CRATES_IO_LIVE_TESTS=1` and must run sequentially
  (`-- --test-threads=1`); they spend the real 1 req/s budget, so do not run
  them casually or in parallel.
- Doctests count: `cargo nextest` skips them, so a full gate also needs
  `cargo test --workspace --doc`.

## Commits

- Subject is `<scope>: <intent>` — intent says *why*, not what. The scope must
  never be a Semantic Commit type keyword (`docs`, `fix`, `feat`, `perf`, …);
  changes to the `docs.rs` module are scoped `client:`. GPG-sign, write the
  message to a file and pass `-F` (never multiple `-m`), 72-column subject and
  body.
