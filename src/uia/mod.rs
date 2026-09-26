//! UI Automation engine: root resolution, locator matching and element
//! reference re-resolution. All functions in this module run on the dedicated
//! UIA worker thread (see [`crate::uia::engine`]).

pub mod engine;
pub mod interact;
pub mod matcher;
pub mod snapshot;
