//! End-to-end session against a real adapter: breakpoint, stack, locals,
//! evaluate, continue to exit. Skipped when no adapter is installed.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::Duration;

use ferrite_dap::{AdapterConfig, DapClient, DapEvent};

const T: Duration = Duration::from_secs(30);

fn wait_for(rx: &mpsc::Receiver<DapEvent>, mut pred: impl FnMut(&DapEvent) -> bool, out: &mut String) -> DapEvent {
    loop {
        let ev = rx.recv_timeout(T).expect("adapter event timed out");
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
        "fn add(a: i32, b: i32) -> i32 {\n    let sum = a + b;\n    sum\n}\n\nfn main() {\n    let x = add(2, 3);\n    println!(\"x = {x}\");\n}\n",
    )
    .unwrap();
    let exe = dir.join(if cfg!(windows) { "hello.exe" } else { "hello" });
    let status = Command::new("rustc").args(["-g", "-o"]).arg(&exe).arg(&src).status().unwrap();
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

    client.resume(thread).unwrap();
    wait_for(&rx, |e| matches!(e, DapEvent::Exited { .. } | DapEvent::Terminated), &mut output);
    // Program output may arrive through the adapter or the inherited pipe.
    eprintln!("program output via adapter: {output:?}");
    client.stop();
}
