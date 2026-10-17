//! Debugger tooltips, like RAD Studio's Tooltip Expression Evaluation:
//! while the program is paused, resting the mouse on a variable shows its
//! value under it. A struct opens one level deep; clicking a row opens or
//! closes the levels below it.

use std::time::Duration;

use ferrite_dap::Variable;
use ferrite_lsp::path_key;
use slint::{ModelRc, VecModel};

use crate::app::{act, post, App, View};
use crate::debug_ops::VarEntry;
use crate::highlight::{spans, Kind, Language};
use crate::VarRow;

/// How long the mouse rests on a name before it is evaluated.
const DWELL: Duration = Duration::from_millis(400);
/// Time allowed for moving the mouse from the name into the tooltip.
const LINGER: Duration = Duration::from_millis(400);
/// Children listed per level; the rest are counted in one last row.
const MAX_CHILDREN: usize = 200;
/// Longest value shown in a row.
const MAX_VALUE: usize = 120;

pub struct DebugTip {
    path: String,
    line: usize,
    /// Columns of the name under the mouse: the tooltip stays while the
    /// mouse is on it.
    word: (usize, usize),
    vars: Vec<VarEntry>,
}

/// What a tooltip at `line`/`col` (character columns) of `text` evaluates.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// The name under the mouse with the `a.b.` path before it.
    pub expression: String,
    /// Columns where the name under the mouse starts and ends.
    pub word: (usize, usize),
}

fn line_start(text: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    text.match_indices('\n').nth(line - 1).map(|(i, _)| i + 1)
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start of the identifier that ends at `end` in `s`, if there is one.
fn ident_start(s: &str, end: usize) -> Option<usize> {
    s[..end].char_indices().rev().take_while(|(_, c)| is_ident(*c)).last().map(|(i, _)| i)
}

/// The expression to evaluate for the mouse at `line`/`col`, or None over
/// keywords, literals, comments, type names, calls and paths.
pub fn target_at(text: &str, lang: Language, line: usize, col: usize) -> Option<Target> {
    let ls = line_start(text, line)?;
    let le = text[ls..].find('\n').map_or(text.len(), |e| ls + e);
    let line_text = &text[ls..le];
    let (at, ch) = line_text.char_indices().nth(col)?;
    if !is_ident(ch) {
        return None;
    }
    let start = ident_start(line_text, at).unwrap_or(at);
    let end = line_text[at..].char_indices().find(|(_, c)| !is_ident(*c)).map_or(line_text.len(), |(i, _)| at + i);
    let word = &line_text[start..end];
    let kind = spans(text, lang)
        .into_iter()
        .find(|s| s.start <= ls + start && ls + start < s.end)
        .map_or(Kind::Plain, |s| s.kind);
    match kind {
        Kind::Plain => {}
        Kind::Keyword if word == "self" => {}
        Kind::Number if line_text[..start].ends_with('.') => {} // tuple field: t.0
        _ => return None,
    }
    let after = line_text[end..].trim_start();
    if after.starts_with('(') || after.starts_with('!') || after.starts_with("::") || line_text[..start].ends_with("::") {
        return None;
    }
    // Walk back over `a.b.` so that `y` in `p.y` evaluates `p.y`.
    let mut from = start;
    while line_text[..from].ends_with('.') && !line_text[..from].ends_with("..") {
        from = ident_start(line_text, from - 1)?; // f().x, a[i].x: not evaluated
    }
    let cols = |b: usize| line_text[..b].chars().count();
    Some(Target { expression: line_text[from..end].to_string(), word: (cols(start), cols(end)) })
}

/// A type as Rust code would usually name it: without module paths, so
/// `alloc::vec::Vec<my_app::Point>` reads `Vec<Point>`.
pub fn short_type(t: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut word = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            word.clear(); // a module: dropped
        } else {
            out.push_str(&word);
            word.clear();
            out.push(c);
        }
    }
    out.push_str(&word);
    out
}

/// GDB names tuple fields `__0`; Rust writes `0`.
fn field_name(name: &str) -> &str {
    match name.strip_prefix("__") {
        Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => n,
        _ => name,
    }
}

fn clip(s: &str, n: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() <= n {
        flat
    } else {
        flat.chars().take(n - 1).chain(['…']).collect()
    }
}

/// `{ x: 2, y: 3 }` from a struct's fields, for adapters (GDB) that leave a
/// struct's own value empty.
fn summary(children: &[VarEntry]) -> String {
    let mut out = String::from("{ ");
    for c in children {
        if out.len() > 60 {
            out.push_str("…, ");
            break;
        }
        let value = if c.var.value.is_empty() { "{…}" } else { c.var.value.as_str() };
        out.push_str(&format!("{}: {}, ", field_name(&c.var.name), clip(value, 24)));
    }
    if out.ends_with(", ") {
        out.truncate(out.len() - 2);
    }
    out.push_str(" }");
    out
}

impl App {
    /// The paused session's client, current frame and id.
    fn paused_frame(&self) -> Option<(std::sync::Arc<ferrite_dap::DapClient>, i64, u64)> {
        let d = self.debug.as_ref().filter(|d| d.stopped)?;
        Some((d.client.clone(), d.frame_id()?, d.id))
    }

    fn tip_covers(&self, cell: Option<(usize, usize)>) -> bool {
        match (&self.tip, cell) {
            (Some(t), Some((line, col))) => line == t.line && (t.word.0..t.word.1).contains(&col),
            _ => false,
        }
    }

    /// The mouse moved to another character of the editor's text; -1 when
    /// it left the text.
    pub fn mouse_cell(&mut self, line: i32, col: i32) {
        let cell = (line >= 0 && col >= 0).then_some((line as usize, col as usize));
        if self.tip_is_stale() {
            self.hide_tip();
        }
        if cell == self.hover_cell {
            return;
        }
        self.hover_cell = cell;
        self.hover_gen += 1;
        let gen = self.hover_gen;
        if self.tip_covers(cell) {
            return;
        }
        if self.tip.is_some() {
            slint::Timer::single_shot(LINGER, || act(|a| a.tip_linger_done()));
        }
        if cell.is_some() && self.paused_frame().is_some() {
            slint::Timer::single_shot(DWELL, move || act(move |a| a.tip_dwell_done(gen)));
        }
    }

    /// The mouse entered or left the tooltip.
    pub fn tip_hover(&mut self, inside: bool) {
        self.tip_hovered = inside;
        if !inside && self.tip.is_some() {
            slint::Timer::single_shot(LINGER, || act(|a| a.tip_linger_done()));
        }
    }

    fn tip_linger_done(&mut self) {
        if self.tip.is_some() && !self.tip_hovered && !self.tip_covers(self.hover_cell) {
            self.hide_tip();
        }
    }

    fn tip_dwell_done(&mut self, gen: u64) {
        if gen != self.hover_gen || self.tip_hovered || self.tip_covers(self.hover_cell) {
            return;
        }
        let Some((line, col)) = self.hover_cell else { return };
        let Some(i) = self.current.filter(|_| self.view == View::Code) else { return };
        let Some((client, frame, id)) = self.paused_frame() else { return };
        let b = &self.buffers[i];
        let Some(target) = target_at(&b.text, Language::for_path(&b.path), line, col) else { return };
        let path = path_key(&b.path);
        let word = target.word;
        let _ = client.evaluate_value(&target.expression, Some(frame), "hover", move |r| {
            post(move |a| {
                // Not a value here (out of scope, a function, ...): no tooltip.
                let Ok(var) = r else { return };
                let still_there = a.hover_cell.is_some_and(|(l, c)| l == line && (word.0..word.1).contains(&c));
                if !still_there || a.debug.as_ref().is_none_or(|d| d.id != id || !d.stopped) {
                    return;
                }
                let has_children = var.reference != 0;
                a.tip = Some(DebugTip { path, line, word, vars: vec![VarEntry { var, depth: 0, expanded: false }] });
                a.refresh_tip();
                if has_children {
                    a.tip_expand(0);
                }
            })
        });
    }

    pub fn hide_tip(&mut self) {
        self.tip_hovered = false;
        if self.tip.take().is_some() {
            let ui = self.ui();
            ui.set_tip_line(-1);
            ui.set_tip_rows(ModelRc::default());
        }
    }

    /// The tooltip belongs to another file than the one shown.
    pub fn tip_is_stale(&self) -> bool {
        let shown = self.current.filter(|_| self.view == View::Code).map(|i| path_key(&self.buffers[i].path));
        self.tip.as_ref().is_some_and(|t| Some(&t.path) != shown.as_ref())
    }

    /// Opens or closes row `i` of the tooltip.
    pub fn tip_expand(&mut self, i: usize) {
        let Some(id) = self.debug.as_ref().map(|d| d.id) else { return };
        let Some(t) = self.tip.as_mut() else { return };
        let Some(entry) = t.vars.get_mut(i) else { return };
        if entry.var.reference == 0 {
            return;
        }
        if entry.expanded {
            entry.expanded = false;
            let depth = entry.depth;
            let end = t.vars[i + 1..].iter().position(|v| v.depth <= depth).map_or(t.vars.len(), |p| i + 1 + p);
            t.vars.drain(i + 1..end);
            self.refresh_tip();
            return;
        }
        entry.expanded = true;
        let (reference, depth, name) = (entry.var.reference, entry.depth, entry.var.name.clone());
        let Some(client) = self.debug.as_ref().map(|d| d.client.clone()) else { return };
        let _ = client.variables(reference, move |vars| {
            post(move |a| {
                if a.debug.as_ref().is_none_or(|d| d.id != id) {
                    return;
                }
                let Some(t) = a.tip.as_mut() else { return };
                let same = t.vars.get(i).is_some_and(|v| v.var.name == name && v.depth == depth && v.expanded);
                if !same {
                    return;
                }
                let total = vars.len();
                let mut children: Vec<VarEntry> = vars
                    .into_iter()
                    .take(MAX_CHILDREN)
                    .map(|var| VarEntry { var, depth: depth + 1, expanded: false })
                    .collect();
                if total > MAX_CHILDREN {
                    let var = Variable {
                        name: "…".into(),
                        value: format!("{} more", total - MAX_CHILDREN),
                        type_name: String::new(),
                        reference: 0,
                    };
                    children.push(VarEntry { var, depth: depth + 1, expanded: false });
                }
                if t.vars[i].var.value.is_empty() && !children.is_empty() {
                    t.vars[i].var.value = summary(&children);
                }
                t.vars.splice(i + 1..i + 1, children);
                a.refresh_tip();
            })
        });
        self.refresh_tip();
    }

    fn refresh_tip(&self) {
        let Some(t) = &self.tip else { return };
        let ui = self.ui();
        let mut width = 0;
        let rows: Vec<VarRow> = t
            .vars
            .iter()
            .map(|v| {
                let value = match v.var.value.as_str() {
                    "" if v.var.reference != 0 => "{…}".to_string(),
                    value => clip(value, MAX_VALUE),
                };
                let name = if v.depth == 0 { v.var.name.as_str() } else { field_name(&v.var.name) };
                let type_name = clip(&short_type(&v.var.type_name), 48);
                let chars = 2 * v.depth + 2 + name.chars().count() + 3 + value.chars().count();
                let type_chars = if type_name.is_empty() { 0 } else { 4 + type_name.chars().count() };
                width = width.max(chars + type_chars);
                VarRow {
                    name: name.into(),
                    value: value.into(),
                    type_name: type_name.into(),
                    depth: v.depth as i32,
                    expandable: v.var.reference != 0 && !v.expanded,
                    expanded: v.expanded,
                }
            })
            .collect();
        ui.set_tip_width(width as i32);
        ui.set_tip_col(t.word.0 as i32);
        ui.set_tip_line(t.line as i32);
        ui.set_tip_rows(ModelRc::new(VecModel::from(rows)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(src: &str, line: usize, word: &str, nth: usize) -> Option<String> {
        let l = src.lines().nth(line).unwrap();
        let byte = l.match_indices(word).nth(nth).unwrap().0;
        let col = l[..byte].chars().count();
        target_at(src, Language::Rust, line, col).map(|t| t.expression)
    }

    #[test]
    fn finds_the_expression_under_the_mouse() {
        let src = "fn f(p: &Point, t: (i32, i32)) {\n    let s = self.items.len() + p.x + t.0; // p.y\n    let n = \"p.y\";\n    other::g(p);\n    println!(\"{}\", Point::ORIGIN);\n}\n";
        assert_eq!(at(src, 1, "s", 0).as_deref(), Some("s"));
        assert_eq!(at(src, 1, "self", 0).as_deref(), Some("self"));
        assert_eq!(at(src, 1, "items", 0).as_deref(), Some("self.items"));
        assert_eq!(at(src, 1, "x", 0).as_deref(), Some("p.x"));
        assert_eq!(at(src, 1, "0", 0).as_deref(), Some("t.0"));
        assert_eq!(at(src, 1, "len", 0), None, "method call");
        assert_eq!(at(src, 1, "let", 0), None, "keyword");
        assert_eq!(at(src, 1, "y", 0), None, "in a comment");
        assert_eq!(at(src, 2, "y", 0), None, "in a string");
        assert_eq!(at(src, 3, "other", 0), None, "path");
        assert_eq!(at(src, 3, "g", 0), None, "call");
        assert_eq!(at(src, 3, "p", 0).as_deref(), Some("p"));
        assert_eq!(at(src, 4, "println", 0), None, "macro");
        assert_eq!(at(src, 4, "Point", 0), None, "type");
        assert_eq!(at(src, 0, "Point", 0), None, "type");
        assert_eq!(at(src, 0, "p", 0).as_deref(), Some("p"));
        // Columns of the word, not the whole path.
        let t = target_at(src, Language::Rust, 1, src.lines().nth(1).unwrap().find("items").unwrap()).unwrap();
        assert_eq!(t.word, (17, 22));
    }

    #[test]
    fn summarises_struct_fields() {
        let v = |name: &str, value: &str| VarEntry {
            var: Variable { name: name.into(), value: value.into(), type_name: String::new(), reference: 0 },
            depth: 1,
            expanded: false,
        };
        assert_eq!(summary(&[v("x", "2"), v("y", "3")]), "{ x: 2, y: 3 }");
        assert_eq!(summary(&[v("__0", "7")]), "{ 0: 7 }");
        assert_eq!(short_type("alloc::vec::Vec<my_app::Point, alloc::alloc::Global>"), "Vec<Point, Global>");
        assert_eq!(short_type("&mut [core::option::Option<i32>; 3]"), "&mut [Option<i32>; 3]");
        assert_eq!(short_type("selftest_app::slint_generatedMainForm::MainForm"), "MainForm");
        assert_eq!(summary(&[v("inner", "")]), "{ inner: {…} }");
        assert_eq!(clip("abcdef", 4), "abc…");
    }
}
