//! Two server processes, one cache directory between them.
//!
//! This is the claim the disk cache is built to make, and it can only be made
//! end to end: a session is a process, so "the second session is cheaper" is a
//! statement about two processes, not about two calls.
//!
//! The cold half needs the network — it is what populates the cache — so the
//! whole test is gated behind `CRATES_IO_LIVE_TESTS`, like the rest of the
//! live suite. Everything the warm half asserts about *reading* the cache is
//! also covered without a network by the integration tests in
//! `crates-io-client`.

use std::{fs, path::PathBuf, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Generous: the cold run downloads and parses a real rustdoc document.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// A crate whose documentation docs.rs has built and which is small enough that
/// the cold run is a fetch rather than an endurance test.
const CRATE: &str = "semver";
const ITEM: &str = "Version::parse";

/// A directory that removes itself.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("mcp-crates-e2e-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("creatable");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One server process, driven one request at a time.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    async fn start(cache_dir: &std::path::Path, log: &std::path::Path) -> Self {
        // The shutdown line carries the counters, and stderr is where it goes.
        let stderr = fs::File::create(log).expect("the log file is creatable");
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-crates"))
            .env("MCP_CRATES_LOG", "mcp_crates=info")
            .env("MCP_CRATES_CACHE_DIR", cache_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("the server binary starts");

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut server = Self { child, stdin, stdout, next_id: 1 };

        server
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "disk-cache-e2e", "version": "1"},
                }),
            )
            .await
            .expect("initialize succeeds");
        server.notify("notifications/initialized").await;
        server
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .expect("serializes");
        self.stdin.write_all(line.as_bytes()).await.expect("written");
        self.stdin.write_all(b"\n").await.expect("written");
        self.stdin.flush().await.expect("flushed");

        let mut reply = String::new();
        tokio::time::timeout(REPLY_TIMEOUT, self.stdout.read_line(&mut reply))
            .await
            .expect("the server replies before the timeout")
            .expect("readable");
        let mut message: Value = serde_json::from_str(&reply).expect("the reply is JSON");
        if let Some(error) = message.get_mut("error") {
            return Err(error.take());
        }
        Ok(message["result"].take())
    }

    async fn notify(&mut self, method: &str) {
        let line = serde_json::to_string(&json!({"jsonrpc": "2.0", "method": method}))
            .expect("serializes");
        self.stdin.write_all(line.as_bytes()).await.expect("written");
        self.stdin.write_all(b"\n").await.expect("written");
        self.stdin.flush().await.expect("flushed");
    }

    /// Ask for one item's documentation, and say how long the answer took.
    async fn documentation(&mut self) -> (Value, Duration) {
        let started = std::time::Instant::now();
        let result = self
            .call(
                "tools/call",
                json!({
                    "name": "get_crate_documentation",
                    "arguments": {"name": CRATE, "item": ITEM, "include_readme": false},
                }),
            )
            .await
            .expect("the tool call succeeds");
        (result, started.elapsed())
    }

    async fn shutdown(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(REPLY_TIMEOUT, self.child.wait()).await;
    }
}

/// One counter out of the shutdown log line.
///
/// Read from the process's own report rather than reconstructed by the test,
/// so what is asserted is what the server would tell an operator.
fn counter(log: &std::path::Path, name: &str) -> u64 {
    let text = fs::read_to_string(log).expect("the log is readable");
    let needle = format!("{name}=");
    text.lines()
        .find(|line| line.contains("has stopped"))
        .and_then(|line| line.split(&needle).nth(1))
        .map(|rest| rest.split(|c: char| !c.is_ascii_digit()).next().unwrap_or(""))
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("no {name} in the shutdown line of {}", log.display()))
}

/// How many of each artifact kind the directory holds, as (bodies, indexes).
///
/// The two are namespaced apart on purpose, and a test that could not tell them
/// apart could not tell which half of the cache was doing the work.
fn kinds(root: &std::path::Path) -> (usize, usize) {
    let names: Vec<String> = fs::read_dir(root)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "mcpc"))
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let bodies = names.iter().filter(|name| name.starts_with("body-")).count();
    (bodies, names.len() - bodies)
}

/// How many files the cache directory holds.
fn entries(root: &std::path::Path) -> usize {
    fs::read_dir(root)
        .map(|dir| {
            dir.filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "mcpc"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn a_second_session_answers_from_disk_without_asking_docs_rs_again() {
    if std::env::var_os("CRATES_IO_LIVE_TESTS").is_none() {
        return;
    }

    let cache = TempDir::new("two-sessions");
    let logs = TempDir::new("two-sessions-logs");
    assert_eq!(entries(&cache.0), 0, "the cache starts empty");

    // Cold: nothing on disk, so this pays for the download and the parse.
    let cold_log = logs.0.join("cold.log");
    let mut cold = Server::start(&cache.0, &cold_log).await;
    let (cold_answer, cold_took) = cold.documentation().await;
    cold.shutdown().await;

    // Two artifacts, one of each kind: the parsed documentation, and the
    // sparse-index body that turned a crate name into a version.
    assert_eq!(entries(&cache.0), 2, "the cold session should have left both artifacts behind");
    assert_eq!(kinds(&cache.0), (1, 1), "one response body and one documentation index");
    assert_eq!(counter(&cold_log, "disk_hits"), 0, "a cold session hits nothing");
    assert_eq!(counter(&cold_log, "disk_writes"), 2, "and writes both of what it built");
    let cold_requests = counter(&cold_log, "network_requests");

    // Warm: a different process, sharing only the directory.
    let warm_log = logs.0.join("warm.log");
    let mut warm = Server::start(&cache.0, &warm_log).await;
    let (warm_answer, warm_took) = warm.documentation().await;
    warm.shutdown().await;

    // The answer has to be the same one, or "faster" would mean nothing.
    assert_eq!(
        cold_answer["structuredContent"]["item"], warm_answer["structuredContent"]["item"],
        "the warm session must answer with the same item"
    );
    assert!(
        !warm_answer["structuredContent"]["item"].is_null(),
        "the item should have resolved: {warm_answer}"
    );

    // The claim that matters, and the one that is exact: the rustdoc document
    // came off disk, so not one of its bytes crossed the wire a second time.
    assert_eq!(counter(&warm_log, "disk_hits"), 2, "the warm session reads both artifacts");
    assert_eq!(counter(&warm_log, "disk_writes"), 0, "and has nothing new to write");

    // With the sparse index persisted too, the warm session has nothing left to
    // ask anyone: the version it resolves and the document it reads both came
    // off disk.
    let warm_requests = counter(&warm_log, "network_requests");
    assert_eq!(
        warm_requests, 0,
        "the warm session made {warm_requests} requests against the cold session's \
         {cold_requests}; it should need none"
    );

    let ratio = cold_took.as_secs_f64() / warm_took.as_secs_f64();
    eprintln!(
        "cold {cold_took:?} / {cold_requests} requests, warm {warm_took:?} / {warm_requests} \
         requests, {ratio:.1}x faster"
    );
    assert!(ratio >= 5.0, "the warm session was only {ratio:.1}x faster than the cold one");
}
