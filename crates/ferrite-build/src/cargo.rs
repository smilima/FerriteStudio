//! Runs cargo and the built program.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use cargo_metadata::diagnostic::DiagnosticLevel;
use cargo_metadata::Message;
use ferrite_rpc::no_console;

/// A compiler error or warning with its primary location.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// `error` or `warning`.
    pub level: String,
    pub message: String,
    pub path: Option<PathBuf>,
    /// 1-based, 0 when there is no location.
    pub line: u32,
    pub column: u32,
    /// rustc's full human-readable rendering.
    pub rendered: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CargoEvent {
    /// A line for the Messages pane.
    Line(String),
    Problem(Problem),
    /// A binary target was built here.
    Executable(PathBuf),
    Finished { success: bool },
}

/// Turns one line of `cargo --message-format=json` output into events.
pub fn parse_line(line: &str, root: &Path) -> Vec<CargoEvent> {
    let Ok(msg) = serde_json::from_str::<Message>(line) else {
        return vec![CargoEvent::Line(line.to_string())];
    };
    match msg {
        Message::CompilerMessage(m) => {
            let d = m.message;
            let level = match d.level {
                DiagnosticLevel::Error | DiagnosticLevel::Ice => "error",
                DiagnosticLevel::Warning => "warning",
                _ => return vec![],
            };
            let primary = d.spans.iter().find(|s| s.is_primary);
            if primary.is_none() && (level == "warning" || d.message.starts_with("aborting due to")) {
                return vec![]; // summaries such as "3 warnings emitted"
            }
            let rendered = d.rendered.clone().unwrap_or_else(|| d.message.clone());
            let mut events: Vec<CargoEvent> =
                rendered.trim_end().lines().map(|l| CargoEvent::Line(l.to_string())).collect();
            events.push(CargoEvent::Problem(Problem {
                level: level.to_string(),
                message: d.message.clone(),
                path: primary.map(|s| root.join(&s.file_name)),
                line: primary.map_or(0, |s| s.line_start as u32),
                column: primary.map_or(0, |s| s.column_start as u32),
                rendered,
            }));
            events
        }
        Message::CompilerArtifact(a) => match (a.executable, a.target.is_bin()) {
            (Some(exe), true) => vec![CargoEvent::Executable(exe.into_std_path_buf())],
            _ => vec![],
        },
        Message::BuildFinished(f) => vec![CargoEvent::Finished { success: f.success }],
        Message::TextLine(t) => vec![CargoEvent::Line(t)],
        _ => vec![],
    }
}

/// A running cargo or program process that can be stopped.
#[derive(Clone)]
pub struct Job {
    child: Arc<Mutex<Child>>,
}

impl Job {
    pub fn kill(&self) {
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).kill();
    }
}

fn pump(stream: impl Read + Send + 'static, f: impl Fn(String) + Send + 'static) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            f(line);
        }
    })
}

/// Waits for a child without holding its lock, so [`Job::kill`] works.
fn wait_loop(child: Arc<Mutex<Child>>) -> Option<i32> {
    loop {
        {
            let mut c = child.lock().unwrap_or_else(|e| e.into_inner());
            match c.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => {}
                Err(_) => return None,
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The GNU target matching a Windows MSVC host (`x86_64-pc-windows-msvc`
/// -> `x86_64-pc-windows-gnu`); GDB needs its DWARF debug info.
pub fn gnu_target_for(host: &str) -> Option<String> {
    host.strip_suffix("-pc-windows-msvc").map(|arch| format!("{arch}-pc-windows-gnu"))
}

/// Whether the active toolchain has the standard library for `target`.
pub fn target_installed(target: &str) -> bool {
    let mut cmd = Command::new("rustc");
    cmd.args(["--print", "sysroot"]);
    no_console(&mut cmd);
    cmd.output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
        .is_some_and(|sysroot| sysroot.join("lib").join("rustlib").join(target).join("lib").is_dir())
}

/// Options for [`build`].
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    /// `--target` triple, or the host's default.
    pub target: Option<String>,
    /// Folders put in front of PATH (a C compiler for the target, say).
    pub path_prepend: Vec<PathBuf>,
    /// Extra environment variables for cargo.
    pub env: Vec<(String, String)>,
}

/// Cargo's linker variable for a target: `CARGO_TARGET_<TRIPLE>_LINKER`.
pub fn linker_env_var(target: &str) -> String {
    format!("CARGO_TARGET_{}_LINKER", target.to_ascii_uppercase().replace(['-', '.'], "_"))
}

fn on_path(exe: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(exe).is_file()))
}

/// A MinGW-w64 GCC to link a Windows GNU target from an MSVC toolchain,
/// with the folder it runs from. `None` when one is already on PATH or
/// none is installed; see [`mingw_linker_missing`].
pub fn mingw_linker(target: &str) -> Option<(PathBuf, PathBuf)> {
    let (gcc, envs): (&str, &[&str]) = match target {
        "x86_64-pc-windows-gnu" => ("x86_64-w64-mingw32-gcc.exe", &["mingw64", "ucrt64"]),
        "i686-pc-windows-gnu" => ("i686-w64-mingw32-gcc.exe", &["mingw32"]),
        _ => return None,
    };
    if on_path(gcc) {
        return None;
    }
    // MINGW64 first: it uses msvcrt, the C runtime Rust's GNU target is built for.
    for root in ferrite_rpc::msys2_roots() {
        for env in envs {
            let bin = root.join(env).join("bin");
            if bin.join(gcc).is_file() {
                return Some((bin.join(gcc), bin));
            }
        }
    }
    None
}

/// True when no MinGW-w64 linker for `target` can be found at all.
pub fn mingw_linker_missing(target: &str) -> bool {
    let gcc = match target {
        "x86_64-pc-windows-gnu" => "x86_64-w64-mingw32-gcc.exe",
        "i686-pc-windows-gnu" => "i686-w64-mingw32-gcc.exe",
        _ => return false,
    };
    !on_path(gcc) && mingw_linker(target).is_none()
}

/// Build options for a Windows GNU target, with a MinGW linker if one is
/// needed and installed.
pub fn gnu_target_options(target: &str) -> BuildOptions {
    let mut o = BuildOptions { target: Some(target.to_string()), ..Default::default() };
    if let Some((gcc, bin)) = mingw_linker(target) {
        o.env.push((linker_env_var(target), gcc.to_string_lossy().into_owned()));
        o.path_prepend.push(bin);
    }
    o
}

/// `cargo build` in `root`. Events arrive on worker threads; the last one
/// is always [`CargoEvent::Finished`].
pub fn build(
    root: &Path,
    options: &BuildOptions,
    on_event: impl Fn(CargoEvent) + Send + Sync + 'static,
) -> std::io::Result<Job> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(["build", "--message-format=json", "--color", "never"]);
    if let Some(t) = &options.target {
        cmd.args(["--target", t]);
    }
    for (k, v) in &options.env {
        cmd.env(k, v);
    }
    if !options.path_prepend.is_empty() {
        let current = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = options.path_prepend.clone();
        dirs.extend(std::env::split_paths(&current));
        if let Ok(path) = std::env::join_paths(dirs) {
            cmd.env("PATH", path);
        }
    }
    cmd.current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console(&mut cmd);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let child = Arc::new(Mutex::new(child));
    let on_event = Arc::new(on_event);
    let finished = Arc::new(Mutex::new(false));

    let (ev, fin, root) = (on_event.clone(), finished.clone(), root.to_path_buf());
    let out = pump(stdout, move |line| {
        for e in parse_line(&line, &root) {
            if matches!(e, CargoEvent::Finished { .. }) {
                *fin.lock().unwrap() = true;
            }
            ev(e);
        }
    });
    let ev = on_event.clone();
    let err = pump(stderr, move |line| ev(CargoEvent::Line(line)));
    let waiter = child.clone();
    thread::spawn(move || {
        let code = wait_loop(waiter);
        let _ = out.join();
        let _ = err.join();
        if !*finished.lock().unwrap() {
            on_event(CargoEvent::Finished { success: code == Some(0) });
        }
    });
    Ok(Job { child })
}

/// Starts a built program, streaming its output. `on_exit` gets the exit code.
pub fn run(
    exe: &Path,
    cwd: &Path,
    on_output: impl Fn(String, bool) + Send + Sync + 'static,
    on_exit: impl FnOnce(Option<i32>) + Send + 'static,
) -> std::io::Result<Job> {
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    no_console(&mut cmd);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let child = Arc::new(Mutex::new(child));
    let on_output = Arc::new(on_output);
    let o = on_output.clone();
    let out = pump(stdout, move |l| o(l, false));
    let err = pump(stderr, move |l| on_output(l, true));
    let waiter = child.clone();
    thread::spawn(move || {
        let code = wait_loop(waiter);
        let _ = out.join();
        let _ = err.join();
        on_exit(code);
    });
    Ok(Job { child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_messages_become_problems() {
        let line = r#"{"reason":"compiler-message","package_id":"path+file:///p#app@0.1.0","manifest_path":"/p/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"app","src_path":"/p/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"message":{"rendered":"error[E0425]: cannot find value `y` in this scope\n --> src/main.rs:2:13\n","$message_type":"diagnostic","children":[],"code":{"code":"E0425","explanation":null},"level":"error","message":"cannot find value `y` in this scope","spans":[{"byte_end":30,"byte_start":29,"column_end":14,"column_start":13,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":"not found in this scope","line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;
        let ev = parse_line(line, Path::new("/p"));
        let Some(CargoEvent::Problem(p)) = ev.last() else { panic!("{ev:?}") };
        assert_eq!(p.level, "error");
        assert_eq!(p.path.as_deref(), Some(Path::new("/p/src/main.rs")));
        assert_eq!((p.line, p.column), (2, 13));
        assert!(matches!(&ev[0], CargoEvent::Line(l) if l.starts_with("error[E0425]")));
    }

    #[test]
    fn gnu_targets_match_msvc_hosts() {
        assert_eq!(gnu_target_for("x86_64-pc-windows-msvc").as_deref(), Some("x86_64-pc-windows-gnu"));
        assert_eq!(gnu_target_for("aarch64-pc-windows-msvc").as_deref(), Some("aarch64-pc-windows-gnu"));
        assert_eq!(gnu_target_for("x86_64-unknown-linux-gnu"), None);
    }

    #[test]
    fn linker_variables_follow_cargo_naming() {
        assert_eq!(linker_env_var("x86_64-pc-windows-gnu"), "CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER");
        let o = gnu_target_options("x86_64-pc-windows-gnu");
        assert_eq!(o.target.as_deref(), Some("x86_64-pc-windows-gnu"));
    }

    #[test]
    fn non_json_lines_pass_through() {
        assert_eq!(parse_line("   Compiling app v0.1.0", Path::new("/p")), vec![CargoEvent::Line("   Compiling app v0.1.0".into())]);
        let fin = parse_line(r#"{"reason":"build-finished","success":false}"#, Path::new("/p"));
        assert_eq!(fin, vec![CargoEvent::Finished { success: false }]);
    }
}
