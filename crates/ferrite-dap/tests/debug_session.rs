//! End-to-end session against a real adapter: breakpoint, stack, locals,
//! evaluate, continue to exit. Skipped when no adapter is installed.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use ferrite_dap::{AdapterConfig, DapClient, DapEvent};

const T: Duration = Duration::from_secs(30);

fn wait_for(rx: &mpsc::Receiver<DapEvent>, mut pred: impl FnMut(&DapEvent) -> bool, out: &mut String) -> DapEvent {
    let mut seen = Vec::new();
    loop {
        let ev = rx.recv_timeout(T).unwrap_or_else(|_| panic!("timed out; events so far: {seen:?}"));
        seen.push(format!("{ev:?}").chars().take(120).collect::<String>());
        if let DapEvent::Output { text, .. } = &ev {
            out.push_str(text);
        }
        if pred(&ev) {
            return ev;
        }
    }
}

#[test]
fn breakpoint_stack_locals_and_exit() {
    let Ok(config) = AdapterConfig::detect() else {
        eprintln!("no debug adapter installed; skipping");
        return;
    };
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dap-session");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("hello.rs");
    std::fs::write(
        &src,
        "fn add(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\n\nstruct Point {\n    x: i32,\n    y: i32,\n}\n\nfn main() {\n    let name = String::from(\"Ada\");\n    let p = Point { x: 2, y: 3 };\n    let x = add(p.x, p.y);\n    println!(\"x = {x}\");\n}\n",
    )
    .unwrap();
    let exe = dir.join(if cfg!(windows) { "hello.exe" } else { "hello" });
    let mut rustc = Command::new("rustc");
    rustc.args(["-g", "-o"]).arg(&exe).arg(&src);
    // GDB reads DWARF only: on the Windows MSVC toolchain build for the GNU
    // target, linking with MinGW as the IDE does.
    let host = ferrite_dap::rust_host().unwrap_or_default();
    if let (true, Some(target)) = (config.needs_dwarf(), ferrite_build::cargo::gnu_target_for(&host)) {
        if !ferrite_build::cargo::target_installed(&target) || ferrite_build::cargo::mingw_linker_missing(&target) {
            eprintln!("GDB found but {target} or its MinGW linker is not installed; skipping");
            return;
        }
        let options = ferrite_build::cargo::gnu_target_options(&target);
        rustc.args(["--target", &target]);
        if let Some((_, gcc)) = options.env.first() {
            rustc.arg(format!("-Clinker={gcc}"));
        }
        rustc.env("PATH", ferrite_dap::path_with(&options.path_prepend));
    }
    eprintln!("debugging with {} {:?} (PATH additions {:?})", config.program.display(), config.args, config.path_prepend);
    let status = rustc.status().unwrap();
    assert!(status.success());

    let (tx, rx) = mpsc::channel();
    let client = DapClient::start(config.clone(), &dir, move |ev| {
        let _ = tx.send(ev);
    })
    .expect("adapter starts");
    let mut output = String::new();

    client.launch(config.launch_arguments(&exe, &dir, &[]), |_| {}).unwrap();
    wait_for(&rx, |e| matches!(e, DapEvent::Initialized), &mut output);

    let (btx, brx) = mpsc::channel();
    client.set_breakpoints(&src, &[2], move |r| btx.send(r).unwrap()).unwrap();
    let bps = brx.recv_timeout(T).unwrap();
    assert_eq!(bps.len(), 1, "{bps:?}");
    client.configuration_done().unwrap();

    let stopped = wait_for(&rx, |e| matches!(e, DapEvent::Stopped { .. }), &mut output);
    let DapEvent::Stopped { thread_id, .. } = stopped else { unreachable!() };
    let thread = match thread_id {
        Some(t) => t,
        None => {
            let (ttx, trx) = mpsc::channel();
            client.threads(move |t| ttx.send(t).unwrap()).unwrap();
            trx.recv_timeout(T).unwrap()[0].0
        }
    };

    let (stx, srx) = mpsc::channel();
    client.stack_trace(thread, move |f| stx.send(f).unwrap()).unwrap();
    let frames = srx.recv_timeout(T).unwrap();
    assert!(frames[0].name.contains("add"), "{frames:?}");
    assert_eq!(frames[0].line, 2);
    assert!(frames[0].path.as_ref().unwrap().ends_with("hello.rs"));

    let (ctx, crx) = mpsc::channel();
    client.scopes(frames[0].id, move |s| ctx.send(s).unwrap()).unwrap();
    let scopes = crx.recv_timeout(T).unwrap();
    let mut names = Vec::new();
    for scope in &scopes {
        let (vtx, vrx) = mpsc::channel();
        client.variables(scope.reference, move |v| vtx.send(v).unwrap()).unwrap();
        for v in vrx.recv_timeout(T).unwrap() {
            names.push(format!("{}={}", v.name, v.value));
        }
    }
    assert!(names.iter().any(|n| n == "a=2"), "{names:?}");
    assert!(names.iter().any(|n| n == "b=3"), "{names:?}");

    let (etx, erx) = mpsc::channel();
    client.evaluate("a + b", Some(frames[0].id), "watch", move |r| etx.send(r).unwrap()).unwrap();
    assert_eq!(erx.recv_timeout(T).unwrap().unwrap(), "5");

    // What a tooltip does: evaluate a struct in the caller's frame, then
    // list its fields.
    let main_frame = frames.iter().find(|f| f.name.contains("main")).expect("main on the stack");
    let (htx, hrx) = mpsc::channel();
    client.evaluate_value("p", Some(main_frame.id), "hover", move |r| htx.send(r).unwrap()).unwrap();
    let p = hrx.recv_timeout(T).unwrap().expect("p evaluates");
    // The Rust pretty-printers are loaded: a String shows its text.
    let (stx, srx) = mpsc::channel();
    client.evaluate_value("name", Some(main_frame.id), "hover", move |r| stx.send(r).unwrap()).unwrap();
    let name = srx.recv_timeout(T).unwrap().expect("name evaluates");
    assert!(name.value.contains("Ada"), "pretty-printed String: {name:?}");
    eprintln!("hover p: {p:?}");
    assert!(p.reference != 0, "a struct has children: {p:?}");
    assert!(p.type_name.contains("Point") || p.value.contains("Point"), "{p:?}");
    let (ftx, frx) = mpsc::channel();
    client.variables(p.reference, move |v| ftx.send(v).unwrap()).unwrap();
    let fields: Vec<String> = frx.recv_timeout(T).unwrap().iter().map(|v| format!("{}={}", v.name, v.value)).collect();
    assert_eq!(fields, ["x=2", "y=3"]);
    let (xtx, xrx) = mpsc::channel();
    client.evaluate_value("p.y", Some(main_frame.id), "hover", move |r| xtx.send(r).unwrap()).unwrap();
    let y = xrx.recv_timeout(T).unwrap().expect("p.y evaluates");
    assert_eq!((y.value.as_str(), y.reference), ("3", 0));

    client.resume(thread).unwrap();
    wait_for(&rx, |e| matches!(e, DapEvent::Exited { .. } | DapEvent::Terminated), &mut output);
    // Program output may arrive through the adapter or the inherited pipe.
    eprintln!("program output via adapter: {output:?}");
    client.stop();
}
