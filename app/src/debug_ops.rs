//! The integrated debugger: breakpoints, stepping, call stack, locals and
//! watches over the Debug Adapter Protocol.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ferrite_dap::{DapClient, DapEvent, Scope, StackFrame, Variable};
use ferrite_lsp::path_key;
use slint::{ModelRc, VecModel};

use crate::app::{post, App};
use crate::{StackRow, VarRow, WatchRow};

/// Session counter, so late events from an ended session are ignored.
static SESSION: AtomicU64 = AtomicU64::new(0);

/// Some adapters (GDB) send `initialized` during the `initialize` handshake,
/// before the session is recorded; remember it so configuration still runs.
static EARLY_INITIALIZED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
pub enum Step {
    Over,
    Into,
    Out,
}

pub struct VarEntry {
    pub var: Variable,
    pub depth: usize,
    pub expanded: bool,
}

pub struct DebugSession {
    pub id: u64,
    pub client: Arc<DapClient>,
    pub thread: Option<i64>,
    pub stopped: bool,
    pub frames: Vec<StackFrame>,
    pub frame: usize,
    pub vars: Vec<VarEntry>,
}

impl DebugSession {
    fn frame_id(&self) -> Option<i64> {
        self.frames.get(self.frame).map(|f| f.id)
    }
}

impl App {
    pub fn start_debugging(&mut self) {
        let (Some(exe), Some(root)) = (self.exe.clone(), self.root().map(Path::to_path_buf)) else {
            self.message("The build produced no executable to debug", "error");
            return;
        };
        let config = match self.debug_adapter() {
            Ok(c) => c,
            Err(e) => {
                self.message(e, "warning");
                self.message("Running without the debugger", "info");
                self.run_program();
                return;
            }
        };
        let id = SESSION.fetch_add(1, Ordering::SeqCst) + 1;
        self.message(format!("Debugging {} with {}", exe.display(), config.describe()), "info");
        self.status("Starting the debugger...");
        self.ui().set_debug_state("running".into());
        std::thread::spawn(move || {
            let launch = config.launch_arguments(&exe, &root, &[]);
            let result = DapClient::start(config, &root, move |ev| post(move |a| a.on_dap(id, ev)));
            post(move |a| a.debug_started(id, result, launch));
        });
    }

    fn debug_started(&mut self, id: u64, result: std::io::Result<Arc<DapClient>>, launch: serde_json::Value) {
        if SESSION.load(Ordering::SeqCst) != id {
            if let Ok(c) = result {
                c.kill();
            }
            return;
        }
        let client = match result {
            Ok(c) => c,
            Err(e) => {
                self.message(format!("The debugger did not start: {e}"), "error");
                self.end_session();
                return;
            }
        };
        self.debug = Some(DebugSession {
            id,
            client: client.clone(),
            thread: None,
            stopped: false,
            frames: Vec::new(),
            frame: 0,
            vars: Vec::new(),
        });
        self.status("Running under the debugger");
        let early = EARLY_INITIALIZED.load(Ordering::SeqCst) == id;
        let _ = client.launch(launch, move |r| {
            if let Err(e) = r {
                post(move |a| {
                    if a.debug.as_ref().is_some_and(|d| d.id == id) {
                        a.message(format!("Launch failed: {e}"), "error");
                        a.stop_debugging();
                    }
                })
            }
        });
        if early {
            self.configure_session();
        }
    }

    /// Sends every breakpoint, then `configurationDone`, which lets the
    /// program start.
    fn configure_session(&mut self) {
        let Some(client) = self.debug.as_ref().map(|d| d.client.clone()) else { return };
        for (path, lines) in self.breakpoints.values() {
            let lines: Vec<u32> = lines.iter().copied().collect();
            let shown = path.clone();
            let _ = client.set_breakpoints(path, &lines, move |result| {
                let unverified: Vec<u32> = result.iter().filter(|(_, ok)| !ok).map(|(l, _)| *l).collect();
                if !unverified.is_empty() {
                    post(move |a| {
                        a.message(
                            format!("Breakpoints not bound yet in {}: lines {unverified:?}", a.rel(&shown)),
                            "warning",
                        )
                    });
                }
            });
        }
        let _ = client.configuration_done();
    }

    fn on_dap(&mut self, id: u64, ev: DapEvent) {
        if SESSION.load(Ordering::SeqCst) != id {
            return;
        }
        match ev {
            DapEvent::Initialized => {
                if self.debug.is_some() {
                    self.configure_session();
                } else {
                    EARLY_INITIALIZED.store(id, Ordering::SeqCst);
                }
            }
            DapEvent::Stopped { reason, thread_id, description } => {
                let Some(d) = self.debug.as_mut() else { return };
                d.stopped = true;
                let client = d.client.clone();
                self.ui().set_debug_state("stopped".into());
                let why = if description.is_empty() { reason } else { description };
                self.status(format!("Paused: {why}"));
                match thread_id {
                    Some(t) => self.load_stack(t),
                    None => {
                        let _ = client.threads(move |threads| {
                            if let Some((t, _)) = threads.first().cloned() {
                                post(move |a| a.load_stack(t));
                            }
                        });
                    }
                }
            }
            DapEvent::Continued => self.mark_running(),
            DapEvent::Output { category, text } => {
                let kind = match category.as_str() {
                    "stderr" => "warning",
                    "console" | "important" => "info",
                    _ => "",
                };
                self.message(text.trim_end_matches('\n'), kind);
            }
            DapEvent::Exited { code } => self.message(format!("Process exited with code {code}"), "info"),
            DapEvent::Terminated | DapEvent::AdapterExited => {
                if self.debug.is_some() {
                    self.message("Debug session ended", "info");
                    self.end_session();
                }
            }
            DapEvent::Log(line) => self.message(line, ""),
        }
    }

    fn load_stack(&mut self, thread: i64) {
        let Some(d) = self.debug.as_mut() else { return };
        d.thread = Some(thread);
        let id = d.id;
        let _ = d.client.stack_trace(thread, move |frames| {
            post(move |a| {
                if a.debug.as_ref().is_some_and(|d| d.id == id) {
                    a.show_stack(frames)
                }
            })
        });
    }

    fn show_stack(&mut self, frames: Vec<StackFrame>) {
        let Some(d) = self.debug.as_mut() else { return };
        d.frames = frames;
        // Start at the first frame with source in the project, skipping
        // library frames such as a panic handler.
        let root = self.project.as_ref().map(|p| p.root.clone());
        let first = d
            .frames
            .iter()
            .position(|f| f.path.as_ref().zip(root.as_ref()).is_some_and(|(p, r)| p.starts_with(r)))
            .unwrap_or(0);
        self.ui().set_bottom_tab(3);
        self.select_frame(first);
    }

    pub fn select_frame(&mut self, i: usize) {
        let Some(d) = self.debug.as_mut() else { return };
        if i >= d.frames.len() {
            return;
        }
        d.frame = i;
        d.vars.clear();
        let frame = d.frames[i].clone();
        let client = d.client.clone();
        let id = d.id;
        self.refresh_stack();
        self.refresh_locals();
        if let Some(path) = frame.path.as_ref().filter(|p| p.is_file()) {
            let line = frame.line.saturating_sub(1) as usize;
            self.open_at(path, |text| crate::app::line_col_to_offset(text, line, 0));
            self.refresh_editor(false);
        }
        let frame_id = frame.id;
        let _ = client.scopes(frame_id, move |scopes| {
            post(move |a| {
                if a.debug.as_ref().is_some_and(|d| d.id == id) {
                    a.load_scopes(frame_id, scopes)
                }
            })
        });
        self.evaluate_watches();
    }

    fn load_scopes(&mut self, frame_id: i64, scopes: Vec<Scope>) {
        let Some(d) = self.debug.as_ref() else { return };
        for scope in scopes.into_iter().filter(|s| !s.name.to_lowercase().contains("register")) {
            let id = d.id;
            let _ = d.client.variables(scope.reference, move |vars| {
                post(move |a| a.add_vars(id, frame_id, vars))
            });
        }
    }

    fn add_vars(&mut self, id: u64, frame_id: i64, vars: Vec<Variable>) {
        let Some(d) = self.debug.as_mut() else { return };
        if d.id != id || d.frame_id() != Some(frame_id) {
            return;
        }
        d.vars.extend(vars.into_iter().map(|var| VarEntry { var, depth: 0, expanded: false }));
        self.refresh_locals();
    }

    /// Double-click on a variable expands or collapses its children.
    pub fn expand_local(&mut self, i: usize) {
        let Some(d) = self.debug.as_mut() else { return };
        let Some(entry) = d.vars.get_mut(i) else { return };
        if entry.var.reference == 0 {
            return;
        }
        if entry.expanded {
            entry.expanded = false;
            let depth = entry.depth;
            let end = d.vars[i + 1..].iter().position(|v| v.depth <= depth).map_or(d.vars.len(), |p| i + 1 + p);
            d.vars.drain(i + 1..end);
            self.refresh_locals();
            return;
        }
        entry.expanded = true;
        let (reference, depth, name) = (entry.var.reference, entry.depth, entry.var.name.clone());
        let id = d.id;
        let _ = d.client.variables(reference, move |vars| {
            post(move |a| {
                let Some(d) = a.debug.as_mut() else { return };
                if d.id != id || d.vars.get(i).is_none_or(|v| v.var.name != name) {
                    return;
                }
                let children = vars.into_iter().map(|var| VarEntry { var, depth: depth + 1, expanded: false });
                d.vars.splice(i + 1..i + 1, children);
                a.refresh_locals();
            })
        });
    }

    fn mark_running(&mut self) {
        let Some(d) = self.debug.as_mut() else { return };
        d.stopped = false;
        d.frames.clear();
        d.vars.clear();
        self.ui().set_debug_state("running".into());
        self.status("Running under the debugger");
        self.refresh_stack();
        self.refresh_locals();
        self.refresh_editor(false);
    }

    pub fn resume(&mut self) {
        let Some(d) = self.debug.as_ref() else { return };
        if let (true, Some(t)) = (d.stopped, d.thread) {
            let _ = d.client.resume(t);
            self.mark_running();
        }
    }

    pub fn step(&mut self, step: Step) {
        let Some(d) = self.debug.as_ref() else { return };
        let (true, Some(t)) = (d.stopped, d.thread) else { return };
        let _ = match step {
            Step::Over => d.client.step_over(t),
            Step::Into => d.client.step_in(t),
            Step::Out => d.client.step_out(t),
        };
        self.mark_running();
    }

    pub fn stop_debugging(&mut self) {
        if let Some(d) = self.debug.as_ref() {
            d.client.stop();
            let client = d.client.clone();
            slint::Timer::single_shot(Duration::from_millis(1500), move || client.kill());
        }
        self.end_session();
    }

    fn end_session(&mut self) {
        SESSION.fetch_add(1, Ordering::SeqCst);
        self.debug = None;
        self.ui().set_debug_state("".into());
        self.status("Ready");
        self.refresh_stack();
        self.refresh_locals();
        self.evaluate_watches();
        self.refresh_editor(false);
    }

    /// Execution line (0-based) to highlight in `path`, or -1.
    pub fn exec_line_for(&self, path: &Path) -> i32 {
        let Some(d) = self.debug.as_ref().filter(|d| d.stopped) else { return -1 };
        match d.frames.get(d.frame) {
            Some(f) if f.path.as_deref().map(path_key) == Some(path_key(path)) => f.line as i32 - 1,
            _ => -1,
        }
    }

    fn refresh_stack(&self) {
        let rows: Vec<StackRow> = match &self.debug {
            Some(d) => d
                .frames
                .iter()
                .enumerate()
                .map(|(i, f)| StackRow {
                    name: f.name.as_str().into(),
                    location: f
                        .path
                        .as_ref()
                        .map(|p| format!("{}:{}", self.rel(p), f.line))
                        .unwrap_or_default()
                        .into(),
                    current: i == d.frame,
                })
                .collect(),
            None => Vec::new(),
        };
        self.ui().set_stack(ModelRc::new(VecModel::from(rows)));
    }

    fn refresh_locals(&self) {
        let rows: Vec<VarRow> = match &self.debug {
            Some(d) => d
                .vars
                .iter()
                .map(|v| VarRow {
                    name: v.var.name.as_str().into(),
                    value: v.var.value.as_str().into(),
                    type_name: v.var.type_name.as_str().into(),
                    depth: v.depth as i32,
                    expandable: v.var.reference != 0 && !v.expanded,
                })
                .collect(),
            None => Vec::new(),
        };
        self.ui().set_locals(ModelRc::new(VecModel::from(rows)));
    }

    // ---- Breakpoints ----

    pub fn toggle_breakpoint_at_caret(&mut self) {
        let line = self.caret_line as u32 + 1;
        self.toggle_breakpoint(line);
    }

    pub fn toggle_breakpoint_line(&mut self, line0: i32) {
        if line0 >= 0 {
            self.toggle_breakpoint(line0 as u32 + 1);
        }
    }

    fn toggle_breakpoint(&mut self, line: u32) {
        let Some(i) = self.current else { return };
        let path = self.buffers[i].path.clone();
        if line as usize > self.buffers[i].text.matches('\n').count() + 1 {
            return;
        }
        let key = path_key(&path);
        let entry = self.breakpoints.entry(key.clone()).or_insert_with(|| (path.clone(), Default::default()));
        if !entry.1.remove(&line) {
            entry.1.insert(line);
        }
        let lines: Vec<u32> = entry.1.iter().copied().collect();
        if lines.is_empty() {
            self.breakpoints.remove(&key);
        }
        if let Some(d) = &self.debug {
            let _ = d.client.set_breakpoints(&path, &lines, |_| {});
        }
        self.refresh_editor(false);
    }

    // ---- Watches ----

    pub fn add_watch(&mut self, expr: String) {
        let expr = expr.trim().to_string();
        if expr.is_empty() || self.watches.iter().any(|(e, _)| *e == expr) {
            return;
        }
        self.watches.push((expr, String::new()));
        self.evaluate_watches();
    }

    pub fn remove_watch(&mut self, i: usize) {
        if i < self.watches.len() {
            self.watches.remove(i);
            self.refresh_watches();
        }
    }

    fn evaluate_watches(&mut self) {
        let target = self.debug.as_ref().filter(|d| d.stopped).and_then(|d| Some((d.client.clone(), d.frame_id()?, d.id)));
        for k in 0..self.watches.len() {
            let expr = self.watches[k].0.clone();
            match &target {
                Some((client, frame, id)) => {
                    let (id, e2) = (*id, expr.clone());
                    let _ = client.evaluate(&expr, Some(*frame), "watch", move |r| {
                        post(move |a| {
                            if a.debug.as_ref().is_some_and(|d| d.id == id) {
                                if let Some(w) = a.watches.get_mut(k).filter(|w| w.0 == e2) {
                                    w.1 = r.unwrap_or_else(|e| format!("<{e}>"));
                                }
                                a.refresh_watches();
                            }
                        })
                    });
                }
                None => self.watches[k].1 = "(available when paused)".into(),
            }
        }
        self.refresh_watches();
    }

    fn refresh_watches(&self) {
        let rows: Vec<WatchRow> = self
            .watches
            .iter()
            .map(|(e, v)| WatchRow { expression: e.as_str().into(), value: v.as_str().into() })
            .collect();
        self.ui().set_watches(ModelRc::new(VecModel::from(rows)));
    }
}
