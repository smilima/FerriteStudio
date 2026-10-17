//! `--selftest <dir>`: drives the IDE through a whole RAD session the way
//! a user would (design a form, double-click for a handler, write code,
//! complete with rust-analyzer, build, debug to a breakpoint, step, reset),
//! saving a screenshot at each stage. Prints SELFTEST PASS or FAIL.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model};

use crate::app::{act, App};

struct Script {
    dir: PathBuf,
    stage: usize,
    stage_started: Instant,
    log: Vec<String>,
}

thread_local! {
    static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
}

pub fn start(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let project = dir.join("selftest_app");
    // Keep the previous run's target folder so reruns skip the Slint build.
    let cache = dir.join("selftest_target_cache");
    if project.join("target").is_dir() && !cache.exists() {
        let _ = std::fs::rename(project.join("target"), &cache);
    }
    let _ = std::fs::remove_dir_all(&project);
    SCRIPT.with(|s| {
        *s.borrow_mut() = Some(Script { dir, stage: 0, stage_started: Instant::now(), log: Vec::new() })
    });
    let timer = Box::leak(Box::new(slint::Timer::default()));
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(300), || act(tick));
}

fn screenshot(app: &App, dir: &std::path::Path, name: &str) -> Result<(), String> {
    let snap = app.ui().window().take_snapshot().map_err(|e| e.to_string())?;
    let (w, h) = (snap.width(), snap.height());
    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in snap.as_slice() {
        ppm.extend_from_slice(&[px.r, px.g, px.b]);
    }
    std::fs::write(dir.join(format!("{name}.ppm")), ppm).map_err(|e| e.to_string())
}

enum Next {
    Wait,
    Advance,
    Fail(String),
}

fn tick(app: &mut App) {
    let Some((stage, elapsed, dir)) =
        SCRIPT.with(|s| s.borrow().as_ref().map(|s| (s.stage, s.stage_started.elapsed(), s.dir.clone())))
    else {
        return;
    };
    let next = run_stage(app, stage, elapsed, &dir);
    let finish = |ok: bool, why: String| {
        SCRIPT.with(|s| {
            let s = s.borrow_mut().take().unwrap();
            for l in &s.log {
                println!("  {l}");
            }
        });
        println!("{} {why}", if ok { "SELFTEST PASS" } else { "SELFTEST FAIL" });
        let _ = slint::quit_event_loop();
    };
    match next {
        Next::Wait if elapsed > Duration::from_secs(1500) => finish(false, format!("stage {stage} timed out")),
        Next::Wait => {}
        Next::Advance => {
            SCRIPT.with(|s| {
                let mut s = s.borrow_mut();
                let s = s.as_mut().unwrap();
                s.log.push(format!("stage {stage} done in {:.1}s", elapsed.as_secs_f32()));
                s.stage += 1;
                s.stage_started = Instant::now();
            });
            if stage == LAST_STAGE {
                app.program_reset();
                finish(true, "all stages passed".into());
            }
        }
        Next::Fail(why) => finish(false, format!("stage {stage}: {why}")),
    }
}

const LAST_STAGE: usize = 10;

fn run_stage(app: &mut App, stage: usize, elapsed: Duration, dir: &std::path::Path) -> Next {
    let ui = app.ui();
    let check = |ok: bool, why: &str| if ok { Next::Advance } else { Next::Fail(why.to_string()) };
    match stage {
        // New Slint Application.
        0 => {
            app.create_project("selftest_app", &dir.to_string_lossy());
            let cache = dir.join("selftest_target_cache");
            if cache.is_dir() {
                let _ = std::fs::rename(&cache, dir.join("selftest_app").join("target"));
            }
            check(app.form.is_some() && ui.get_view() == "design", "project did not open in the designer")
        }
        // Drop a label, an edit and a button, and set their properties.
        1 => {
            app.palette_add("Text");
            app.prop_edited("text", "Your name:");
            app.prop_edited("x", "24");
            app.prop_edited("y", "32");
            app.palette_add("LineEdit");
            app.prop_edited("placeholder-text", "Type a name");
            app.prop_edited("x", "120");
            app.prop_edited("y", "24");
            app.palette_add("Button");
            app.prop_edited("text", "Greet");
            app.prop_edited("x", "120");
            app.prop_edited("y", "72");
            app.palette_add("CheckBox");
            app.prop_edited("x", "24");
            app.prop_edited("y", "128");
            app.palette_add("Slider");
            app.prop_edited("x", "24");
            app.prop_edited("y", "176");
            app.prop_edited("value", "40");
            let names: Vec<String> = app.form.as_ref().unwrap().form.controls.iter().map(|c| c.name.clone()).collect();
            check(names == ["label1", "edit1", "button1", "check1", "slider1"], "unexpected control names")
        }
        2 => {
            if elapsed < Duration::from_millis(900) {
                return Next::Wait;
            }
            app.select(2);
            match screenshot(app, dir, "1-designer") {
                Ok(()) => Next::Advance,
                Err(e) => Next::Fail(e),
            }
        }
        // Double-click the button: handler stub, wiring, caret in the stub.
        3 => {
            app.default_event(2);
            let Some(i) = app.current else { return Next::Fail("no editor after double-click".into()) };
            let text = app.buffers[i].text.clone();
            let ok = text.contains("pub fn button1_clicked(form: &MainForm)") && ui.get_view() == "code";
            if !ok {
                return Next::Fail("handler stub missing".into());
            }
            let body = "    let name = form.get_edit1_text();\n    form.set_label1_text(format!(\"Hello, {name}!\").into());\n    println!(\"greeted {name}\");\n";
            let new = text.replace("    // button1.clicked\n", body);
            let caret = new.find("println!").unwrap();
            app.replace_text(i, new, caret);
            app.save_all();
            let wiring = std::fs::read_to_string(app.project.as_ref().unwrap().path("src/main_form_wiring.rs")).unwrap();
            check(wiring.contains("events::button1_clicked(&form);"), "wiring not generated")
        }
        // Wait for rust-analyzer, then complete after `form.`.
        4 => {
            let ready = app.lsp_ready;
            if !ready && elapsed < Duration::from_secs(1400) {
                return Next::Wait;
            }
            if !ready {
                return Next::Fail(format!("rust-analyzer not ready: {}", ui.get_lsp_status()));
            }
            let i = app.current.unwrap();
            let at = app.buffers[i].text.find("    println!").unwrap();
            let mut text = app.buffers[i].text.clone();
            text.insert_str(at, "    form.set_\n");
            let caret = at + "    form.set_".len();
            app.replace_text(i, text, caret);
            app.caret_offset = caret;
            app.request_completion(true);
            Next::Advance
        }
        5 => {
            if !ui.get_completion_open() {
                if elapsed > Duration::from_secs(20) && elapsed.as_secs() % 5 == 0 {
                    app.request_completion(true);
                }
                return Next::Wait;
            }
            let labels: Vec<String> = ui.get_completions().iter().map(|c| c.label.to_string()).collect();
            if !labels.iter().any(|l| l.starts_with("set_label1_text")) {
                return Next::Fail(format!("completion lacks set_label1_text: {labels:?}"));
            }
            if let Err(e) = screenshot(app, dir, "2-completion") {
                return Next::Fail(e);
            }
            // Undo the probe line.
            app.close_popups();
            let i = app.current.unwrap();
            let text = app.buffers[i].text.replace("    form.set_\n", "");
            app.replace_text(i, text, 0);
            app.save_all();
            Next::Advance
        }
        // Breakpoint on the wiring call in main.rs, then F9 (build + debug).
        6 => {
            let main = app.project.as_ref().unwrap().path("src/main.rs");
            app.open_path(&main);
            let i = app.current.unwrap();
            let line = app.buffers[i].text.lines().position(|l| l.contains("main_form_wiring::connect")).unwrap();
            app.toggle_breakpoint_line(line as i32);
            app.run();
            Next::Advance
        }
        7 => {
            if ui.get_debug_state() != "stopped" {
                let failed = app.build_job.is_none()
                    && ui.get_debug_state() == ""
                    && app.build_problems.iter().any(|p| p.level == "error");
                if failed {
                    return Next::Fail(format!("build failed: {:?}", app.build_problems.first().map(|p| &p.message)));
                }
                return Next::Wait;
            }
            if ui.get_locals().row_count() == 0 || ui.get_exec_line() < 0 {
                return Next::Wait;
            }
            app.add_watch("form".into());
            Next::Advance
        }
        8 => {
            if elapsed < Duration::from_millis(1500) {
                return Next::Wait;
            }
            let expected = format!("Ln {},", ui.get_exec_line() + 1);
            if !ui.get_caret_text().starts_with(&expected) {
                return Next::Fail(format!("status bar shows {:?}, expected {expected}", ui.get_caret_text()));
            }
            match screenshot(app, dir, "3-debugger") {
                Ok(()) => {
                    let before = ui.get_exec_line();
                    app.step(crate::debug_ops::Step::Over);
                    SCRIPT.with(|s| s.borrow_mut().as_mut().unwrap().log.push(format!("stopped at line {}", before + 1)));
                    Next::Advance
                }
                Err(e) => Next::Fail(e),
            }
        }
        9 => {
            if ui.get_debug_state() != "stopped" || ui.get_exec_line() < 0 || elapsed < Duration::from_millis(1200) {
                return Next::Wait;
            }
            SCRIPT.with(|s| {
                s.borrow_mut().as_mut().unwrap().log.push(format!("after F8 at line {}", ui.get_exec_line() + 1))
            });
            match screenshot(app, dir, "4-step-over") {
                Ok(()) => Next::Advance,
                Err(e) => Next::Fail(e),
            }
        }
        10 => {
            app.program_reset();
            app.open_path(&app.project.as_ref().unwrap().path("ui/main_form.slint"));
            let slint = std::fs::read_to_string(app.project.as_ref().unwrap().path("ui/main_form.slint")).unwrap();
            check(slint.contains("clicked => { root.button1_clicked(); }"), "form file lacks the handler")
        }
        _ => Next::Wait,
    }
}
