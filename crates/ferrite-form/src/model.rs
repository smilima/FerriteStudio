//! The in-memory form the designer edits.

use std::collections::BTreeMap;
use std::fmt;

use crate::catalog::{kind_spec, ArgSpec, KindSpec, PropType};

/// Designer grid in logical pixels; moves and resizes snap to it.
pub const GRID: i32 = 8;

/// Minimum size a control can be resized to.
pub const MIN_SIZE: i32 = 8;

/// A form: one Slint `Window` component with absolutely placed controls,
/// the same model as a VCL form.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    /// Component name, e.g. `MainForm`; becomes the Rust type name.
    pub name: String,
    pub title: String,
    pub width: i32,
    pub height: i32,
    /// Controls in z-order: later entries draw on top.
    pub controls: Vec<Control>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub name: String,
    /// Slint element name, a key into [`crate::KINDS`].
    pub kind: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Property values in inspector form (see [`PropType`]); only values
    /// that differ from Slint's default are stored.
    pub props: BTreeMap<String, String>,
    /// Slint callback name to Rust handler function name.
    pub handlers: BTreeMap<String, String>,
}

/// A connected event: which handler a control's callback calls.
#[derive(Clone, Debug, PartialEq)]
pub struct HandlerRef<'a> {
    pub control: &'a str,
    pub callback: &'static str,
    pub handler: &'a str,
    pub args: &'static [ArgSpec],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormError {
    UnknownKind(String),
    UnknownProperty { kind: String, prop: String },
    UnknownCallback { kind: String, callback: String },
    InvalidName(String),
    DuplicateName(String),
    InvalidValue { prop: String, value: String, expected: &'static str },
    NoSuchControl(usize),
}

impl fmt::Display for FormError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormError::UnknownKind(k) => write!(f, "unknown control type `{k}`"),
            FormError::UnknownProperty { kind, prop } => {
                write!(f, "`{kind}` has no property `{prop}`")
            }
            FormError::UnknownCallback { kind, callback } => {
                write!(f, "`{kind}` has no event `{callback}`")
            }
            FormError::InvalidName(n) => write!(
                f,
                "`{n}` is not a valid name: use letters, digits and `_`, starting with a letter"
            ),
            FormError::DuplicateName(n) => write!(f, "the name `{n}` is already used on this form"),
            FormError::InvalidValue { prop, value, expected } => {
                write!(f, "`{value}` is not a valid value for `{prop}`: expected {expected}")
            }
            FormError::NoSuchControl(i) => write!(f, "no control at index {i}"),
        }
    }
}

impl std::error::Error for FormError {}

/// Rounds a coordinate to the designer grid.
pub fn snap(v: i32) -> i32 {
    ((v as f32) / GRID as f32).round() as i32 * GRID
}

/// `MainForm` -> `main_form`, used for file and module names.
pub fn to_snake_case(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            if prev_lower {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower = false;
        } else if ch == '-' {
            out.push('_');
            prev_lower = false;
        } else {
            out.push(ch);
            prev_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out
}

const RESERVED: &[&str] = &[
    "root", "parent", "self", "true", "false", "if", "for", "in", "out", "property", "callback",
    "export", "import", "component", "struct", "enum", "global", "fn", "let", "mut", "match",
    "type", "use", "mod", "pub", "impl", "crate", "super", "return", "loop", "while", "move",
    "ref", "static", "const", "trait", "where", "async", "await", "dyn",
];

/// Names must work both as Slint element ids and as parts of Rust
/// identifiers, so `-` (legal in Slint) is not allowed.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic());
    first_ok
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&name)
        && !name.ends_with('_')
        && !name.contains("__")
}

/// Checks a value typed in the Object Inspector and returns it normalised.
pub fn validate_value(prop: &str, ty: PropType, value: &str) -> Result<String, FormError> {
    let v = value.trim();
    let bad = |expected| FormError::InvalidValue {
        prop: prop.to_string(),
        value: value.to_string(),
        expected,
    };
    match ty {
        PropType::String => Ok(value.to_string()),
        PropType::StringList => Ok(split_list(value).join(", ")),
        PropType::Bool => match v {
            "true" | "True" | "1" => Ok("true".into()),
            "false" | "False" | "0" => Ok("false".into()),
            _ => Err(bad("true or false")),
        },
        PropType::Int => v.parse::<i32>().map(|n| n.to_string()).map_err(|_| bad("a whole number")),
        PropType::Length | PropType::Float => {
            let num = v.strip_suffix("px").unwrap_or(v).trim();
            match num.parse::<f64>() {
                Ok(n) if n.is_finite() => Ok(format_number(n)),
                _ => Err(bad("a number")),
            }
        }
        PropType::Color => {
            if v.is_empty() {
                return Ok(String::new());
            }
            let hex = v.strip_prefix('#').unwrap_or(v);
            let ok = matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|c| c.is_ascii_hexdigit());
            if ok {
                Ok(format!("#{}", hex.to_ascii_lowercase()))
            } else {
                Err(bad("a colour like #3b82f6, or empty for the default"))
            }
        }
    }
}

/// Splits an inspector list value (`a, b, c`) into items.
pub fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Formats a number without a trailing `.0`.
pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// `button1` -> `Button1`, used for initial captions as C++Builder does.
fn capitalised(name: &str) -> String {
    let mut c = name.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

impl Form {
    /// A new, empty form like File > New > Form.
    pub fn new(name: &str) -> Self {
        Form {
            name: name.to_string(),
            title: name.to_string(),
            width: 480,
            height: 320,
            controls: Vec::new(),
        }
    }

    pub fn spec(&self, index: usize) -> Result<&'static KindSpec, FormError> {
        let c = self.controls.get(index).ok_or(FormError::NoSuchControl(index))?;
        kind_spec(&c.kind).ok_or_else(|| FormError::UnknownKind(c.kind.clone()))
    }

    pub fn find(&self, name: &str) -> Option<usize> {
        self.controls.iter().position(|c| c.name == name)
    }

    /// True if `name` is free for a control: not a control, and not a
    /// handler or published property (they share the form's namespace).
    fn name_is_free(&self, name: &str, ignore: Option<usize>) -> bool {
        let clash = self.controls.iter().enumerate().any(|(i, c)| {
            let own = Some(i) == ignore;
            c.handlers.values().any(|h| h == name)
                || (!own && (c.name == name || published_name(c).as_deref() == Some(name)))
        });
        !clash && name != self.name
    }

    /// The next free name for a prefix: `button1`, `button2`, ...
    pub fn unique_name(&self, prefix: &str) -> String {
        (1..)
            .map(|n| format!("{prefix}{n}"))
            .find(|n| self.name_is_free(n, None))
            .expect("unbounded range")
    }

    /// Drops a control from the palette at a point, snapped to the grid.
    /// Returns its index.
    pub fn add_control(&mut self, kind: &str, x: i32, y: i32) -> Result<usize, FormError> {
        let spec = kind_spec(kind).ok_or_else(|| FormError::UnknownKind(kind.to_string()))?;
        let name = self.unique_name(spec.prefix);
        let (w, h) = spec.default_size;
        let x = snap(x).clamp(0, (self.width - w).max(0));
        let y = snap(y).clamp(0, (self.height - h).max(0));
        let mut props = BTreeMap::new();
        for (prop, value) in spec.initial {
            props.insert(prop.to_string(), value.replace("{Name}", &capitalised(&name)));
        }
        self.controls.push(Control {
            name,
            kind: spec.kind.to_string(),
            x,
            y,
            width: w,
            height: h,
            props,
            handlers: BTreeMap::new(),
        });
        Ok(self.controls.len() - 1)
    }

    pub fn remove_control(&mut self, index: usize) -> Result<Control, FormError> {
        if index >= self.controls.len() {
            return Err(FormError::NoSuchControl(index));
        }
        Ok(self.controls.remove(index))
    }

    /// Renames a control. Handler function names are kept, as in
    /// C++Builder, so existing handler code keeps compiling.
    pub fn rename_control(&mut self, index: usize, new_name: &str) -> Result<(), FormError> {
        if index >= self.controls.len() {
            return Err(FormError::NoSuchControl(index));
        }
        if !is_valid_name(new_name) {
            return Err(FormError::InvalidName(new_name.to_string()));
        }
        if self.controls[index].name == new_name {
            return Ok(());
        }
        if !self.name_is_free(new_name, Some(index)) {
            return Err(FormError::DuplicateName(new_name.to_string()));
        }
        self.controls[index].name = new_name.to_string();
        Ok(())
    }

    /// Renames the form component (its Rust type name).
    pub fn rename_form(&mut self, new_name: &str) -> Result<(), FormError> {
        let mut chars = new_name.chars();
        let ok = matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
            && chars.all(|c| c.is_ascii_alphanumeric());
        if !ok {
            return Err(FormError::InvalidName(new_name.to_string()));
        }
        if self.controls.iter().any(|c| c.name == new_name) {
            return Err(FormError::DuplicateName(new_name.to_string()));
        }
        self.name = new_name.to_string();
        Ok(())
    }

    /// Sets a property from inspector text. Values equal to Slint's default
    /// are removed so the file only carries what the user changed.
    pub fn set_prop(&mut self, index: usize, prop: &str, value: &str) -> Result<(), FormError> {
        let spec = self.spec(index)?;
        let pspec = spec.prop(prop).ok_or_else(|| FormError::UnknownProperty {
            kind: spec.kind.to_string(),
            prop: prop.to_string(),
        })?;
        let v = validate_value(prop, pspec.ty, value)?;
        let c = &mut self.controls[index];
        if v == pspec.default {
            c.props.remove(prop);
        } else {
            c.props.insert(prop.to_string(), v);
        }
        Ok(())
    }

    /// Current inspector value of a property, falling back to the default.
    pub fn prop_value(&self, index: usize, prop: &str) -> Option<String> {
        let spec = self.spec(index).ok()?;
        let pspec = spec.prop(prop)?;
        Some(
            self.controls[index]
                .props
                .get(prop)
                .cloned()
                .unwrap_or_else(|| pspec.default.to_string()),
        )
    }

    pub fn set_bounds(&mut self, index: usize, x: i32, y: i32, w: i32, h: i32) -> Result<(), FormError> {
        let c = self.controls.get_mut(index).ok_or(FormError::NoSuchControl(index))?;
        c.x = x;
        c.y = y;
        c.width = w.max(MIN_SIZE);
        c.height = h.max(MIN_SIZE);
        Ok(())
    }

    /// Moves a control by a drag delta, snapping the result to the grid.
    pub fn drag_by(&mut self, index: usize, dx: i32, dy: i32) -> Result<(), FormError> {
        let (x, y, w, h) = {
            let c = self.controls.get(index).ok_or(FormError::NoSuchControl(index))?;
            (c.x, c.y, c.width, c.height)
        };
        let nx = snap(x + dx).clamp(-w + GRID, self.width - GRID);
        let ny = snap(y + dy).clamp(-h + GRID, self.height - GRID);
        self.set_bounds(index, nx, ny, w, h)
    }

    /// Resizes a control by a drag delta on its corner handle.
    pub fn resize_by(&mut self, index: usize, dw: i32, dh: i32) -> Result<(), FormError> {
        let c = self.controls.get(index).ok_or(FormError::NoSuchControl(index))?;
        let (x, y) = (c.x, c.y);
        let w = snap(c.x + c.width + dw) - c.x;
        let h = snap(c.y + c.height + dh) - c.y;
        self.set_bounds(index, x, y, w, h)
    }

    /// Default handler name for a control's callback, e.g. `button1_clicked`.
    pub fn default_handler_name(control: &str, callback: &str) -> String {
        format!("{}_{}", control, callback.replace('-', "_"))
    }

    /// Connects a callback to a handler (creating the name if needed) and
    /// returns the handler name, like double-clicking in the Events tab.
    pub fn ensure_handler(&mut self, index: usize, callback: &str) -> Result<String, FormError> {
        let spec = self.spec(index)?;
        if spec.callback(callback).is_none() {
            return Err(FormError::UnknownCallback {
                kind: spec.kind.to_string(),
                callback: callback.to_string(),
            });
        }
        if let Some(h) = self.controls[index].handlers.get(callback) {
            return Ok(h.clone());
        }
        let base = Self::default_handler_name(&self.controls[index].name, callback);
        let mut name = base.clone();
        let mut n = 2;
        while !self.name_is_free(&name, None) {
            name = format!("{base}{n}");
            n += 1;
        }
        self.controls[index].handlers.insert(callback.to_string(), name.clone());
        Ok(name)
    }

    /// Disconnects a callback; the handler function stays in the source.
    pub fn clear_handler(&mut self, index: usize, callback: &str) -> Result<(), FormError> {
        let c = self.controls.get_mut(index).ok_or(FormError::NoSuchControl(index))?;
        c.handlers.remove(callback);
        Ok(())
    }

    /// Every connected event, in control order.
    pub fn handlers(&self) -> Vec<HandlerRef<'_>> {
        let mut out = Vec::new();
        for c in &self.controls {
            let Some(spec) = kind_spec(&c.kind) else { continue };
            for cb in spec.callbacks {
                if let Some(h) = c.handlers.get(cb.name) {
                    out.push(HandlerRef { control: &c.name, callback: cb.name, handler: h, args: cb.args });
                }
            }
        }
        out
    }
}

/// Name of the form property that publishes a control's main value,
/// e.g. `edit1_text` for `edit1.text`.
pub fn published_name(c: &Control) -> Option<String> {
    let spec = kind_spec(&c.kind)?;
    let (prop, _) = spec.published?;
    Some(format!("{}_{}", c.name, prop.replace('-', "_")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_cpp_builder_conventions() {
        let mut f = Form::new("MainForm");
        let a = f.add_control("Button", 13, 21).unwrap();
        let b = f.add_control("Button", 100, 100).unwrap();
        assert_eq!(f.controls[a].name, "button1");
        assert_eq!(f.controls[b].name, "button2");
        assert_eq!(f.controls[a].props["text"], "Button1");
        assert_eq!((f.controls[a].x, f.controls[a].y), (16, 24));
    }

    #[test]
    fn default_values_are_not_stored() {
        let mut f = Form::new("MainForm");
        let i = f.add_control("CheckBox", 0, 0).unwrap();
        f.set_prop(i, "checked", "true").unwrap();
        assert_eq!(f.controls[i].props.get("checked").map(String::as_str), Some("true"));
        f.set_prop(i, "checked", "false").unwrap();
        assert!(!f.controls[i].props.contains_key("checked"));
        assert!(f.set_prop(i, "checked", "maybe").is_err());
        assert!(f.set_prop(i, "nonsense", "1").is_err());
    }

    #[test]
    fn handlers_get_unique_names_and_survive_renames() {
        let mut f = Form::new("MainForm");
        let i = f.add_control("Button", 0, 0).unwrap();
        let h = f.ensure_handler(i, "clicked").unwrap();
        assert_eq!(h, "button1_clicked");
        assert_eq!(f.ensure_handler(i, "clicked").unwrap(), h);
        f.rename_control(i, "btn_ok").unwrap();
        assert_eq!(f.handlers()[0].handler, "button1_clicked");
        assert!(f.rename_control(i, "1bad").is_err());
        assert!(f.rename_control(i, "button1_clicked").is_err());
    }

    #[test]
    fn values_are_validated_and_normalised() {
        assert_eq!(validate_value("w", PropType::Length, " 12px ").unwrap(), "12");
        assert_eq!(validate_value("v", PropType::Float, "0.50").unwrap(), "0.5");
        assert_eq!(validate_value("c", PropType::Color, "3B82F6").unwrap(), "#3b82f6");
        assert!(validate_value("c", PropType::Color, "blue").is_err());
        assert_eq!(validate_value("m", PropType::StringList, "a,  b ,,c").unwrap(), "a, b, c");
        assert_eq!(to_snake_case("MainForm"), "main_form");
        assert_eq!(to_snake_case("AboutBox2"), "about_box2");
    }

    #[test]
    fn drag_and_resize_snap_to_grid() {
        let mut f = Form::new("MainForm");
        let i = f.add_control("Button", 16, 16).unwrap();
        f.drag_by(i, 13, 3).unwrap();
        assert_eq!((f.controls[i].x, f.controls[i].y), (32, 16));
        f.resize_by(i, 9, -30).unwrap();
        assert_eq!((f.controls[i].width, f.controls[i].height), (104, MIN_SIZE));
    }
}
