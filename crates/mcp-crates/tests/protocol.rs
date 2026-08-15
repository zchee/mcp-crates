//! End-to-end tests that drive the real binary over the MCP stdio transport.
//!
//! Everything here is answered without contacting crates.io: the handshake, the
//! tool catalogue, and the argument validation that runs before any request is
//! made. That last part is the point — a tool that rejects a malformed crate
//! name must do so without spending the request budget on it.

use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Fails the test rather than hanging the suite if the server stops answering.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// A running server, driven one request at a time.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    /// Start the server and complete the MCP handshake.
    async fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_mcp-crates"))
            // Log output would interleave with nothing useful here, and stderr
            // is inherited by the test harness.
            .env("MCP_CRATES_LOG", "error")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the server binary starts");

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        let mut server = Self { child, stdin, stdout, next_id: 1 };

        let initialized = server
            .call(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "protocol-test", "version": "1"},
                }),
            )
            .await
            .expect("initialize succeeds");
        assert!(
            initialized["capabilities"]["tools"].is_object(),
            "the server must advertise tools: {initialized}"
        );

        server.notify("notifications/initialized").await;
        server
    }

    /// Send a request and wait for its reply, returning the result or the error.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id;
        self.next_id += 1;

        let line = serde_json::to_string(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
        )
        .expect("the request serializes");
        self.stdin.write_all(line.as_bytes()).await.expect("the request is written");
        self.stdin.write_all(b"\n").await.expect("the newline is written");
        self.stdin.flush().await.expect("the request is flushed");

        let mut reply = String::new();
        tokio::time::timeout(REPLY_TIMEOUT, self.stdout.read_line(&mut reply))
            .await
            .expect("the server replies before the timeout")
            .expect("the reply is readable");
        assert!(!reply.is_empty(), "the server closed the connection instead of replying");

        let mut message: Value = serde_json::from_str(&reply).expect("the reply is JSON");
        assert_eq!(message["id"], id, "replies arrive in order on a single stream");
        if let Some(error) = message.get_mut("error") {
            return Err(error.take());
        }
        Ok(message["result"].take())
    }

    /// Send a notification, which has no reply.
    async fn notify(&mut self, method: &str) {
        let line = serde_json::to_string(&json!({"jsonrpc": "2.0", "method": method}))
            .expect("the notification serializes");
        self.stdin.write_all(line.as_bytes()).await.expect("the notification is written");
        self.stdin.write_all(b"\n").await.expect("the newline is written");
        self.stdin.flush().await.expect("the notification is flushed");
    }

    /// Invoke a tool.
    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, Value> {
        self.call("tools/call", json!({"name": name, "arguments": arguments})).await
    }

    /// Close stdin and wait for the server to exit.
    async fn shutdown(mut self) {
        drop(self.stdin);
        let status = tokio::time::timeout(REPLY_TIMEOUT, self.child.wait())
            .await
            .expect("the server exits when its input closes")
            .expect("the exit status is readable");
        assert!(status.success(), "the server exited with {status}");
    }
}

#[tokio::test]
async fn the_server_advertises_exactly_the_documented_tools() {
    let mut server = Server::start().await;

    let result = server.call("tools/list", json!({})).await.expect("tools/list succeeds");
    let mut names: Vec<&str> = result["tools"]
        .as_array()
        .expect("tools is an array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("each tool is named"))
        .collect();
    names.sort_unstable();

    assert_eq!(
        names,
        [
            "get_crate_dependencies",
            "get_crate_documentation",
            "get_crate_info",
            "get_crate_versions",
            "search_crates",
        ]
    );

    server.shutdown().await;
}

#[tokio::test]
async fn every_tool_documents_its_arguments() {
    let mut server = Server::start().await;

    let result = server.call("tools/list", json!({})).await.expect("tools/list succeeds");
    for tool in result["tools"].as_array().expect("tools is an array") {
        let name = tool["name"].as_str().expect("each tool is named");

        assert!(
            tool["description"].as_str().is_some_and(|text| text.len() > 40),
            "{name} needs a description a model can choose from"
        );

        let schema = &tool["inputSchema"];
        assert_eq!(schema["type"], "object", "{name} must take an object");

        // A description on every property is what lets a model fill arguments
        // in without guessing.
        for (property, definition) in
            schema["properties"].as_object().expect("properties is an object")
        {
            assert!(
                definition.get("description").is_some_and(Value::is_string),
                "{name}.{property} is undocumented"
            );
        }
    }

    server.shutdown().await;
}

#[tokio::test]
async fn the_crate_name_is_required_by_the_schema_of_every_lookup_tool() {
    let mut server = Server::start().await;

    let result = server.call("tools/list", json!({})).await.expect("tools/list succeeds");
    for tool in result["tools"].as_array().expect("tools is an array") {
        let name = tool["name"].as_str().expect("each tool is named");
        if name == "search_crates" {
            continue;
        }
        let required: Vec<&str> = tool["inputSchema"]["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, ["name"], "{name} should require only the crate name");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_malformed_crate_name_is_rejected_without_contacting_the_registry() {
    let mut server = Server::start().await;

    // No network call can be made for any of these, so a slow or unreachable
    // registry cannot make this test hang.
    for name in ["../../etc/passwd", "not a crate", ""] {
        let error = server
            .call_tool("get_crate_info", json!({"name": name}))
            .await
            .expect_err("a malformed name is rejected");

        assert_eq!(error["code"], -32602, "{name:?} should be an invalid-params error: {error}");
        assert_eq!(error["data"]["kind"], "invalid_crate_name", "{name:?}: {error}");
        assert_eq!(error["data"]["retryable"], false, "{name:?}: {error}");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn an_unparsable_version_requirement_is_rejected_before_any_request() {
    let mut server = Server::start().await;

    let error = server
        .call_tool("get_crate_dependencies", json!({"name": "serde", "version": "not a version"}))
        .await
        .expect_err("a malformed requirement is rejected");

    assert_eq!(error["code"], -32602, "{error}");
    assert_eq!(error["data"]["kind"], "invalid_version", "{error}");

    server.shutdown().await;
}

#[tokio::test]
async fn out_of_range_pagination_is_rejected() {
    let mut server = Server::start().await;

    for arguments in [
        json!({"query": "serde", "limit": 0}),
        json!({"query": "serde", "limit": 101}),
        json!({"query": "serde", "page": 0}),
    ] {
        let error = server
            .call_tool("search_crates", arguments.clone())
            .await
            .expect_err("out of range pagination is rejected");
        assert_eq!(error["code"], -32602, "{arguments}: {error}");
    }

    server.shutdown().await;
}

#[tokio::test]
async fn a_zero_readme_budget_is_rejected_rather_than_returning_a_marker() {
    let mut server = Server::start().await;

    let error = server
        .call_tool("get_crate_documentation", json!({"name": "serde", "max_readme_chars": 0}))
        .await
        .expect_err("a zero budget is rejected");
    assert_eq!(error["code"], -32602, "{error}");

    server.shutdown().await;
}

#[tokio::test]
async fn a_search_with_no_filter_at_all_is_rejected() {
    let mut server = Server::start().await;

    let error = server
        .call_tool("search_crates", json!({}))
        .await
        .expect_err("a search must say what to search for");
    assert_eq!(error["code"], -32602, "{error}");

    server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_tool_is_reported_rather_than_ignored() {
    let mut server = Server::start().await;

    let error = server
        .call_tool("delete_crate", json!({"name": "serde"}))
        .await
        .expect_err("an unknown tool is an error");
    assert!(error["message"].is_string(), "{error}");

    server.shutdown().await;
}
