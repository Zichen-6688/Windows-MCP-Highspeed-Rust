//! Depth-limited BFS snapshot of a UIA subtree, batched through a cache
//! request so each element costs roughly one cross-process round trip.

use serde_json::json;
use uiautomation::core::{UICacheRequest, UICondition};
use uiautomation::types::{TreeScope, UIProperty};
use uiautomation::{UIAutomation, UIElement};

use crate::error::AppError;
use crate::locator::{compare, control_type_name};
use crate::model::SnapshotNode;

/// Options for a snapshot (mirrors the `snapshot` tool parameters).
#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub depth: u32,
    pub max_children: usize,
    pub filter: Option<Vec<String>>,
    pub name_contains: Option<String>,
    pub include_offscreen: bool,
    pub max_value_length: usize,
}

/// Pattern availability properties cached per element, in output order.
pub(crate) const PATTERN_PROPERTIES: &[(UIProperty, &str)] = &[
    (UIProperty::IsInvokePatternAvailable, "Invoke"),
    (UIProperty::IsValuePatternAvailable, "Value"),
    (UIProperty::IsTextPatternAvailable, "Text"),
    (UIProperty::IsTogglePatternAvailable, "Toggle"),
    (UIProperty::IsExpandCollapsePatternAvailable, "ExpandCollapse"),
    (UIProperty::IsSelectionItemPatternAvailable, "SelectionItem"),
    (UIProperty::IsSelectionPatternAvailable, "Selection"),
    (UIProperty::IsScrollPatternAvailable, "Scroll"),
    (UIProperty::IsScrollItemPatternAvailable, "ScrollItem"),
    (UIProperty::IsWindowPatternAvailable, "Window"),
    (UIProperty::IsRangeValuePatternAvailable, "RangeValue"),
    (UIProperty::IsGridPatternAvailable, "Grid"),
    (UIProperty::IsGridItemPatternAvailable, "GridItem"),
    (UIProperty::IsLegacyIAccessiblePatternAvailable, "LegacyIAccessible"),
];

/// Build the cache request shared by every snapshot: all node properties plus
/// pattern-availability flags, so reads are served from the cache.
fn build_cache_request(automation: &UIAutomation) -> Result<UICacheRequest, AppError> {
    let cache = automation.create_cache_request().map_err(AppError::from)?;
    let properties = [
        UIProperty::Name,
        UIProperty::ControlType,
        UIProperty::AutomationId,
        UIProperty::ClassName,
        UIProperty::BoundingRectangle,
        UIProperty::IsOffscreen,
        UIProperty::FrameworkId,
        UIProperty::ValueValue,
    ];
    for property in properties {
        cache.add_property(property).map_err(AppError::from)?;
    }
    for (property, _) in PATTERN_PROPERTIES {
        cache.add_property(*property).map_err(AppError::from)?;
    }
    Ok(cache)
}

/// Take a snapshot of the subtree rooted at `root`.
pub fn snapshot(
    automation: &UIAutomation,
    root: &UIElement,
    opts: &SnapshotOptions,
) -> Result<serde_json::Value, AppError> {
    let cache = build_cache_request(automation)?;
    // Refresh the root itself through the cache so cached getters work on it.
    let root = root.build_updated_cache(&cache).map_err(AppError::from)?;
    let true_condition = automation.create_true_condition().map_err(AppError::from)?;
    let node = snapshot_node(automation, &cache, &true_condition, &root, opts.depth, opts)?;
    serde_json::to_value(node).map_err(|e| AppError::Internal(e.to_string()))
}

fn cached_bool(element: &UIElement, property: UIProperty) -> bool {
    element
        .get_cached_property_value(property)
        .ok()
        .and_then(|v| TryInto::<bool>::try_into(v).ok())
        .unwrap_or(false)
}

fn snapshot_node(
    automation: &UIAutomation,
    cache: &UICacheRequest,
    true_condition: &UICondition,
    element: &UIElement,
    depth: u32,
    opts: &SnapshotOptions,
) -> Result<SnapshotNode, AppError> {
    let control_type = element
        .get_cached_control_type()
        .map_err(AppError::from)?;
    let name = element.get_cached_name().map_err(AppError::from)?;
    let automation_id = element.get_cached_automation_id().map_err(AppError::from)?;
    let class_name = element.get_cached_classname().map_err(AppError::from)?;
    let rect = element
        .get_cached_bounding_rectangle()
        .map_err(AppError::from)?;
    let offscreen = element.is_cached_offscreen().map_err(AppError::from)?;
    let rid = element.get_runtime_id().unwrap_or_default();

    let mut patterns = Vec::new();
    for (property, label) in PATTERN_PROPERTIES {
        if cached_bool(element, *property) {
            patterns.push(label.to_string());
        }
    }

    // `value` only when the element supports Value/Text.
    let mut value = None;
    if patterns.iter().any(|p| p == "Value" || p == "Text")
        && let Ok(variant) = element.get_cached_property_value(UIProperty::ValueValue)
        && let Ok(text) = TryInto::<String>::try_into(variant)
    {
        let text = text.chars().take(opts.max_value_length).collect::<String>();
        value = Some(text);
    }

    let mut node = SnapshotNode {
        control_type: control_type_name(control_type).to_string(),
        name,
        automation_id,
        class_name,
        rect: [
            rect.get_left(),
            rect.get_top(),
            rect.get_right() - rect.get_left(),
            rect.get_bottom() - rect.get_top(),
        ],
        rid,
        patterns,
        value,
        offscreen: Some(offscreen),
        children: Vec::new(),
    };

    if depth > 1 {
        snapshot_children(automation, cache, true_condition, element, depth, opts, &mut node)?;
    }
    Ok(node)
}

fn snapshot_children(
    automation: &UIAutomation,
    cache: &UICacheRequest,
    true_condition: &UICondition,
    element: &UIElement,
    depth: u32,
    opts: &SnapshotOptions,
    node: &mut SnapshotNode,
) -> Result<(), AppError> {
    // One batched cross-process call fetches all children with cached values.
    let children = element
        .find_all_build_cache(TreeScope::Children, true_condition, cache)
        .map_err(AppError::from)?;

    let mut kept = 0usize;
    let mut truncated = false;
    for child in children {
        if kept >= opts.max_children {
            truncated = true;
            break;
        }
        if !include_child(&child, opts)? {
            continue;
        }
        let child_node = snapshot_node(automation, cache, true_condition, &child, depth - 1, opts)?;
        node.children.push(
            serde_json::to_value(child_node).map_err(|e| AppError::Internal(e.to_string()))?,
        );
        kept += 1;
    }
    if truncated {
        node.children.push(json!({ "truncated": true }));
    }
    Ok(())
}

/// Apply the `filter` / `name_contains` / `include_offscreen` rules to a child.
fn include_child(child: &UIElement, opts: &SnapshotOptions) -> Result<bool, AppError> {
    if !opts.include_offscreen && child.is_cached_offscreen().map_err(AppError::from)? {
        return Ok(false);
    }
    if let Some(filter) = &opts.filter {
        let control_type = child.get_cached_control_type().map_err(AppError::from)?;
        let name = control_type_name(control_type);
        if !filter.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            return Ok(false);
        }
    }
    if let Some(needle) = &opts.name_contains {
        let name = child.get_cached_name().map_err(AppError::from)?;
        if !compare::contains(needle, &name) {
            return Ok(false);
        }
    }
    Ok(true)
}
