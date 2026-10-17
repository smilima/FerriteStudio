//! Ferrite projects: a cargo package plus `Ferrite.toml`.

use std::fs;
use std::path::{Path, PathBuf};

use ferrite_form::codegen::{events_template, sync_events, wiring_rs, SyncResult};
use ferrite_form::{read_slint, to_snake_case, write_slint, Form};
use serde::{Deserialize, Serialize};

pub const MANIFEST: &str = "Ferrite.toml";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub project: ProjectInfo,
    #[serde(default, rename = "form", skip_serializing_if = "Vec::is_empty")]
    pub forms: Vec<FormEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProjectInfo {
    pub name: String,
}

/// One form and the files it owns, all relative to the project root.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FormEntry {
    pub name: String,
    /// The `.slint` form file (designer-owned).
    pub file: String,
    /// User-owned handler code; the designer only appends and prunes stubs.
    pub events: String,
    /// Generated wiring, rewritten on every save.
    pub wiring: String,
    /// Every handler the designer has created in `events`, so it can prune
    /// stubs left empty without touching other user functions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<String>,
}

impl FormEntry {
    pub fn for_form(name: &str) -> FormEntry {
        let snake = to_snake_case(name);
        FormEntry {
            name: name.to_string(),
            file: format!("ui/{snake}.slint"),
            events: format!("src/{snake}_events.rs"),
            wiring: format!("src/{snake}_wiring.rs"),
            handlers: Vec::new(),
        }
    }

    /// Rust module names of the events and wiring files.
    pub fn modules(&self) -> (String, String) {
        let stem = |p: &str| Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        (stem(&self.events), stem(&self.wiring))
    }
}

#[derive(Clone, Debug)]
pub struct Project {
    pub root: PathBuf,
    pub manifest: Manifest,
}

/// The texts a form save writes.
#[derive(Debug)]
pub struct FormOutputs {
    pub slint: String,
    pub wiring: String,
    pub events: SyncResult,
}

/// Cargo package names: ASCII letters, digits, `-` and `_`, starting with a letter.
pub fn valid_package_name(name: &str) -> bool {
    let mut c = name.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic())
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        && name.len() <= 64
}

fn cargo_toml(name: &str) -> String {
    format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nslint = \"1.18\"\n\n[build-dependencies]\nslint-build = \"1.18\"\n"
    )
}

fn build_rs(entry: &FormEntry) -> String {
    format!(
        "// Compiles the forms into Rust at build time.\nfn main() {{\n    \
         slint_build::compile(\"{}\").expect(\"failed to compile the Slint forms\");\n}}\n",
        entry.file
    )
}

fn main_rs(entry: &FormEntry) -> String {
    let (events, wiring) = entry.modules();
    format!(
        "// Created by Ferrite Studio. The form is {file}; its event handlers\n\
         // live in {events_file}.\n\
         #![cfg_attr(not(debug_assertions), windows_subsystem = \"windows\")]\n\
         \n\
         slint::include_modules!();\n\
         \n\
         mod {events};\n\
         mod {wiring};\n\
         \n\
         use slint::ComponentHandle;\n\
         \n\
         fn main() -> Result<(), slint::PlatformError> {{\n    \
             let form = {name}::new()?;\n    \
             {wiring}::connect(&form);\n    \
             form.run()\n\
         }}\n",
        file = entry.file,
        events_file = entry.events,
        name = entry.name,
    )
}

fn write(root: &Path, rel: &str, text: &str) -> Result<(), String> {
    let path = root.join(rel);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

impl Project {
    /// File > New > Slint Application: creates `parent/name` with one empty form.
    pub fn create(parent: &Path, name: &str) -> Result<Project, String> {
        if !valid_package_name(name) {
            return Err(format!(
                "`{name}` is not a valid cargo package name: use letters, digits, `-` and `_`, starting with a letter"
            ));
        }
        let root = parent.join(name);
        if root.exists() && fs::read_dir(&root).map(|mut d| d.next().is_some()).unwrap_or(true) {
            return Err(format!("{} already exists and is not empty", root.display()));
        }
        let mut form = Form::new("MainForm");
        form.title = name.to_string();
        let entry = FormEntry::for_form(&form.name);
        let (events_mod, _) = entry.modules();

        write(&root, "Cargo.toml", &cargo_toml(name))?;
        write(&root, ".gitignore", "/target\n")?;
        write(&root, "build.rs", &build_rs(&entry))?;
        write(&root, "src/main.rs", &main_rs(&entry))?;
        write(&root, &entry.file, &write_slint(&form))?;
        write(&root, &entry.wiring, &wiring_rs(&form, &entry.file, &events_mod))?;
        write(&root, &entry.events, &events_template(&form))?;

        let root = strip_verbatim(root.canonicalize().map_err(|e| e.to_string())?);
        let project = Project {
            root,
            manifest: Manifest { project: ProjectInfo { name: name.to_string() }, forms: vec![entry] },
        };
        project.save_manifest()?;
        Ok(project)
    }

    /// Opens a Ferrite project, or a plain cargo package as code-only.
    pub fn open(dir: &Path) -> Result<Project, String> {
        let root = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
        let root = strip_verbatim(root);
        let manifest_path = root.join(MANIFEST);
        if manifest_path.is_file() {
            let text = fs::read_to_string(&manifest_path).map_err(|e| format!("{MANIFEST}: {e}"))?;
            let manifest: Manifest = toml::from_str(&text).map_err(|e| format!("{MANIFEST}: {e}"))?;
            return Ok(Project { root, manifest });
        }
        if root.join("Cargo.toml").is_file() {
            let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            return Ok(Project { root, manifest: Manifest { project: ProjectInfo { name }, forms: vec![] } });
        }
        Err(format!("{} has no Cargo.toml", root.display()))
    }

    pub fn save_manifest(&self) -> Result<(), String> {
        if self.manifest.forms.is_empty() && !self.root.join(MANIFEST).exists() {
            return Ok(()); // plain cargo project: leave it untouched
        }
        let text = toml::to_string_pretty(&self.manifest).map_err(|e| e.to_string())?;
        let header = "# Ferrite Studio project file: lists the forms and the files each one owns.\n";
        write(&self.root, MANIFEST, &format!("{header}{text}"))
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    pub fn form_entry_for(&self, path: &Path) -> Option<&FormEntry> {
        self.manifest.forms.iter().find(|f| self.path(&f.file) == path)
    }

    pub fn load_form(&self, entry: &FormEntry) -> Result<Form, String> {
        let path = self.path(&entry.file);
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let form = read_slint(&text).map_err(|e| format!("{}, {e}", entry.file))?;
        if form.name != entry.name {
            return Err(format!("{} defines {}, but {MANIFEST} expects {}", entry.file, form.name, entry.name));
        }
        Ok(form)
    }

    /// Everything a form save produces, given the current events source.
    pub fn form_outputs(&self, entry: &FormEntry, form: &Form, events_source: &str) -> FormOutputs {
        let (events_mod, _) = entry.modules();
        FormOutputs {
            slint: write_slint(form),
            wiring: wiring_rs(form, &entry.file, &events_mod),
            events: sync_events(events_source, form, &entry.handlers),
        }
    }

    /// Records handlers the designer created, for later stub pruning.
    pub fn remember_handlers(&mut self, form_name: &str, form: &Form) {
        if let Some(entry) = self.manifest.forms.iter_mut().find(|f| f.name == form_name) {
            for h in form.handlers() {
                if !entry.handlers.iter().any(|x| x == h.handler) {
                    entry.handlers.push(h.handler.to_string());
                }
            }
        }
    }

    pub fn forget_handlers(&mut self, form_name: &str, removed: &[String]) {
        if let Some(entry) = self.manifest.forms.iter_mut().find(|f| f.name == form_name) {
            entry.handlers.retain(|h| !removed.contains(h));
        }
    }

    /// Source files for the Project Manager, manifest files first.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        collect(&self.root, &self.root, &mut out);
        out.sort_by_key(|p| {
            let rel = p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().replace('\\', "/");
            let rank = match rel.as_str() {
                "Cargo.toml" => 0,
                MANIFEST => 1,
                "build.rs" => 2,
                r if r.starts_with("ui/") => 3,
                r if r.starts_with("src/") => 4,
                _ => 5,
            };
            (rank, rel)
        });
        out
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "target" && !name.starts_with('.') && p.strip_prefix(root).map_or(0, |r| r.components().count()) < 6 {
                collect(root, &p, out);
            }
        } else if matches!(p.extension().and_then(|e| e.to_str()), Some("rs" | "slint" | "toml" | "md")) {
            out.push(p);
        }
    }
}

/// `\\?\C:\x` -> `C:\x`, so paths match what tools print on Windows.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ferrite-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn new_project_has_every_file_and_reopens() {
        let parent = temp("create");
        let p = Project::create(&parent, "hello_app").unwrap();
        for f in ["Cargo.toml", "build.rs", "src/main.rs", "ui/main_form.slint", "src/main_form_events.rs", "src/main_form_wiring.rs", MANIFEST] {
            assert!(p.path(f).is_file(), "{f} missing");
        }
        let again = Project::open(&p.root).unwrap();
        assert_eq!(again.manifest, p.manifest);
        let form = again.load_form(&again.manifest.forms[0]).unwrap();
        assert_eq!(form.title, "hello_app");
        assert!(Project::create(&parent, "hello_app").is_err());
        assert!(Project::create(&parent, "1bad").is_err());
        let main = fs::read_to_string(p.path("src/main.rs")).unwrap();
        assert!(main.contains("main_form_wiring::connect(&form);"));
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn handlers_are_remembered_for_pruning() {
        let parent = temp("handlers");
        let mut p = Project::create(&parent, "app").unwrap();
        let entry = p.manifest.forms[0].clone();
        let mut form = p.load_form(&entry).unwrap();
        let b = form.add_control("Button", 8, 8).unwrap();
        form.ensure_handler(b, "clicked").unwrap();
        p.remember_handlers("MainForm", &form);
        p.save_manifest().unwrap();
        let reopened = Project::open(&p.root).unwrap();
        assert_eq!(reopened.manifest.forms[0].handlers, vec!["button1_clicked"]);
        let _ = fs::remove_dir_all(parent);
    }
}
