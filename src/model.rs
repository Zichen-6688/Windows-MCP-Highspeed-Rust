//! Serde models shared between tools: element references, snapshot nodes and
//! tool result payloads.

use serde::{Deserialize, Serialize};

/// A stable JSON-string reference to a UI Automation element.
///
/// Serialized form (used as the `ref` string parameter of the tools):
/// `{"rid":[42,65536,3],"query":"//Window[@Name='Notepad']//Edit"}`
///
/// * `rid` — the UIA runtime id captured when the element was found. Used for
///   identity comparison when the reference is re-resolved.
/// * `query` — the locator that originally found the element. Re-running it is
///   the actual re-resolution mechanism (UIA has no ElementFromRuntimeId).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ElementRef {
    pub rid: Vec<i32>,
    pub query: String,
}

impl ElementRef {
    /// Parse a `ref` string passed by a client.
    pub fn parse(s: &str) -> Result<Self, crate::error::AppError> {
        serde_json::from_str(s).map_err(|e| {
            crate::error::AppError::InvalidParams(format!(
                "`ref` is not a valid element reference JSON: {e}. Expected {{\"rid\":[...],\"query\":\"...\"}}"
            ))
        })
    }

    /// Serialize to the compact JSON string form.
    pub fn to_ref_string(&self) -> String {
        // Errors are impossible for this simple struct; fall back to a best-effort.
        serde_json::to_string(self).unwrap_or_else(|_| "{\"rid\":[],\"query\":\"\"}".to_string())
    }
}

/// One node of the `snapshot` output tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotNode {
    pub control_type: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub automation_id: String,
    #[serde(skip_serializing_if = "String::is_empty", rename = "class")]
    pub class_name: String,
    pub rect: [i32; 4],
    pub rid: Vec<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offscreen: Option<bool>,
    /// Child nodes (values are `SnapshotNode`-shaped JSON; a truncation is
    /// marked with `{"truncated": true}`).
    pub children: Vec<serde_json::Value>,
}

/// One entry of the `find_elements` output array.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindResult {
    /// Element reference string (pass to `ref`-typed params).
    pub r#ref: String,
    pub control_type: String,
    pub name: String,
    pub automation_id: String,
    /// `[left, top, width, height]` in screen coordinates.
    pub rect: [i32; 4],
    /// 1-based index of the match in document order.
    pub index: usize,
}

/// `describe` output: full property dump of a single element.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DescribeResult {
    pub control_type: String,
    pub localized_control_type: String,
    pub name: String,
    pub automation_id: String,
    #[serde(rename = "class")]
    pub class_name: String,
    pub framework_id: String,
    pub help_text: String,
    pub rect: [i32; 4],
    pub rid: Vec<i32>,
    pub process_id: u32,
    pub native_window_handle: i64,
    pub is_enabled: bool,
    pub has_keyboard_focus: bool,
    pub is_keyboard_focusable: bool,
    pub is_offscreen: bool,
    pub is_password: bool,
    pub is_content_element: bool,
    pub is_control_element: bool,
    pub patterns: Vec<String>,
}

/// Options controlling root resolution (shared by snapshot / find / wait tools).
#[derive(Debug, Clone, Default)]
pub struct RootSelector {
    pub root_mode: Option<String>,
    pub hwnd: Option<i64>,
    pub pid: Option<i64>,
    pub locator: Option<String>,
}
