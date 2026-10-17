//! Writes a [`Form`] as canonical `.slint` source.
//!
//! The output is deliberately regular so [`crate::reader`] can parse it
//! back: one statement per line, controls in z-order, and only properties
//! that differ from Slint's defaults.

use std::fmt::Write as _;

use crate::catalog::{kind_spec, PropType};
use crate::model::{published_name, split_list, Form};

pub const HEADER: &str =
    "// Ferrite Studio form. The Form Designer reads and rewrites this file;\n\
     // put code in the matching *_events.rs file instead of here.\n";

/// Escapes text for a Slint string literal.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Formats an inspector value as a Slint expression.
pub fn slint_value(ty: PropType, value: &str) -> String {
    match ty {
        PropType::String => quote(value),
        PropType::Length => format!("{value}px"),
        PropType::Bool | PropType::Int | PropType::Float | PropType::Color => value.to_string(),
        PropType::StringList => {
            let items: Vec<String> = split_list(value).iter().map(|s| quote(s)).collect();
            format!("[{}]", items.join(", "))
        }
    }
}

pub fn write_slint(form: &Form) -> String {
    let mut out = String::from(HEADER);

    let mut imports: Vec<&str> = Vec::new();
    for c in &form.controls {
        if let Some(spec) = kind_spec(&c.kind) {
            if spec.std_widget && !imports.contains(&spec.kind) {
                imports.push(spec.kind);
            }
        }
    }
    imports.sort_unstable();
    if !imports.is_empty() {
        let _ = writeln!(out, "import {{ {} }} from \"std-widgets.slint\";", imports.join(", "));
    }

    let _ = writeln!(out, "\nexport component {} inherits Window {{", form.name);
    let _ = writeln!(out, "    title: {};", quote(&form.title));
    let _ = writeln!(out, "    width: {}px;", form.width);
    let _ = writeln!(out, "    height: {}px;", form.height);

    let mut wrote_published = false;
    for c in &form.controls {
        let (Some(spec), Some(name)) = (kind_spec(&c.kind), published_name(c)) else { continue };
        let (prop, ty) = spec.published.expect("published_name implies published");
        if !wrote_published {
            out.push('\n');
            wrote_published = true;
        }
        let _ = writeln!(
            out,
            "    in-out property <{}> {} <=> {}.{};",
            ty.slint_type(),
            name,
            c.name,
            prop
        );
    }

    let handlers = form.handlers();
    if !handlers.is_empty() {
        out.push('\n');
    }
    for h in &handlers {
        let types: Vec<&str> = h.args.iter().map(|a| a.ty.slint_type()).collect();
        let _ = writeln!(out, "    callback {}({});", h.handler, types.join(", "));
    }

    for c in &form.controls {
        let Some(spec) = kind_spec(&c.kind) else { continue };
        out.push('\n');
        let _ = writeln!(out, "    {} := {} {{", c.name, c.kind);
        let _ = writeln!(out, "        x: {}px;", c.x);
        let _ = writeln!(out, "        y: {}px;", c.y);
        let _ = writeln!(out, "        width: {}px;", c.width);
        let _ = writeln!(out, "        height: {}px;", c.height);
        for p in spec.props {
            if let Some(v) = c.props.get(p.name) {
                if v != p.default {
                    let _ = writeln!(out, "        {}: {};", p.name, slint_value(p.ty, v));
                }
            }
        }
        for cb in spec.callbacks {
            let Some(handler) = c.handlers.get(cb.name) else { continue };
            let args: Vec<&str> = cb.args.iter().map(|a| a.name).collect();
            let params = if args.is_empty() { String::new() } else { format!("({})", args.join(", ")) };
            let _ = writeln!(
                out,
                "        {}{} => {{ root.{}({}); }}",
                cb.name,
                params,
                handler,
                args.join(", ")
            );
        }
        let _ = writeln!(out, "    }}");
    }

    out.push_str("}\n");
    out
}
