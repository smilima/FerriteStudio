//! Build, Run and Program Reset.

use ferrite_build::cargo::{self, CargoEvent};

use crate::app::{post, AfterBuild, App};

impl App {
    /// Ctrl+F9. Saves everything first, as RAD Studio does before compiling.
    pub fn build(&mut self, after: AfterBuild) {
        let Some(root) = self.root().map(|r| r.to_path_buf()) else { return };
        if self.build_job.is_some() {
            self.status("A build is already running");
            return;
        }
        if self.run_job.is_some() || self.debug.is_some() {
            self.program_reset();
        }
        self.save_all();
        self.messages.set_vec(Vec::new());
        self.build_problems.clear();
        self.refresh_problems();
        self.exe = None;
        self.after_build = after;
        self.ui().set_busy(true);
        self.ui().set_bottom_tab(0);
        self.status("Building...");
        self.message(format!("cargo build ({})", root.display()), "info");
        match cargo::build(&root, |ev| post(move |a| a.on_cargo(ev))) {
            Ok(job) => self.build_job = Some(job),
            Err(e) => {
                self.ui().set_busy(false);
                self.message(format!("Could not start cargo: {e}"), "error");
                self.status("cargo is not available");
            }
        }
    }

    fn on_cargo(&mut self, ev: CargoEvent) {
        match ev {
            CargoEvent::Line(l) => {
                let kind = if l.starts_with("error") {
                    "error"
                } else if l.starts_with("warning") {
                    "warning"
                } else {
                    ""
                };
                self.message(l, kind);
            }
            CargoEvent::Problem(p) => {
                self.build_problems.push(p);
                self.refresh_problems();
            }
            CargoEvent::Executable(path) => self.exe = Some(path),
            CargoEvent::Finished { success } => {
                self.build_job = None;
                self.ui().set_busy(false);
                let errors = self.build_problems.iter().filter(|p| p.level == "error").count();
                let warnings = self.build_problems.len() - errors;
                if success {
                    self.message(format!("Build succeeded, {warnings} warning(s)"), "info");
                    self.status("Build succeeded");
                    match self.after_build {
                        AfterBuild::Run => self.run_program(),
                        AfterBuild::Debug => self.start_debugging(),
                        AfterBuild::Nothing => {}
                    }
                } else {
                    self.message(format!("Build failed: {errors} error(s), {warnings} warning(s)"), "error");
                    self.status("Build failed");
                    if !self.build_problems.is_empty() {
                        self.ui().set_bottom_tab(1);
                        self.problem_activated(0);
                    }
                }
                self.after_build = AfterBuild::Nothing;
            }
        }
    }

    /// F9: continue when stopped in the debugger, otherwise build and debug.
    pub fn run(&mut self) {
        if let Some(d) = &self.debug {
            if d.stopped {
                self.resume();
            }
            return;
        }
        self.build(AfterBuild::Debug);
    }

    pub fn run_program(&mut self) {
        let (Some(exe), Some(root)) = (self.exe.clone(), self.root().map(|r| r.to_path_buf())) else {
            self.message("The build produced no executable to run", "error");
            return;
        };
        self.message(format!("Running {}", exe.display()), "info");
        self.status("Running");
        let job = cargo::run(
            &exe,
            &root,
            |line, is_err| post(move |a| a.message(line, if is_err { "warning" } else { "" })),
            |code| {
                post(move |a| {
                    a.run_job = None;
                    a.ui().set_busy(false);
                    a.message(format!("Process exited with code {}", code.map_or("?".into(), |c| c.to_string())), "info");
                    a.status("Ready");
                })
            },
        );
        match job {
            Ok(j) => {
                self.run_job = Some(j);
                self.ui().set_busy(true);
            }
            Err(e) => self.message(format!("Could not start {}: {e}", exe.display()), "error"),
        }
    }

    /// Ctrl+F2: stop whatever is running.
    pub fn program_reset(&mut self) {
        if let Some(job) = self.build_job.take() {
            job.kill();
            self.message("Build cancelled", "warning");
        }
        if let Some(job) = self.run_job.take() {
            job.kill();
        }
        if self.debug.is_some() {
            self.stop_debugging();
        }
        self.after_build = AfterBuild::Nothing;
        self.ui().set_busy(false);
    }
}
