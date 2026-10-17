//! Ferrite Studio: a RAD IDE for Rust and Slint.
//!
//! Usage: `ferrite-studio [project-folder]`
//!        `ferrite-studio --selftest <scratch-folder>` runs a scripted
//!        end-to-end session and saves screenshots there.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

mod app;
mod build_ops;
mod code_ops;
mod debug_ops;
mod form_ops;
mod highlight;
mod selftest;
mod views;

use std::path::PathBuf;

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    app::install(&ui);
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--selftest") => {
            let dir = args.next().map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            selftest::start(dir);
        }
        Some(path) => {
            let p = PathBuf::from(path);
            app::act(move |a| a.open_project(&p));
        }
        None => {}
    }
    ui.run()
}
