//! A small rust-analyzer client.
//!
//! The IDE talks in byte offsets; this crate converts to and from LSP
//! positions (UTF-8 when the server agrees, UTF-16 otherwise), keeps the
//! text of every open document, and turns server notifications into
//! [`LspEvent`]s. Results arrive on the reader thread through callbacks,
//! and the IDE forwards them to its UI thread.

pub mod text;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use ferrite_rpc::Endpoint;
use lsp_types::{CompletionResponse, CompletionTextEdit, Hover, HoverContents, MarkedString};
use serde_json::{json, Value};

pub use text::Encoding;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A diagnostic converted to byte offsets in the document it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    pub line: u32,
    pub start: usize,
    pub end: usize,
    pub severity: Severity,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum LspEvent {
    Diagnostics { path: PathBuf, items: Vec<Diag> },
    /// Indexing progress and readiness, for the status bar.
    Status(String),
    Log(String),
    Exited,
}

/// One completion entry, with the edit already in byte offsets.
#[derive(Clone, Debug)]
pub struct Completion {
    pub label: String,
    pub detail: String,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// The server's answer to a completion request.
#[derive(Clone, Debug, Default)]
pub struct CompletionList {
    pub items: Vec<Completion>,
    /// The server left items out; ask again as the user keeps typing.
    pub incomplete: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub line: u32,
    /// Column in the server's encoding; see [`LspClient::encoding`].
    pub character: u32,
}

/// A text edit in byte offsets of the document snapshot it was made for.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

type Pending = Arc<Mutex<HashMap<i64, Box<dyn FnOnce(Result<Value, String>) + Send>>>>;
type Docs = Arc<Mutex<HashMap<String, (i32, String)>>>;

pub struct LspClient {
    endpoint: Arc<Endpoint>,
    next_id: AtomicI64,
    pending: Pending,
    docs: Docs,
    encoding: Encoding,
}

fn runs(server: &Path) -> bool {
    let mut cmd = Command::new(server);
    cmd.arg("--version");
    ferrite_rpc::no_console(&mut cmd);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Version numbers in a name such as `rust-lang.rust-analyzer-0.3.3081-win32-x64`.
fn version_key(name: &str) -> Vec<u32> {
    name.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect()
}

/// Every place a rust-analyzer may live, best first: `FERRITE_RUST_ANALYZER`,
/// PATH, `~/.cargo/bin` (the rustup proxy works only once the component is
/// installed), then servers bundled with the VS Code family's rust-analyzer
/// extension, newest first.
pub fn server_candidates() -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "rust-analyzer.exe" } else { "rust-analyzer" };
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("FERRITE_RUST_ANALYZER") {
        out.push(PathBuf::from(p));
    }
    if let Some(path) = std::env::var_os("PATH") {
        out.extend(std::env::split_paths(&path).map(|d| d.join(exe)));
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    if let Some(home) = home {
        out.push(home.join(".cargo").join("bin").join(exe));
        let mut bundled: Vec<PathBuf> = [".vscode", ".vscode-insiders", ".vscode-oss", ".cursor"]
            .iter()
            .flat_map(|editor| std::fs::read_dir(home.join(editor).join("extensions")).into_iter().flatten().flatten())
            .map(|e| e.path())
            .filter(|d| d.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rust-lang.rust-analyzer-")))
            .map(|d| d.join("server").join(exe))
            .collect();
        bundled.sort_by_key(|p| {
            let dir = p.parent().and_then(Path::parent).and_then(Path::file_name);
            std::cmp::Reverse(dir.map(|n| version_key(&n.to_string_lossy())).unwrap_or_default())
        });
        out.extend(bundled);
    }
    out.dedup();
    out
}

/// The first rust-analyzer that actually runs.
pub fn find_server() -> Option<PathBuf> {
    server_candidates().into_iter().filter(|p| p.is_file()).find(|p| runs(p))
}

/// `C:\a b\c.rs` -> `file:///C:/a%20b/c.rs`, `/a/b.rs` -> `file:///a/b.rs`.
pub fn path_to_uri(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                decoded.push(b);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i]);
        i += 1;
    }
    let s = String::from_utf8(decoded).ok()?;
    let is_drive = s.len() >= 3 && s.as_bytes()[0] == b'/' && s.as_bytes()[2] == b':';
    if is_drive {
        Some(PathBuf::from(s[1..].replace('/', "\\")))
    } else {
        Some(PathBuf::from(s))
    }
}

/// Path key for comparing files across the IDE and the server, which may
/// spell a Windows drive letter differently.
pub fn path_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn severity(v: Option<lsp_types::DiagnosticSeverity>) -> Severity {
    match v {
        Some(lsp_types::DiagnosticSeverity::ERROR) | None => Severity::Error,
        Some(lsp_types::DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(lsp_types::DiagnosticSeverity::INFORMATION) => Severity::Info,
        _ => Severity::Hint,
    }
}

/// Hover contents as plain text, with code fences dropped.
pub fn hover_text(h: &Hover) -> String {
    let raw = match &h.contents {
        HoverContents::Markup(m) => m.value.clone(),
        HoverContents::Scalar(s) => marked(s),
        HoverContents::Array(a) => a.iter().map(marked).collect::<Vec<_>>().join("\n\n"),
    };
    raw.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .map(|l| l.replace("\\_", "_"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn marked(s: &MarkedString) -> String {
    match s {
        MarkedString::String(s) => s.clone(),
        MarkedString::LanguageString(l) => l.value.clone(),
    }
}

impl LspClient {
    /// Starts the server for a cargo project and completes the
    /// `initialize` handshake. Blocks for up to a minute, so call it from
    /// a worker thread.
    pub fn start(
        root: &Path,
        server: &Path,
        on_event: impl Fn(LspEvent) + Send + Sync + 'static,
    ) -> io::Result<Arc<LspClient>> {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let docs: Docs = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_cell: Arc<OnceLock<Weak<Endpoint>>> = Arc::new(OnceLock::new());
        let encoding_cell: Arc<OnceLock<Encoding>> = Arc::new(OnceLock::new());
        let on_event = Arc::new(on_event);

        let mut cmd = Command::new(server);
        cmd.current_dir(root);
        let reader_pending = pending.clone();
        let reader_docs = docs.clone();
        let reader_endpoint = endpoint_cell.clone();
        let reader_encoding = encoding_cell.clone();
        let ev_msg = on_event.clone();
        let ev_err = on_event.clone();
        let ev_exit = on_event.clone();
        let endpoint = Endpoint::spawn(
            cmd,
            move |msg| {
                handle_message(msg, &reader_pending, &reader_docs, &reader_endpoint, &reader_encoding, &*ev_msg)
            },
            move |line| ev_err(LspEvent::Log(line)),
            move || ev_exit(LspEvent::Exited),
        )?;
        let _ = endpoint_cell.set(Arc::downgrade(&endpoint));

        let client = LspClient {
            endpoint,
            next_id: AtomicI64::new(1),
            pending,
            docs,
            encoding: Encoding::Utf8,
        };

        let root_uri = path_to_uri(root);
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "Ferrite Studio", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "window": { "workDoneProgress": true },
                "experimental": { "serverStatusNotification": true },
                "textDocument": {
                    "synchronization": { "didSave": true },
                    "completion": { "completionItem": { "snippetSupport": false } },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": {},
                    "formatting": {},
                    "publishDiagnostics": { "relatedInformation": false }
                }
            },
            "initializationOptions": { "checkOnSave": true }
        });
        let (tx, rx) = mpsc::channel();
        client.request("initialize", params, move |r| {
            let _ = tx.send(r);
        })?;
        let result = rx
            .recv_timeout(Duration::from_secs(60))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "rust-analyzer did not answer initialize"))?
            .map_err(io::Error::other)?;
        let encoding = match result.pointer("/capabilities/positionEncoding").and_then(Value::as_str) {
            Some("utf-8") => Encoding::Utf8,
            _ => Encoding::Utf16,
        };
        let _ = encoding_cell.set(encoding);
        client.notify("initialized", json!({}))?;
        Ok(Arc::new(LspClient { encoding, ..client }))
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    fn request(
        &self,
        method: &str,
        params: Value,
        cb: impl FnOnce(Result<Value, String>) + Send + 'static,
    ) -> io::Result<()> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(id, Box::new(cb));
        let sent = self
            .endpoint
            .send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        if sent.is_err() {
            self.pending.lock().unwrap().remove(&id);
        }
        sent
    }

    fn notify(&self, method: &str, params: Value) -> io::Result<()> {
        self.endpoint.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn snapshot(&self, path: &Path) -> Option<String> {
        self.docs.lock().unwrap().get(&path_to_uri(path)).map(|(_, t)| t.clone())
    }

    fn pos(&self, text: &str, offset: usize) -> Value {
        let (line, character) = text::offset_to_position(text, offset, self.encoding);
        json!({ "line": line, "character": character })
    }

    pub fn did_open(&self, path: &Path, text: &str) -> io::Result<()> {
        let uri = path_to_uri(path);
        self.docs.lock().unwrap().insert(uri.clone(), (1, text.to_string()));
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "rust", "version": 1, "text": text } }),
        )
    }

    pub fn is_open(&self, path: &Path) -> bool {
        self.docs.lock().unwrap().contains_key(&path_to_uri(path))
    }

    /// Sends the whole new text (full document sync).
    pub fn did_change(&self, path: &Path, text: &str) -> io::Result<()> {
        let uri = path_to_uri(path);
        let version = {
            let mut docs = self.docs.lock().unwrap();
            let entry = docs.entry(uri.clone()).or_insert((0, String::new()));
            entry.0 += 1;
            entry.1 = text.to_string();
            entry.0
        };
        self.notify(
            "textDocument/didChange",
            json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
        )
    }

    /// Triggers rust-analyzer's check-on-save (`cargo check`).
    pub fn did_save(&self, path: &Path) -> io::Result<()> {
        self.notify("textDocument/didSave", json!({ "textDocument": { "uri": path_to_uri(path) } }))
    }

    /// Asks for completions at `offset`. The callback gets the server's
    /// error message if the request failed.
    pub fn completion(
        &self,
        path: &Path,
        offset: usize,
        cb: impl FnOnce(Result<CompletionList, String>) + Send + 'static,
    ) -> io::Result<()> {
        let Some(text) = self.snapshot(path) else { return Ok(cb(Err("the file is not open".into()))) };
        let enc = self.encoding;
        // Default replacement range: the identifier left of the caret.
        let ident_start = text[..offset.min(text.len())]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map_or(offset, |(i, _)| i);
        let params = json!({ "textDocument": { "uri": path_to_uri(path) }, "position": self.pos(&text, offset) });
        self.request("textDocument/completion", params, move |r| {
            let v = match r {
                Ok(v) => v,
                Err(e) => return cb(Err(e)),
            };
            let (mut items, incomplete) = match serde_json::from_value::<Option<CompletionResponse>>(v) {
                Ok(Some(CompletionResponse::Array(items))) => (items, false),
                Ok(Some(CompletionResponse::List(list))) => (list.items, list.is_incomplete),
                Ok(None) => (Vec::new(), false),
                Err(e) => return cb(Err(format!("unreadable completion reply: {e}"))),
            };
            items.sort_by(|a, b| {
                a.sort_text.as_deref().unwrap_or(&a.label).cmp(b.sort_text.as_deref().unwrap_or(&b.label))
            });
            let out = items
                .into_iter()
                .take(200)
                .map(|item| {
                    let (start, end, new_text) = match &item.text_edit {
                        Some(CompletionTextEdit::Edit(e)) => (
                            text::position_to_offset(&text, e.range.start.line, e.range.start.character, enc),
                            text::position_to_offset(&text, e.range.end.line, e.range.end.character, enc),
                            e.new_text.clone(),
                        ),
                        Some(CompletionTextEdit::InsertAndReplace(e)) => (
                            text::position_to_offset(&text, e.replace.start.line, e.replace.start.character, enc),
                            text::position_to_offset(&text, e.replace.end.line, e.replace.end.character, enc),
                            e.new_text.clone(),
                        ),
                        None => {
                            (ident_start, offset, item.insert_text.clone().unwrap_or_else(|| item.label.clone()))
                        }
                    };
                    Completion {
                        label: item.label.clone(),
                        detail: item.detail.clone().unwrap_or_default(),
                        start,
                        end,
                        text: new_text,
                    }
                })
                .collect();
            cb(Ok(CompletionList { items: out, incomplete }))
        })
    }

    pub fn hover(&self, path: &Path, offset: usize, cb: impl FnOnce(Option<String>) + Send + 'static) -> io::Result<()> {
        let Some(text) = self.snapshot(path) else { return Ok(cb(None)) };
        let params = json!({ "textDocument": { "uri": path_to_uri(path) }, "position": self.pos(&text, offset) });
        self.request("textDocument/hover", params, move |r| {
            let hover = r.ok().and_then(|v| serde_json::from_value::<Option<Hover>>(v).ok()).flatten();
            cb(hover.map(|h| hover_text(&h)).filter(|s| !s.is_empty()))
        })
    }

    pub fn definition(
        &self,
        path: &Path,
        offset: usize,
        cb: impl FnOnce(Option<Location>) + Send + 'static,
    ) -> io::Result<()> {
        let Some(text) = self.snapshot(path) else { return Ok(cb(None)) };
        let params = json!({ "textDocument": { "uri": path_to_uri(path) }, "position": self.pos(&text, offset) });
        self.request("textDocument/definition", params, move |r| {
            let Ok(v) = r else { return cb(None) };
            let first = if v.is_array() { v.get(0).cloned() } else { Some(v) };
            let loc = first.and_then(|l| {
                // Location { uri, range } or LocationLink { targetUri, targetSelectionRange }
                let uri = l.get("uri").or_else(|| l.get("targetUri"))?.as_str()?.to_string();
                let range = l.get("range").or_else(|| l.get("targetSelectionRange"))?;
                Some(Location {
                    path: uri_to_path(&uri)?,
                    line: range.pointer("/start/line")?.as_u64()? as u32,
                    character: range.pointer("/start/character")?.as_u64()? as u32,
                })
            });
            cb(loc)
        })
    }

    pub fn formatting(&self, path: &Path, cb: impl FnOnce(Vec<Edit>) + Send + 'static) -> io::Result<()> {
        let Some(text) = self.snapshot(path) else { return Ok(cb(Vec::new())) };
        let enc = self.encoding;
        let params = json!({
            "textDocument": { "uri": path_to_uri(path) },
            "options": { "tabSize": 4, "insertSpaces": true }
        });
        self.request("textDocument/formatting", params, move |r| {
            let edits: Vec<lsp_types::TextEdit> =
                r.ok().and_then(|v| serde_json::from_value::<Option<_>>(v).ok()).flatten().unwrap_or_default();
            let out = edits
                .into_iter()
                .map(|e| Edit {
                    start: text::position_to_offset(&text, e.range.start.line, e.range.start.character, enc),
                    end: text::position_to_offset(&text, e.range.end.line, e.range.end.character, enc),
                    text: e.new_text,
                })
                .collect();
            cb(out)
        })
    }

    /// Polite shutdown; the process is killed when the client is dropped.
    pub fn shutdown(&self) {
        let _ = self.request("shutdown", Value::Null, |_| {});
        let _ = self.notify("exit", Value::Null);
    }
}

/// Applies non-overlapping edits computed against `text`.
pub fn apply_edits(text: &str, edits: &[Edit]) -> String {
    let mut sorted: Vec<&Edit> = edits.iter().collect();
    sorted.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = text.to_string();
    for e in sorted {
        if e.start <= e.end && e.end <= out.len() {
            out.replace_range(e.start..e.end, &e.text);
        }
    }
    out
}

fn handle_message(
    msg: Value,
    pending: &Pending,
    docs: &Docs,
    endpoint: &OnceLock<Weak<Endpoint>>,
    encoding: &OnceLock<Encoding>,
    on_event: &(dyn Fn(LspEvent) + Send + Sync),
) {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).map(str::to_string);
    match (id, method) {
        // Response to one of our requests.
        (Some(id), None) => {
            let Some(id) = id.as_i64() else { return };
            let cb = pending.lock().unwrap().remove(&id);
            if let Some(cb) = cb {
                let result = match msg.get("error") {
                    Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
                    None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                };
                cb(result);
            }
        }
        // Request from the server: answer so it never waits on us.
        (Some(id), Some(method)) => {
            let result = match method.as_str() {
                "workspace/configuration" => {
                    let n = msg.pointer("/params/items").and_then(Value::as_array).map_or(0, Vec::len);
                    Value::Array(vec![Value::Null; n])
                }
                _ => Value::Null,
            };
            if let Some(ep) = endpoint.get().and_then(Weak::upgrade) {
                let _ = ep.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
            }
        }
        (None, Some(method)) => {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            match method.as_str() {
                "textDocument/publishDiagnostics" => {
                    let Some(uri) = params.get("uri").and_then(Value::as_str) else { return };
                    let Some(path) = uri_to_path(uri) else { return };
                    let enc = encoding.get().copied().unwrap_or(Encoding::Utf16);
                    let text = docs
                        .lock()
                        .unwrap()
                        .get(uri)
                        .map(|(_, t)| t.clone())
                        .or_else(|| std::fs::read_to_string(&path).ok())
                        .unwrap_or_default();
                    let raw: Vec<lsp_types::Diagnostic> = params
                        .get("diagnostics")
                        .cloned()
                        .and_then(|d| serde_json::from_value(d).ok())
                        .unwrap_or_default();
                    let items = raw
                        .into_iter()
                        .map(|d| Diag {
                            line: d.range.start.line,
                            start: text::position_to_offset(&text, d.range.start.line, d.range.start.character, enc),
                            end: text::position_to_offset(&text, d.range.end.line, d.range.end.character, enc),
                            severity: severity(d.severity),
                            message: d.message,
                        })
                        .collect();
                    on_event(LspEvent::Diagnostics { path, items });
                }
                "$/progress" => {
                    let v = &params["value"];
                    let title = v.get("title").and_then(Value::as_str).unwrap_or("");
                    let message = v.get("message").and_then(Value::as_str).unwrap_or("");
                    let pct = v.get("percentage").and_then(Value::as_u64);
                    let text = match (v.get("kind").and_then(Value::as_str), pct) {
                        (Some("end"), _) => String::new(),
                        (_, Some(p)) => format!("{title} {p}% {message}"),
                        _ => format!("{title} {message}"),
                    };
                    on_event(LspEvent::Status(text.trim().to_string()));
                }
                "experimental/serverStatus" => {
                    let quiescent = params.get("quiescent").and_then(Value::as_bool).unwrap_or(false);
                    let health = params.get("health").and_then(Value::as_str).unwrap_or("ok");
                    let text = match (health, quiescent) {
                        ("ok", true) => "rust-analyzer: ready".to_string(),
                        ("ok", false) => "rust-analyzer: working".to_string(),
                        (h, _) => format!(
                            "rust-analyzer: {h} {}",
                            params.get("message").and_then(Value::as_str).unwrap_or("")
                        ),
                    };
                    on_event(LspEvent::Status(text.trim().to_string()));
                }
                "window/logMessage" | "window/showMessage" => {
                    if let Some(m) = params.get("message").and_then(Value::as_str) {
                        on_event(LspEvent::Log(m.to_string()));
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_round_trip() {
        let p = if cfg!(windows) {
            PathBuf::from(r"C:\Users\andre\My Docs\a.rs")
        } else {
            PathBuf::from("/home/andre/My Docs/a.rs")
        };
        let uri = path_to_uri(&p);
        assert!(uri.contains("My%20Docs"));
        assert_eq!(uri_to_path(&uri).unwrap(), p);
        assert_eq!(uri_to_path("file:///c%3A/x/y.rs").unwrap(), PathBuf::from("c:\\x\\y.rs"));
    }

    #[test]
    fn bundled_servers_sort_newest_first() {
        assert!(version_key("rust-lang.rust-analyzer-0.3.3081-win32-x64") > version_key("rust-lang.rust-analyzer-0.3.2999-win32-x64"));
        assert!(!server_candidates().is_empty());
    }

    #[test]
    fn edits_apply_back_to_front() {
        let edits = vec![
            Edit { start: 0, end: 2, text: "fn".into() },
            Edit { start: 8, end: 9, text: "()".into() },
        ];
        assert_eq!(apply_edits("FN main x", &edits), "fn main ()");
    }
}
