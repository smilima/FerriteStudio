//! Build, Run and Program Reset.

use ferrite_build::cargo::{self, BuildOptions, CargoEvent};
use ferrite_dap::AdapterConfig;

use crate::app::{post, AfterBuild, App};

impl App {
    /// The debug adapter, detected once per session.
    pub fn debug_adapter(&mut self) -> Result<AdapterConfig, String> {
        if let Some(cfg) = &self.adapter {
            return Ok(cfg.clone());
        }
        self.status("Looking for a debugger...");
        let cfg = AdapterConfig::detect()?;
        let dlls = if cfg.path_prepend.is_empty() {
            String::new()
        } else {
            let dirs: Vec<String> = cfg.path_prepend.iter().map(|d| d.display().to_string()).collect();
            format!(" (DLLs from {})", dirs.join(", "))
        };
        self.message(format!("Debugger: {}{dlls}", cfg.program.display()), "info");
        self.adapter = Some(cfg.clone());
        Ok(cfg)
    }

    /// Build options for a debug session: GDB reads DWARF only, so on the
    /// Windows MSVC toolchain the program is built for the GNU target.
    fn debug_build_options(&mut self, cfg: &AdapterConfig) -> Result<BuildOptions, String> {
        if !cfg.needs_dwarf() {
            return Ok(BuildOptions::default());
        }
        let host = ferrite_dap::rust_host().unwrap_or_default();
        let Some(target) = cargo::gnu_target_for(&host) else { return Ok(BuildOptions::default()) };
        if !cargo::target_installed(&target) {
            return Err(format!(
                "GDB cannot read the debug info (PDB) of the {host} toolchain. \
                 Install the GNU target once with: rustup target add {target}"
            ));
        }
        if cargo::mingw_linker_missing(&target) {
            return Err(format!(
                "Building for {target} needs a MinGW-w64 GCC to link. Install MSYS2's \
                 mingw-w64-x86_64-gcc (pacman -S mingw-w64-x86_64-gcc) or put x86_64-w64-mingw32-gcc on PATH."
            ));
        }
        let options = cargo::gnu_target_options(&target);
        let linker = options.env.first().map(|(_, gcc)| format!(", linking with {gcc}")).unwrap_or_default();
        self.message(format!("Building for {target} so GDB can read the debug info{linker}"), "info");
        Ok(options)
    }

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
        self.messages.set_vec(Vec::new());
        let mut after = after;
        let mut options = BuildOptions::default();
        if after == AfterBuild::Debug {
            match self.debug_adapter() {
                Ok(cfg) => match self.debug_build_options(&cfg) {
                    Ok(o) => options = o,
                    Err(e) => {
                        self.message(&e, "error");
                        self.status("Cannot debug: see Messages");
                        self.ui().set_bottom_tab(0);
                        return;
                    }
                },
                Err(e) => {
                    self.message(&e, "warning");
                    self.message("Running without the debugger", "info");
                    after = AfterBuild::Run;
                }
            }
        }
        self.save_all();
        self.build_problems.clear();
        self.refresh_problems();
        self.exe = None;
        self.after_build = after;
        self.ui().set_busy(true);
        self.ui().set_bottom_tab(0);
        self.status("Building...");
        let target = options.target.as_deref().map(|t| format!(" --target {t}")).unwrap_or_default();
        self.message(format!("cargo build{target} ({})", root.display()), "info");
        match cargo::build(&root, &options, |ev| post(move |a| a.on_cargo(ev))) {
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
