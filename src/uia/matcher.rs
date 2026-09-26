//! Root resolution and locator matching against live UIA elements.
//!
//! Everything here executes on the UIA worker thread and performs pure UIA
//! calls (no async, no JSON). JSON conversion happens one layer up.

use uiautomation::core::UICondition;
use uiautomation::types::{Point, PropertyConditionFlags, TreeScope, UIProperty};
use uiautomation::variants::Variant;
use uiautomation::{UIAutomation, UIElement};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::error::AppError;
use crate::locator::{self, Axis, Attr, Locator, Predicate, Step};
use crate::model::ElementRef;
use crate::model::RootSelector;

/// A root element plus how it was obtained, so callers can decide whether an
/// expensive walker-based verification is warranted.
pub struct ResolvedRoot {
    pub element: UIElement,
    /// True only when the root is the desktop (UIA tree root) element.
    ///
    /// Native `FindAll` works reliably from the desktop root, but some
    /// providers (WinUI/Win32 mixes such as the Windows 11 Notepad editor)
    /// serve a *shallower* view to `FindAll` rooted at an individual window
    /// element, while the tree walker sees the full subtree. Under window
    /// roots an empty native result therefore has to be double-checked with
    /// the walker before it can be trusted.
    pub is_desktop: bool,
}

/// Resolve the root element for snapshot / find / wait tools.
///
/// Precedence: `hwnd` > `pid` > `locator` > `root_mode`.
pub fn resolve_root(
    automation: &UIAutomation,
    selector: &RootSelector,
) -> Result<ResolvedRoot, AppError> {
    if let Some(hwnd) = selector.hwnd {
        if hwnd == 0 {
            return Err(AppError::InvalidParams("hwnd must not be 0".into()));
        }
        return automation
            .element_from_handle((hwnd as isize).into())
            .map(|element| ResolvedRoot { element, is_desktop: false })
            .map_err(AppError::from);
    }

    if let Some(pid) = selector.pid {
        if pid <= 0 {
            return Err(AppError::InvalidParams("pid must be positive".into()));
        }
        return root_for_pid(automation, pid as u32);
    }

    if let Some(loc) = &selector.locator {
        let locator = locator::parse(loc)?;
        let root = automation.get_root_element().map_err(AppError::from)?;
        // Desktop-rooted native search is the reliable fast path.
        let mut matches = find_matches(automation, &root, &locator, false)?;
        let element = matches.drain(..).next().ok_or_else(|| {
            AppError::ElementNotFound(format!("locator '{loc}' matched no element"))
        })?;
        return Ok(ResolvedRoot { element, is_desktop: false });
    }

    match selector.root_mode.as_deref().unwrap_or("desktop") {
        "desktop" => automation
            .get_root_element()
            .map(|element| ResolvedRoot { element, is_desktop: true })
            .map_err(AppError::from),
        "foreground" => {
            let hwnd = unsafe { GetForegroundWindow() };
            if hwnd.0.is_null() {
                return Err(AppError::ElementNotFound(
                    "no foreground window (the desktop may be locked)".into(),
                ));
            }
            automation
                .element_from_handle(hwnd.into())
                .map(|element| ResolvedRoot { element, is_desktop: false })
                .map_err(AppError::from)
        }
        "cursor" => {
            let mut pt = windows::Win32::Foundation::POINT { x: 0, y: 0 };
            let ok = unsafe { GetCursorPos(&mut pt) };
            if !ok.is_ok() {
                return Err(AppError::Uia("GetCursorPos failed".into()));
            }
            automation
                .element_from_point(Point::new(pt.x, pt.y))
                .map(|element| ResolvedRoot { element, is_desktop: false })
                .map_err(AppError::from)
        }
        other => Err(AppError::InvalidParams(format!(
            "unknown root_mode '{other}' (expected desktop, foreground or cursor)"
        ))),
    }
}

/// State passed through `EnumWindows` LPARAM.
struct EnumState {
    pid: u32,
    hwnd: Option<isize>,
}

unsafe extern "system" fn enum_windows_proc(
    hwnd: HWND,
    lparam: LPARAM,
) -> windows::core::BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut EnumState) };
    let mut window_pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut window_pid)) };
    if window_pid == state.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
        state.hwnd = Some(hwnd.0 as isize);
        return windows::core::BOOL(0); // FALSE stops enumeration
    }
    windows::core::BOOL(1)
}

/// Find the main window of a process and return it as a UIA element.
fn root_for_pid(automation: &UIAutomation, pid: u32) -> Result<ResolvedRoot, AppError> {
    let mut state = EnumState { pid, hwnd: None };
    unsafe {
        EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut state as *mut EnumState as isize),
        )
        .map_err(|e| AppError::Uia(format!("EnumWindows failed: {e}")))?;
    }
    let hwnd = state
        .hwnd
        .ok_or_else(|| AppError::ElementNotFound(format!("no visible main window for pid {pid}")))?;
    automation
        .element_from_handle(hwnd.into())
        .map(|element| ResolvedRoot { element, is_desktop: false })
        .map_err(AppError::from)
}

/// Find all elements under `root` matching `locator`, in document order.
///
/// `verify_with_walker`: when the native condition search returns nothing
/// under a non-desktop root, fall back to the walker-based matcher, because
/// some providers hide parts of their subtree from `FindAll` rooted at a
/// window element (see [`ResolvedRoot`]). The walker sees the same view as
/// `snapshot`, keeping find and snapshot consistent.
pub fn find_matches(
    automation: &UIAutomation,
    root: &UIElement,
    locator: &Locator,
    verify_with_walker: bool,
) -> Result<Vec<UIElement>, AppError> {
    if locator.0.is_empty() {
        return Err(AppError::InvalidParams("locator has no steps".into()));
    }
    if locator::is_native_fast_path(locator) {
        let results = find_matches_native(automation, root, &locator.0[0])?;
        if !results.is_empty() || !verify_with_walker {
            return Ok(results);
        }
        // Native search saw nothing under a window root: double-check with
        // the walker before reporting "not found".
    }
    let walker = automation.get_control_view_walker().map_err(AppError::from)?;
    let mut out = Vec::new();
    search_step(automation, &walker, root, 0, &locator.0, &mut out)?;
    Ok(out)
}

/// Fast path: single step with `=` predicates → compile a native UIA
/// condition and let the provider do the filtering.
fn find_matches_native(
    automation: &UIAutomation,
    root: &UIElement,
    step: &Step,
) -> Result<Vec<UIElement>, AppError> {
    let condition = build_condition(automation, step)?;
    let scope = match step.axis {
        Axis::Child => TreeScope::Children,
        Axis::Descendant => TreeScope::Descendants,
    };
    let mut results = root.find_all(scope, &condition).map_err(AppError::from)?;
    apply_nth(step, &mut results);
    Ok(results)
}

/// AND-combine property conditions for the control type and all attribute
/// predicates of a single step. `=` compiles to an equality condition
/// (case-insensitive, matching the documented `=` semantics), `*=` to a
/// substring condition, `!=` to the negation of an equality condition. An
/// `[n]` predicate is not translatable and is applied client-side afterwards.
fn build_condition(automation: &UIAutomation, step: &Step) -> Result<UICondition, AppError> {
    let mut condition = automation.create_true_condition().map_err(AppError::from)?;
    if let Some(ct) = step.control_type {
        let ct_condition = automation.create_property_condition(
            UIProperty::ControlType,
            Variant::from(ct as i32),
            Some(PropertyConditionFlags::None),
        )?;
        condition = automation
            .create_and_condition(condition, ct_condition)
            .map_err(AppError::from)?;
    }
    for predicate in &step.predicates {
        let eq_condition = match predicate {
            Predicate::Eq(attr, value) | Predicate::Ne(attr, value) => Some(
                automation.create_property_condition(
                    attr_property(*attr),
                    Variant::from(value.clone()),
                    Some(PropertyConditionFlags::IgnoreCase),
                )?,
            ),
            Predicate::Contains(attr, value) => Some(automation.create_property_condition(
                attr_property(*attr),
                Variant::from(value.clone()),
                Some(PropertyConditionFlags::All),
            )?),
            Predicate::Nth(_) => None,
        };
        if let Predicate::Ne(_, _) = predicate {
            // `!=` → NOT(equal), case-insensitive on both sides.
            let inner = eq_condition.expect("Ne always builds a condition");
            let not_condition = automation.create_not_condition(inner).map_err(AppError::from)?;
            condition = automation
                .create_and_condition(condition, not_condition)
                .map_err(AppError::from)?;
        } else if let Some(property_condition) = eq_condition {
            condition = automation
                .create_and_condition(condition, property_condition)
                .map_err(AppError::from)?;
        }
    }
    Ok(condition)
}

fn attr_property(attr: Attr) -> UIProperty {
    match attr {
        Attr::Name => UIProperty::Name,
        Attr::AutomationId => UIProperty::AutomationId,
        Attr::ClassName => UIProperty::ClassName,
        Attr::Value => UIProperty::ValueValue,
    }
}

/// Apply an `[n]` predicate (1-based) to an ordered match list.
fn apply_nth(step: &Step, matches: &mut Vec<UIElement>) {
    for predicate in &step.predicates {
        if let Predicate::Nth(n) = predicate {
            if *n > matches.len() {
                matches.clear();
            } else {
                let chosen = matches[*n - 1].clone();
                matches.clear();
                matches.push(chosen);
            }
            return;
        }
    }
}

/// Recursive walker-based matcher for general locators.
///
/// For step `i`, enumerate candidates under the current context according to
/// the step's axis (direct children or all descendants, in document order),
/// keep those matching the control type and attribute predicates, narrow to
/// the nth if an `[n]` predicate is present, then recurse into each survivor
/// for step `i + 1`.
fn search_step(
    automation: &UIAutomation,
    walker: &uiautomation::UITreeWalker,
    context: &UIElement,
    step_idx: usize,
    steps: &[Step],
    out: &mut Vec<UIElement>,
) -> Result<(), AppError> {
    let step = &steps[step_idx];
    let candidates = enumerate_candidates(walker, context, step.axis)?;
    let mut matched: Vec<UIElement> = Vec::new();
    for candidate in candidates {
        if element_matches_step(automation, &candidate, step)? {
            matched.push(candidate);
        }
    }
    apply_nth(step, &mut matched);

    if step_idx + 1 == steps.len() {
        out.extend(matched);
    } else {
        for element in &matched {
            search_step(automation, walker, element, step_idx + 1, steps, out)?;
        }
    }
    Ok(())
}

/// Enumerate candidate elements according to the axis, in document order.
fn enumerate_candidates(
    walker: &uiautomation::UITreeWalker,
    context: &UIElement,
    axis: Axis,
) -> Result<Vec<UIElement>, AppError> {
    let mut result = Vec::new();
    match axis {
        Axis::Child => {
            let mut current = match walker.get_first_child(context) {
                Ok(c) => c,
                Err(_) => return Ok(result),
            };
            loop {
                result.push(current.clone());
                match walker.get_next_sibling(&current) {
                    Ok(next) => current = next,
                    Err(_) => break,
                }
            }
        }
        Axis::Descendant => {
            // Iterative DFS pre-order using the control-view walker. The
            // context element itself is tagged so it is never emitted: `//x`
            // selects descendants, not the context.
            let mut stack: Vec<(UIElement, bool)> = vec![(context.clone(), false)];
            let mut context_seen = false;
            while let Some((element, expanded)) = stack.pop() {
                if expanded {
                    if !context_seen {
                        context_seen = true; // skip the context element itself
                    } else {
                        result.push(element);
                    }
                } else {
                    stack.push((element.clone(), true));
                    // Push children in reverse order so they pop in order.
                    let mut children = Vec::new();
                    let mut current = match walker.get_first_child(&element) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    loop {
                        children.push(current.clone());
                        match walker.get_next_sibling(&current) {
                            Ok(next) => current = next,
                            Err(_) => break,
                        }
                    }
                    for child in children.into_iter().rev() {
                        stack.push((child, false));
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Check control type and attribute predicates (nth excluded) of a step.
fn element_matches_step(
    automation: &UIAutomation,
    element: &UIElement,
    step: &Step,
) -> Result<bool, AppError> {
    if let Some(expected) = step.control_type {
        let actual = element.get_control_type().map_err(AppError::from)?;
        if actual != expected {
            return Ok(false);
        }
    }
    for predicate in &step.predicates {
        let ok = match predicate {
            Predicate::Eq(attr, expected) => {
                locator::compare::eq(expected, &attr_value(automation, element, *attr)?)
            }
            Predicate::Ne(attr, expected) => {
                locator::compare::ne(expected, &attr_value(automation, element, *attr)?)
            }
            Predicate::Contains(attr, expected) => {
                locator::compare::contains(expected, &attr_value(automation, element, *attr)?)
            }
            Predicate::Nth(_) => true, // handled separately
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Fetch the attribute value used by predicates. Missing values are treated
/// as empty strings; `Value` reads the Value pattern.
fn attr_value(
    automation: &UIAutomation,
    element: &UIElement,
    attr: Attr,
) -> Result<String, AppError> {
    let _ = automation;
    match attr {
        Attr::Name => Ok(element.get_name().unwrap_or_default()),
        Attr::AutomationId => Ok(element.get_automation_id().unwrap_or_default()),
        Attr::ClassName => Ok(element.get_classname().unwrap_or_default()),
        Attr::Value => match element.get_pattern::<uiautomation::patterns::UIValuePattern>() {
            Ok(p) => Ok(p.get_value().unwrap_or_default()),
            Err(_) => Ok(String::new()),
        },
    }
}

/// Re-resolve an element reference: re-run its stored query, then pick the
/// match whose runtime id equals the stored one; fall back to a unique match.
pub fn resolve_ref(automation: &UIAutomation, reference: &ElementRef) -> Result<UIElement, AppError> {
    let locator = locator::parse(&reference.query)?;
    let root = automation.get_root_element().map_err(AppError::from)?;
    // Desktop-rooted native search is reliable; no walker verification.
    let matches = find_matches(automation, &root, &locator, false)?;

    // Primary: identity via runtime id.
    for candidate in &matches {
        if let Ok(rid) = candidate.get_runtime_id()
            && !rid.is_empty()
            && rid == reference.rid
        {
            return Ok(candidate.clone());
        }
    }
    // Fallback: exactly one match → unambiguous.
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => Err(AppError::StaleElement(format!(
            "the query '{}' no longer matches any element",
            reference.query
        ))),
        n => Err(AppError::StaleElement(format!(
            "the query '{}' now matches {n} elements and none has the recorded runtime id",
            reference.query
        ))),
    }
}
