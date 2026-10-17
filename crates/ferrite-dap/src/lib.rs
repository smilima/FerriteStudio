//! Debug Adapter Protocol client.
//!
//! Works with any adapter that speaks DAP over stdio. [`AdapterConfig::detect`]
//! picks GDB's built-in DAP mode (`gdb -i=dap`, GDB 14 and later) for DWARF
//! toolchains and LLVM's `lldb-dap` for the Windows MSVC toolchain and macOS.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use ferrite_rpc::{no_console, Endpoint};
use serde_json::{json, Value};

#[derive(Clone, Debug)]
pub enum DapEvent {
    /// The adapter is ready for breakpoints and `configurationDone`.
    Initialized,
    Stopped { reason: String, thread_id: Option<i64>, description: String },
    Continued,
    /// Program or adapter output (`stdout`, `stderr`, `console`).
    Output { category: String, text: String },
    Exited { code: i64 },
    Terminated,
    Log(String),
    AdapterExited,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StackFrame {
    pub id: i64,
    pub name: String,
    pub path: Option<PathBuf>,
    /// 1-based.
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scope {
    pub name: String,
    pub reference: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub type_name: String,
    /// Non-zero when the variable has children.
    pub reference: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterKind {
    Gdb,
    LldbDap,
    Custom,
}

/// Which adapter to run and how.
#[derive(Clone, Debug)]
pub struct AdapterConfig {
    pub kind: AdapterKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Folders put in front of PATH for the adapter (and so the debuggee),
    /// such as MSYS2's `ucrt64\bin`, which holds a MinGW GDB's DLLs.
    pub path_prepend: Vec<PathBuf>,
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let exe = exe_name(name);
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join(&exe)).find(|p| p.is_file())
    })
}

/// The current PATH with `dirs` in front.
pub fn path_with(dirs: &[PathBuf]) -> OsString {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut all: Vec<PathBuf> = dirs.to_vec();
    all.extend(std::env::split_paths(&current));
    std::env::join_paths(all).unwrap_or(current)
}

fn command_output(program: &Path, args: &[&str], path_prepend: &[PathBuf]) -> Option<(bool, String)> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if !path_prepend.is_empty() {
        cmd.env("PATH", path_with(path_prepend));
    }
    no_console(&mut cmd);
    let out = cmd.output().ok()?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

/// Major version from `gdb --version`, e.g. 15 for "GNU gdb (...) 15.1".
pub fn gdb_major_version(version_output: &str) -> Option<u32> {
    let first = version_output.lines().next()?;
    let last = first.split_whitespace().last()?;
    last.split('.').next()?.parse().ok()
}

/// Rust's host triple from `rustc -vV`.
pub fn rust_host() -> Option<String> {
    let (ok, out) = command_output(Path::new("rustc"), &["-vV"], &[])?;
    if !ok {
        return None;
    }
    out.lines().find_map(|l| l.strip_prefix("host: ")).map(str::to_string)
}

/// Why a GDB cannot serve as Ferrite Studio's debug adapter, if it cannot.
/// DAP mode needs GDB 14 or later with working embedded Python.
pub fn gdb_problem(gdb: &Path, path_prepend: &[PathBuf]) -> Option<String> {
    let Some((ok, version)) = command_output(gdb, &["--version"], path_prepend) else {
        return Some("it does not start".into());
    };
    if !ok {
        return Some("it does not start (missing DLLs?)".into());
    }
    match gdb_major_version(&version) {
        Some(v) if v >= 14 => {}
        Some(v) => return Some(format!("it is version {v}; DAP mode needs 14 or later")),
        None => return Some("its version is unknown".into()),
    }
    let probe = command_output(gdb, &["-nx", "-q", "-batch", "-ex", "python import gdb.dap"], path_prepend);
    match probe {
        Some((true, out)) if !out.contains("Error") && !out.contains("Traceback") => None,
        Some((_, out)) => Some(format!(
            "its Python cannot load the DAP module: {}",
            out.lines().find(|l| !l.trim().is_empty()).unwrap_or("unknown error").trim()
        )),
        None => Some("it does not start".into()),
    }
}

/// MSYS2 installations to search on Windows.
fn msys2_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(r) = std::env::var_os("MSYS2_ROOT") {
        roots.push(PathBuf::from(r));
    }
    for r in [r"C:\msys64", r"C:\tools\msys64"] {
        roots.push(PathBuf::from(r));
    }
    roots.retain(|r| r.is_dir());
    roots.dedup();
    roots
}

const MSYS2_ENVS: &[&str] = &["ucrt64", "mingw64", "clang64"];

/// Version numbers in a folder name such as `gdb-18.1`, for sorting.
fn version_key(name: &str) -> Vec<u32> {
    name.split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse().ok()).collect()
}

/// GDB candidates with the folders each needs on PATH, best first: GDB on
/// PATH, then MSYS2 builds under `opt` (newest first), then MSYS2 packages.
fn gdb_candidates() -> Vec<(PathBuf, Vec<Vec<PathBuf>>)> {
    let mut out: Vec<(PathBuf, Vec<Vec<PathBuf>>)> = Vec::new();
    if let Some(p) = find_on_path("gdb") {
        out.push((p, vec![vec![]]));
    }
    if cfg!(windows) {
        for root in msys2_roots() {
            let env_bins: Vec<PathBuf> =
                MSYS2_ENVS.iter().map(|e| root.join(e).join("bin")).filter(|b| b.is_dir()).collect();
            let mut opt: Vec<PathBuf> = std::fs::read_dir(root.join("opt"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|d| {
                    d.file_name().is_some_and(|n| n.to_string_lossy().starts_with("gdb"))
                        && d.join("bin").join(exe_name("gdb")).is_file()
                })
                .collect();
            opt.sort_by_key(|d| std::cmp::Reverse(version_key(&d.file_name().unwrap().to_string_lossy())));
            for d in opt {
                // A self-built GDB needs the DLLs of the environment it was built in.
                let tries = env_bins.iter().map(|b| vec![b.clone()]).collect();
                out.push((d.join("bin").join(exe_name("gdb")), tries));
            }
            for bin in &env_bins {
                let gdb = bin.join(exe_name("gdb"));
                if gdb.is_file() {
                    out.push((gdb, vec![vec![bin.clone()]]));
                }
            }
        }
    }
    out
}

/// MSYS2 environment `bin` folders that might hold the DLLs of `program`.
fn env_bins_for(program: &Path) -> Vec<Vec<PathBuf>> {
    let mut tries = vec![vec![]];
    for root in msys2_roots() {
        if program.starts_with(&root) {
            if let Some(bin) = program.parent() {
                if MSYS2_ENVS.iter().any(|e| bin.ends_with(Path::new(e).join("bin"))) {
                    tries.push(vec![bin.to_path_buf()]);
                }
            }
            tries.extend(MSYS2_ENVS.iter().map(|e| vec![root.join(e).join("bin")]));
        }
    }
    tries
}

/// Splits `FERRITE_DAP` into a program and arguments; the program may be quoted.
fn split_command_line(cmdline: &str) -> Option<(PathBuf, Vec<String>)> {
    let s = cmdline.trim();
    let (program, rest) = if let Some(q) = s.strip_prefix('"') {
        let end = q.find('"')?;
        (q[..end].to_string(), &q[end + 1..])
    } else {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        (s[..end].to_string(), &s[end..])
    };
    (!program.is_empty()).then(|| (PathBuf::from(program), rest.split_whitespace().map(str::to_string).collect()))
}

fn gdb_args() -> Vec<String> {
    let mut args = vec!["-i=dap".to_string(), "-q".to_string()];
    // Load the Rust pretty-printers shipped with the toolchain, as rust-gdb does.
    if let Some((true, sysroot)) = command_output(Path::new("rustc"), &["--print", "sysroot"], &[]) {
        let etc = Path::new(sysroot.trim()).join("lib").join("rustlib").join("etc");
        if etc.is_dir() {
            args.push(format!("--directory={}", etc.display()));
            args.push("-iex".into());
            args.push(format!("add-auto-load-safe-path {}", etc.display()));
        }
    }
    args
}

impl AdapterConfig {
    /// `FERRITE_DAP` (a full command line) wins; otherwise choose by toolchain.
    /// On the Windows MSVC toolchain LLVM's lldb-dap is preferred because it
    /// reads PDB; otherwise GDB is used, including MSYS2 installs not on PATH.
    pub fn detect() -> Result<AdapterConfig, String> {
        if let Some((program, args)) = std::env::var("FERRITE_DAP").ok().as_deref().and_then(split_command_line) {
            let name = program.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            let kind = if name.starts_with("gdb") {
                AdapterKind::Gdb
            } else if name.contains("lldb") {
                AdapterKind::LldbDap
            } else {
                AdapterKind::Custom
            };
            let mut path_prepend = Vec::new();
            if kind == AdapterKind::Gdb {
                let tries = env_bins_for(&program);
                let found = tries.iter().find(|dirs| gdb_problem(&program, dirs).is_none());
                match found {
                    Some(dirs) => path_prepend = dirs.clone(),
                    None => {
                        let why = gdb_problem(&program, tries.last().map_or(&[][..], |d| d.as_slice()))
                            .unwrap_or_default();
                        return Err(format!("FERRITE_DAP names {}, but {why}", program.display()));
                    }
                }
            }
            return Ok(AdapterConfig { kind, program, args, path_prepend });
        }

        let host = rust_host().unwrap_or_default();
        let prefer_lldb = host.contains("windows-msvc") || host.contains("apple");
        let lldb = || {
            find_on_path("lldb-dap").or_else(|| {
                let llvm = PathBuf::from(r"C:\Program Files\LLVM\bin\lldb-dap.exe");
                (cfg!(windows) && llvm.is_file()).then_some(llvm)
            })
        };
        if prefer_lldb {
            if let Some(p) = lldb() {
                return Ok(AdapterConfig { kind: AdapterKind::LldbDap, program: p, args: vec![], path_prepend: vec![] });
            }
        }
        let mut rejected = Vec::new();
        for (gdb, tries) in gdb_candidates() {
            match tries.iter().find(|dirs| gdb_problem(&gdb, dirs).is_none()) {
                Some(dirs) => {
                    return Ok(AdapterConfig { kind: AdapterKind::Gdb, program: gdb, args: gdb_args(), path_prepend: dirs.clone() });
                }
                None => {
                    let why = gdb_problem(&gdb, tries.first().map_or(&[][..], |d| d.as_slice())).unwrap_or_default();
                    rejected.push(format!("{} ({why})", gdb.display()));
                }
            }
        }
        if let Some(p) = lldb() {
            return Ok(AdapterConfig { kind: AdapterKind::LldbDap, program: p, args: vec![], path_prepend: vec![] });
        }
        let mut msg = format!(
            "No debug adapter found for {host}. Install GDB 14+ with Python (for example MSYS2's, under C:\\msys64) \
             or LLVM's lldb-dap, or set FERRITE_DAP to an adapter command line."
        );
        if !rejected.is_empty() {
            msg.push_str(&format!(" Rejected: {}.", rejected.join("; ")));
        }
        Err(msg)
    }

    /// GDB reads DWARF only, so on the Windows MSVC toolchain programs must be
    /// built for the matching GNU target to be debuggable.
    pub fn needs_dwarf(&self) -> bool {
        self.kind == AdapterKind::Gdb
    }

    /// The command that starts the adapter, with its PATH additions.
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if !self.path_prepend.is_empty() {
            cmd.env("PATH", path_with(&self.path_prepend));
        }
        cmd
    }

    pub fn describe(&self) -> String {
        let name = self.program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match self.kind {
            AdapterKind::Gdb => format!("{name} -i=dap"),
            _ => name,
        }
    }

    /// Arguments for the `launch` request in this adapter's dialect.
    pub fn launch_arguments(&self, program: &Path, cwd: &Path, args: &[String]) -> Value {
        let mut v = json!({
            "program": program.to_string_lossy(),
            "args": args,
            "cwd": cwd.to_string_lossy(),
        });
        match self.kind {
            AdapterKind::Gdb => v["stopAtBeginningOfMainSubprogram"] = json!(false),
            AdapterKind::LldbDap | AdapterKind::Custom => v["stopOnEntry"] = json!(false),
        }
        v
    }
}

type Pending = Arc<Mutex<HashMap<i64, Box<dyn FnOnce(Result<Value, String>) + Send>>>>;

pub struct DapClient {
    endpoint: Arc<Endpoint>,
    seq: AtomicI64,
    pending: Pending,
    pub config: AdapterConfig,
}

impl DapClient {
    /// Starts the adapter and completes `initialize`. Blocks up to 20 s.
    pub fn start(
        config: AdapterConfig,
        cwd: &Path,
        on_event: impl Fn(DapEvent) + Send + Sync + 'static,
    ) -> io::Result<Arc<DapClient>> {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let endpoint_cell: Arc<OnceLock<Weak<Endpoint>>> = Arc::new(OnceLock::new());
        let on_event = Arc::new(on_event);
        let mut cmd = config.command();
        cmd.current_dir(cwd);

        let (p, cell, ev) = (pending.clone(), endpoint_cell.clone(), on_event.clone());
        let ev_err = on_event.clone();
        let ev_exit = on_event.clone();
        let endpoint = Endpoint::spawn(
            cmd,
            move |msg| handle_message(msg, &p, &cell, &*ev),
            move |line| ev_err(DapEvent::Log(line)),
            move || ev_exit(DapEvent::AdapterExited),
        )?;
        let _ = endpoint_cell.set(Arc::downgrade(&endpoint));
        let client = Arc::new(DapClient { endpoint, seq: AtomicI64::new(1), pending, config });

        let (tx, rx) = mpsc::channel();
        client.request(
            "initialize",
            json!({
                "clientID": "ferrite-studio",
                "clientName": "Ferrite Studio",
                "adapterID": client.config.describe(),
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsVariableType": true,
                "supportsRunInTerminalRequest": false,
            }),
            move |r| {
                let _ = tx.send(r);
            },
        )?;
        rx.recv_timeout(Duration::from_secs(20))
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "debug adapter did not answer initialize"))?
            .map_err(io::Error::other)?;
        Ok(client)
    }

    fn request(
        &self,
        command: &str,
        arguments: Value,
        cb: impl FnOnce(Result<Value, String>) + Send + 'static,
    ) -> io::Result<()> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.pending.lock().unwrap().insert(seq, Box::new(cb));
        let sent = self
            .endpoint
            .send(&json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments }));
        if sent.is_err() {
            self.pending.lock().unwrap().remove(&seq);
        }
        sent
    }

    pub fn launch(&self, arguments: Value, cb: impl FnOnce(Result<(), String>) + Send + 'static) -> io::Result<()> {
        self.request("launch", arguments, move |r| cb(r.map(|_| ())))
    }

    /// Replaces all breakpoints of one file. Lines are 1-based; the callback
    /// gets the line the adapter actually used and whether it is verified.
    pub fn set_breakpoints(
        &self,
        path: &Path,
        lines: &[u32],
        cb: impl FnOnce(Vec<(u32, bool)>) + Send + 'static,
    ) -> io::Result<()> {
        let bps: Vec<Value> = lines.iter().map(|l| json!({ "line": l })).collect();
        let requested = lines.to_vec();
        self.request(
            "setBreakpoints",
            json!({ "source": { "path": path.to_string_lossy() }, "breakpoints": bps, "lines": lines }),
            move |r| {
                let list = r
                    .ok()
                    .and_then(|b| b.get("breakpoints").and_then(Value::as_array).cloned())
                    .unwrap_or_default();
                let out = list
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        let line = b
                            .get("line")
                            .and_then(Value::as_u64)
                            .map(|l| l as u32)
                            .unwrap_or_else(|| requested.get(i).copied().unwrap_or(0));
                        (line, b.get("verified").and_then(Value::as_bool).unwrap_or(false))
                    })
                    .collect();
                cb(out)
            },
        )
    }

    pub fn configuration_done(&self) -> io::Result<()> {
        self.request("configurationDone", json!({}), |_| {})
    }

    fn thread_request(&self, command: &str, thread: i64) -> io::Result<()> {
        self.request(command, json!({ "threadId": thread }), |_| {})
    }

    pub fn resume(&self, thread: i64) -> io::Result<()> {
        self.thread_request("continue", thread)
    }

    pub fn step_over(&self, thread: i64) -> io::Result<()> {
        self.thread_request("next", thread)
    }

    pub fn step_in(&self, thread: i64) -> io::Result<()> {
        self.thread_request("stepIn", thread)
    }

    pub fn step_out(&self, thread: i64) -> io::Result<()> {
        self.thread_request("stepOut", thread)
    }

    pub fn pause(&self, thread: i64) -> io::Result<()> {
        self.thread_request("pause", thread)
    }

    pub fn threads(&self, cb: impl FnOnce(Vec<(i64, String)>) + Send + 'static) -> io::Result<()> {
        self.request("threads", json!({}), move |r| {
            let list = r.ok().and_then(|b| b.get("threads").and_then(Value::as_array).cloned()).unwrap_or_default();
            cb(list
                .iter()
                .filter_map(|t| {
                    Some((t.get("id")?.as_i64()?, t.get("name").and_then(Value::as_str).unwrap_or("").to_string()))
                })
                .collect())
        })
    }

    pub fn stack_trace(&self, thread: i64, cb: impl FnOnce(Vec<StackFrame>) + Send + 'static) -> io::Result<()> {
        self.request("stackTrace", json!({ "threadId": thread, "startFrame": 0, "levels": 64 }), move |r| {
            let list =
                r.ok().and_then(|b| b.get("stackFrames").and_then(Value::as_array).cloned()).unwrap_or_default();
            cb(list
                .iter()
                .filter_map(|f| {
                    Some(StackFrame {
                        id: f.get("id")?.as_i64()?,
                        name: f.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
                        path: f.pointer("/source/path").and_then(Value::as_str).map(PathBuf::from),
                        line: f.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
                    })
                })
                .collect())
        })
    }

    pub fn scopes(&self, frame: i64, cb: impl FnOnce(Vec<Scope>) + Send + 'static) -> io::Result<()> {
        self.request("scopes", json!({ "frameId": frame }), move |r| {
            let list = r.ok().and_then(|b| b.get("scopes").and_then(Value::as_array).cloned()).unwrap_or_default();
            cb(list
                .iter()
                .filter_map(|s| {
                    Some(Scope {
                        name: s.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        reference: s.get("variablesReference")?.as_i64()?,
                    })
                })
                .collect())
        })
    }

    pub fn variables(&self, reference: i64, cb: impl FnOnce(Vec<Variable>) + Send + 'static) -> io::Result<()> {
        self.request("variables", json!({ "variablesReference": reference }), move |r| {
            let list =
                r.ok().and_then(|b| b.get("variables").and_then(Value::as_array).cloned()).unwrap_or_default();
            cb(list
                .iter()
                .map(|v| Variable {
                    name: v.get("name").and_then(Value::as_str).unwrap_or("").to_string(),
                    value: v.get("value").and_then(Value::as_str).unwrap_or("").to_string(),
                    type_name: v.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                    reference: v.get("variablesReference").and_then(Value::as_i64).unwrap_or(0),
                })
                .collect())
        })
    }

    /// Evaluates an expression in a frame; `context` is "watch", "hover" or "repl".
    pub fn evaluate(
        &self,
        expression: &str,
        frame: Option<i64>,
        context: &str,
        cb: impl FnOnce(Result<String, String>) + Send + 'static,
    ) -> io::Result<()> {
        let mut args = json!({ "expression": expression, "context": context });
        if let Some(f) = frame {
            args["frameId"] = json!(f);
        }
        self.request("evaluate", args, move |r| {
            cb(r.map(|b| b.get("result").and_then(Value::as_str).unwrap_or("").to_string()))
        })
    }

    /// Ends the session and the debuggee (Program Reset).
    pub fn stop(&self) {
        let _ = self.request("disconnect", json!({ "terminateDebuggee": true }), |_| {});
    }

    pub fn kill(&self) {
        self.endpoint.kill();
    }
}

fn handle_message(
    msg: Value,
    pending: &Pending,
    endpoint: &OnceLock<Weak<Endpoint>>,
    on_event: &(dyn Fn(DapEvent) + Send + Sync),
) {
    match msg.get("type").and_then(Value::as_str) {
        Some("response") => {
            let Some(seq) = msg.get("request_seq").and_then(Value::as_i64) else { return };
            let cb = pending.lock().unwrap().remove(&seq);
            if let Some(cb) = cb {
                let ok = msg.get("success").and_then(Value::as_bool).unwrap_or(false);
                let r = if ok {
                    Ok(msg.get("body").cloned().unwrap_or(Value::Null))
                } else {
                    let text = msg
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| msg.pointer("/body/error/format").and_then(Value::as_str))
                        .unwrap_or("request failed");
                    Err(text.to_string())
                };
                cb(r);
            }
        }
        Some("event") => {
            let body = msg.get("body").cloned().unwrap_or(Value::Null);
            let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            let ev = match msg.get("event").and_then(Value::as_str).unwrap_or("") {
                "initialized" => DapEvent::Initialized,
                "stopped" => DapEvent::Stopped {
                    reason: s("reason"),
                    thread_id: body.get("threadId").and_then(Value::as_i64),
                    description: s("description"),
                },
                "continued" => DapEvent::Continued,
                "output" => DapEvent::Output { category: s("category"), text: s("output") },
                "exited" => DapEvent::Exited { code: body.get("exitCode").and_then(Value::as_i64).unwrap_or(0) },
                "terminated" => DapEvent::Terminated,
                _ => return,
            };
            on_event(ev);
        }
        Some("request") => {
            // Reverse requests such as runInTerminal are not supported.
            let seq = msg.get("seq").and_then(Value::as_i64).unwrap_or(0);
            let command = msg.get("command").cloned().unwrap_or(Value::Null);
            if let Some(ep) = endpoint.get().and_then(Weak::upgrade) {
                let _ = ep.send(&json!({
                    "seq": 0, "type": "response", "request_seq": seq, "command": command,
                    "success": false, "message": "not supported by Ferrite Studio"
                }));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdb_versions_parse() {
        assert_eq!(gdb_major_version("GNU gdb (Ubuntu 15.1-1ubuntu1~24.04.1) 15.1\n"), Some(15));
        assert_eq!(gdb_major_version("GNU gdb (GDB) 14.2\nCopyright"), Some(14));
        assert_eq!(gdb_major_version(""), None);
    }

    #[test]
    fn command_lines_and_versions_parse() {
        let (p, a) = split_command_line(r#""C:\Program Files\x\gdb.exe" -i=dap -q"#).unwrap();
        assert_eq!(p, PathBuf::from(r"C:\Program Files\x\gdb.exe"));
        assert_eq!(a, ["-i=dap", "-q"]);
        let (p, a) = split_command_line("gdb -i=dap").unwrap();
        assert_eq!((p, a), (PathBuf::from("gdb"), vec!["-i=dap".to_string()]));
        assert!(version_key("gdb-18.1") > version_key("gdb-17.2"));
        assert!(version_key("gdb-18.10") > version_key("gdb-18.9"));
    }

    #[test]
    fn launch_arguments_follow_each_dialect() {
        let gdb = AdapterConfig { kind: AdapterKind::Gdb, program: "gdb".into(), args: vec![], path_prepend: vec![] };
        let v = gdb.launch_arguments(Path::new("/p/app"), Path::new("/p"), &[]);
        assert_eq!(v["stopAtBeginningOfMainSubprogram"], false);
        let lldb = AdapterConfig { kind: AdapterKind::LldbDap, program: "lldb-dap".into(), args: vec![], path_prepend: vec![] };
        assert_eq!(lldb.launch_arguments(Path::new("/p/app"), Path::new("/p"), &[])["stopOnEntry"], false);
    }
}
