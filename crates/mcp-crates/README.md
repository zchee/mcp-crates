# mcp-crates

A [Model Context Protocol](https://modelcontextprotocol.io) server for the
[crates.io](https://crates.io) registry, exposing five read-only tools:

- `search_crates` — find crates by topic, keyword or category
- `get_crate_info` — a crate's metadata and current release
- `get_crate_versions` — release history, filterable by Cargo requirement
- `get_crate_dependencies` — one version's dependency graph
- `get_crate_documentation` — a crate's README, or one item's documentation

```sh
cargo install --path .
```

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

Registry access, pacing and caching live in the
[`crates-io-client`](../crates-io-client) crate; this crate is the protocol
surface over it. See the [workspace README](../../README.md) for configuration
and for how the one-request-per-second budget is spent.

## License

Apache-2.0.
