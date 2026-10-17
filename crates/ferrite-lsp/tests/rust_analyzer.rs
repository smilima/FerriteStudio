//! Talks to a real rust-analyzer: diagnostics, completion, hover,
//! go-to-definition and formatting. Skipped when it is not installed.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ferrite_lsp::{apply_edits, LspClient, LspEvent, Severity};

fn rust_analyzer() -> Option<PathBuf> {
    let ok = Command::new("rust-analyzer").arg("--version").output().ok()?.status.success();
    ok.then(|| PathBuf::from("rust-analyzer"))
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[test]
fn diagnostics_completion_hover_definition_and_format() {
    let Some(server) = rust_analyzer() else {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    };
    // Outside any cargo workspace, so cargo treats it as its own package.
    let root = std::env::temp_dir().join(format!("ferrite-lsp-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    write(&root, "Cargo.toml", "[package]\nname = \"lsp_project\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    let main = "struct Greeter { name: String }\n\nimpl Greeter {\n    fn greet(&self) -> String { format!(\"Hello, {}\", self.name) }\n}\n\nfn main() {\n    let g = Greeter { name: \"Ada\".into() };\n    let n: i32 = \"x\";\n    println!(\"{}\", g.greet());\n}\n";
    write(&root, "src/main.rs", main);
    let path = root.join("src/main.rs");

    let (tx, rx) = mpsc::channel();
    let client = LspClient::start(&root, &server, move |ev| {
        let _ = tx.send(ev);
    })
    .expect("rust-analyzer starts");
    client.did_open(&path, main).unwrap();

    // Wait for the type mismatch on `let n: i32 = "x";`.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut found = false;
    while Instant::now() < deadline && !found {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(LspEvent::Diagnostics { path: p, items }) => {
                eprintln!("diagnostics for {}: {items:?}", p.display());
                if p.ends_with("src/main.rs") {
                    found = items.iter().any(|d| d.severity == Severity::Error && d.line == 8);
                }
            }
            Ok(other) => eprintln!("{other:?}"),
            Err(_) => {}
        }
    }
    assert!(found, "expected a type error on line 9");

    // Completion after `g.` inside main.
    let edited = main.replace("    println!(\"{}\", g.greet());", "    println!(\"{}\", g.gre);");
    client.did_change(&path, &edited).unwrap();
    let offset = edited.find("g.gre").unwrap() + "g.gre".len();
    let (ctx, crx) = mpsc::channel();
    let mut labels = Vec::new();
    for _ in 0..20 {
        let ctx = ctx.clone();
        client.completion(&path, offset, move |items| ctx.send(items).unwrap()).unwrap();
        let items = crx.recv_timeout(Duration::from_secs(30)).unwrap();
        labels = items.iter().map(|c| c.label.clone()).collect::<Vec<_>>();
        if let Some(c) = items.iter().find(|c| c.label.starts_with("greet")) {
            let applied = apply_edits(&edited, &[ferrite_lsp::Edit { start: c.start, end: c.end, text: c.text.clone() }]);
            assert!(applied.contains("g.greet"), "{applied}");
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    assert!(labels.iter().any(|l| l.starts_with("greet")), "{labels:?}");
    client.did_change(&path, main).unwrap();

    // Hover on `greet` in the call.
    let at = main.rfind("greet()").unwrap() + 1;
    let (htx, hrx) = mpsc::channel();
    client.hover(&path, at, move |h| htx.send(h).unwrap()).unwrap();
    let hover = hrx.recv_timeout(Duration::from_secs(30)).unwrap().unwrap_or_default();
    assert!(hover.contains("fn greet"), "{hover}");

    // Go to the definition of `greet`.
    let (dtx, drx) = mpsc::channel();
    client.definition(&path, at, move |l| dtx.send(l).unwrap()).unwrap();
    let loc = drx.recv_timeout(Duration::from_secs(30)).unwrap().expect("a definition");
    assert!(loc.path.ends_with("src/main.rs"));
    assert_eq!(loc.line, 3);

    // Formatting rewrites the one-line method body.
    let (ftx, frx) = mpsc::channel();
    client.formatting(&path, move |e| ftx.send(e).unwrap()).unwrap();
    let edits = frx.recv_timeout(Duration::from_secs(30)).unwrap();
    let formatted = apply_edits(main, &edits);
    assert!(formatted.contains("fn greet(&self) -> String {\n        format!"), "{formatted}");
    client.shutdown();
}
