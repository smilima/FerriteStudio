//! Conversions from the IDE's state to the structs the Slint UI shows.

use ferrite_form::model::{published_name, Form};
use ferrite_form::{kind_spec, Control, PropType};
use slint::{Color, ModelRc, SharedString, VecModel};

use crate::{ControlItem, EventRow, PropRow, SyntaxLayer};

/// The code editor's colour layers for a file.
pub fn syntax_layers(path: &std::path::Path, text: &str) -> Vec<SyntaxLayer> {
    use crate::highlight::{layers, Language};
    layers(text, Language::for_path(path))
        .into_iter()
        .map(|(kind, text)| SyntaxLayer { kind: kind.name().into(), text: text.into() })
        .collect()
}

pub fn parse_color(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#')?;
    let v = |i: usize, n: usize| u8::from_str_radix(&hex[i..i + n], 16).ok();
    match hex.len() {
        3 => {
            let d = |i| v(i, 1).map(|x| x * 17);
            Some(Color::from_rgb_u8(d(0)?, d(1)?, d(2)?))
        }
        6 => Some(Color::from_rgb_u8(v(0, 2)?, v(2, 2)?, v(4, 2)?)),
        8 => Some(Color::from_argb_u8(v(6, 2)?, v(0, 2)?, v(2, 2)?, v(4, 2)?)),
        _ => None,
    }
}

fn value(c: &Control, prop: &str) -> String {
    let default = kind_spec(&c.kind).and_then(|k| k.prop(prop)).map_or("", |p| p.default);
    c.props.get(prop).cloned().unwrap_or_else(|| default.to_string())
}

fn num(c: &Control, prop: &str) -> f32 {
    value(c, prop).parse().unwrap_or(0.0)
}

fn flag(c: &Control, prop: &str) -> bool {
    value(c, prop) == "true"
}

/// Everything the canvas needs to draw a control like the running app.
pub fn control_item(c: &Control) -> ControlItem {
    let items: Vec<SharedString> = ferrite_form::model::split_list(&value(c, "model")).into_iter().map(Into::into).collect();
    let bg = parse_color(&value(c, "background"));
    let fg = parse_color(&value(c, "color"));
    ControlItem {
        name: c.name.clone().into(),
        kind: c.kind.clone().into(),
        x: c.x,
        y: c.y,
        w: c.width,
        h: c.height,
        text: value(c, "text").into(),
        placeholder: value(c, "placeholder-text").into(),
        checked: flag(c, "checked"),
        enabled: value(c, "enabled") != "false",
        primary: flag(c, "primary"),
        checkable: flag(c, "checkable"),
        read_only: flag(c, "read-only"),
        fvalue: num(c, "value"),
        fmin: num(c, "minimum"),
        fmax: num(c, "maximum"),
        ivalue: num(c, "value") as i32,
        imin: num(c, "minimum") as i32,
        imax: num(c, "maximum") as i32,
        items: ModelRc::new(VecModel::from(items)),
        current_index: num(c, "current-index") as i32,
        progress: num(c, "progress"),
        indeterminate: flag(c, "indeterminate"),
        background: bg.unwrap_or_default(),
        has_background: bg.is_some(),
        fg: fg.unwrap_or_default(),
        has_fg: fg.is_some(),
        font_size: num(c, "font-size"),
        font_weight: num(c, "font-weight") as i32,
        border_radius: num(c, "border-radius"),
        border_width: num(c, "border-width"),
        border_color: parse_color(&value(c, "border-color")).unwrap_or_default(),
    }
}

fn row(name: &str, ty: &str, value: impl Into<SharedString>, readonly: bool) -> PropRow {
    let value = value.into();
    PropRow { name: name.into(), type_name: ty.into(), flag: value == "true", value, readonly }
}

/// Object Inspector rows for the selected control, or the form itself.
pub fn prop_rows(form: &Form, selected: Option<usize>) -> (String, Vec<PropRow>) {
    let Some(i) = selected.filter(|&i| i < form.controls.len()) else {
        let rows = vec![
            row("Name", "string", form.name.clone(), false),
            row("title", "string", form.title.clone(), false),
            row("width", "length", form.width.to_string(), false),
            row("height", "length", form.height.to_string(), false),
            row("Rust type", "info", form.name.clone(), true),
        ];
        return (format!("{}: Window", form.name), rows);
    };
    let c = &form.controls[i];
    let mut rows = vec![
        row("Name", "string", c.name.clone(), false),
        row("x", "length", c.x.to_string(), false),
        row("y", "length", c.y.to_string(), false),
        row("width", "length", c.width.to_string(), false),
        row("height", "length", c.height.to_string(), false),
    ];
    if let Some(spec) = kind_spec(&c.kind) {
        for p in spec.props {
            let v = c.props.get(p.name).cloned().unwrap_or_else(|| p.default.to_string());
            let ty = match p.ty {
                PropType::Bool => "bool",
                other => other.label(),
            };
            rows.push(row(p.name, ty, v, false));
        }
        if let (Some(name), Some((_, ty))) = (published_name(c), spec.published) {
            rows.push(row("Rust access", "info", format!("form.get_{name}() -> {}", short_type(ty.rust_type())), true));
        }
    }
    (format!("{}: {}", c.name, c.kind), rows)
}

fn short_type(t: &str) -> &str {
    t.rsplit("::").next().unwrap_or(t)
}

/// Events tab rows for the selection.
pub fn event_rows(form: &Form, selected: Option<usize>) -> Vec<EventRow> {
    let Some(c) = selected.and_then(|i| form.controls.get(i)) else { return Vec::new() };
    let Some(spec) = kind_spec(&c.kind) else { return Vec::new() };
    spec.callbacks
        .iter()
        .map(|cb| {
            let args: Vec<String> = cb.args.iter().map(|a| format!("{}: {}", a.name, a.ty.slint_type())).collect();
            EventRow {
                name: cb.name.into(),
                signature: format!("({})", args.join(", ")).into(),
                handler: c.handlers.get(cb.name).cloned().unwrap_or_default().into(),
            }
        })
        .collect()
}

/// `1\n2\n...\nn` for the editor gutter.
pub fn line_numbers(count: usize) -> String {
    let mut s = String::with_capacity(count * 5);
    for n in 1..=count.max(1) {
        if n > 1 {
            s.push('\n');
        }
        s.push_str(&n.to_string());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse() {
        assert_eq!(parse_color("#fff"), Some(Color::from_rgb_u8(255, 255, 255)));
        assert_eq!(parse_color("#336699"), Some(Color::from_rgb_u8(0x33, 0x66, 0x99)));
        assert_eq!(parse_color("#33669980").map(|c| c.alpha()), Some(0x80));
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn inspector_lists_form_then_control_rows() {
        let mut f = Form::new("MainForm");
        let (heading, rows) = prop_rows(&f, None);
        assert_eq!(heading, "MainForm: Window");
        assert_eq!(rows[1].name, "title");
        let i = f.add_control("LineEdit", 8, 8).unwrap();
        let (heading, rows) = prop_rows(&f, Some(i));
        assert_eq!(heading, "edit1: LineEdit");
        assert!(rows.iter().any(|r| r.name == "placeholder-text"));
        assert!(rows.iter().any(|r| r.value.contains("get_edit1_text")));
        assert_eq!(event_rows(&f, Some(i))[0].signature, "(text: string)");
        assert_eq!(line_numbers(3), "1\n2\n3");
    }
}
