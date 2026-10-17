//! Application state and the glue between the Slint UI and the crates.
//!
//! All state lives in one [`App`] on the UI thread. UI callbacks and worker
//! threads reach it through [`act`] and [`post`]; when the state is already
//! borrowed (a Slint callback fired while Rust was updating the UI), the
//! action is deferred to the next event-loop turn instead of panicking.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ferrite_build::{FormEntry, Job, Problem, Project};
use ferrite_form::{Form, KINDS};
use ferrite_lsp::{path_key, Completion, Diag, LspClient};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::debug_ops::DebugSession;
use crate::{AppWindow, FileItem, Mark, MessageRow, PaletteItem, ProblemRow, TabItem};

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` with the application state, now or on the next event-loop turn.
pub fn act<F: FnOnce(&mut App) + 'static>(f: F) {
    let mut f = Some(f);
    let busy = APP.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let (Some(app), Some(f)) = (guard.as_mut(), f.take()) {
                f(app);
            }
            false
        }
        Err(_) => true,
    });
    if let (true, Some(f)) = (busy, f) {
        slint::Timer::single_shot(Duration::ZERO, move || act(f));
    }
}

/// Runs `f` on the UI thread from any thread.
pub fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || act(f));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Welcome,
    Design,
    Code,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterBuild {
    Nothing,
    Run,
    Debug,
}

pub struct FormDoc {
    pub entry: FormEntry,
    pub form: Form,
    pub selected: Option<usize>,
    pub dirty: bool,
}

pub struct Buffer {
    pub path: PathBuf,
    pub text: String,
    pub saved: String,
    pub read_only: bool,
    pub crlf: bool,
    pub caret: usize,
}

impl Buffer {
    pub fn dirty(&self) -> bool {
        self.text != self.saved
    }

    pub fn is_rust(&self) -> bool {
        self.path.extension().is_some_and(|e| e == "rs")
    }
}

/// Where a Problems row leads: file, 0-based line, 0-based character column.
#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

pub struct App {
    pub ui: slint::Weak<AppWindow>,
    pub project: Option<Project>,
    pub form: Option<FormDoc>,
    pub buffers: Vec<Buffer>,
    pub current: Option<usize>,
    pub view: View,
    pub messages: Rc<VecModel<MessageRow>>,
    pub build_problems: Vec<Problem>,
    pub lsp_diags: BTreeMap<String, (PathBuf, Vec<Diag>)>,
    pub problem_targets: Vec<Target>,
    pub lsp: Option<Arc<LspClient>>,
    pub lsp_generation: u64,
    /// rust-analyzer is idle with its workspace loaded.
    pub lsp_ready: bool,
    pub lsp_base: String,
    /// 1-based breakpoint lines per file.
    pub breakpoints: BTreeMap<String, (PathBuf, BTreeSet<u32>)>,
    pub build_job: Option<Job>,
    pub run_job: Option<Job>,
    pub after_build: AfterBuild,
    pub exe: Option<PathBuf>,
    pub debug: Option<DebugSession>,
    pub watches: Vec<(String, String)>,
    pub completions: Vec<Completion>,
    pub shown_completions: Vec<usize>,
    pub caret_line: usize,
    pub caret_offset: usize,
    pub hover: String,
}

pub fn mono_font() -> &'static str {
    if cfg!(windows) {
        "Consolas"
    } else if cfg!(target_os = "macos") {
        "Menlo"
    } else {
        "DejaVu Sans Mono"
    }
}

fn config_file() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("ferrite-studio").join("recent.txt"))
}

fn load_recent() -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_file()?).ok()?;
    text.lines().next().map(PathBuf::from).filter(|p| p.is_dir())
}

fn save_recent(path: &Path) {
    if let Some(f) = config_file() {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, path.to_string_lossy().as_bytes());
    }
}

/// Default parent folder for new projects: Documents\Rust when it exists.
pub fn default_location() -> PathBuf {
    let docs = dirs::document_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
    let rust = docs.join("Rust");
    if rust.is_dir() {
        rust
    } else {
        docs
    }
}

pub fn install(ui: &AppWindow) {
    let app = App {
        ui: ui.as_weak(),
        project: None,
        form: None,
        buffers: Vec::new(),
        current: None,
        view: View::Welcome,
        messages: Rc::new(VecModel::default()),
        build_problems: Vec::new(),
        lsp_diags: BTreeMap::new(),
        problem_targets: Vec::new(),
        lsp: None,
        lsp_generation: 0,
        lsp_ready: false,
        lsp_base: String::new(),
        breakpoints: BTreeMap::new(),
        build_job: None,
        run_job: None,
        after_build: AfterBuild::Nothing,
        exe: None,
        debug: None,
        watches: Vec::new(),
        completions: Vec::new(),
        shown_completions: Vec::new(),
        caret_line: 0,
        caret_offset: 0,
        hover: String::new(),
    };
    ui.set_messages(ModelRc::from(app.messages.clone()));
    ui.set_mono_font(mono_font().into());
    let palette: Vec<PaletteItem> =
        KINDS.iter().map(|k| PaletteItem { kind: k.kind.into(), hint: k.hint.into() }).collect();
    ui.set_palette(ModelRc::new(VecModel::from(palette)));
    ui.set_new_location(default_location().to_string_lossy().into_owned().into());
    if let Some(recent) = load_recent() {
        ui.set_open_path(recent.to_string_lossy().into_owned().into());
    }
    APP.with(|cell| *cell.borrow_mut() = Some(app));
    wire_callbacks(ui);
}

fn wire_callbacks(ui: &AppWindow) {
    use crate::debug_ops::Step;

    ui.on_show_welcome(|| act(|a| a.show_welcome()));
    ui.on_new_project(|name, loc| act(move |a| a.create_project(&name, &loc)));
    ui.on_browse_new_location(|| act(|a| a.browse_new_location()));
    ui.on_open_project(|p| act(move |a| a.open_project(Path::new(p.trim()))));
    ui.on_browse_open(|| act(|a| a.browse_open()));
    ui.on_save_all(|| act(|a| a.save_all()));
    ui.on_build(|| act(|a| a.build(AfterBuild::Nothing)));
    ui.on_run(|| act(|a| a.run()));
    ui.on_run_without_debugging(|| act(|a| a.build(AfterBuild::Run)));
    ui.on_program_reset(|| act(|a| a.program_reset()));
    ui.on_step_over(|| act(|a| a.step(Step::Over)));
    ui.on_step_into(|| act(|a| a.step(Step::Into)));
    ui.on_step_out(|| act(|a| a.step(Step::Out)));
    ui.on_toggle_breakpoint_at_caret(|| act(|a| a.toggle_breakpoint_at_caret()));
    ui.on_toggle_breakpoint(|line| act(move |a| a.toggle_breakpoint_line(line)));
    ui.on_toggle_design_code(|| act(|a| a.toggle_design_code()));
    ui.on_open_file(|p| act(move |a| a.open_path(Path::new(p.as_str()))));
    ui.on_activate_tab(|i| act(move |a| a.activate_tab(i as usize)));
    ui.on_close_tab(|i| act(move |a| a.close_tab(i as usize)));
    ui.on_palette_pick(|k| act(move |a| a.palette_pick(&k)));
    ui.on_palette_add(|k| act(move |a| a.palette_add(&k)));

    ui.on_select(|i| act(move |a| a.select(i)));
    ui.on_drop_control(|x, y| act(move |a| a.drop_control(x, y)));
    ui.on_drag_control(|i, dx, dy| act(move |a| a.drag_control(i as usize, dx, dy)));
    ui.on_resize_control(|i, dw, dh| act(move |a| a.resize_control(i as usize, dw, dh)));
    ui.on_resize_form(|dw, dh| act(move |a| a.resize_form(dw, dh)));
    ui.on_control_double_clicked(|i| act(move |a| a.default_event(i as usize)));
    ui.on_delete_selected(|| act(|a| a.delete_selected()));
    ui.on_nudge(|dx, dy| act(move |a| a.nudge(dx, dy)));
    ui.on_prop_edited(|n, v| act(move |a| a.prop_edited(&n, &v)));
    ui.on_event_activated(|n| act(move |a| a.event_activated(&n)));

    ui.on_editor_edited(|t| act(move |a| a.editor_edited(t.to_string())));
    ui.on_caret_moved(|line, _col, offset| act(move |a| a.caret_moved(line, offset)));
    ui.on_request_completion(|| act(|a| a.request_completion()));
    ui.on_completion_chosen(|i| act(move |a| a.completion_chosen(i as usize)));
    ui.on_completion_cancel(|| act(|a| a.close_popups()));
    ui.on_hover_at_caret(|| act(|a| a.hover_at_caret()));
    ui.on_goto_definition(|| act(|a| a.goto_definition()));
    ui.on_format_document(|| act(|a| a.format_document()));
    ui.on_add_watch_at_caret(|| act(|a| a.add_watch_at_caret()));
    ui.on_editor_special_key(|k| act(move |a| a.special_key(&k)));

    ui.on_problem_activated(|i| act(move |a| a.problem_activated(i as usize)));
    ui.on_stack_activated(|i| act(move |a| a.select_frame(i as usize)));
    ui.on_local_activated(|i| act(move |a| a.expand_local(i as usize)));
    ui.on_add_watch(|e| act(move |a| a.add_watch(e.to_string())));
    ui.on_remove_watch(|i| act(move |a| a.remove_watch(i as usize)));
}

impl App {
    pub fn ui(&self) -> AppWindow {
        self.ui.upgrade().expect("main window is alive")
    }

    pub fn message(&self, text: impl AsRef<str>, kind: &str) {
        for line in text.as_ref().lines() {
            self.messages.push(MessageRow { text: line.into(), kind: kind.into() });
        }
        let excess = self.messages.row_count().saturating_sub(5000);
        for _ in 0..excess {
            self.messages.remove(0);
        }
        self.ui().invoke_scroll_messages_to_end();
    }

    pub fn status(&self, text: impl AsRef<str>) {
        self.ui().set_status(text.as_ref().into());
    }

    pub fn root(&self) -> Option<&Path> {
        self.project.as_ref().map(|p| p.root.as_path())
    }

    /// Path relative to the project, for labels.
    pub fn rel(&self, path: &Path) -> String {
        let r = self.root().and_then(|root| path.strip_prefix(root).ok()).unwrap_or(path);
        r.to_string_lossy().replace('\\', "/")
    }

    // ---- Projects ----

    pub fn show_welcome(&mut self) {
        self.view = View::Welcome;
        self.refresh_view();
    }

    pub fn browse_new_location(&mut self) {
        let start = PathBuf::from(self.ui().get_new_location().as_str());
        if let Some(dir) = rfd::FileDialog::new().set_directory(start).set_title("Location for the new project").pick_folder() {
            self.ui().set_new_location(dir.to_string_lossy().into_owned().into());
        }
        self.show_welcome();
    }

    pub fn browse_open(&mut self) {
        let start = self.root().map(Path::to_path_buf).unwrap_or_else(default_location);
        if let Some(dir) = rfd::FileDialog::new().set_directory(start).set_title("Open a project folder").pick_folder() {
            self.open_project(&dir);
        }
    }

    pub fn create_project(&mut self, name: &str, location: &str) {
        let parent = if location.trim().is_empty() { default_location() } else { PathBuf::from(location.trim()) };
        if let Err(e) = std::fs::create_dir_all(&parent) {
            self.status(format!("Cannot create {}: {e}", parent.display()));
            return;
        }
        match Project::create(&parent, name.trim()) {
            Ok(p) => {
                let root = p.root.clone();
                self.open_project(&root);
                self.message(format!("Created {}", root.display()), "info");
            }
            Err(e) => {
                self.status(&e);
                self.message(e, "error");
            }
        }
    }

    pub fn open_project(&mut self, dir: &Path) {
        let project = match Project::open(dir) {
            Ok(p) => p,
            Err(e) => {
                self.status(&e);
                self.message(e, "error");
                return;
            }
        };
        self.close_project();
        save_recent(&project.root);
        self.ui().set_open_path(project.root.to_string_lossy().into_owned().into());
        self.ui().set_project_name(project.manifest.project.name.clone().into());
        let entry = project.manifest.forms.first().cloned();
        let main_rs = project.path("src/main.rs");
        let name = project.manifest.project.name.clone();
        self.project = Some(project);

        let mut opened_form = false;
        if let Some(entry) = entry {
            let loaded = self.project.as_ref().unwrap().load_form(&entry);
            match loaded {
                Ok(form) => {
                    self.form = Some(FormDoc { entry, form, selected: None, dirty: false });
                    opened_form = true;
                }
                Err(e) => {
                    self.message(format!("The designer cannot open this form: {e}"), "error");
                    let path = self.project.as_ref().unwrap().path(&entry.file);
                    self.open_path(&path);
                }
            }
        }
        if opened_form {
            self.show_design();
        } else if main_rs.is_file() && self.current.is_none() {
            self.open_path(&main_rs);
        }
        self.refresh_files();
        self.refresh_tabs();
        self.refresh_problems();
        self.status(format!("Opened {name}"));
        self.start_lsp();
    }

    fn close_project(&mut self) {
        self.program_reset();
        if let Some(lsp) = self.lsp.take() {
            lsp.shutdown();
        }
        self.lsp_generation += 1;
        self.lsp_ready = false;
        self.project = None;
        self.form = None;
        self.buffers.clear();
        self.current = None;
        self.build_problems.clear();
        self.lsp_diags.clear();
        self.breakpoints.clear();
        self.exe = None;
        self.watches.clear();
        self.messages.set_vec(Vec::new());
        self.ui().set_project_name(SharedString::new());
        self.ui().set_lsp_status(SharedString::new());
    }

    // ---- Buffers and tabs ----

    pub fn buffer_index(&self, path: &Path) -> Option<usize> {
        let key = path_key(path);
        self.buffers.iter().position(|b| path_key(&b.path) == key)
    }

    pub fn open_buffer(&mut self, path: &Path) -> Result<usize, String> {
        if let Some(i) = self.buffer_index(path) {
            return Ok(i);
        }
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let crlf = raw.contains("\r\n");
        let text = if crlf { raw.replace("\r\n", "\n") } else { raw };
        let read_only = self.root().is_none_or(|r| !path.starts_with(r));
        self.buffers.push(Buffer {
            path: path.to_path_buf(),
            saved: text.clone(),
            text,
            read_only,
            crlf,
            caret: 0,
        });
        let i = self.buffers.len() - 1;
        if let (Some(lsp), true) = (&self.lsp, self.buffers[i].is_rust()) {
            let _ = lsp.did_open(path, &self.buffers[i].text);
        }
        Ok(i)
    }

    /// Opens any file: forms in the designer, everything else in the editor.
    pub fn open_path(&mut self, path: &Path) {
        let is_form = self.form.as_ref().zip(self.project.as_ref()).is_some_and(|(f, p)| {
            path_key(&p.path(&f.entry.file)) == path_key(path)
        });
        if is_form {
            self.show_design();
            return;
        }
        match self.open_buffer(path) {
            Ok(i) => self.show_buffer(i),
            Err(e) => {
                self.status(&e);
                self.message(e, "error");
            }
        }
    }

    /// Opens a file in the editor with the caret at an offset computed from
    /// its text, moving the caret once (two quick moves would race with the
    /// editor's caret callbacks).
    pub fn open_at(&mut self, path: &Path, locate: impl FnOnce(&str) -> usize) {
        match self.open_buffer(path) {
            Ok(i) => {
                let off = locate(&self.buffers[i].text).min(self.buffers[i].text.len());
                self.buffers[i].caret = off;
                if self.current == Some(i) && self.view == View::Code {
                    self.goto_offset(off);
                } else {
                    self.show_buffer(i);
                }
            }
            Err(e) => {
                self.status(&e);
                self.message(e, "error");
            }
        }
    }

    pub fn show_buffer(&mut self, i: usize) {
        self.close_popups();
        self.current = Some(i);
        self.view = View::Code;
        self.refresh_view();
        self.refresh_editor(true);
        self.refresh_tabs();
        self.refresh_files();
        let caret = self.buffers[i].caret;
        self.goto_offset(caret);
    }

    /// Tabs: the form's design view first (when there is a form), then buffers.
    pub fn activate_tab(&mut self, i: usize) {
        let offset = usize::from(self.form.is_some());
        if self.form.is_some() && i == 0 {
            self.show_design();
        } else if i - offset < self.buffers.len() {
            self.show_buffer(i - offset);
        }
    }

    pub fn close_tab(&mut self, i: usize) {
        let offset = usize::from(self.form.is_some());
        if self.form.is_some() && i == 0 {
            return;
        }
        let bi = i - offset;
        if bi >= self.buffers.len() {
            return;
        }
        if self.buffers[bi].dirty() {
            self.save_buffer(bi);
        }
        self.buffers.remove(bi);
        self.current = match self.current {
            Some(c) if c == bi => None,
            Some(c) if c > bi => Some(c - 1),
            other => other,
        };
        if self.current.is_none() && self.view == View::Code {
            if !self.buffers.is_empty() {
                self.show_buffer(bi.min(self.buffers.len() - 1));
                return;
            } else if self.form.is_some() {
                self.show_design();
                return;
            }
        }
        self.refresh_tabs();
        self.refresh_editor(true);
    }

    // ---- Saving ----

    fn write_if_changed(&self, path: &Path, text: &str) -> Result<bool, String> {
        if std::fs::read_to_string(path).ok().as_deref() == Some(text) {
            return Ok(false);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(true)
    }

    fn save_buffer(&mut self, i: usize) {
        let b = &self.buffers[i];
        if b.read_only {
            return;
        }
        let on_disk = if b.crlf { b.text.replace('\n', "\r\n") } else { b.text.clone() };
        match std::fs::write(&b.path, on_disk) {
            Ok(()) => {
                let path = b.path.clone();
                self.buffers[i].saved = self.buffers[i].text.clone();
                if let (Some(lsp), true) = (&self.lsp, self.buffers[i].is_rust()) {
                    let _ = lsp.did_save(&path);
                }
            }
            Err(e) => self.message(format!("Could not save {}: {e}", b.path.display()), "error"),
        }
    }

    /// Writes the form (its .slint file, wiring and handler stubs) and every
    /// modified editor buffer.
    pub fn save_all(&mut self) {
        if let (Some(doc), Some(project)) = (self.form.as_ref(), self.project.as_ref()) {
            let events_path = project.path(&doc.entry.events);
            let events_text = match self.buffer_index(&events_path) {
                Some(i) => self.buffers[i].text.clone(),
                None => std::fs::read_to_string(&events_path)
                    .unwrap_or_else(|_| ferrite_form::codegen::events_template(&doc.form)),
            };
            let out = project.form_outputs(&doc.entry, &doc.form, &events_text);
            let slint_path = project.path(&doc.entry.file);
            let wiring_path = project.path(&doc.entry.wiring);
            let form_name = doc.form.name.clone();
            for (path, text) in [(&slint_path, &out.slint), (&wiring_path, &out.wiring)] {
                if let Err(e) = self.write_if_changed(path, text) {
                    self.message(e, "error");
                }
                if let Some(i) = self.buffer_index(path) {
                    self.buffers[i].text = text.clone();
                    self.buffers[i].saved = text.clone();
                }
            }
            match self.buffer_index(&events_path) {
                Some(i) => {
                    if self.buffers[i].text != out.events.text {
                        self.buffers[i].text = out.events.text.clone();
                        if self.current == Some(i) {
                            self.refresh_editor(true);
                        }
                    }
                }
                None => {
                    if let Err(e) = self.write_if_changed(&events_path, &out.events.text) {
                        self.message(e, "error");
                    }
                }
            }
            for name in &out.events.removed {
                self.message(format!("Removed empty handler {name}"), "info");
            }
            let project = self.project.as_mut().unwrap();
            project.forget_handlers(&form_name, &out.events.removed);
            if let Err(e) = project.save_manifest() {
                self.message(e, "error");
            }
            if let Some(doc) = self.form.as_mut() {
                doc.dirty = false;
            }
        }
        for i in 0..self.buffers.len() {
            if self.buffers[i].dirty() {
                self.save_buffer(i);
            }
        }
        self.refresh_tabs();
        self.status("Saved");
    }

    // ---- Refreshing the UI ----

    pub fn refresh_view(&self) {
        let ui = self.ui();
        ui.set_view(
            match self.view {
                View::Welcome => "welcome",
                View::Design => "design",
                View::Code => "code",
            }
            .into(),
        );
        ui.set_has_form(self.form.is_some());
    }

    pub fn refresh_files(&self) {
        let Some(project) = &self.project else {
            self.ui().set_files(ModelRc::default());
            return;
        };
        let current = self.current.filter(|_| self.view == View::Code).map(|i| path_key(&self.buffers[i].path));
        let form_file = self.form.as_ref().map(|f| path_key(&project.path(&f.entry.file)));
        let items: Vec<FileItem> = project
            .files()
            .iter()
            .map(|p| {
                let key = path_key(p);
                let is_form = form_file.as_deref() == Some(key.as_str());
                let label = if is_form {
                    format!("{}  [{}]", self.rel(p), self.form.as_ref().unwrap().form.name)
                } else {
                    self.rel(p)
                };
                FileItem {
                    label: label.into(),
                    path: p.to_string_lossy().into_owned().into(),
                    is_form,
                    active: (is_form && self.view == View::Design) || current.as_deref() == Some(key.as_str()),
                }
            })
            .collect();
        self.ui().set_files(ModelRc::new(VecModel::from(items)));
    }

    pub fn refresh_tabs(&self) {
        let mut tabs = Vec::new();
        if let Some(doc) = &self.form {
            tabs.push(TabItem {
                label: format!("{} (Design)", doc.form.name).into(),
                dirty: doc.dirty,
                active: self.view == View::Design,
                closable: false,
            });
        }
        for (i, b) in self.buffers.iter().enumerate() {
            let name = b.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let label = if b.read_only { format!("{name} (read-only)") } else { name };
            tabs.push(TabItem {
                label: label.into(),
                dirty: b.dirty(),
                active: self.view == View::Code && self.current == Some(i),
                closable: true,
            });
        }
        self.ui().set_tabs(ModelRc::new(VecModel::from(tabs)));
        self.refresh_caret_text();
    }

    pub fn refresh_caret_text(&self) {
        let text = match (self.view, self.current) {
            (View::Code, Some(i)) => {
                let b = &self.buffers[i];
                let line_start = b.text[..self.caret_offset.min(b.text.len())].rfind('\n').map_or(0, |p| p + 1);
                let col = b.text[line_start..self.caret_offset.min(b.text.len())].chars().count();
                format!("Ln {}, Col {}{}", self.caret_line + 1, col + 1, if b.dirty() { "  *" } else { "" })
            }
            (View::Design, _) => {
                let dirty = self.form.as_ref().is_some_and(|f| f.dirty);
                if dirty { "Form modified".into() } else { String::new() }
            }
            _ => String::new(),
        };
        self.ui().set_caret_text(text.into());
    }

    /// Updates the editor from the current buffer. `reset_text` also replaces
    /// the text itself, which moves the caret, so it is only used when the
    /// buffer changes underneath the editor.
    pub fn refresh_editor(&self, reset_text: bool) {
        let ui = self.ui();
        let Some(b) = self.current.and_then(|i| self.buffers.get(i)) else {
            ui.set_editor_text(SharedString::new());
            ui.set_line_numbers("1".into());
            ui.set_line_count(1);
            ui.set_breakpoints(ModelRc::default());
            ui.set_marks(ModelRc::default());
            ui.set_exec_line(-1);
            return;
        };
        if reset_text {
            ui.set_editor_text(b.text.as_str().into());
            ui.set_editor_read_only(b.read_only);
        }
        let lines = b.text.matches('\n').count() + 1;
        if reset_text || ui.get_line_count() as usize != lines {
            ui.set_line_count(lines as i32);
            ui.set_line_numbers(crate::views::line_numbers(lines).into());
        }
        let key = path_key(&b.path);
        let bps: Vec<i32> = self
            .breakpoints
            .get(&key)
            .map(|(_, lines)| lines.iter().map(|l| *l as i32 - 1).collect())
            .unwrap_or_default();
        ui.set_breakpoints(ModelRc::new(VecModel::from(bps)));
        ui.set_marks(ModelRc::new(VecModel::from(self.marks_for(b))));
        ui.set_exec_line(self.exec_line_for(&b.path));
    }

    fn marks_for(&self, b: &Buffer) -> Vec<Mark> {
        let mut marks = Vec::new();
        let starts = ferrite_lsp::text::line_starts(&b.text);
        if let Some((_, diags)) = self.lsp_diags.get(&path_key(&b.path)) {
            for d in diags {
                let level = match d.severity {
                    ferrite_lsp::Severity::Error => "error",
                    ferrite_lsp::Severity::Warning => "warning",
                    _ => continue,
                };
                let line = d.line as usize;
                let Some(&ls) = starts.get(line) else { continue };
                let start = d.start.clamp(ls, b.text.len());
                let line_end = b.text[ls..].find('\n').map_or(b.text.len(), |e| ls + e);
                let end = d.end.clamp(start, line_end.max(start));
                marks.push(Mark {
                    line: line as i32,
                    col: b.text.get(ls..start).map_or(0, |s| s.chars().count()) as i32,
                    len: b.text.get(start..end).map_or(1, |s| s.chars().count()).max(1) as i32,
                    level: level.into(),
                });
            }
        }
        for p in &self.build_problems {
            if p.path.as_deref().map(path_key) == Some(path_key(&b.path)) && p.line > 0 {
                marks.push(Mark {
                    line: p.line as i32 - 1,
                    col: p.column.saturating_sub(1) as i32,
                    len: 1,
                    level: p.level.as_str().into(),
                });
            }
        }
        marks
    }

    pub fn refresh_problems(&mut self) {
        let mut rows = Vec::new();
        let mut targets = Vec::new();
        for p in &self.build_problems {
            let location = match &p.path {
                Some(path) => format!("{}:{}:{}", self.rel(path), p.line, p.column),
                None => String::new(),
            };
            rows.push(ProblemRow {
                level: p.level.as_str().into(),
                source: "cargo".into(),
                message: p.message.as_str().into(),
                location: location.into(),
            });
            targets.push(p.path.clone().map(|path| Target {
                path,
                line: p.line.saturating_sub(1) as usize,
                col: p.column.saturating_sub(1) as usize,
            }));
        }
        for severity in [ferrite_lsp::Severity::Error, ferrite_lsp::Severity::Warning] {
            for (path, diags) in self.lsp_diags.values() {
                for d in diags.iter().filter(|d| d.severity == severity) {
                    rows.push(ProblemRow {
                        level: if severity == ferrite_lsp::Severity::Error { "error" } else { "warning" }.into(),
                        source: "rust-analyzer".into(),
                        message: d.message.lines().next().unwrap_or("").into(),
                        location: format!("{}:{}", self.rel(path), d.line + 1).into(),
                    });
                    targets.push(Some(Target { path: path.clone(), line: d.line as usize, col: usize::MAX }));
                }
            }
        }
        // Rows without a location get an empty target; `col == usize::MAX`
        // marks a rust-analyzer diagnostic, whose exact offset is looked up
        // when the row is activated.
        self.problem_targets =
            targets.into_iter().map(|t| t.unwrap_or(Target { path: PathBuf::new(), line: 0, col: 0 })).collect();
        self.ui().set_problems(ModelRc::new(VecModel::from(rows)));
        if let Some(b) = self.current.and_then(|i| self.buffers.get(i)) {
            self.ui().set_marks(ModelRc::new(VecModel::from(self.marks_for(b))));
        }
    }

    pub fn problem_activated(&mut self, i: usize) {
        let Some(t) = self.problem_targets.get(i).cloned() else { return };
        if t.path.as_os_str().is_empty() {
            return;
        }
        let diag_offset = if t.col == usize::MAX {
            self.lsp_diags
                .get(&path_key(&t.path))
                .and_then(|(_, ds)| ds.iter().find(|d| d.line as usize == t.line))
                .map(|d| d.start)
        } else {
            None
        };
        self.open_at(&t.path, |text| diag_offset.unwrap_or_else(|| line_col_to_offset(text, t.line, t.col)));
    }

    /// Moves the caret in the current buffer and scrolls to it.
    pub fn goto_offset(&mut self, offset: usize) {
        let Some(i) = self.current else { return };
        let text = &self.buffers[i].text;
        let offset = offset.min(text.len());
        let line = text[..offset].matches('\n').count();
        self.caret_offset = offset;
        self.caret_line = line;
        self.buffers[i].caret = offset;
        self.ui().invoke_goto_offset(offset as i32, line as i32);
        self.refresh_caret_text();
    }
}

/// 0-based line and character column to a byte offset, clamped to the line.
pub fn line_col_to_offset(text: &str, line: usize, col: usize) -> usize {
    let starts = ferrite_lsp::text::line_starts(text);
    let Some(&ls) = starts.get(line) else { return text.len() };
    let line_end = text[ls..].find('\n').map_or(text.len(), |e| ls + e);
    text[ls..line_end].char_indices().nth(col).map_or(line_end, |(i, _)| ls + i)
}
