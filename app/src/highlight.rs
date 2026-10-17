//! Syntax colouring for the code editor.
//!
//! A small lexer per language finds the coloured spans (keywords, strings,
//! comments, ...). The editor draws its text in a fixed-width font, so each
//! colour is drawn as one text layer the size of the whole document in
//! which every character of another colour is replaced by a space. The
//! layers stack exactly under the editor's own (transparent) text, which
//! keeps the number of drawn items fixed however long the file is.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Keyword,
    Type,
    Str,
    Number,
    Comment,
    Macro,
    Attribute,
    Lifetime,
}

const KINDS: [Kind; 9] = [
    Kind::Plain,
    Kind::Keyword,
    Kind::Type,
    Kind::Str,
    Kind::Number,
    Kind::Comment,
    Kind::Macro,
    Kind::Attribute,
    Kind::Lifetime,
];

impl Kind {
    /// The name the editor's colour scheme uses for this kind.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Plain => "plain",
            Kind::Keyword => "keyword",
            Kind::Type => "type",
            Kind::Str => "string",
            Kind::Number => "number",
            Kind::Comment => "comment",
            Kind::Macro => "macro",
            Kind::Attribute => "attribute",
            Kind::Lifetime => "lifetime",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Rust,
    Slint,
    Toml,
    Plain,
}

impl Language {
    pub fn for_path(path: &Path) -> Language {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Language::Rust,
            Some("slint") => Language::Slint,
            Some("toml") => Language::Toml,
            _ => Language::Plain,
        }
    }
}

/// A coloured byte range of the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// The coloured spans of `text`, in order and not overlapping. Text outside
/// every span is plain. Span boundaries always fall on ASCII characters or
/// the end of the text, so they are valid `str` indices.
pub fn spans(text: &str, lang: Language) -> Vec<Span> {
    let mut out = Vec::new();
    match lang {
        Language::Rust => rust(text.as_bytes(), &mut out),
        Language::Slint => slint(text.as_bytes(), &mut out),
        Language::Toml => toml(text.as_bytes(), &mut out),
        Language::Plain => {}
    }
    out
}

/// One text layer per colour that occurs, each as long as the document.
pub fn layers(text: &str, lang: Language) -> Vec<(Kind, String)> {
    let spans = spans(text, lang);
    let mut texts: Vec<String> = KINDS.iter().map(|_| String::with_capacity(text.len() / 4)).collect();
    // Whitespace (and placeholder spaces) not yet written to each layer:
    // only written when something visible follows on the same line, so
    // the layers carry no trailing blanks.
    let mut pending: Vec<String> = KINDS.iter().map(|_| String::new()).collect();
    let mut used = [false; KINDS.len()];
    let mut next = 0;
    let mut line_start = 0;
    for line in text.split('\n') {
        // Characters outside this range may be wider or narrower than one
        // column (combining marks, CJK, emoji); such lines stay uncoloured
        // so that no layer drifts out of line with the editor's text.
        let uniform = line.chars().all(|c| c == '\t' || c == '\r' || (!c.is_control() && (c as u32) < 0x300));
        for (off, c) in line.char_indices() {
            let at = line_start + off;
            if c == ' ' || c == '\t' || c == '\r' {
                for p in &mut pending {
                    p.push(c);
                }
                continue;
            }
            let kind = if uniform {
                while next < spans.len() && spans[next].end <= at {
                    next += 1;
                }
                match spans.get(next) {
                    Some(s) if s.start <= at => s.kind,
                    _ => Kind::Plain,
                }
            } else {
                Kind::Plain
            };
            let k = KINDS.iter().position(|x| *x == kind).unwrap_or(0);
            for (j, p) in pending.iter_mut().enumerate() {
                if j == k {
                    texts[j].push_str(p);
                    texts[j].push(c);
                    p.clear();
                } else {
                    p.push(' ');
                }
            }
            used[k] = true;
        }
        for (t, p) in texts.iter_mut().zip(&mut pending) {
            t.push('\n');
            p.clear();
        }
        line_start += line.len() + 1;
    }
    KINDS
        .iter()
        .zip(texts)
        .zip(used)
        .filter(|(_, u)| *u)
        .map(|((k, mut t), _)| {
            t.pop();
            (*k, t)
        })
        .collect()
}

// ---------------------------------------------------------------- lexers

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
    "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "yield", "abstract",
    "become", "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "try",
];

const RUST_PRIMITIVES: &[&str] = &[
    "bool", "char", "str", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    "f16", "f32", "f64", "f128",
];

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p)
}

/// End of a `"` string whose body starts at `i` (after the quote).
fn quoted_end(b: &[u8], mut i: usize, quote: u8) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

/// End of `/* ... */` starting at `i`, honouring nesting when `nested`.
fn block_comment_end(b: &[u8], mut i: usize, nested: bool) -> usize {
    let mut depth = 0;
    while i < b.len() {
        if b[i] == b'/' && at(b, i + 1) == b'*' {
            depth += 1;
            i += 2;
            if !nested && depth > 1 {
                depth = 1;
            }
        } else if b[i] == b'*' && at(b, i + 1) == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// End of a Rust number literal starting at `i` (a digit).
fn number_end(b: &[u8], mut i: usize) -> usize {
    if b[i] == b'0' && matches!(at(b, i + 1), b'x' | b'o' | b'b') {
        i += 2;
        while is_ident(at(b, i)) && at(b, i) < 0x80 {
            i += 1;
        }
        return i;
    }
    while at(b, i).is_ascii_digit() || at(b, i) == b'_' {
        i += 1;
    }
    if at(b, i) == b'.' && at(b, i + 1).is_ascii_digit() {
        i += 1;
        while at(b, i).is_ascii_digit() || at(b, i) == b'_' {
            i += 1;
        }
    }
    if matches!(at(b, i), b'e' | b'E')
        && (at(b, i + 1).is_ascii_digit() || (matches!(at(b, i + 1), b'+' | b'-') && at(b, i + 2).is_ascii_digit()))
    {
        i += 2;
        while at(b, i).is_ascii_digit() || at(b, i) == b'_' {
            i += 1;
        }
    }
    // Type suffix: 10u32, 1.5f64.
    while at(b, i).is_ascii_alphanumeric() || at(b, i) == b'_' {
        i += 1;
    }
    i
}

/// If a raw string (`r"..."`, `r#"..."#`) starts at `i` (the `r`), its end.
fn raw_string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while at(b, j) == b'#' {
        j += 1;
    }
    if at(b, j) != b'"' {
        return None;
    }
    let hashes = j - i - 1;
    let mut k = j + 1;
    while k < b.len() {
        if b[k] == b'"' && b[k + 1..].iter().take(hashes).filter(|&&c| c == b'#').count() == hashes {
            return Some(k + 1 + hashes);
        }
        k += 1;
    }
    Some(b.len())
}

/// End of the `]` matching the `[` at `i`, skipping strings.
fn bracket_end(b: &[u8], mut i: usize) -> usize {
    let mut depth = 0;
    while i < b.len() {
        match b[i] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            b'"' => {
                i = quoted_end(b, i + 1, b'"');
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// The length in bytes of the UTF-8 character starting with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

fn push(out: &mut Vec<Span>, start: usize, end: usize, kind: Kind) {
    if end > start {
        out.push(Span { start, end, kind });
    }
}

fn rust(b: &[u8], out: &mut Vec<Span>) {
    let n = b.len();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && at(b, i + 1) == b'/' {
            let e = line_end(b, i);
            push(out, i, e, Kind::Comment);
            i = e;
        } else if c == b'/' && at(b, i + 1) == b'*' {
            let e = block_comment_end(b, i, true);
            push(out, i, e, Kind::Comment);
            i = e;
        } else if c == b'"' {
            let e = quoted_end(b, i + 1, b'"');
            push(out, i, e, Kind::Str);
            i = e;
        } else if c == b'\'' {
            // A character literal, or a lifetime / loop label.
            let next = at(b, i + 1);
            if next == b'\\' {
                let mut e = i + 3;
                while e < n && b[e] != b'\'' && b[e] != b'\n' && e < i + 12 {
                    e += 1;
                }
                let e = if at(b, e) == b'\'' { e + 1 } else { i + 2 };
                push(out, i, e, Kind::Str);
                i = e;
            } else if next != 0 && next != b'\n' && at(b, i + 1 + utf8_len(next)) == b'\'' {
                let e = i + 2 + utf8_len(next);
                push(out, i, e, Kind::Str);
                i = e;
            } else if is_ident_start(next) {
                let mut e = i + 1;
                while is_ident(at(b, e)) {
                    e += 1;
                }
                push(out, i, e, Kind::Lifetime);
                i = e;
            } else {
                i += 1;
            }
        } else if c == b'#' && (at(b, i + 1) == b'[' || (at(b, i + 1) == b'!' && at(b, i + 2) == b'[')) {
            let open = if at(b, i + 1) == b'[' { i + 1 } else { i + 2 };
            let e = bracket_end(b, open);
            push(out, i, e, Kind::Attribute);
            i = e;
        } else if c.is_ascii_digit() {
            let e = number_end(b, i);
            push(out, i, e, Kind::Number);
            i = e;
        } else if is_ident_start(c) {
            // String prefixes: r"..", r#".."#, b"..", br"..", c"..", cr"..", b'.'.
            let byte_or_c = c == b'b' || c == b'c';
            let raw = if c == b'r' {
                raw_string_end(b, i)
            } else if byte_or_c && at(b, i + 1) == b'r' {
                raw_string_end(b, i + 1)
            } else {
                None
            };
            let quoted = match (byte_or_c, at(b, i + 1)) {
                _ if raw.is_some() => raw,
                (true, b'"') => Some(quoted_end(b, i + 2, b'"')),
                (true, b'\'') if c == b'b' => Some(quoted_end(b, i + 2, b'\'')),
                _ => None,
            };
            if let Some(e) = quoted {
                push(out, i, e, Kind::Str);
                i = e;
                continue;
            }
            let mut e = i;
            while is_ident(at(b, e)) {
                e += 1;
            }
            // Raw identifier: r#type is an ordinary name.
            if &b[i..e] == b"r" && at(b, e) == b'#' && is_ident_start(at(b, e + 1)) {
                e += 1;
                while is_ident(at(b, e)) {
                    e += 1;
                }
                i = e;
                continue;
            }
            let word = std::str::from_utf8(&b[i..e]).unwrap_or("");
            if at(b, e) == b'!' && at(b, e + 1) != b'=' && !RUST_KEYWORDS.contains(&word) {
                push(out, i, e + 1, Kind::Macro);
                i = e + 1;
                continue;
            }
            if RUST_KEYWORDS.contains(&word) {
                push(out, i, e, Kind::Keyword);
            } else if RUST_PRIMITIVES.contains(&word) || word.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                push(out, i, e, Kind::Type);
            }
            i = e;
        } else {
            i += 1;
        }
    }
}

const SLINT_KEYWORDS: &[&str] = &[
    "import", "from", "export", "component", "inherits", "property", "callback", "function", "public", "pure",
    "private", "protected", "in", "out", "in-out", "struct", "enum", "global", "if", "else", "for", "return",
    "animate", "states", "transitions", "when", "root", "self", "parent", "true", "false", "changed", "accept",
    "reject",
];

const SLINT_TYPES: &[&str] = &[
    "string", "int", "float", "bool", "color", "brush", "length", "physical-length", "relative-font-size",
    "duration", "angle", "percent", "image", "easing",
];

fn slint(b: &[u8], out: &mut Vec<Span>) {
    let n = b.len();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && at(b, i + 1) == b'/' {
            let e = line_end(b, i);
            push(out, i, e, Kind::Comment);
            i = e;
        } else if c == b'/' && at(b, i + 1) == b'*' {
            let e = block_comment_end(b, i, false);
            push(out, i, e, Kind::Comment);
            i = e;
        } else if c == b'"' {
            let e = quoted_end(b, i + 1, b'"');
            push(out, i, e, Kind::Str);
            i = e;
        } else if c == b'#' && at(b, i + 1).is_ascii_hexdigit() {
            let mut e = i + 1;
            while at(b, e).is_ascii_hexdigit() {
                e += 1;
            }
            push(out, i, e, Kind::Number);
            i = e;
        } else if c == b'@' && is_ident_start(at(b, i + 1)) {
            let mut e = i + 1;
            while is_ident(at(b, e)) || (at(b, e) == b'-' && is_ident_start(at(b, e + 1))) {
                e += 1;
            }
            push(out, i, e, Kind::Attribute);
            i = e;
        } else if c.is_ascii_digit() || (c == b'.' && at(b, i + 1).is_ascii_digit()) {
            // Numbers with units: 10px, 1.5s, 50%.
            let mut e = i;
            while at(b, e).is_ascii_digit() || at(b, e) == b'.' {
                e += 1;
            }
            while at(b, e).is_ascii_alphabetic() || at(b, e) == b'%' {
                e += 1;
            }
            push(out, i, e, Kind::Number);
            i = e;
        } else if is_ident_start(c) {
            // Slint names may contain dashes: font-size, in-out.
            let mut e = i;
            while is_ident(at(b, e)) || (at(b, e) == b'-' && is_ident_start(at(b, e + 1))) {
                e += 1;
            }
            let word = std::str::from_utf8(&b[i..e]).unwrap_or("");
            if SLINT_KEYWORDS.contains(&word) {
                push(out, i, e, Kind::Keyword);
            } else if SLINT_TYPES.contains(&word) || word.starts_with(|ch: char| ch.is_ascii_uppercase()) {
                push(out, i, e, Kind::Type);
            }
            i = e;
        } else {
            i += 1;
        }
    }
}

fn toml(b: &[u8], out: &mut Vec<Span>) {
    let n = b.len();
    let mut i = 0;
    let mut line_start = true;
    while i < n {
        let c = b[i];
        match c {
            b'\n' => {
                line_start = true;
                i += 1;
                continue;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let at_line_start = std::mem::replace(&mut line_start, false);
        if c == b'#' {
            let e = line_end(b, i);
            push(out, i, e, Kind::Comment);
            i = e;
        } else if c == b'[' && at_line_start {
            // [table] or [[array-of-tables]]
            let le = line_end(b, i);
            let e = b[i..le].iter().rposition(|&x| x == b']').map_or(le, |p| i + p + 1);
            push(out, i, e, Kind::Keyword);
            i = e;
        } else if c == b'"' || c == b'\'' {
            let triple = at(b, i + 1) == c && at(b, i + 2) == c;
            let e = if triple {
                let mut k = i + 3;
                while k < n && !(b[k] == c && at(b, k + 1) == c && at(b, k + 2) == c) {
                    k += if c == b'"' && b[k] == b'\\' { 2 } else { 1 };
                }
                (k + 3).min(n)
            } else if c == b'"' {
                quoted_end(b, i + 1, b'"')
            } else {
                match b[i + 1..].iter().position(|&x| x == b'\'' || x == b'\n') {
                    Some(p) if b[i + 1 + p] == b'\'' => i + 2 + p,
                    Some(p) => i + 1 + p,
                    None => n,
                }
            };
            push(out, i, e, Kind::Str);
            i = e;
        } else if c.is_ascii_digit() || ((c == b'+' || c == b'-') && at(b, i + 1).is_ascii_digit()) {
            // Numbers and dates: 1, 1.5, 1e3, 1979-05-27T07:32:00Z.
            let mut e = i + 1;
            while at(b, e).is_ascii_alphanumeric() || matches!(at(b, e), b'_' | b'.' | b':' | b'-' | b'+') {
                e += 1;
            }
            push(out, i, e, Kind::Number);
            i = e;
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'-' {
            let mut e = i;
            while at(b, e).is_ascii_alphanumeric() || matches!(at(b, e), b'_' | b'-') {
                e += 1;
            }
            let word = &b[i..e];
            let mut k = e;
            while matches!(at(b, k), b' ' | b'\t') {
                k += 1;
            }
            if word == b"true" || word == b"false" {
                push(out, i, e, Kind::Keyword);
            } else if at(b, k) == b'=' || at(b, k) == b'.' {
                push(out, i, e, Kind::Type);
            }
            i = e;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str, lang: Language) -> Vec<(&str, Kind)> {
        spans(text, lang).into_iter().map(|s| (&text[s.start..s.end], s.kind)).collect()
    }

    #[test]
    fn rust_tokens() {
        let src = r##"#[derive(Debug)]
pub struct Point<'a> { x: i32, name: &'a str }
fn main() {
    // say hello
    let c = 'x'; let n = '\n'; let s = "a \"q\" b"; let r = r#"raw "x""#;
    println!("{}", 1_000u32 + 0x1F + 2.5e-3 as u32);
    let ok = a != b; let t = (1, 2).0; 'outer: loop { break 'outer; }
    /* block /* nested */ still */ let b = b"bytes"; let r#type = 1;
}
"##;
        let k = kinds(src, Language::Rust);
        let has = |t: &str, kind: Kind| k.iter().any(|(s, kk)| *s == t && *kk == kind);
        assert!(has("#[derive(Debug)]", Kind::Attribute), "{k:?}");
        assert!(has("pub", Kind::Keyword) && has("struct", Kind::Keyword) && has("fn", Kind::Keyword));
        assert!(has("Point", Kind::Type) && has("i32", Kind::Type) && has("str", Kind::Type));
        assert!(has("'a", Kind::Lifetime) && has("'outer", Kind::Lifetime));
        assert!(has("// say hello", Kind::Comment));
        assert!(has("'x'", Kind::Str) && has("'\\n'", Kind::Str));
        assert!(has("\"a \\\"q\\\" b\"", Kind::Str), "{k:?}");
        assert!(has("r#\"raw \"x\"\"#", Kind::Str), "{k:?}");
        assert!(has("println!", Kind::Macro));
        assert!(has("1_000u32", Kind::Number) && has("0x1F", Kind::Number) && has("2.5e-3", Kind::Number));
        assert!(has("0", Kind::Number), "tuple index");
        assert!(has("/* block /* nested */ still */", Kind::Comment), "{k:?}");
        assert!(has("b\"bytes\"", Kind::Str));
        assert!(!k.iter().any(|(s, _)| *s == "a" || *s == "b" || s.contains("type")), "{k:?}");
    }

    #[test]
    fn slint_and_toml_tokens() {
        let k = kinds(
            "import { Button } from \"std-widgets.slint\";\nexport component A inherits Window {\n    in-out property <length> w: 10px; // c\n    background: #ff8800;\n    @children\n}\n",
            Language::Slint,
        );
        let has = |t: &str, kind: Kind| k.iter().any(|(s, kk)| *s == t && *kk == kind);
        assert!(has("import", Kind::Keyword) && has("in-out", Kind::Keyword) && has("property", Kind::Keyword));
        assert!(has("Button", Kind::Type) && has("length", Kind::Type) && has("Window", Kind::Type));
        assert!(has("10px", Kind::Number) && has("#ff8800", Kind::Number) && has("@children", Kind::Attribute));
        assert!(has("\"std-widgets.slint\"", Kind::Str) && has("// c", Kind::Comment));
        assert!(!k.iter().any(|(s, _)| *s == "background"), "{k:?}");

        let k = kinds("[package]\nname = \"x\" # c\nedition = 2021\n[dependencies]\nslint = { version = \"1.18\", default-features = false }\n", Language::Toml);
        let has = |t: &str, kind: Kind| k.iter().any(|(s, kk)| *s == t && *kk == kind);
        assert!(has("[package]", Kind::Keyword) && has("[dependencies]", Kind::Keyword));
        assert!(has("name", Kind::Type) && has("version", Kind::Type) && has("default-features", Kind::Type));
        assert!(has("\"x\"", Kind::Str) && has("# c", Kind::Comment) && has("2021", Kind::Number));
        assert!(has("false", Kind::Keyword));
    }

    #[test]
    fn layers_line_up_and_cover_the_text() {
        let src = "fn main() {\n\tlet s = \"hé\"; // ünï\n    let 日本 = 1;\n}";
        let layers = layers(src, Language::Rust);
        // Every layer has the same lines; each visible character of the
        // source appears in exactly one layer at the same column.
        let src_lines: Vec<&str> = src.split('\n').collect();
        for (_, t) in &layers {
            assert_eq!(t.split('\n').count(), src_lines.len(), "{t:?}");
        }
        for (li, line) in src_lines.iter().enumerate() {
            for (col, ch) in line.chars().enumerate() {
                let owners: Vec<_> = layers
                    .iter()
                    .filter(|(_, t)| t.split('\n').nth(li).and_then(|l| l.chars().nth(col)) == Some(ch))
                    .map(|(k, _)| *k)
                    .collect();
                if ch.is_whitespace() {
                    continue;
                }
                assert_eq!(owners.len(), 1, "{ch:?} at {li}:{col} in {owners:?}");
            }
        }
        let kind_of = |k: Kind| layers.iter().find(|(kk, _)| *kk == k).map(|(_, t)| t.as_str()).unwrap();
        assert_eq!(kind_of(Kind::Keyword).split('\n').next(), Some("fn"));
        assert_eq!(kind_of(Kind::Str).split('\n').nth(1), Some("\t        \"hé\""));
        // The line with wide characters is drawn uncoloured.
        assert_eq!(kind_of(Kind::Plain).split('\n').nth(2), Some("    let 日本 = 1;"));
        assert!(!layers.iter().any(|(_, t)| t.lines().any(|l| l.ends_with(' '))));
    }
}
