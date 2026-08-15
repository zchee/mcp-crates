# crates-io-client

A rate-limited, cache-aware read client for the Rust crate ecosystem: the
[crates.io](https://crates.io) API, its sparse index, rendered READMEs, and
[docs.rs](https://docs.rs).

crates.io asks API clients to stay at or below one request per second and to
send a `User-Agent` that identifies the application. This crate does both, and
is built so that the limit is rarely the thing you wait on.

```rust,no_run
use crates_io_client::{Client, Config, Selector};

# async fn example() -> Result<(), crates_io_client::Error> {
let client = Client::new(Config::new("my-app/1.0 (+https://example.com/my-app)"))?;

// One CDN request; every version, dependency and feature comes back with it.
let index = client.index("serde").await?;
let latest = index.resolve(&Selector::Default, false)?;
println!("serde {} has {} dependencies", latest.vers, latest.deps.len());
# Ok(())
# }
```

## Where the data comes from

| Source | Serves | Budget |
|---|---|---|
| `crates.io/api/v1` | search, crate metadata | one request per second |
| `index.crates.io` | every version, dependency and feature of a crate | CDN, `ETag`, built for Cargo |
| `static.crates.io` | rendered READMEs | CDN, `ETag`, immutable per version |
| `docs.rs` | build status, rustdoc JSON | its own budget |

The API budget is the scarce one, so it is spent only on questions the API alone
can answer. Version and dependency queries go to the sparse index, where one
request covers a whole crate.

## What it does to avoid requests

Every fetch passes through four layers: a freshness check that answers from
memory, a coalescing gate so concurrent callers share one request, conditional
revalidation so an unchanged resource costs headers rather than a body, and a
per-origin pacer that enforces the policy on whatever is left.

Parsed forms are memoized alongside the bytes they came from and survive
revalidation, so a document is parsed once per distinct payload however many
times it is read.

## Other properties worth knowing

- Only crates.io and docs.rs hosts are reachable, checked on every redirect hop,
  so a redirect cannot steer the client at an unrelated origin.
- Crate names are validated against the registry's rules before they reach a
  URL.
- Response bodies and rustdoc decompression are both bounded, so a pathological
  payload cannot exhaust memory.
- Errors carry a stable `kind` discriminant and a `retryable` flag, so callers
  can branch without parsing prose.

## Testing

```sh
cargo test -p crates-io-client
```

Tests that talk to the real registry are skipped unless asked for, since they
spend real request budget:

```sh
CRATES_IO_LIVE_TESTS=1 cargo test -p crates-io-client --test live -- --nocapture
```

## License

Apache-2.0.
