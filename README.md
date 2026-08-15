# mcp-crates

A [Model Context Protocol](https://modelcontextprotocol.io) server for the
[crates.io](https://crates.io) registry. It gives a language model read-only
access to the Rust package ecosystem: search for crates, inspect their metadata
and release history, read their dependency graphs, and pull documentation down
to the level of an individual method.

crates.io asks API clients to stay at or below **one request per second** and to
send a `User-Agent` that identifies the application. This server does both, and
is built so that the limit is rarely the thing you wait on.

## Tools

| Tool | Answers |
|---|---|
| `search_crates` | Which crates match this topic, keyword or category? |
| `get_crate_info` | What is this crate, how popular is it, what does its current release look like? |
| `get_crate_versions` | What has been released, and what satisfies `^1.2`? |
| `get_crate_dependencies` | What does this version depend on, with which features and version requirements? |
| `get_crate_documentation` | What does this crate's README say, or what does `Deserializer::deserialize_any` do? |

Version arguments accept an exact version (`1.0.219`), a Cargo requirement
(`^1.0`, `>=0.4, <0.6`), or `latest`. They default to the newest release that is
neither yanked nor a pre-release — the version `cargo add` would pick.

## Install

```sh
cargo install --path crates/mcp-crates
```

## Use

The server speaks MCP over stdio. Register it with your client:

```json
{
  "mcpServers": {
    "crates": {
      "command": "mcp-crates",
      "args": ["--contact", "you@example.com"]
    }
  }
}
```

`--contact` is appended to the `User-Agent`. crates.io
[asks](https://crates.io/data-access) that you provide a way to be reached, so
they can get in touch before resorting to a block. It is optional, but supplying
it is the polite thing to do.

### Configuration

Every flag has an environment variable equivalent.

| Flag | Environment variable | Default | Purpose |
|---|---|---|---|
| `--contact` | `MCP_CRATES_CONTACT` | none | Contact details added to the `User-Agent` |
| `--user-agent` | `MCP_CRATES_USER_AGENT` | `mcp-crates/<version> (+<repository>)` | Replace the `User-Agent` outright |
| `--api-interval-ms` | `MCP_CRATES_API_INTERVAL_MS` | `1000` | Minimum spacing between API requests |
| `--queue-wait-secs` | `MCP_CRATES_QUEUE_WAIT_SECS` | `30` | How long a request may queue before it is shed |
| `--cache-mib` | `MCP_CRATES_CACHE_MIB` | `128` | Response cache ceiling |
| `--log` | `MCP_CRATES_LOG` | `mcp_crates=info,crates_io_client=info` | Log filter; logs go to stderr |

`--api-interval-ms` is clamped upward to the one-per-second policy. Asking for a
faster rate logs a warning and is ignored.

## How the request budget is spent

One request per second is not much. Most of the work in this server is about not
needing it.

**Ask the cheap source.** crates.io publishes the same data in more than one
place, at very different prices. The sparse index at `index.crates.io` — the one
Cargo itself uses — returns *every* version of a crate with its full dependency
list, feature table and yank status, in a single CDN request that carries an
`ETag` and is not under the API's budget. `get_crate_versions` and
`get_crate_dependencies` therefore cost no API budget at all.

**Ask for less.** The crate detail endpoint defaults to returning everything,
which for `serde` is **432 KB**, almost all of it per-version records the sparse
index reports better. Narrowing the `include` parameter to the fields that
describe the crate itself brings the same request down to **3.6 KB**.

That turned out to matter for correctness too: crates.io only computes
`max_stable_version` and `newest_version` when the per-version payload is
requested, and reports serde's highest stable version as *absent* otherwise.
Those fields are read from the sparse index instead, where they are always
right.

**Do not ask twice.** Every fetch passes through four layers:

1. **Freshness** — a cached response inside its lifetime is returned without
   touching the network.
2. **Coalescing** — concurrent callers asking for the same URL wait on one gate,
   so a burst of tool calls issues one request between them.
3. **Revalidation** — a stale response holding an `ETag` is refreshed
   conditionally, so the usual `304` transfers headers instead of a body.
4. **Pacing** — whatever survives is emitted through a per-origin rate limiter.

Lifetimes follow how quickly the data can actually change: anything addressed by
an exact version is immutable once published and is held for a week; live
registry state is held for minutes and revalidated cheaply. A `404` is
remembered briefly too, so a typo does not spend budget on every repeat.

Parsed forms are memoized next to the bytes they came from and survive
revalidation, so a crate's index document is parsed once per distinct payload
however many times it is read.

In practice, four consecutive tool calls about `serde` — metadata twice, then
versions, then dependencies — cost **two requests**, and the other six lookups
come from cache.

**Pace without a queue.** The rate limiter is a virtual-scheduling pacer: each
caller claims the next slot on a shared timeline with one atomic
compare-exchange, then sleeps until it arrives. No mutex, no background timer, no
channel. Slots are handed out in arrival order, so nothing starves, and a caller
that would have to wait past a configured ceiling is told so immediately rather
than stalling a tool call indefinitely.

## Item-level documentation

`get_crate_documentation` reads the rustdoc JSON that docs.rs generates for each
release. That document's path table lists only items with a canonical path,
which leaves out every method and associated type — for `serde`, 81 items, none
of them the thing anyone actually asks about. This server folds in trait members
and inherent `impl` blocks as well, bringing serde to 289 items and making this
work:

```json
{"name": "serde", "item": "Deserializer::deserialize_any"}
```

Methods from derived trait impls are deliberately left out; including them would
bury a crate's own API under thousands of `clone`, `fmt` and `eq` entries.

Lookups widen in steps — exact path, then path suffix, then bare name, then
re-export, then substring — so `Value::as_str` finds
`serde_json::value::Value::as_str` without the caller knowing which module it
lives in. A query matching several items returns them all as suggestions rather
than silently picking one.

Facade crates need the re-export step. A crate whose public API is mostly
`pub use` of its own sub-crates documents almost nothing itself: rustdoc
attributes each re-exported item to the crate that defines it, and `ratatui`
is left with 16 items of its own against 6805 foreign ones it merely mentions.
Following the `use` items separates the 125 genuine re-exports from the rest,
so asking `ratatui` about `Frame` answers with where to look:

```
"Frame" is not documented by this crate, which only re-exports it from
ratatui_core; call this tool again for that crate with item
"ratatui_core::terminal::frame::Frame"
```

READMEs come back as Markdown rather than the HTML crates.io stores, with images
and heading anchors dropped: a reader that cannot see a badge gains nothing from
its URL.

## Layout

```
crates/crates-io-client   registry access, pacing, caching — no MCP dependency
crates/mcp-crates         the MCP server built on it
```

`crates-io-client` is usable on its own as a polite, cache-aware read client for
crates.io, its sparse index, and docs.rs.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

The tests that talk to the real registry are skipped by default, since they
spend real request budget:

```sh
CRATES_IO_LIVE_TESTS=1 cargo test -p crates-io-client --test live -- --nocapture
```

## License

Apache-2.0. See [LICENSE](LICENSE).
