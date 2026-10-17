//! Code editor actions and the rust-analyzer connection.

use std::path::{Path, PathBuf};

use ferrite_lsp::{apply_edits, path_key, Edit, Location, LspClient, LspEvent};
use slint::{ModelRc, VecModel};

use crate::app::{post, App, View};
use crate::CompletionRow;

/// Byte range of the identifier around `offset`.
pub fn ident_range(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..offset].char_indices().rev().take_while(|(_, c)| is_ident(*c)).last().map_or(offset, |(i, _)| i);
    let end = text[offset..].char_indices().find(|(_, c)| !is_ident(*c)).map_or(text.len(), |(i, _)| offset + i);
    (start, end)
}

/// Text inserted by an edit, if the edit was a pure insertion.
fn inserted<'a>(old: &str, new: &'a str) -> Option<(usize, &'a str)> {
    if new.len() <= old.len() {
        return None;
    }
    let prefix = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    let mut p = prefix;
    while !new.is_char_boundary(p) {
        p -= 1;
    }
    let len = new.len() - old.len();
    (new.get(p + len..) == old.get(p..)).then(|| (p, &new[p..p + len]))
}

/// The newline and indentation to insert for Enter at `offset`, and where
/// the caret goes within it.
pub fn newline_with_indent(text: &str, offset: usize) -> (String, usize) {
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = text[line_start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let before = text[line_start..offset].trim_end();
    let opens = before.ends_with('{') || before.ends_with('(') || before.ends_with('[');
    let closes_next = text[offset..].trim_start_matches(' ').starts_with(['}', ')', ']']);
    if opens && closes_next {
        let s = format!("\n{indent}    \n{indent}");
        let caret = 1 + indent.len() + 4;
        (s, caret)
    } else if opens {
        let s = format!("\n{indent}    ");
        let caret = s.len();
        (s, caret)
    } else {
        let s = format!("\n{indent}");
        let caret = s.len();
        (s, caret)
    }
}

impl App {
    pub fn start_lsp(&mut self) {
        let Some(root) = self.root().map(Path::to_path_buf) else { return };
        let Some(server) = ferrite_lsp::find_server() else {
            self.ui().set_lsp_status("rust-analyzer not found".into());
            self.message(
                "rust-analyzer was not found. Install it with: rustup component add rust-analyzer rust-src \
                 (or set FERRITE_RUST_ANALYZER to a rust-analyzer executable)",
                "warning",
            );
            return;
        };
        let cargo_bin = dirs::home_dir().map(|h| h.join(".cargo").join("bin"));
        if !cargo_bin.is_some_and(|b| server.starts_with(b)) {
            self.message(format!("rust-analyzer: using {}", server.display()), "info");
        }
        self.lsp_generation += 1;
        let gen = self.lsp_generation;
        self.ui().set_lsp_status("rust-analyzer: starting".into());
        std::thread::spawn(move || {
            let result = LspClient::start(&root, &server, move |ev| {
                post(move |a| {
                    if a.lsp_generation == gen {
                        a.on_lsp(ev)
                    }
                })
            });
            post(move |a| {
                if a.lsp_generation != gen {
                    if let Ok(c) = result {
                        c.shutdown();
                    }
                    return;
                }
                match result {
                    Ok(client) => {
                        for b in a.buffers.iter().filter(|b| b.is_rust()) {
                            let _ = client.did_open(&b.path, &b.text);
                        }
                        a.lsp = Some(client);
                        a.lsp_base = "rust-analyzer: indexing".into();
                        a.ui().set_lsp_status("rust-analyzer: indexing".into());
                    }
                    Err(e) => {
                        a.ui().set_lsp_status("rust-analyzer failed".into());
                        a.message(format!("rust-analyzer: {e}"), "error");
                    }
                }
            });
        });
    }

    pub fn on_lsp(&mut self, ev: LspEvent) {
        match ev {
            LspEvent::Diagnostics { path, items } => {
                let key = path_key(&path);
                if items.is_empty() {
                    self.lsp_diags.remove(&key);
                } else {
                    self.lsp_diags.insert(key, (path, items));
                }
                self.refresh_problems();
            }
            // serverStatus says whether the server is idle; $/progress
            // reports work in between and ends with an empty message.
            LspEvent::Status(s) if s.starts_with("rust-analyzer") => {
                let was_ready = self.lsp_ready;
                self.lsp_ready = s == "rust-analyzer: ready";
                self.lsp_base = s.clone();
                self.ui().set_lsp_status(s.into());
                if self.lsp_ready && !was_ready {
                    self.retry_pending_completion();
                }
            }
            LspEvent::Status(s) if s.is_empty() => self.ui().set_lsp_status(self.lsp_base.as_str().into()),
            LspEvent::Status(s) => self.ui().set_lsp_status(format!("rust-analyzer: {s}").into()),
            LspEvent::Exited => {
                self.lsp = None;
                self.lsp_ready = false;
                self.ui().set_lsp_status("rust-analyzer stopped".into());
            }
            _ => {}
        }
    }

    fn current_rust(&self) -> Option<(usize, PathBuf)> {
        let i = self.current.filter(|_| self.view == View::Code)?;
        self.buffers[i].is_rust().then(|| (i, self.buffers[i].path.clone()))
    }

    pub fn editor_edited(&mut self, text: String) {
        let Some(i) = self.current else { return };
        self.hide_tip();
        let was_dirty = self.buffers[i].dirty();
        let old = std::mem::replace(&mut self.buffers[i].text, text);
        let typed = inserted(&old, &self.buffers[i].text).map(|(at, s)| (at, s.to_string()));
        self.refresh_editor(false);
        if was_dirty != self.buffers[i].dirty() {
            self.refresh_tabs();
        }
        if let (Some(lsp), true) = (&self.lsp, self.buffers[i].is_rust()) {
            let _ = lsp.did_change(&self.buffers[i].path, &self.buffers[i].text);
        }
        if !self.hover.is_empty() {
            self.hover.clear();
            self.ui().set_hover_text("".into());
        }
        match typed {
            Some((at, s)) => {
                self.caret_offset = at + s.len();
                let text = &self.buffers[i].text;
                let triggers = s == "." || (s == ":" && text[..at].ends_with(':'));
                let word = s.chars().all(|c| c.is_alphanumeric() || c == '_');
                if triggers {
                    self.request_completion(false);
                } else if self.ui().get_completion_open() || self.pending_completion.is_some() {
                    if word {
                        self.filter_completions();
                        // The server cut the list short: ask again with
                        // the longer prefix.
                        if self.completion_incomplete || self.pending_completion.is_some() {
                            self.request_completion(false);
                        }
                    } else {
                        self.close_popups();
                    }
                }
            }
            None => {
                if self.ui().get_completion_open() || self.pending_completion.is_some() {
                    self.close_popups();
                }
            }
        }
    }

    /// The editor reports the caret's exact byte offset; the line is derived
    /// from it rather than from pixel positions, which lag behind layout
    /// right after the text is replaced.
    pub fn caret_moved(&mut self, _line: i32, offset: i32) {
        let Some(i) = self.current else { return };
        let text = &self.buffers[i].text;
        let mut offset = (offset.max(0) as usize).min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        self.caret_line = text[..offset].matches('\n').count();
        self.caret_offset = offset;
        self.buffers[i].caret = offset;
        self.refresh_caret_text();
        if !self.hover.is_empty() {
            self.hover.clear();
            self.ui().set_hover_text("".into());
        }
    }

    pub fn close_popups(&mut self) {
        let ui = self.ui();
        ui.set_completion_open(false);
        self.shown_completions.clear();
        self.pending_completion = None;
        self.hide_tip();
        if !self.hover.is_empty() {
            self.hover.clear();
            ui.set_hover_text("".into());
        }
    }

    /// Asks rust-analyzer for completions at the caret. `explicit` is
    /// Ctrl+Space, which reports when there is nothing to offer; `.` and
    /// `::` open the list quietly.
    pub fn request_completion(&mut self, explicit: bool) {
        let Some((_, path)) = self.current_rust() else { return };
        let Some(lsp) = self.lsp.clone() else {
            self.status("Code completion needs rust-analyzer, which is not running");
            return;
        };
        let offset = self.caret_offset;
        if !self.lsp_ready {
            // Until the project is loaded (build scripts run, macros
            // expanded) the answer would lack the form's methods; show what
            // there is now and ask again when rust-analyzer is ready.
            self.pending_completion = Some((path.clone(), offset));
            self.status("rust-analyzer is still loading the project; completion will open when it is ready");
        }
        let target = path.clone();
        let _ = lsp.completion(&target, offset, move |r| post(move |a| a.completion_arrived(&path, offset, explicit, r)));
    }

    /// Whether the caret is still where a completion was asked for, or
    /// further along the same word.
    fn caret_still_at(&self, path: &Path, offset: usize) -> bool {
        let Some(i) = self.current.filter(|_| self.view == View::Code) else { return false };
        let text = &self.buffers[i].text;
        path_key(&self.buffers[i].path) == path_key(path)
            && self.caret_offset >= offset
            && text.get(offset..self.caret_offset).is_some_and(|s| s.chars().all(|c| c.is_alphanumeric() || c == '_'))
    }

    fn completion_arrived(
        &mut self,
        path: &Path,
        offset: usize,
        explicit: bool,
        result: Result<ferrite_lsp::CompletionList, String>,
    ) {
        if !self.caret_still_at(path, offset) {
            return;
        }
        let list = match result {
            Ok(list) => list,
            Err(e) => {
                self.status(&format!("Code completion failed: {e}"));
                return;
            }
        };
        if self.lsp_ready {
            self.pending_completion = None;
        }
        self.completion_incomplete = list.incomplete;
        if list.items.is_empty() {
            self.completions.clear();
            self.ui().set_completion_open(false);
            if explicit && self.lsp_ready {
                self.status("No completions at the caret");
            }
            return;
        }
        self.completions = list.items;
        self.filter_completions();
    }

    /// rust-analyzer became ready: answer a completion asked for while it
    /// was loading, if the caret has not moved away.
    pub fn retry_pending_completion(&mut self) {
        let Some((path, offset)) = self.pending_completion.take() else { return };
        if self.caret_still_at(&path, offset) {
            self.request_completion(false);
        }
    }

    fn filter_completions(&mut self) {
        let Some(i) = self.current else { return };
        let text = &self.buffers[i].text;
        let (start, _) = ident_range(text, self.caret_offset);
        let prefix = text[start..self.caret_offset.min(text.len())].to_lowercase();
        self.shown_completions = self
            .completions
            .iter()
            .enumerate()
            .filter(|(_, c)| prefix.is_empty() || c.label.to_lowercase().starts_with(&prefix))
            .map(|(k, _)| k)
            .take(200)
            .collect();
        let ui = self.ui();
        if self.shown_completions.is_empty() {
            ui.set_completion_open(false);
            return;
        }
        let rows: Vec<CompletionRow> = self
            .shown_completions
            .iter()
            .map(|&k| CompletionRow {
                label: self.completions[k].label.as_str().into(),
                detail: self.completions[k].detail.as_str().into(),
            })
            .collect();
        ui.set_completions(ModelRc::new(VecModel::from(rows)));
        ui.set_completion_index(0);
        ui.set_completion_open(true);
    }

    pub fn completion_chosen(&mut self, k: usize) {
        let Some(&idx) = self.shown_completions.get(k) else { return };
        let Some(i) = self.current else { return };
        let c = self.completions[idx].clone();
        let text = &self.buffers[i].text;
        let start = c.start.min(text.len());
        let end = c.end.max(self.caret_offset).min(text.len()).max(start);
        let new_text = apply_edits(text, &[Edit { start, end, text: c.text.clone() }]);
        self.close_popups();
        self.replace_text(i, new_text, start + c.text.len());
    }

    /// Replaces a buffer's text from code (not typing) and moves the caret.
    pub fn replace_text(&mut self, i: usize, text: String, caret: usize) {
        self.buffers[i].text = text;
        if let (Some(lsp), true) = (&self.lsp, self.buffers[i].is_rust()) {
            let _ = lsp.did_change(&self.buffers[i].path, &self.buffers[i].text);
        }
        if self.current == Some(i) {
            self.refresh_editor(true);
            self.goto_offset(caret);
        }
        self.refresh_tabs();
    }

    pub fn insert_at_caret(&mut self, s: &str, caret_in_s: usize) {
        let Some(i) = self.current else { return };
        if self.buffers[i].read_only {
            return;
        }
        let at = self.caret_offset.min(self.buffers[i].text.len());
        let mut text = self.buffers[i].text.clone();
        text.insert_str(at, s);
        self.replace_text(i, text, at + caret_in_s);
    }

    pub fn special_key(&mut self, key: &str) {
        match key {
            "tab" => self.insert_at_caret("    ", 4),
            "return" => {
                let Some(i) = self.current else { return };
                let (s, caret) = newline_with_indent(&self.buffers[i].text, self.caret_offset.min(self.buffers[i].text.len()));
                self.insert_at_caret(&s, caret);
            }
            _ => self.close_popups(),
        }
    }

    pub fn hover_at_caret(&mut self) {
        let Some((_, path)) = self.current_rust() else { return };
        let Some(lsp) = self.lsp.clone() else { return };
        let _ = lsp.hover(&path, self.caret_offset, |h| {
            post(move |a| match h {
                Some(text) => {
                    a.hover = text.clone();
                    a.ui().set_hover_text(text.into());
                }
                None => a.status("No information at the caret"),
            })
        });
    }

    pub fn goto_definition(&mut self) {
        let Some((_, path)) = self.current_rust() else { return };
        let Some(lsp) = self.lsp.clone() else { return };
        let _ = lsp.definition(&path, self.caret_offset, |loc| {
            post(move |a| match loc {
                Some(l) => a.goto_location(l),
                None => a.status("No declaration found"),
            })
        });
    }

    fn goto_location(&mut self, loc: Location) {
        let enc = self.lsp.as_ref().map_or(ferrite_lsp::Encoding::Utf8, |l| l.encoding());
        self.open_at(&loc.path, |text| ferrite_lsp::text::position_to_offset(text, loc.line, loc.character, enc));
    }

    pub fn format_document(&mut self) {
        let Some((i, path)) = self.current_rust() else { return };
        if self.buffers[i].read_only {
            return;
        }
        let Some(lsp) = self.lsp.clone() else { return };
        let target = path.clone();
        let _ = lsp.formatting(&target, move |edits| {
            post(move |a| {
                let Some(i) = a.buffer_index(&path) else { return };
                if edits.is_empty() {
                    a.status("Already formatted");
                    return;
                }
                let text = apply_edits(&a.buffers[i].text, &edits);
                let caret = a.caret_offset.min(text.len());
                a.replace_text(i, text, caret);
                a.status("Formatted with rustfmt");
            })
        });
    }

    pub fn add_watch_at_caret(&mut self) {
        let Some(i) = self.current else { return };
        let text = &self.buffers[i].text;
        let (s, e) = ident_range(text, self.caret_offset);
        if s < e {
            let word = text[s..e].to_string();
            self.add_watch(word);
            self.ui().set_bottom_tab(4);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_keeps_and_extends_indentation() {
        let t = "fn a() {\n    if x {}\n}";
        let at = t.find("{}").unwrap() + 1;
        let (s, caret) = newline_with_indent(t, at);
        assert_eq!(s, "\n        \n    ");
        assert_eq!(caret, 9);
        let (s, _) = newline_with_indent(t, t.find("x {").unwrap());
        assert_eq!(s, "\n    ");
    }

    #[test]
    fn insertions_and_identifiers_are_found() {
        assert_eq!(inserted("ab", "a.b"), Some((1, ".")));
        assert_eq!(inserted("abc", "ab"), None);
        assert_eq!(ident_range("let form_x = 1;", 7), (4, 10));
    }
}
