//! Interaction jobs: describe, invoke, set_value, get_text, scroll, highlight.
//! Each function performs UIA calls only (runs on the worker thread) and
//! returns ready-to-send JSON.

use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UIScrollItemPattern, UIScrollPattern,
    UISelectionItemPattern, UITextPattern, UITogglePattern, UIValuePattern, UIWindowPattern,
};
use uiautomation::types::ScrollAmount;
use uiautomation::UIAutomation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, VIRTUAL_KEY,
};

use crate::error::AppError;
use crate::locator::control_type_name;
use crate::model::{DescribeResult, ElementRef};
use crate::uia::matcher;
use crate::uia::snapshot::PATTERN_PROPERTIES;

/// Upper bound for the SendInput fallback typing path (documented limit).
const KEYBOARD_INPUT_LIMIT: usize = 4000;

/// Scroll at most this many steps per call.
const MAX_SCROLL_STEPS: i32 = 20;

/// `describe`: full property dump of the referenced element.
pub fn describe(automation: &UIAutomation, reference: &ElementRef) -> Result<serde_json::Value, AppError> {
    let element = matcher::resolve_ref(automation, reference)?;
    let rect = element.get_bounding_rectangle().map_err(AppError::from)?;

    let mut patterns = Vec::new();
    for (property, label) in PATTERN_PROPERTIES {
        let available = element
            .get_property_value(*property)
            .ok()
            .and_then(|v| TryInto::<bool>::try_into(v).ok())
            .unwrap_or(false);
        if available {
            patterns.push(label.to_string());
        }
    }

    let result = DescribeResult {
        control_type: element
            .get_control_type()
            .map(control_type_name)
            .unwrap_or("Custom")
            .to_string(),
        localized_control_type: element.get_localized_control_type().unwrap_or_default(),
        name: element.get_name().unwrap_or_default(),
        automation_id: element.get_automation_id().unwrap_or_default(),
        class_name: element.get_classname().unwrap_or_default(),
        framework_id: element.get_framework_id().unwrap_or_default(),
        help_text: element.get_help_text().unwrap_or_default(),
        rect: [
            rect.get_left(),
            rect.get_top(),
            rect.get_right() - rect.get_left(),
            rect.get_bottom() - rect.get_top(),
        ],
        rid: element.get_runtime_id().unwrap_or_default(),
        process_id: element.get_process_id().unwrap_or_default(),
        native_window_handle: element
            .get_native_window_handle()
            .map(|h| -> isize { h.into() })
            .map(|v| v as i64)
            .unwrap_or_default(),
        is_enabled: element.is_enabled().unwrap_or_default(),
        has_keyboard_focus: element.has_keyboard_focus().unwrap_or_default(),
        is_keyboard_focusable: element.is_keyboard_focusable().unwrap_or_default(),
        is_offscreen: element.is_offscreen().unwrap_or_default(),
        is_password: element.is_password().unwrap_or_default(),
        is_content_element: element.is_content_element().unwrap_or_default(),
        is_control_element: element.is_control_element().unwrap_or_default(),
        patterns,
    };
    serde_json::to_value(result).map_err(|e| AppError::Internal(e.to_string()))
}

/// `invoke`: pattern-based actions on the referenced element.
pub fn invoke(
    automation: &UIAutomation,
    reference: &ElementRef,
    action: &str,
) -> Result<serde_json::Value, AppError> {
    let element = matcher::resolve_ref(automation, reference)?;

    // Bring the element on screen first when possible.
    if let Ok(scroll_item) = element.get_pattern::<UIScrollItemPattern>() {
        let _ = scroll_item.scroll_into_view();
    }

    match action {
        "invoke" => {
            let pattern: UIInvokePattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern(
                    "element does not support the Invoke pattern".into(),
                )
            })?;
            pattern.invoke().map_err(AppError::from)?;
        }
        "toggle" => {
            let pattern: UITogglePattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern("element does not support the Toggle pattern".into())
            })?;
            pattern.toggle().map_err(AppError::from)?;
        }
        "expand" => {
            let pattern: UIExpandCollapsePattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern(
                    "element does not support the ExpandCollapse pattern".into(),
                )
            })?;
            pattern.expand().map_err(AppError::from)?;
        }
        "collapse" => {
            let pattern: UIExpandCollapsePattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern(
                    "element does not support the ExpandCollapse pattern".into(),
                )
            })?;
            pattern.collapse().map_err(AppError::from)?;
        }
        "select" => {
            let pattern: UISelectionItemPattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern(
                    "element does not support the SelectionItem pattern".into(),
                )
            })?;
            pattern.select().map_err(AppError::from)?;
        }
        "scroll_into_view" => {
            let pattern: UIScrollItemPattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern(
                    "element does not support the ScrollItem pattern".into(),
                )
            })?;
            pattern.scroll_into_view().map_err(AppError::from)?;
        }
        "close" => {
            let pattern: UIWindowPattern = element.get_pattern().map_err(|_| {
                AppError::UnsupportedPattern(
                    "element does not support the Window pattern (not a window?)".into(),
                )
            })?;
            pattern.close().map_err(AppError::from)?;
        }
        other => {
            return Err(AppError::InvalidParams(format!(
                "unknown action '{other}' (expected invoke, toggle, expand, collapse, select, scroll_into_view or close)"
            )))
        }
    }

    Ok(serde_json::json!({
        "action": action,
        "result": "ok",
    }))
}

/// `set_value`: write text into an element.
///
/// * `method = "value"` — ValuePattern.SetValue only.
/// * `method = "keyboard"` — focus, Ctrl+A, type via SendInput (KEYEVENTF_UNICODE).
/// * `method = "auto"` (default) — try ValuePattern, fall back to keyboard.
pub fn set_value(
    automation: &UIAutomation,
    reference: &ElementRef,
    value: &str,
    method: &str,
) -> Result<serde_json::Value, AppError> {
    let element = matcher::resolve_ref(automation, reference)?;
    let text: String = value.chars().take(KEYBOARD_INPUT_LIMIT).collect();

    let use_keyboard = match method {
        "auto" | "value" | "keyboard" => method == "keyboard",
        other => {
            return Err(AppError::InvalidParams(format!(
                "unknown method '{other}' (expected auto, value or keyboard)"
            )))
        }
    };

    if !use_keyboard {
        match element.get_pattern::<UIValuePattern>() {
            Ok(pattern) => match pattern.set_value(&text) {
                Ok(()) => {
                    return Ok(serde_json::json!({
                        "method": "value",
                        "value": text,
                        "note": if text.len() < value.len() {
                            format!("input truncated to {KEYBOARD_INPUT_LIMIT} characters")
                        } else {
                            String::new()
                        },
                    }))
                }
                Err(e) => {
                    if method == "value" {
                        return Err(AppError::Uia(format!("ValuePattern.SetValue failed: {e}")));
                    }
                    // auto: fall through to the keyboard path.
                }
            },
            Err(_) => {
                if method == "value" {
                    return Err(AppError::UnsupportedPattern(
                        "element does not support the Value pattern".into(),
                    ));
                }
            }
        }
    }

    // Keyboard fallback.
    element.set_focus().map_err(AppError::from)?;
    select_all_and_type(&text)?;
    Ok(serde_json::json!({
        "method": "keyboard",
        "value": text,
    }))
}

/// Press Ctrl+A then type the text with unicode key events.
fn select_all_and_type(text: &str) -> Result<(), AppError> {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(4 + text.len() * 2);

    let key_event = |vk: u16, scan: u16, flags: u32| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    const VK_CONTROL: u16 = 0x11;
    const VK_A: u16 = 0x41;

    // Ctrl+A: select all existing content.
    inputs.push(key_event(VK_CONTROL, 0, 0));
    inputs.push(key_event(VK_A, 0, 0));
    inputs.push(key_event(VK_A, 0, KEYEVENTF_KEYUP.0));
    inputs.push(key_event(VK_CONTROL, 0, KEYEVENTF_KEYUP.0));

    // Unicode typing (BMP only; surrogate pairs are skipped).
    for c in text.chars() {
        let code = c as u32;
        if (0xD800..0xE000).contains(&code) || code > 0xFFFF {
            continue;
        }
        let scan = code as u16;
        inputs.push(key_event(0, scan, KEYEVENTF_UNICODE.0));
        inputs.push(key_event(0, scan, KEYEVENTF_UNICODE.0 | KEYEVENTF_KEYUP.0));
    }

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(AppError::Uia(format!(
            "SendInput sent {sent} of {} events",
            inputs.len()
        )));
    }
    Ok(())
}

/// `get_text`: read text via ValuePattern, falling back to TextPattern.
pub fn get_text(automation: &UIAutomation, reference: &ElementRef) -> Result<serde_json::Value, AppError> {
    let element = matcher::resolve_ref(automation, reference)?;

    if let Ok(pattern) = element.get_pattern::<UIValuePattern>()
        && let Ok(value) = pattern.get_value()
    {
        return Ok(serde_json::json!({ "text": value, "source": "ValuePattern" }));
    }
    if let Ok(pattern) = element.get_pattern::<UITextPattern>()
        && let Ok(range) = pattern.get_document_range()
        && let Ok(text) = range.get_text(-1)
    {
        return Ok(serde_json::json!({ "text": text, "source": "TextPattern" }));
    }
    Err(AppError::UnsupportedPattern(
        "element supports neither the Value nor the Text pattern".into(),
    ))
}

/// `scroll`: scroll the element or its nearest scrollable ancestor.
pub fn scroll(
    automation: &UIAutomation,
    reference: &ElementRef,
    direction: &str,
    amount: i32,
) -> Result<serde_json::Value, AppError> {
    let element = matcher::resolve_ref(automation, reference)?;
    let steps = amount.clamp(1, MAX_SCROLL_STEPS);

    // Find the element or nearest ancestor supporting the Scroll pattern.
    let mut current = element.clone();
    let mut scroll_pattern: Option<UIScrollPattern> = None;
    for _ in 0..50 {
        if let Ok(pattern) = current.get_pattern::<UIScrollPattern>() {
            scroll_pattern = Some(pattern);
            break;
        }
        let walker = automation.get_control_view_walker().map_err(AppError::from)?;
        match walker.get_parent(&current) {
            Ok(parent) => current = parent,
            Err(_) => break,
        }
    }
    let pattern = scroll_pattern.ok_or_else(|| {
        AppError::UnsupportedPattern(
            "neither the element nor its ancestors support the Scroll pattern".into(),
        )
    })?;

    let small = steps <= 1;
    let (horizontal, vertical) = match direction {
        "up" => (
            ScrollAmount::NoAmount,
            if small { ScrollAmount::SmallDecrement } else { ScrollAmount::LargeDecrement },
        ),
        "down" => (
            ScrollAmount::NoAmount,
            if small { ScrollAmount::SmallIncrement } else { ScrollAmount::LargeIncrement },
        ),
        "left" => (
            if small { ScrollAmount::SmallDecrement } else { ScrollAmount::LargeDecrement },
            ScrollAmount::NoAmount,
        ),
        "right" => (
            if small { ScrollAmount::SmallIncrement } else { ScrollAmount::LargeIncrement },
            ScrollAmount::NoAmount,
        ),
        other => {
            return Err(AppError::InvalidParams(format!(
                "unknown direction '{other}' (expected up, down, left or right)"
            )))
        }
    };

    for _ in 0..if small { steps } else { 1 } {
        pattern.scroll(horizontal, vertical).map_err(AppError::from)?;
    }

    Ok(serde_json::json!({
        "direction": direction,
        "steps": if small { steps } else { 1 },
        "large": !small,
        "result": "ok",
    }))
}

/// `highlight`: draw an overlay at the element's bounds; returns the rect.
pub fn highlight(
    automation: &UIAutomation,
    reference: &ElementRef,
    duration_ms: u64,
    color: &str,
) -> Result<serde_json::Value, AppError> {
    let element = matcher::resolve_ref(automation, reference)?;
    let rect = element.get_bounding_rectangle().map_err(AppError::from)?;
    let rect = [
        rect.get_left(),
        rect.get_top(),
        rect.get_right() - rect.get_left(),
        rect.get_bottom() - rect.get_top(),
    ];
    let rgb = crate::highlight::parse_color(color)?;
    crate::highlight::show_overlay(rect, duration_ms, rgb)?;
    Ok(serde_json::json!({
        "rect": rect,
        "duration_ms": duration_ms,
        "color": color,
    }))
}
