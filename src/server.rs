//! MCP server: exposes the 9 UIA tools to LLM clients over stdio.

use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Implementation, ServerCapabilities, ServerConfig,
};
use rmcp::schemars::JsonSchema;
use rmcp::serde::Deserialize;
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

use crate::error::AppError;
use crate::model::{ElementRef, FindResult, RootSelector};
use crate::uia::engine::{UiaRuntime, DEFAULT_JOB_TIMEOUT};
use crate::uia::{interact, matcher, snapshot};

/// One-shot timeout for simple (non-polling) UIA jobs.
const JOB_TIMEOUT: Duration = DEFAULT_JOB_TIMEOUT;

/// Wrap a tool outcome into a `CallToolResult`.
fn outcome(result: Result<serde_json::Value, AppError>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(value) => Ok(CallToolResult::success(vec![ContentBlock::text(
            value.to_string(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
            e.to_json_body().to_string(),
        )])),
    }
}

/// Parameters for `snapshot`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SnapshotParams {
    /// Where to start the snapshot: "desktop" (default), "foreground" (the
    /// active window) or "cursor" (the element under the mouse pointer).
    #[schemars(description = "Root selector: desktop | foreground | cursor")]
    pub root_mode: String,
    /// Optional window handle (decimal) to use as the root. Highest precedence.
    #[schemars(description = "Optional window handle (decimal) used as the snapshot root")]
    pub hwnd: Option<i64>,
    /// Optional process id: snapshot starts at that process's main window.
    #[schemars(description = "Optional process id: snapshot starts at its main window")]
    pub pid: Option<i64>,
    /// Optional locator selecting the root element (overrides root_mode).
    #[schemars(description = "Optional locator selecting the snapshot root element")]
    pub locator: Option<String>,
    /// How many levels of the subtree to include (1 = the root element only).
    #[schemars(description = "Subtree depth to include (1 = root only)")]
    pub depth: u32,
    /// Maximum children serialized per node. Extra children are replaced by a
    /// `{"truncated": true}` marker node.
    #[schemars(description = "Maximum children serialized per node")]
    pub max_children: usize,
    /// Comma-separated control types to keep (e.g. "Button,Edit,Text").
    #[schemars(description = "Comma-separated control types to keep, e.g. Button,Edit")]
    pub filter: Option<String>,
    /// Keep only elements whose name contains this substring (case-insensitive).
    #[schemars(description = "Keep only elements whose name contains this substring")]
    pub name_contains: Option<String>,
    /// Include offscreen elements (hidden from view) in the output.
    #[schemars(description = "Include offscreen elements")]
    pub include_offscreen: bool,
    /// Value/Text content is truncated to this many characters per node.
    #[schemars(description = "Truncate element values to this many characters")]
    pub max_value_length: usize,
}

impl Default for SnapshotParams {
    fn default() -> Self {
        SnapshotParams {
            root_mode: "desktop".into(),
            hwnd: None,
            pid: None,
            locator: None,
            depth: 8,
            max_children: 64,
            filter: None,
            name_contains: None,
            include_offscreen: false,
            max_value_length: 128,
        }
    }
}

/// Shared root-selector parameters for `find_elements` / `wait_for_element`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RootParams {
    /// Where to search: "desktop" (default), "foreground" or "cursor".
    #[schemars(description = "Root selector: desktop | foreground | cursor")]
    pub root_mode: String,
    /// Optional window handle (decimal) to search under. Highest precedence.
    #[schemars(description = "Optional window handle (decimal) to search under")]
    pub hwnd: Option<i64>,
    /// Optional process id: search under that process's main window.
    #[schemars(description = "Optional process id: search under its main window")]
    pub pid: Option<i64>,
    /// Optional locator selecting the root element to search under.
    #[schemars(description = "Optional locator selecting the root element to search under")]
    pub root_locator: Option<String>,
}

impl Default for RootParams {
    fn default() -> Self {
        RootParams {
            root_mode: "desktop".into(),
            hwnd: None,
            pid: None,
            root_locator: None,
        }
    }
}

/// Parameters for `find_elements`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FindParams {
    /// The locator to search for, e.g. `//Window[@Name='Notepad']//Edit`.
    #[schemars(description = "Locator expression, e.g. //Window[@Name='Notepad']//Edit")]
    pub locator: String,
    #[serde(flatten)]
    pub root: RootParams,
    /// Maximum number of matches to return.
    #[schemars(description = "Maximum number of matches to return")]
    pub max_results: usize,
    /// If > 0, poll until at least one match appears or this many ms elapse.
    #[schemars(description = "If > 0, poll until a match appears or this many ms elapse")]
    pub timeout_ms: u64,
}

impl Default for FindParams {
    fn default() -> Self {
        FindParams {
            locator: String::new(),
            root: RootParams::default(),
            max_results: 32,
            timeout_ms: 0,
        }
    }
}

/// Parameters for `wait_for_element`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct WaitParams {
    /// The locator to wait for.
    #[schemars(description = "Locator expression to wait for")]
    pub locator: String,
    #[serde(flatten)]
    pub root: RootParams,
    /// Maximum time to wait in milliseconds.
    #[schemars(description = "Maximum time to wait (ms)")]
    pub timeout_ms: u64,
    /// Delay between polls in milliseconds.
    #[schemars(description = "Delay between polls (ms)")]
    pub poll_ms: u64,
}

impl Default for WaitParams {
    fn default() -> Self {
        WaitParams {
            locator: String::new(),
            root: RootParams::default(),
            timeout_ms: 10_000,
            poll_ms: 200,
        }
    }
}

/// Parameters for `describe`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeParams {
    /// Element reference string returned by `find_elements`.
    #[schemars(description = "Element reference string from find_elements")]
    pub r#ref: String,
}

/// Parameters for `invoke`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct InvokeParams {
    /// Element reference string returned by `find_elements`.
    #[schemars(description = "Element reference string from find_elements")]
    pub r#ref: String,
    /// Action to perform.
    #[schemars(description = "Action: invoke | toggle | expand | collapse | select | scroll_into_view | close")]
    pub action: String,
}

impl Default for InvokeParams {
    fn default() -> Self {
        InvokeParams {
            r#ref: String::new(),
            action: "invoke".into(),
        }
    }
}

/// Parameters for `set_value`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SetValueParams {
    /// Element reference string returned by `find_elements`.
    #[schemars(description = "Element reference string from find_elements")]
    pub r#ref: String,
    /// The text to write.
    #[schemars(description = "Text to write")]
    pub value: String,
    /// How to write it.
    #[schemars(description = "Method: auto | value | keyboard")]
    pub method: String,
}

impl Default for SetValueParams {
    fn default() -> Self {
        SetValueParams {
            r#ref: String::new(),
            value: String::new(),
            method: "auto".into(),
        }
    }
}

/// Parameters for `get_text`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetTextParams {
    /// Element reference string returned by `find_elements`.
    #[schemars(description = "Element reference string from find_elements")]
    pub r#ref: String,
}

/// Parameters for `scroll`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ScrollParams {
    /// Element reference string returned by `find_elements`.
    #[schemars(description = "Element reference string from find_elements")]
    pub r#ref: String,
    /// Scroll direction.
    #[schemars(description = "Direction: up | down | left | right")]
    pub direction: String,
    /// How much to scroll: 1 = small step, >= 2 = large page-sized steps.
    #[schemars(description = "How much to scroll (1 = small step, >=2 = large steps)")]
    pub amount: i32,
}

impl Default for ScrollParams {
    fn default() -> Self {
        ScrollParams {
            r#ref: String::new(),
            direction: "down".into(),
            amount: 3,
        }
    }
}

/// Parameters for `highlight`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(default)]
pub struct HighlightParams {
    /// Element reference string returned by `find_elements`.
    #[schemars(description = "Element reference string from find_elements")]
    pub r#ref: String,
    /// How long to show the overlay.
    #[schemars(description = "How long to show the overlay (ms)")]
    pub duration_ms: u64,
    /// Border color.
    #[schemars(description = "Border color: red | green | blue | yellow | cyan | magenta | white | black | orange | #RRGGBB")]
    pub color: String,
}

impl Default for HighlightParams {
    fn default() -> Self {
        HighlightParams {
            r#ref: String::new(),
            duration_ms: 1500,
            color: "red".into(),
        }
    }
}

/// The MCP server: holds the UIA runtime and exposes the tools.
pub struct UiaServer {
    runtime: UiaRuntime,
}

impl UiaServer {
    pub fn new() -> Result<Self, AppError> {
        Ok(UiaServer {
            runtime: UiaRuntime::start()?,
        })
    }
}

#[tool_router]
impl UiaServer {
    /// Capture a JSON snapshot of the UI Automation tree.
    #[tool(
        description = "Capture a JSON snapshot of the UI Automation tree starting at the desktop, the foreground window, the element under the cursor, a window handle, a process id, or a root locator. Each node has control_type, name, automation_id, class, rect, rid, patterns and (optionally) value; children are nested. Root precedence: hwnd > pid > locator > root_mode. Use depth, max_children, filter and name_contains to keep the output small. This is the primary 'see the screen' tool: fast (batched cache requests) and semantic, not pixels."
    )]
    async fn snapshot(
        &self,
        Parameters(params): Parameters<SnapshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let selector = RootSelector {
            root_mode: Some(params.root_mode),
            hwnd: params.hwnd,
            pid: params.pid,
            locator: params.locator,
        };
        let filter = params.filter.map(|f| {
            f.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        });
        let opts = snapshot::SnapshotOptions {
            depth: params.depth.max(1),
            max_children: params.max_children.max(1),
            filter,
            name_contains: params.name_contains,
            include_offscreen: params.include_offscreen,
            max_value_length: params.max_value_length,
        };
        let result = self
            .runtime
            .call(JOB_TIMEOUT, move |automation| {
                let root = matcher::resolve_root(automation, &selector)?;
                snapshot::snapshot(automation, &root.element, &opts)
            })
            .await;
        outcome(result)
    }

    /// Find elements matching a locator.
    #[tool(
        description = "Find UI elements matching a locator expression (e.g. //Window[@Name='Notepad']//Edit). Returns an array of matches with an opaque `ref` string, control_type, name, automation_id, rect and 1-based index. Pass `ref` to describe, invoke, set_value, get_text, scroll and highlight. Simple locators run through a native UIA condition (fast); general locators use tree matching. With timeout_ms > 0 the search polls until a match appears or the timeout elapses. An empty result is not an error."
    )]
    async fn find_elements(
        &self,
        Parameters(params): Parameters<FindParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.locator.trim().is_empty() {
            return outcome(Err(AppError::InvalidParams(
                "locator must not be empty".into(),
            )));
        }
        let selector = RootSelector {
            root_mode: Some(params.root.root_mode),
            hwnd: params.root.hwnd,
            pid: params.root.pid,
            locator: params.root.root_locator,
        };
        let max_results = params.max_results.max(1);
        let poll = Duration::from_millis(200);
        let deadline = Instant::now() + Duration::from_millis(params.timeout_ms);

        loop {
            let locator = params.locator.clone();
            let selector = selector.clone();
            let result = self
                .runtime
                .call(JOB_TIMEOUT, move |automation| {
                    find_job(automation, &selector, &locator, max_results)
                })
                .await;

            match result {
                Ok(value) => {
                    let found = value.as_array().map(|a| !a.is_empty()).unwrap_or(false);
                    if found || params.timeout_ms == 0 || Instant::now() >= deadline {
                        return outcome(Ok(value));
                    }
                }
                Err(e) => return outcome(Err(e)),
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Describe a referenced element in full.
    #[tool(
        description = "Return the full property dump of an element referenced by `ref` (from find_elements): core properties, bounding rect, runtime id, framework id, help text and the list of supported UI Automation patterns. Use it to understand what an element is and which operations (invoke, set_value, scroll, ...) are available."
    )]
    async fn describe(
        &self,
        Parameters(params): Parameters<DescribeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = async {
            let reference = ElementRef::parse(&params.r#ref)?;
            self.runtime
                .call(JOB_TIMEOUT, move |automation| {
                    interact::describe(automation, &reference)
                })
                .await
        }
        .await;
        outcome(result)
    }

    /// Invoke a pattern action on a referenced element.
    #[tool(
        description = "Perform a pattern-based action on the element referenced by `ref`. Actions: invoke (click/press via Invoke pattern), toggle (check boxes), expand/collapse (menus, trees), select (tabs, list items), scroll_into_view, close (windows, via WindowPattern.Close). ScrollIntoView is applied automatically before the action when supported. The element must support the matching pattern (see describe)."
    )]
    async fn invoke(
        &self,
        Parameters(params): Parameters<InvokeParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = async {
            let reference = ElementRef::parse(&params.r#ref)?;
            self.runtime
                .call(JOB_TIMEOUT, move |automation| {
                    interact::invoke(automation, &reference, &params.action)
                })
                .await
        }
        .await;
        outcome(result)
    }

    /// Set the value of a referenced element.
    #[tool(
        description = "Write text into the element referenced by `ref` (edit box, combo box, document, ...). method 'auto' (default) tries ValuePattern.SetValue and falls back to keyboard input (focus + Ctrl+A + unicode typing via SendInput) when the Value pattern is unavailable or rejects the value; 'value' uses only SetValue; 'keyboard' uses only keyboard input. Inputs longer than 4000 characters are truncated."
    )]
    async fn set_value(
        &self,
        Parameters(params): Parameters<SetValueParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = async {
            let reference = ElementRef::parse(&params.r#ref)?;
            self.runtime
                .call(JOB_TIMEOUT, move |automation| {
                    interact::set_value(automation, &reference, &params.value, &params.method)
                })
                .await
        }
        .await;
        outcome(result)
    }

    /// Read text from a referenced element.
    #[tool(
        description = "Read the plain text content of the element referenced by `ref`. Uses ValuePattern.Value when available, falling back to TextPattern's document range (for documents and larger text areas). Returns the text and which pattern produced it."
    )]
    async fn get_text(
        &self,
        Parameters(params): Parameters<GetTextParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = async {
            let reference = ElementRef::parse(&params.r#ref)?;
            self.runtime
                .call(JOB_TIMEOUT, move |automation| {
                    interact::get_text(automation, &reference)
                })
                .await
        }
        .await;
        outcome(result)
    }

    /// Scroll an element or its scrollable ancestor.
    #[tool(
        description = "Scroll the element referenced by `ref` (or its nearest scrollable ancestor) in the given direction. amount 1 performs a small step (like an arrow key); amount >= 2 performs large page-sized steps (repeated `amount` times, capped at 20). Directions: up | down | left | right."
    )]
    async fn scroll(
        &self,
        Parameters(params): Parameters<ScrollParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = async {
            let reference = ElementRef::parse(&params.r#ref)?;
            self.runtime
                .call(JOB_TIMEOUT, move |automation| {
                    interact::scroll(automation, &reference, &params.direction, params.amount)
                })
                .await
        }
        .await;
        outcome(result)
    }

    /// Wait for an element matching a locator to appear.
    #[tool(
        description = "Poll find_elements until at least one element matches `locator` or `timeout_ms` elapse. Returns `{ \"matches\": [...], \"elapsed_ms\": n }` (matches entries carry the same fields as find_elements); if the timeout expires, an empty matches array with a Timeout error body is returned (not a protocol error). Use it after actions that change the UI asynchronously (opening dialogs, loading content). Polling-only (no event subscription)."
    )]
    async fn wait_for_element(
        &self,
        Parameters(params): Parameters<WaitParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if params.locator.trim().is_empty() {
            return outcome(Err(AppError::InvalidParams(
                "locator must not be empty".into(),
            )));
        }
        let selector = RootSelector {
            root_mode: Some(params.root.root_mode),
            hwnd: params.root.hwnd,
            pid: params.root.pid,
            locator: params.root.root_locator,
        };
        let max_results = 32usize;
        let poll = Duration::from_millis(params.poll_ms.max(10));
        let timeout = Duration::from_millis(params.timeout_ms);
        let started = Instant::now();

        loop {
            let locator = params.locator.clone();
            let selector = selector.clone();
            let result = self
                .runtime
                .call(JOB_TIMEOUT, move |automation| {
                    find_job(automation, &selector, &locator, max_results)
                })
                .await;

            let elapsed = started.elapsed();
            match result {
                Ok(value) => {
                    let found = value.as_array().map(|a| !a.is_empty()).unwrap_or(false);
                    if found {
                        // `find_job` returns a bare array; wrap it so the
                        // payload is `{ "matches": [...], "elapsed_ms": n }`
                        // (the shape this tool documents).
                        return outcome(Ok(serde_json::json!({
                            "matches": value,
                            "elapsed_ms": elapsed.as_millis() as u64,
                        })));
                    }
                    if elapsed >= timeout {
                        return outcome(Ok(serde_json::json!({
                            "matches": [],
                            "elapsed_ms": elapsed.as_millis() as u64,
                            "error": {
                                "code": "Timeout",
                                "message": format!("no element matched '{}' within {} ms", params.locator, params.timeout_ms),
                                "hint": "Increase timeout_ms, or verify the locator with snapshot/find_elements."
                            }
                        })));
                    }
                }
                Err(e) => return outcome(Err(e)),
            }
            tokio::time::sleep(poll).await;
        }
    }

    /// Highlight an element on screen for human debugging.
    #[tool(
        description = "Draw a colored click-through overlay border around the element referenced by `ref` for `duration_ms` milliseconds (default 1500) so a human can see exactly what the model is looking at. color is a name (red, green, blue, yellow, cyan, magenta, white, black, orange) or #RRGGBB. Returns the highlighted rect. Does not steal focus."
    )]
    async fn highlight(
        &self,
        Parameters(params): Parameters<HighlightParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let result = async {
            let reference = ElementRef::parse(&params.r#ref)?;
            self.runtime
                .call(JOB_TIMEOUT, move |automation| {
                    interact::highlight(automation, &reference, params.duration_ms, &params.color)
                })
                .await
        }
        .await;
        outcome(result)
    }
}

/// The find body shared by `find_elements` and `wait_for_element`.
fn find_job(
    automation: &uiautomation::UIAutomation,
    selector: &RootSelector,
    locator: &str,
    max_results: usize,
) -> Result<serde_json::Value, AppError> {
    let locator_parsed = crate::locator::parse(locator)?;
    let root = matcher::resolve_root(automation, selector)?;
    let matches = matcher::find_matches(automation, &root.element, &locator_parsed, !root.is_desktop)?;
    let mut results = Vec::with_capacity(matches.len().min(max_results));
    for (i, element) in matches.into_iter().take(max_results).enumerate() {
        let rect = element.get_bounding_rectangle().map_err(AppError::from)?;
        let rid = element.get_runtime_id().unwrap_or_default();
        let reference = ElementRef {
            rid,
            query: locator.to_string(),
        };
        results.push(FindResult {
            r#ref: reference.to_ref_string(),
            control_type: element
                .get_control_type()
                .map(crate::locator::control_type_name)
                .unwrap_or("Custom")
                .to_string(),
            name: element.get_name().unwrap_or_default(),
            automation_id: element.get_automation_id().unwrap_or_default(),
            rect: [
                rect.get_left(),
                rect.get_top(),
                rect.get_right() - rect.get_left(),
                rect.get_bottom() - rect.get_top(),
            ],
            index: i + 1,
        });
    }
    serde_json::to_value(results).map_err(|e| AppError::Internal(e.to_string()))
}

#[tool_handler]
impl ServerHandler for UiaServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "windows-mcp-highspeed",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Windows desktop automation over the UI Accessibility tree (no screenshots). \
                 Workflow: call `snapshot` to see the UI, `find_elements` with a locator to get \
                 element refs, then act on refs with `describe`, `invoke`, `set_value`, `get_text`, \
                 `scroll`, `highlight`, or wait for UI changes with `wait_for_element`.",
            )
    }
}
