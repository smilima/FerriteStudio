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

/// `cargo build` in `root`. Events arrive on worker threads; the last one
/// is always [`CargoEvent::Finished`].
pub fn build(root: &Path, on_event: impl Fn(CargoEvent) + Send + Sync + 'static) -> std::io::Result<Job> {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.args(["build", "--message-format=json", "--color", "never"])
        .current_dir(root)
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
    fn non_json_lines_pass_through() {
        assert_eq!(parse_line("   Compiling app v0.1.0", Path::new("/p")), vec![CargoEvent::Line("   Compiling app v0.1.0".into())]);
        let fin = parse_line(r#"{"reason":"build-finished","success":false}"#, Path::new("/p"));
        assert_eq!(fin, vec![CargoEvent::Finished { success: false }]);
    }
}
