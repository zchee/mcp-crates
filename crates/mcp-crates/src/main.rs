//! Command line entry point for the crates.io MCP server.

use std::{sync::Arc, time::Duration};

use clap::Parser;
use crates_io_client::{Client, Config};
use mcp_crates::CratesServer;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

/// Indexing a rustdoc document is hundreds of thousands of short-lived
/// allocations arriving in a burst on one blocking thread, which is the shape
/// the system allocator's per-size-class locking handles worst. mimalloc serves
/// that burst from thread-local free lists instead.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The crates.io crawler policy: no more than one API request per second.
///
/// Configurable upwards but never downwards; a client that ignores this risks
/// being blocked, and a server that made it easy to ignore would be a hazard to
/// whoever ran it.
const POLICY_MIN_API_INTERVAL_MS: u64 = 1000;

/// A Model Context Protocol server for crates.io.
#[derive(Debug, Parser)]
#[command(name = "mcp-crates", version, about, long_about = None)]
struct Cli {
    /// Contact details to advertise in the User-Agent header, such as an email
    /// address or a repository URL.
    ///
    /// crates.io asks that clients identify themselves and offer a way to make
    /// contact, so that they can reach out before resorting to a block.
    #[arg(long, env = "MCP_CRATES_CONTACT", value_name = "CONTACT")]
    contact: Option<String>,

    /// Replace the User-Agent header outright, rather than appending contact
    /// details to the default.
    #[arg(long, env = "MCP_CRATES_USER_AGENT", value_name = "STRING")]
    user_agent: Option<String>,

    /// Minimum milliseconds between crates.io API requests.
    ///
    /// Values below the published one-per-second policy are raised to it.
    #[arg(
        long,
        env = "MCP_CRATES_API_INTERVAL_MS",
        value_name = "MS",
        default_value_t = POLICY_MIN_API_INTERVAL_MS
    )]
    api_interval_ms: u64,

    /// Longest a request may wait behind the pacing queue before it is shed.
    #[arg(long, env = "MCP_CRATES_QUEUE_WAIT_SECS", value_name = "SECS", default_value_t = 30)]
    queue_wait_secs: u64,

    /// Approximate ceiling on the response cache, in mebibytes.
    #[arg(long, env = "MCP_CRATES_CACHE_MIB", value_name = "MIB", default_value_t = 128)]
    cache_mib: u64,

    /// Do not keep parsed documentation on disk between runs.
    ///
    /// The server is started afresh for every client session, so without a
    /// disk cache each one re-downloads and re-parses rustdoc JSON that cannot
    /// have changed since the last.
    #[arg(long, env = "MCP_CRATES_NO_DISK_CACHE")]
    no_disk_cache: bool,

    /// Where to keep it, instead of the platform's cache directory.
    #[arg(long, env = "MCP_CRATES_CACHE_DIR", value_name = "DIR")]
    cache_dir: Option<std::path::PathBuf>,

    /// Ceiling on the documentation cache, in mebibytes.
    ///
    /// Enforced at startup by deleting the least recently written entries.
    #[arg(long, env = "MCP_CRATES_DISK_CACHE_MIB", value_name = "MIB", default_value_t = 512)]
    disk_cache_mib: u64,

    /// Log filter, in `tracing` `EnvFilter` syntax. Logs are written to stderr,
    /// because stdout carries the protocol.
    #[arg(
        long,
        env = "MCP_CRATES_LOG",
        value_name = "FILTER",
        default_value = "mcp_crates=info,crates_io_client=info"
    )]
    log: String,
}

impl Cli {
    /// The User-Agent to identify this server with.
    fn user_agent(&self) -> String {
        if let Some(explicit) = &self.user_agent {
            return explicit.clone();
        }
        let base = concat!(
            env!("CARGO_PKG_NAME"),
            "/",
            env!("CARGO_PKG_VERSION"),
            " (+",
            env!("CARGO_PKG_REPOSITORY"),
        );
        match &self.contact {
            Some(contact) => format!("{base}; {contact})"),
            None => format!("{base})"),
        }
    }

    /// Turn the command line into a client configuration.
    fn config(&self) -> Config {
        let mut config = Config::new(self.user_agent());
        config.api_min_interval =
            Duration::from_millis(self.api_interval_ms.max(POLICY_MIN_API_INTERVAL_MS));
        config.max_queue_wait = Duration::from_secs(self.queue_wait_secs);
        config.cache_capacity_bytes = self.cache_mib.saturating_mul(1024 * 1024);
        config.disk_cache = !self.no_disk_cache;
        config.cache_dir.clone_from(&self.cache_dir);
        config.disk_cache_capacity_bytes = self.disk_cache_mib.saturating_mul(1024 * 1024);
        config
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // stdout is the protocol channel, so diagnostics must go to stderr.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_ansi(false)
        .init();

    if cli.api_interval_ms < POLICY_MIN_API_INTERVAL_MS {
        tracing::warn!(
            requested_ms = cli.api_interval_ms,
            applied_ms = POLICY_MIN_API_INTERVAL_MS,
            "the requested API interval is faster than the crates.io crawler policy allows and \
             has been raised to the policy minimum"
        );
    }

    let config = cli.config();
    tracing::info!(
        user_agent = %config.user_agent,
        api_interval_ms = config.api_min_interval.as_millis(),
        cache_mib = cli.cache_mib,
        disk_cache = config.disk_cache,
        disk_cache_dir = ?config.cache_dir,
        "starting the crates.io MCP server"
    );

    let client = Arc::new(Client::new(config)?);
    let service = CratesServer::new(Arc::clone(&client)).serve(stdio()).await?;

    let reason = service.waiting().await?;
    let stats = client.stats();
    tracing::info!(
        ?reason,
        cache_hits = stats.cache_hits,
        coalesced = stats.coalesced,
        network_requests = stats.network_requests,
        not_modified = stats.not_modified,
        bytes_received = stats.bytes_received,
        disk_hits = stats.disk_hits,
        disk_writes = stats.disk_writes,
        "the crates.io MCP server has stopped"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("mcp-crates").chain(args.iter().copied()))
            .expect("the arguments parse")
    }

    #[test]
    fn the_default_user_agent_identifies_the_server_and_its_repository() {
        let agent = parse(&[]).user_agent();

        assert!(agent.starts_with("mcp-crates/"), "{agent}");
        assert!(agent.contains("github.com/zchee/mcp-crates"), "{agent}");
    }

    #[test]
    fn contact_details_are_appended_rather_than_replacing_the_identity() {
        let agent = parse(&["--contact", "dev@example.com"]).user_agent();

        assert!(agent.starts_with("mcp-crates/"), "{agent}");
        assert!(agent.contains("dev@example.com"), "{agent}");
        assert!(agent.ends_with(')'), "{agent}");
    }

    #[test]
    fn an_explicit_user_agent_wins_outright() {
        let agent = parse(&["--user-agent", "custom/1.0 (me@example.com)"]).user_agent();
        assert_eq!(agent, "custom/1.0 (me@example.com)");
    }

    #[test]
    fn an_api_interval_faster_than_policy_is_raised_to_the_policy_minimum() {
        let config = parse(&["--api-interval-ms", "10"]).config();
        assert_eq!(config.api_min_interval, Duration::from_millis(POLICY_MIN_API_INTERVAL_MS));
    }

    #[test]
    fn a_slower_api_interval_is_honoured() {
        let config = parse(&["--api-interval-ms", "2500"]).config();
        assert_eq!(config.api_min_interval, Duration::from_millis(2500));
    }
}
