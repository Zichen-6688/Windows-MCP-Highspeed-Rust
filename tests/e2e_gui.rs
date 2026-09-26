//! GUI end-to-end tests against a real Notepad instance. Ignored by default;
//! run once manually with `cargo test --test e2e_gui -- --ignored`.
//! Notepad is always cleaned up (killed) on failure.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde_json::{Value, json};

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
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn server binary");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel::<Value>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(line.trim())
                    && value.get("id").is_some()
                {
                    let _ = tx.send(value);
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
        let mut text = json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })
        .to_string();
        text.push('\n');
        self.stdin.write_all(text.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        let mut text = json!({
            "jsonrpc": "2.0", "method": method, "params": params,
        })
        .to_string();
        text.push('\n');
        self.stdin.write_all(text.as_bytes()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn expect_response(&mut self, id: u64, timeout: Duration) -> Value {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for response {id}");
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

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.send("tools/call", json!({ "name": name, "arguments": arguments }));
        let response = self.expect_response(id, Duration::from_secs(30));
        response
            .get("result")
            .unwrap_or_else(|| panic!("tool '{name}' failed at protocol level: {response}"))
            .clone()
    }

    fn init(&mut self) {
        let id = self.send(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "e2e-gui", "version": "0.1.0" }
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

/// Kills notepad.exe; used as the failure-cleanup guard.
fn kill_notepad() {
    let _ = Command::new("taskkill")
        .args(["/F", "/IM", "notepad.exe"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn temp_file() -> PathBuf {
    let path = std::env::temp_dir().join("windows-mcp-highspeed-e2e.txt");
    // Pre-create the file: for a *missing* path Notepad shows a modal
    // "file not found — create it?" dialog (localized), and while that modal
    // is up the editor element is absent from the UIA tree, which makes the
    // workflow test flaky. An existing empty file opens silently.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::write(&path, b"\n");
    path
}

fn parse_result(result: &Value) -> Value {
    assert_ne!(
        result["isError"], json!(true),
        "tool returned error: {}",
        result["content"][0]["text"]
    );
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap())
        .expect("tool result must be JSON")
}

#[test]
#[ignore]
fn notepad_full_workflow() {
    kill_notepad();
    let file = temp_file();

    // Launch Notepad with a concrete file to skip first-run UI. The window
    // can take a few seconds to appear (and on Windows 11 the editor may run
    // under a different pid than the launcher), so locate it by name and use
    // its window handle for rooting.
    let mut notepad = Command::new("notepad.exe")
        .arg(&file)
        .spawn()
        .expect("launch notepad.exe");

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = McpClient::spawn();
        client.init();

        // 1. Wait for the Notepad window (class name is locale-independent,
        //    unlike the localized window title), then resolve its handle.
        let window = parse_result(&client.call_tool(
            "wait_for_element",
            json!({ "locator": "//Window[@ClassName='Notepad']", "timeout_ms": 20000 }),
        ));
        let window_ref = window["matches"][0]["ref"].as_str().unwrap().to_string();
        let info = parse_result(&client.call_tool("describe", json!({ "ref": window_ref })));
        let hwnd = info["native_window_handle"]
            .as_i64()
            .expect("describe must include native_window_handle");
        eprintln!("notepad window: name={:?} hwnd={hwnd}", info["name"]);

        // 2. Snapshot the Notepad window subtree.
        let started = std::time::Instant::now();
        let snapshot = parse_result(&client.call_tool(
            "snapshot",
            json!({ "hwnd": hwnd, "depth": 3, "max_children": 48 }),
        ));
        let snapshot_ms = started.elapsed().as_millis();
        assert_eq!(snapshot["control_type"], "Window", "root must be the Notepad window: {}", snapshot["name"]);
        eprintln!("snapshot(hwnd, depth 3) took {snapshot_ms} ms");

        // 3. Find the text-editing element, waiting briefly for each candidate
        //    locator: the editor materializes asynchronously after the window
        //    appears. New Notepad builds expose the RichEdit as a Document,
        //    older ones as an Edit; fall back to the window class.
        let mut edit_ref: Option<String> = None;
        for locator in ["//Document", "//Edit", "//*[@ClassName='RichEditD2DPT']"] {
            let found = parse_result(&client.call_tool(
                "wait_for_element",
                json!({ "locator": locator, "hwnd": hwnd, "timeout_ms": 8000 }),
            ));
            if let Some(first) = found["matches"].as_array().and_then(|a| a.first()) {
                eprintln!("found text element via {locator}: name={:?}", first["name"]);
                edit_ref = Some(first["ref"].as_str().unwrap().to_string());
                break;
            }
        }
        let edit_ref = edit_ref.expect("no Edit/Document element found in Notepad");

        // 4. Write text, read it back.
        let text = "Hello from windows-mcp-highspeed!";
        let set = parse_result(&client.call_tool(
            "set_value",
            json!({ "ref": edit_ref, "value": text }),
        ));
        eprintln!("set_value used method {:?}", set["method"]);
        std::thread::sleep(Duration::from_millis(400));
        let got = parse_result(&client.call_tool("get_text", json!({ "ref": edit_ref })));
        assert_eq!(got["text"], text, "read-back text must match");

        // 5. describe + highlight the edit element.
        let _describe = parse_result(&client.call_tool("describe", json!({ "ref": edit_ref })));
        let _highlight = parse_result(&client.call_tool(
            "highlight",
            json!({ "ref": edit_ref, "duration_ms": 300, "color": "cyan" }),
        ));
        std::thread::sleep(Duration::from_millis(500));

        // 6. Close via WindowPattern Close.
        parse_result(&client.call_tool(
            "invoke",
            json!({ "ref": window_ref, "action": "close" }),
        ));
    }));

    // Kill Notepad before waiting on it, so a mid-test failure cannot block
    // for minutes on the still-open window.
    if outcome.is_err() {
        kill_notepad();
    }
    let exited = notepad.wait().ok().map(|s| s.success()).unwrap_or(false);
    if !exited {
        kill_notepad();
    }
    let _ = std::fs::remove_file(&file);
    if let Err(payload) = outcome {
        std::panic::resume_unwind(payload);
    }
}
