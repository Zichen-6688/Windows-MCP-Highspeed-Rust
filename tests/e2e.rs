//! End-to-end MCP protocol test: speaks JSON-RPC 2.0 over the server's stdio
//! (newline-delimited framing per the MCP stdio spec) and exercises the core
//! tool surface. No GUI applications are launched here.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

const TOOL_NAMES: [&str; 9] = [
    "snapshot",
    "find_elements",
    "describe",
    "invoke",
    "set_value",
    "get_text",
    "scroll",
    "wait_for_element",
    "highlight",
];

/// Minimal newline-delimited JSON-RPC client over the server's stdio.
struct McpClient {
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
    child: Child,
}

impl McpClient {
    fn spawn() -> Self {
        let exe = env!("CARGO_BIN_EXE_windows-mcp-highspeed");
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn server binary");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = channel::<Value>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                    // Skip notifications; the test only awaits responses.
                    if value.get("id").is_some() {
                        let _ = tx.send(value);
                    }
                }
            }
        });

        McpClient {
            stdin,
            rx,
            next_id: 0,
            child,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut text = message.to_string();
        text.push('\n');
        self.stdin.write_all(text.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut text = message.to_string();
        text.push('\n');
        self.stdin.write_all(text.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    /// Read the next response, failing after `timeout`.
    fn expect_response(&mut self, id: u64, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for response to request {id}");
            }
            match self.rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(message) => {
                    if message.get("id") == Some(&json!(id)) {
                        return message;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("server closed stdout while waiting for response {id}");
                }
            }
        }
    }

    fn call_tool(&mut self, name: &str, arguments: Value, timeout: Duration) -> Value {
        let id = self.send(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        );
        let response = self.expect_response(id, timeout);
        response
            .get("result")
            .expect("tools/call should return a result, not a protocol error")
            .clone()
    }

    fn init(&mut self) {
        let id = self.send(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "e2e-test", "version": "0.1.0" }
            }),
        );
        self.expect_response(id, Duration::from_secs(15));
        self.notify("notifications/initialized", json!({}));
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_protocol_lifecycle_and_tools() {
    let mut client = McpClient::spawn();
    let timeout = Duration::from_secs(30);

    // initialize
    let id = client.send(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "e2e-test", "version": "0.1.0" }
        }),
    );
    let response = client.expect_response(id, timeout);
    let result = response.get("result").expect("initialize result");
    assert_eq!(
        result["serverInfo"]["name"],
        "windows-mcp-highspeed",
        "server info name mismatch: {result}"
    );
    let protocol = result["protocolVersion"].as_str().unwrap_or("");
    assert!(
        !protocol.is_empty(),
        "initialize result should include a protocolVersion: {result}"
    );

    // initialized notification
    client.notify("notifications/initialized", json!({}));

    // tools/list — exactly the 9 expected tools
    let id = client.send("tools/list", json!({}));
    let response = client.expect_response(id, timeout);
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list should return an array")
        .clone();
    assert_eq!(tools.len(), TOOL_NAMES.len(), "expected 9 tools: {tools:#?}");
    for name in TOOL_NAMES {
        assert!(
            tools.iter().any(|t| t["name"] == name),
            "tool '{name}' missing from tools/list"
        );
    }

    // snapshot desktop at depth 1 → valid JSON with a children array
    let result = client.call_tool(
        "snapshot",
        json!({ "root_mode": "desktop", "depth": 1 }),
        timeout,
    );
    assert_ne!(
        result["isError"], json!(true),
        "snapshot should succeed: {result}"
    );
    let content = &result["content"][0]["text"];
    let snapshot: Value = serde_json::from_str(
        content.as_str().expect("snapshot text content"),
    )
    .expect("snapshot result must be valid JSON");
    assert_eq!(snapshot["control_type"], "Pane", "desktop root is a Pane");
    assert!(
        snapshot["children"].is_array(),
        "snapshot must contain a children array: {snapshot}"
    );

    // find_elements with a bogus locator → empty array or tool-level error,
    // never a protocol error or a crash.
    let result = client.call_tool(
        "find_elements",
        json!({ "locator": "//NoSuchControlType[@Name='zzz']", "max_results": 5 }),
        timeout,
    );
    if result.get("isError") == Some(&json!(true)) {
        let body: Value = serde_json::from_str(
            result["content"][0]["text"].as_str().unwrap(),
        )
        .unwrap();
        assert!(body["error"]["code"].is_string());
    } else {
        let matches = result["content"][0]["text"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .expect("find_elements result must be a JSON array");
        assert!(
            matches.as_array().map(|a| a.is_empty()).unwrap_or(false),
            "bogus locator should yield an empty array: {matches}"
        );
    }

    // find_elements with a valid locator that matches nothing → empty array, no error.
    let result = client.call_tool(
        "find_elements",
        json!({ "locator": "//Button[@Name='NoSuchButtonXYZ123']", "max_results": 5 }),
        timeout,
    );
    assert_ne!(result["isError"], json!(true), "no match is not an error: {result}");
    let matches: Value = serde_json::from_str(
        result["content"][0]["text"].as_str().unwrap(),
    )
    .unwrap();
    assert_eq!(matches.as_array().unwrap().len(), 0);

    // describe with a malformed ref → tool-level InvalidParams error body.
    let result = client.call_tool("describe", json!({ "ref": "not-json" }), timeout);
    assert_eq!(result["isError"], json!(true), "malformed ref must error");
    let body: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["error"]["code"], "InvalidParams");

    // highlight with a bogus ref → tool-level error, server stays alive.
    let result = client.call_tool(
        "highlight",
        json!({ "ref": "{\"rid\":[1],\"query\":\"//Nope[@Name='x']\"}" }),
        timeout,
    );
    assert_eq!(result["isError"], json!(true));
}

#[test]
fn server_stays_alive_after_bad_jsonrpc() {
    let mut client = McpClient::spawn();
    client.init();

    // Unknown method → defined JSON-RPC error, server must keep running.
    let id = client.send("nonexistent/method", json!({}));
    let response = client.expect_response(id, Duration::from_secs(15));
    assert!(response.get("error").is_some(), "unknown method must error");

    // Server still answers afterwards.
    let id = client.send("tools/list", json!({}));
    let response = client.expect_response(id, Duration::from_secs(15));
    assert!(
        response["result"]["tools"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "server must still respond to tools/list after the unknown method"
    );
}
