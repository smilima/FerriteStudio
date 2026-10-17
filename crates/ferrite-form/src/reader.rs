//! Parses the canonical `.slint` subset that [`crate::writer`] produces.
//!
//! Anything outside that subset (layouts, expressions, nested elements,
//! hand-written bindings) is reported as a [`ReadError`] with a line number,
//! and the IDE opens such a file in the code editor instead of the designer.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use crate::catalog::{kind_spec, PropType};
use crate::model::{format_number, Control, Form};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ReadError {}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64, String),
    Color(String),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    line: usize,
}

const PUNCTS: &[&str] = &["<=>", ":=", "=>", "{", "}", "(", ")", "[", "]", ";", ":", ",", ".", "<", ">"];

fn err<T>(line: usize, message: impl Into<String>) -> Result<T, ReadError> {
    Err(ReadError { line, message: message.into() })
}

fn tokenize(src: &str) -> Result<Vec<Token>, ReadError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if c.is_whitespace() {
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                if chars[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            i += 2;
        } else if c == '"' {
            let start_line = line;
            i += 1;
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return err(start_line, "unterminated string"),
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some('\\') => {
                        match chars.get(i + 1) {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('"') => s.push('"'),
                            Some('\\') => s.push('\\'),
                            Some('u') if chars.get(i + 2) == Some(&'{') => {
                                let end = (i + 3..chars.len()).find(|&j| chars[j] == '}');
                                let Some(end) = end else { return err(line, "bad \\u escape") };
                                let hex: String = chars[i + 3..end].iter().collect();
                                let ch = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32);
                                let Some(ch) = ch else { return err(line, "bad \\u escape") };
                                s.push(ch);
                                i = end - 1;
                            }
                            Some('{') => {
                                return err(line, "string interpolation is outside the designer's subset")
                            }
                            _ => return err(line, "unknown escape in string"),
                        }
                        i += 2;
                    }
                    Some(&ch) => {
                        if ch == '\n' {
                            line += 1;
                        }
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            toks.push(Token { tok: Tok::Str(s), line: start_line });
        } else if c == '#' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_hexdigit() {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            toks.push(Token { tok: Tok::Color(s.to_ascii_lowercase()), line });
        } else if c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit() || *d == '.'))
            || (c == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let num: String = chars[start..i].iter().collect();
            let ustart = i;
            while i < chars.len() && (chars[i].is_ascii_alphabetic() || chars[i] == '%') {
                i += 1;
            }
            let unit: String = chars[ustart..i].iter().collect();
            let Ok(n) = num.parse::<f64>() else { return err(line, format!("bad number `{num}`")) };
            toks.push(Token { tok: Tok::Num(n, unit), line });
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric()
                    || chars[i] == '_'
                    || (chars[i] == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_alphabetic())))
            {
                i += 1;
            }
            toks.push(Token { tok: Tok::Ident(chars[start..i].iter().collect()), line });
        } else {
            let rest: String = chars[i..chars.len().min(i + 3)].iter().collect();
            match PUNCTS.iter().find(|p| rest.starts_with(**p)) {
                Some(p) => {
                    toks.push(Token { tok: Tok::Punct(p), line });
                    i += p.chars().count();
                }
                None => return err(line, format!("unexpected character `{c}`")),
            }
        }
    }
    toks.push(Token { tok: Tok::Eof, line });
    Ok(toks)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.pos + n).min(self.toks.len() - 1)].tok
    }

    fn line(&self) -> usize {
        self.toks[self.pos].line
    }

    fn next(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn expect_punct(&mut self, p: &str) -> Result<(), ReadError> {
        let line = self.line();
        match self.next() {
            Tok::Punct(q) if q == p => Ok(()),
            other => err(line, format!("expected `{p}`, found {}", describe(&other))),
        }
    }

    fn eat_punct(&mut self, p: &str) -> bool {
        if matches!(self.peek(), Tok::Punct(q) if *q == p) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String, ReadError> {
        let line = self.line();
        match self.next() {
            Tok::Ident(s) => Ok(s),
            other => err(line, format!("expected a name, found {}", describe(&other))),
        }
    }

    fn keyword(&mut self, kw: &str) -> Result<(), ReadError> {
        let line = self.line();
        match self.next() {
            Tok::Ident(s) if s == kw => Ok(()),
            other => err(line, format!("expected `{kw}`, found {}", describe(&other))),
        }
    }

    /// A type in angle brackets or a callback parameter list: `string` or `[string]`.
    fn type_name(&mut self) -> Result<String, ReadError> {
        if self.eat_punct("[") {
            let inner = self.ident()?;
            self.expect_punct("]")?;
            Ok(format!("[{inner}]"))
        } else {
            self.ident()
        }
    }

    fn value(&mut self, ty: PropType, prop: &str) -> Result<String, ReadError> {
        let line = self.line();
        let tok = self.next();
        let mismatch = || err(line, format!("`{prop}` has a value the designer cannot edit"));
        match (ty, tok) {
            (PropType::String, Tok::Str(s)) => Ok(s),
            (PropType::Length, Tok::Num(n, u)) if u == "px" || u.is_empty() => Ok(format_number(n)),
            (PropType::Float, Tok::Num(n, u)) if u.is_empty() => Ok(format_number(n)),
            (PropType::Int, Tok::Num(n, u)) if u.is_empty() && n.fract() == 0.0 => Ok(format_number(n)),
            (PropType::Bool, Tok::Ident(b)) if b == "true" || b == "false" => Ok(b),
            (PropType::Color, Tok::Color(c)) => Ok(c),
            (PropType::StringList, Tok::Punct("[")) => {
                let mut items = Vec::new();
                loop {
                    let line = self.line();
                    match self.next() {
                        Tok::Punct("]") => break,
                        Tok::Str(s) => {
                            if s.contains(',') {
                                return err(line, "list items cannot contain commas in the designer");
                            }
                            items.push(s);
                            if !self.eat_punct(",") {
                                self.expect_punct("]")?;
                                break;
                            }
                        }
                        _ => return mismatch(),
                    }
                }
                Ok(items.join(", "))
            }
            _ => mismatch(),
        }
    }

    fn name_list(&mut self) -> Result<Vec<String>, ReadError> {
        let mut out = Vec::new();
        if self.eat_punct(")") {
            return Ok(out);
        }
        loop {
            out.push(self.ident()?);
            if self.eat_punct(")") {
                return Ok(out);
            }
            self.expect_punct(",")?;
        }
    }
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(s) => format!("`{s}`"),
        Tok::Str(_) => "a string".into(),
        Tok::Num(..) => "a number".into(),
        Tok::Color(c) => format!("`{c}`"),
        Tok::Punct(p) => format!("`{p}`"),
        Tok::Eof => "the end of the file".into(),
    }
}

/// Parses a form file written by the designer.
pub fn read_slint(src: &str) -> Result<Form, ReadError> {
    let mut p = Parser { toks: tokenize(src)?, pos: 0 };

    // import { A, B } from "std-widgets.slint";
    while matches!(p.peek(), Tok::Ident(s) if s == "import") {
        p.next();
        p.expect_punct("{")?;
        while !p.eat_punct("}") {
            p.ident()?;
            p.eat_punct(",");
        }
        p.keyword("from")?;
        let line = p.line();
        match p.next() {
            Tok::Str(s) if s == "std-widgets.slint" => {}
            _ => return err(line, "only imports from std-widgets.slint are supported by the designer"),
        }
        p.expect_punct(";")?;
    }

    p.keyword("export")?;
    p.keyword("component")?;
    let name = p.ident()?;
    p.keyword("inherits")?;
    let line = p.line();
    if p.ident()? != "Window" {
        return err(line, "a form must inherit Window");
    }
    p.expect_punct("{")?;

    let mut form = Form::new(&name);
    let mut declared_callbacks: HashSet<String> = HashSet::new();
    let mut published: Vec<(usize, String)> = Vec::new();

    loop {
        let line = p.line();
        match p.peek().clone() {
            Tok::Punct("}") => {
                p.next();
                break;
            }
            Tok::Ident(kw) if kw == "in-out" => {
                p.next();
                p.keyword("property")?;
                p.expect_punct("<")?;
                p.type_name()?;
                p.expect_punct(">")?;
                p.ident()?;
                p.expect_punct("<=>")?;
                let target = p.ident()?;
                p.expect_punct(".")?;
                p.ident()?;
                p.expect_punct(";")?;
                published.push((line, target));
            }
            Tok::Ident(kw) if kw == "callback" => {
                p.next();
                let cb = p.ident()?;
                p.expect_punct("(")?;
                if !p.eat_punct(")") {
                    loop {
                        p.type_name()?;
                        if p.eat_punct(")") {
                            break;
                        }
                        p.expect_punct(",")?;
                    }
                }
                p.expect_punct(";")?;
                declared_callbacks.insert(cb);
            }
            Tok::Ident(id) if matches!(p.peek_at(1), Tok::Punct(":")) => {
                p.next();
                p.next();
                match id.as_str() {
                    "title" => form.title = p.value(PropType::String, "title")?,
                    "width" => form.width = parse_int(&p.value(PropType::Length, "width")?, line)?,
                    "height" => form.height = parse_int(&p.value(PropType::Length, "height")?, line)?,
                    other => return err(line, format!("form property `{other}` is outside the designer's subset")),
                }
                p.expect_punct(";")?;
            }
            Tok::Ident(id) if matches!(p.peek_at(1), Tok::Punct(":=")) => {
                p.next();
                p.next();
                let control = read_control(&mut p, id, line)?;
                if form.controls.iter().any(|c| c.name == control.name) {
                    return err(line, format!("duplicate control name `{}`", control.name));
                }
                form.controls.push(control);
            }
            other => return err(line, format!("unexpected {} in the form body", describe(&other))),
        }
    }
    let line = p.line();
    if !matches!(p.peek(), Tok::Eof) {
        return err(line, "unexpected content after the form component");
    }

    for (line, target) in published {
        if form.find(&target).is_none() {
            return err(line, format!("property alias refers to unknown control `{target}`"));
        }
    }
    for c in &form.controls {
        for h in c.handlers.values() {
            if !declared_callbacks.contains(h) {
                return err(1, format!("handler `{h}` is not declared as a callback on the form"));
            }
        }
    }
    Ok(form)
}

fn parse_int(v: &str, line: usize) -> Result<i32, ReadError> {
    v.parse::<f64>()
        .ok()
        .filter(|n| n.fract() == 0.0)
        .map(|n| n as i32)
        .map_or_else(|| err(line, format!("`{v}` must be a whole number of pixels")), Ok)
}

fn read_control(p: &mut Parser, name: String, line: usize) -> Result<Control, ReadError> {
    let kind = p.ident()?;
    let Some(spec) = kind_spec(&kind) else {
        return err(line, format!("`{kind}` is not on the Tool Palette"));
    };
    p.expect_punct("{")?;
    let mut c = Control {
        name,
        kind: spec.kind.to_string(),
        x: 0,
        y: 0,
        width: spec.default_size.0,
        height: spec.default_size.1,
        props: BTreeMap::new(),
        handlers: BTreeMap::new(),
    };
    loop {
        let line = p.line();
        let id = match p.next() {
            Tok::Punct("}") => break,
            Tok::Ident(id) => id,
            other => return err(line, format!("unexpected {} inside `{}`", describe(&other), c.name)),
        };
        if p.eat_punct(":") {
            match id.as_str() {
                "x" => c.x = parse_int(&p.value(PropType::Length, "x")?, line)?,
                "y" => c.y = parse_int(&p.value(PropType::Length, "y")?, line)?,
                "width" => c.width = parse_int(&p.value(PropType::Length, "width")?, line)?,
                "height" => c.height = parse_int(&p.value(PropType::Length, "height")?, line)?,
                prop => {
                    let Some(ps) = spec.prop(prop) else {
                        return err(line, format!("`{kind}` property `{prop}` is outside the designer's subset"));
                    };
                    let v = p.value(ps.ty, prop)?;
                    c.props.insert(prop.to_string(), v);
                }
            }
            p.expect_punct(";")?;
            continue;
        }
        // Event handler: `clicked => { root.h(); }` or `edited(text) => { root.h(text); }`
        let Some(cb) = spec.callback(&id) else {
            return err(line, format!("`{kind}` has no event `{id}`"));
        };
        let params = if p.eat_punct("(") { p.name_list()? } else { Vec::new() };
        p.expect_punct("=>")?;
        p.expect_punct("{")?;
        p.keyword("root")?;
        p.expect_punct(".")?;
        let handler = p.ident()?;
        p.expect_punct("(")?;
        let args = p.name_list()?;
        p.expect_punct(";")?;
        p.expect_punct("}")?;
        if params.len() != cb.args.len() || args != params {
            return err(line, format!("handler for `{}` must forward its arguments unchanged", cb.name));
        }
        c.handlers.insert(cb.name.to_string(), handler);
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writer::write_slint;

    fn sample() -> Form {
        let mut f = Form::new("MainForm");
        f.title = "Greeter \"v1\"\nline two".into();
        for kind in crate::KINDS {
            f.add_control(kind.kind, 16, 16).unwrap();
        }
        let edit = f.find("edit1").unwrap();
        f.set_prop(edit, "placeholder-text", "Your name").unwrap();
        f.ensure_handler(edit, "edited").unwrap();
        let btn = f.find("button1").unwrap();
        f.ensure_handler(btn, "clicked").unwrap();
        let slider = f.find("slider1").unwrap();
        f.set_prop(slider, "value", "12.5").unwrap();
        f.ensure_handler(slider, "changed").unwrap();
        f
    }

    #[test]
    fn round_trip_is_lossless() {
        let form = sample();
        let text = write_slint(&form);
        let back = read_slint(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(back, form);
        assert_eq!(write_slint(&back), text);
    }

    #[test]
    fn hand_edits_outside_the_subset_are_reported_with_a_line() {
        let src = "export component F inherits Window {\n  title: \"x\";\n  VerticalLayout { }\n}\n";
        let e = read_slint(src).unwrap_err();
        assert_eq!(e.line, 3);
        let src = "export component F inherits Window {\n  width: 10px + 2px;\n}\n";
        assert!(read_slint(src).is_err());
    }
}
