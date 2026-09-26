//! Locator grammar: parser + tree matcher over UI Automation elements.
//!
//! Grammar (see README for the full reference):
//! ```text
//! locator   := ("//" | "/") step (("/" | "//") step)*
//! step      := ("*" | ControlType) ("[" predicate "]")?
//! predicate := "@" attr ("=" | "!=" | "*=") quoted-string | integer (nth index)
//! attr      := Name | AutomationId | ClassName | Value
//! ```
//!
//! * `//` selects among all descendants, `/` among direct children.
//! * `=` exact match (case-sensitive, falling back to case-insensitive exact),
//!   `!=` negated case-insensitive comparison, `*= ` case-insensitive contains.
//! * An integer predicate selects the n-th (1-based) match of that step.

use uiautomation::types::ControlType;

use crate::error::AppError;

/// Axis between two locator steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// `/` — direct children only.
    Child,
    /// `//` — all descendants.
    Descendant,
}

/// Element attribute usable in a predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attr {
    Name,
    AutomationId,
    ClassName,
    Value,
}

/// One predicate inside `[...]`.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// `@attr = "value"` — exact (with case-insensitive fallback).
    Eq(Attr, String),
    /// `@attr != "value"` — case-insensitive inequality.
    Ne(Attr, String),
    /// `@attr *= "value"` — case-insensitive contains.
    Contains(Attr, String),
    /// `[n]` — n-th (1-based) match of the step.
    Nth(usize),
}

/// One step of a parsed locator.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub axis: Axis,
    /// `None` = wildcard `*`.
    pub control_type: Option<ControlType>,
    pub predicates: Vec<Predicate>,
}

/// A parsed locator: an ordered list of steps.
#[derive(Debug, Clone, PartialEq)]
pub struct Locator(pub Vec<Step>);

/// Map a locator control-type name to the UIA `ControlType`.
pub fn control_type_from_name(name: &str) -> Option<ControlType> {
    Some(match name {
        "AppBar" => ControlType::AppBar,
        "Button" => ControlType::Button,
        "Calendar" => ControlType::Calendar,
        "CheckBox" => ControlType::CheckBox,
        "ComboBox" => ControlType::ComboBox,
        "Custom" => ControlType::Custom,
        "DataGrid" => ControlType::DataGrid,
        "DataItem" => ControlType::DataItem,
        "Document" => ControlType::Document,
        "Edit" => ControlType::Edit,
        "Group" => ControlType::Group,
        "Header" => ControlType::Header,
        "HeaderItem" => ControlType::HeaderItem,
        "Hyperlink" => ControlType::Hyperlink,
        "Image" => ControlType::Image,
        "List" => ControlType::List,
        "ListItem" => ControlType::ListItem,
        "Menu" => ControlType::Menu,
        "MenuBar" => ControlType::MenuBar,
        "MenuItem" => ControlType::MenuItem,
        "Pane" => ControlType::Pane,
        "ProgressBar" => ControlType::ProgressBar,
        "RadioButton" => ControlType::RadioButton,
        "ScrollBar" => ControlType::ScrollBar,
        "SemanticZoom" => ControlType::SemanticZoom,
        "Separator" => ControlType::Separator,
        "Slider" => ControlType::Slider,
        "Spinner" => ControlType::Spinner,
        "SplitButton" => ControlType::SplitButton,
        "StatusBar" => ControlType::StatusBar,
        "Tab" => ControlType::Tab,
        "TabItem" => ControlType::TabItem,
        "Table" => ControlType::Table,
        "Text" => ControlType::Text,
        "Thumb" => ControlType::Thumb,
        "TitleBar" => ControlType::TitleBar,
        "ToolBar" => ControlType::ToolBar,
        "ToolTip" => ControlType::ToolTip,
        "Tree" => ControlType::Tree,
        "TreeItem" => ControlType::TreeItem,
        "Window" => ControlType::Window,
        _ => return None,
    })
}

/// Map a UIA `ControlType` back to its locator name.
pub fn control_type_name(ct: ControlType) -> &'static str {
    match ct {
        ControlType::AppBar => "AppBar",
        ControlType::Button => "Button",
        ControlType::Calendar => "Calendar",
        ControlType::CheckBox => "CheckBox",
        ControlType::ComboBox => "ComboBox",
        ControlType::Custom => "Custom",
        ControlType::DataGrid => "DataGrid",
        ControlType::DataItem => "DataItem",
        ControlType::Document => "Document",
        ControlType::Edit => "Edit",
        ControlType::Group => "Group",
        ControlType::Header => "Header",
        ControlType::HeaderItem => "HeaderItem",
        ControlType::Hyperlink => "Hyperlink",
        ControlType::Image => "Image",
        ControlType::List => "List",
        ControlType::ListItem => "ListItem",
        ControlType::Menu => "Menu",
        ControlType::MenuBar => "MenuBar",
        ControlType::MenuItem => "MenuItem",
        ControlType::Pane => "Pane",
        ControlType::ProgressBar => "ProgressBar",
        ControlType::RadioButton => "RadioButton",
        ControlType::ScrollBar => "ScrollBar",
        ControlType::SemanticZoom => "SemanticZoom",
        ControlType::Separator => "Separator",
        ControlType::Slider => "Slider",
        ControlType::Spinner => "Spinner",
        ControlType::SplitButton => "SplitButton",
        ControlType::StatusBar => "StatusBar",
        ControlType::Tab => "Tab",
        ControlType::TabItem => "TabItem",
        ControlType::Table => "Table",
        ControlType::Text => "Text",
        ControlType::Thumb => "Thumb",
        ControlType::TitleBar => "TitleBar",
        ControlType::ToolBar => "ToolBar",
        ControlType::ToolTip => "ToolTip",
        ControlType::Tree => "Tree",
        ControlType::TreeItem => "TreeItem",
        ControlType::Window => "Window",
    }
}

struct Parser<'a> {
    chars: std::str::Chars<'a>,
    /// One-character pushback buffer.
    peeked: Option<char>,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Parser {
            chars: input.chars(),
            peeked: None,
        }
    }

    fn next(&mut self) -> Option<char> {
        match self.peeked.take() {
            Some(c) => Some(c),
            None => self.chars.next(),
        }
    }

    fn peek(&mut self) -> Option<char> {
        if self.peeked.is_none() {
            self.peeked = self.chars.next();
        }
        self.peeked
    }

    fn expect(&mut self, c: char) -> Result<(), AppError> {
        match self.next() {
            Some(got) if got == c => Ok(()),
            got => Err(AppError::InvalidParams(format!(
                "locator: expected '{c}' but found {}",
                got.map(|c| format!("'{c}'")).unwrap_or_else(|| "end of input".into())
            ))),
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.next();
        }
    }

    /// Parse an identifier: letters, digits, underscore.
    fn ident(&mut self) -> Result<String, AppError> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                s.push(c);
                self.next();
            } else {
                break;
            }
        }
        if s.is_empty() {
            Err(AppError::InvalidParams(
                "locator: expected an identifier".into(),
            ))
        } else {
            Ok(s)
        }
    }

    /// Parse a quoted string. Supports both single and double quotes; no
    /// escape sequences (backslash is kept literal).
    fn quoted(&mut self) -> Result<String, AppError> {
        let quote = self.next().ok_or_else(|| {
            AppError::InvalidParams("locator: expected a quoted string".into())
        })?;
        if quote != '\'' && quote != '"' {
            return Err(AppError::InvalidParams(format!(
                "locator: expected quote character but found '{quote}'"
            )));
        }
        let mut s = String::new();
        loop {
            match self.next() {
                Some(c) if c == quote => return Ok(s),
                Some(c) => s.push(c),
                None => {
                    return Err(AppError::InvalidParams(
                        "locator: unterminated quoted string".into(),
                    ))
                }
            }
        }
    }

    fn predicate(&mut self) -> Result<Predicate, AppError> {
        self.skip_ws();
        if self.peek() == Some('@') {
            self.next();
            let attr = match self.ident()?.as_str() {
                "Name" => Attr::Name,
                "AutomationId" => Attr::AutomationId,
                "ClassName" => Attr::ClassName,
                "Value" => Attr::Value,
                other => {
                    return Err(AppError::InvalidParams(format!(
                        "locator: unknown attribute '{other}' (expected Name, AutomationId, ClassName or Value)"
                    )))
                }
            };
            self.skip_ws();
            let op = match self.next() {
                Some('!') => {
                    self.expect('=')?;
                    "!="
                }
                Some('=') => "=",
                Some('*') => {
                    self.expect('=')?;
                    "*="
                }
                other => {
                    return Err(AppError::InvalidParams(format!(
                        "locator: expected '=', '!=' or '*=' but found {}",
                        other.map(|c| format!("'{c}'")).unwrap_or_else(|| "end of input".into())
                    )))
                }
            };
            self.skip_ws();
            let value = self.quoted()?;
            Ok(match op {
                "=" => Predicate::Eq(attr, value),
                "!=" => Predicate::Ne(attr, value),
                _ => Predicate::Contains(attr, value),
            })
        } else if matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            let mut n = String::new();
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                n.push(self.next().unwrap());
            }
            let idx: usize = n.parse().map_err(|_| {
                AppError::InvalidParams(format!("locator: invalid index '{n}'"))
            })?;
            if idx == 0 {
                return Err(AppError::InvalidParams(
                    "locator: nth-match index is 1-based and must be >= 1".into(),
                ));
            }
            Ok(Predicate::Nth(idx))
        } else {
            Err(AppError::InvalidParams(
                "locator: expected '@attribute' or an index inside '[]'".into(),
            ))
        }
    }

    fn step(&mut self, axis: Axis) -> Result<Step, AppError> {
        self.skip_ws();
        let control_type = if self.peek() == Some('*') {
            self.next();
            None
        } else {
            let name = self.ident()?;
            Some(control_type_from_name(&name).ok_or_else(|| {
                AppError::InvalidParams(format!(
                    "locator: unknown control type '{name}' (see README for the supported list)"
                ))
            })?)
        };
        let mut predicates = Vec::new();
        self.skip_ws();
        // Multiple `[...]` groups are allowed: //Button[@Name='a'][2].
        while self.peek() == Some('[') {
            self.next();
            predicates.push(self.predicate()?);
            self.skip_ws();
            while self.peek() == Some(',') {
                self.next();
                predicates.push(self.predicate()?);
            }
            self.skip_ws();
            self.expect(']')?;
            self.skip_ws();
        }
        Ok(Step {
            axis,
            control_type,
            predicates,
        })
    }
}

/// Parse a locator string into a [`Locator`].
pub fn parse(input: &str) -> Result<Locator, AppError> {
    if input.trim().is_empty() {
        return Err(AppError::InvalidParams("locator must not be empty".into()));
    }
    let mut p = Parser::new(input);
    // First axis.
    p.expect('/')?;
    let first_axis = if p.peek() == Some('/') {
        p.next();
        Axis::Descendant
    } else {
        Axis::Child
    };
    let mut steps = vec![p.step(first_axis)?];
    loop {
        p.skip_ws();
        match p.peek() {
            Some('/') => {
                p.next();
                if p.peek() == Some('/') {
                    p.next();
                    steps.push(p.step(Axis::Descendant)?);
                } else {
                    steps.push(p.step(Axis::Child)?);
                }
            }
            None => break,
            Some(c) => {
                return Err(AppError::InvalidParams(format!(
                    "locator: unexpected character '{c}' (expected '/' or end of input)"
                )))
            }
        }
    }
    Ok(Locator(steps))
}

/// Does this locator translate to a single native UIA condition (fast path)?
///
/// True for any single-step locator: the control type and every predicate
/// (`=`, `!=`, `*=`) compile to native UIA conditions; only the nth-match
/// index is applied client-side. The provider filters the tree without a
/// client-side walk, which matters on huge desktops.
pub fn is_native_fast_path(loc: &Locator) -> bool {
    loc.0.len() == 1
}

/// String comparison helpers implementing the predicate semantics. Matching is
/// case-insensitive for `!=` and `*=`, exact for `=` with a case-insensitive
/// exact fallback (Windows app names vary in case).
pub mod compare {
    /// `=` — case-sensitive exact, else case-insensitive exact.
    pub fn eq(expected: &str, actual: &str) -> bool {
        expected == actual || expected.eq_ignore_ascii_case(actual)
    }

    /// `!=` — case-insensitive inequality.
    pub fn ne(expected: &str, actual: &str) -> bool {
        !expected.eq_ignore_ascii_case(actual)
    }

    /// `*=` — case-insensitive contains.
    pub fn contains(needle: &str, haystack: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let needle = needle.to_lowercase();
        let haystack = haystack.to_lowercase();
        haystack.contains(&needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_descendant() {
        let loc = parse("//Window").unwrap();
        assert_eq!(loc.0.len(), 1);
        assert_eq!(loc.0[0].axis, Axis::Descendant);
        assert_eq!(loc.0[0].control_type, Some(ControlType::Window));
        assert!(loc.0[0].predicates.is_empty());
    }

    #[test]
    fn parse_absolute_child_path() {
        let loc = parse("/Window/Edit").unwrap();
        assert_eq!(loc.0.len(), 2);
        assert_eq!(loc.0[0].axis, Axis::Child);
        assert_eq!(loc.0[1].axis, Axis::Child);
        assert_eq!(loc.0[1].control_type, Some(ControlType::Edit));
    }

    #[test]
    fn parse_mixed_axes() {
        let loc = parse("//Window//Button/Edit").unwrap();
        assert_eq!(
            loc.0.iter().map(|s| s.axis).collect::<Vec<_>>(),
            vec![Axis::Descendant, Axis::Descendant, Axis::Child]
        );
    }

    #[test]
    fn parse_predicates_all_ops() {
        let loc = parse("//Window[@Name='Notepad']//Edit[@AutomationId!=\"x\", @ClassName*='Ed']").unwrap();
        assert_eq!(loc.0.len(), 2);
        assert_eq!(
            loc.0[0].predicates,
            vec![Predicate::Eq(Attr::Name, "Notepad".into())]
        );
        assert_eq!(
            loc.0[1].predicates,
            vec![
                Predicate::Ne(Attr::AutomationId, "x".into()),
                Predicate::Contains(Attr::ClassName, "Ed".into()),
            ]
        );
    }

    #[test]
    fn parse_double_quoted_values() {
        let loc = parse("//Button[@Name=\"OK\"]").unwrap();
        assert_eq!(
            loc.0[0].predicates,
            vec![Predicate::Eq(Attr::Name, "OK".into())]
        );
    }

    #[test]
    fn parse_nth_index() {
        let loc = parse("//Button[2]").unwrap();
        assert_eq!(loc.0[0].predicates, vec![Predicate::Nth(2)]);
    }

    #[test]
    fn parse_wildcard() {
        let loc = parse("//*[@Name='x']").unwrap();
        assert_eq!(loc.0[0].control_type, None);
    }

    #[test]
    fn parse_multiple_predicate_groups() {
        let loc = parse("//Button[@AutomationId='a'][@ClassName='b'][2]").unwrap();
        assert_eq!(loc.0[0].control_type, Some(ControlType::Button));
        assert_eq!(
            loc.0[0].predicates,
            vec![
                Predicate::Eq(Attr::AutomationId, "a".into()),
                Predicate::Eq(Attr::ClassName, "b".into()),
                Predicate::Nth(2),
            ]
        );
    }

    #[test]
    fn parse_full_example() {
        let loc = parse("//Window[@Name='Notepad']//Edit").unwrap();
        assert_eq!(
            loc,
            Locator(vec![
                Step {
                    axis: Axis::Descendant,
                    control_type: Some(ControlType::Window),
                    predicates: vec![Predicate::Eq(Attr::Name, "Notepad".into())],
                },
                Step {
                    axis: Axis::Descendant,
                    control_type: Some(ControlType::Edit),
                    predicates: vec![],
                },
            ])
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        for bad in [
            "",
            "   ",
            "Window",
            "//",
            "//[",
            "//Window[@Unknown='x']",
            "//Window[@Name='x",
            "//Window[0]",
            "//Windowx",
            "//Window[@Name~'x']",
            "//Window extra",
        ] {
            assert!(
                parse(bad).is_err(),
                "expected parse failure for {bad:?}"
            );
        }
    }

    #[test]
    fn fast_path_detection() {
        // Every single-step locator runs natively (control type + any
        // combination of =, !=, *= predicates compile to UIA conditions);
        // only the nth index is applied client-side.
        assert!(is_native_fast_path(&parse("//Edit").unwrap()));
        assert!(is_native_fast_path(&parse("//Edit[@Name='x']").unwrap()));
        assert!(is_native_fast_path(&parse("//Edit[@Name!='x']").unwrap()));
        assert!(is_native_fast_path(&parse("//Edit[@Name*='x']").unwrap()));
        assert!(is_native_fast_path(&parse("//Edit[@Value='x']").unwrap()));
        assert!(is_native_fast_path(&parse("//Button[@AutomationId='a'][@ClassName='b']").unwrap()));
        assert!(is_native_fast_path(&parse("//Edit[1]").unwrap()));
        // Multi-step locators need the walker matcher.
        assert!(!is_native_fast_path(&parse("//Window//Edit").unwrap()));
        assert!(!is_native_fast_path(&parse("/Window/Edit[@Name='x']").unwrap()));
    }

    #[test]
    fn compare_semantics() {
        assert!(compare::eq("OK", "OK"));
        assert!(compare::eq("Notepad", "notepad")); // case-insensitive fallback
        assert!(!compare::eq("OK", "Cancel"));
        assert!(compare::ne("a", "B"));
        assert!(!compare::ne("A", "a"));
        assert!(compare::contains("pad", "Notepad"));
        assert!(compare::contains("NOTE", "notepad"));
        assert!(!compare::contains("xyz", "Notepad"));
        assert!(compare::contains("", "anything"));
    }

    #[test]
    fn control_type_round_trip() {
        for name in [
            "Window", "Button", "Edit", "Text", "ComboBox", "List", "ListItem", "Tree",
            "TreeItem", "Menu", "MenuItem", "Tab", "TabItem", "CheckBox", "RadioButton",
            "Hyperlink", "Document", "Pane", "Group", "ToolBar", "StatusBar", "DataGrid",
            "DataItem", "ScrollBar", "ProgressBar", "Slider", "Spinner", "ToolTip", "Custom",
        ] {
            let ct = control_type_from_name(name).unwrap();
            assert_eq!(control_type_name(ct), name);
        }
        assert_eq!(control_type_from_name("windows"), None);
    }
}
