//! Compiles designer output with Slint's own compiler (slint-interpreter),
//! so every catalog property, callback and published alias is checked
//! against the real widget definitions of the pinned Slint version.

use ferrite_form::{write_slint, Form, PropType, KINDS};
use slint_interpreter::{Compiler, ValueType};

fn sample_value(ty: PropType, default: &str) -> &'static str {
    match ty {
        PropType::String => "Sample",
        PropType::Length => "12",
        PropType::Bool => {
            if default == "true" {
                "false"
            } else {
                "true"
            }
        }
        PropType::Int => "3",
        PropType::Float => "0.5",
        PropType::Color => "#336699",
        PropType::StringList => "Alpha, Beta",
    }
}

fn every_feature_form() -> Form {
    let mut form = Form::new("MainForm");
    form.title = "All controls".into();
    form.width = 800;
    form.height = 600;
    for (i, kind) in KINDS.iter().enumerate() {
        let idx = form.add_control(kind.kind, 8, (i as i32) * 40).unwrap();
        for p in kind.props {
            form.set_prop(idx, p.name, sample_value(p.ty, p.default)).unwrap();
        }
        for cb in kind.callbacks {
            form.ensure_handler(idx, cb.name).unwrap();
        }
    }
    form
}

fn compile(source: &str) -> slint_interpreter::CompilationResult {
    let compiler = Compiler::default();
    spin_on::spin_on(compiler.build_from_source(source.to_string(), Default::default()))
}

#[test]
fn designer_output_compiles_with_slint() {
    let form = every_feature_form();
    let source = write_slint(&form);
    let result = compile(&source);
    let errors: Vec<String> = result
        .diagnostics()
        .filter(|d| d.level() == slint_interpreter::DiagnosticLevel::Error)
        .map(|d| d.to_string())
        .collect();
    assert!(errors.is_empty(), "Slint rejected the form:\n{}\n\n{source}", errors.join("\n"));

    let def = result.component("MainForm").expect("MainForm exported");
    let props: Vec<(String, ValueType)> = def.properties().collect();
    let callbacks: Vec<String> = def.callbacks().collect();

    for c in &form.controls {
        let spec = ferrite_form::kind_spec(&c.kind).unwrap();
        if let Some((prop, _)) = spec.published {
            let name = format!("{}_{}", c.name, prop.replace('-', "_"));
            assert!(
                props.iter().any(|(p, _)| p.replace('-', "_") == name),
                "published property {name} missing; have {props:?}"
            );
        }
    }
    for h in form.handlers() {
        assert!(
            callbacks.iter().any(|c| c.replace('-', "_") == h.handler),
            "callback {} missing; have {callbacks:?}",
            h.handler
        );
    }
}

#[test]
fn generated_rust_types_match_slint() {
    // A callback's Rust parameter type must be what Slint generates.
    let form = every_feature_form();
    let result = compile(&write_slint(&form));
    let def = result.component("MainForm").unwrap();
    let props: Vec<(String, ValueType)> = def.properties().collect();
    for c in &form.controls {
        let spec = ferrite_form::kind_spec(&c.kind).unwrap();
        let Some((prop, ty)) = spec.published else { continue };
        let name = format!("{}_{}", c.name, prop.replace('-', "_"));
        let (_, vt) = props.iter().find(|(p, _)| p.replace('-', "_") == name).unwrap();
        let expected = match ty {
            PropType::String => ValueType::String,
            PropType::Bool => ValueType::Bool,
            PropType::Int | PropType::Float | PropType::Length => ValueType::Number,
            PropType::Color => ValueType::Brush,
            PropType::StringList => ValueType::Model,
        };
        assert_eq!(*vt, expected, "{name}");
    }
}
