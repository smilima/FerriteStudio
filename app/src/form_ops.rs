//! Form Designer and Object Inspector actions.

use ferrite_form::codegen::handler_body_offset;
use ferrite_form::{kind_spec, snap, GRID};
use slint::{ModelRc, VecModel};

use crate::app::{App, View};
use crate::views;

impl App {
    pub fn show_design(&mut self) {
        if self.form.is_none() {
            return;
        }
        self.close_popups();
        self.view = View::Design;
        self.refresh_view();
        self.refresh_designer();
        self.refresh_tabs();
        self.refresh_files();
        self.ui().invoke_focus_designer();
    }

    /// F12: switch between the form and its events file, as RAD Studio
    /// toggles between a form and its unit.
    pub fn toggle_design_code(&mut self) {
        let (Some(doc), Some(project)) = (&self.form, &self.project) else { return };
        if self.view == View::Design {
            let path = project.path(&doc.entry.events);
            self.open_path(&path);
        } else {
            self.show_design();
        }
    }

    pub fn refresh_designer(&self) {
        let Some(doc) = &self.form else { return };
        let ui = self.ui();
        let items: Vec<_> = doc.form.controls.iter().map(views::control_item).collect();
        ui.set_controls(ModelRc::new(VecModel::from(items)));
        ui.set_selected(doc.selected.map_or(-1, |i| i as i32));
        ui.set_form_title(doc.form.title.as_str().into());
        ui.set_form_width(doc.form.width);
        ui.set_form_height(doc.form.height);
        self.refresh_inspector();
    }

    pub fn refresh_inspector(&self) {
        let ui = self.ui();
        let Some(doc) = &self.form else {
            ui.set_inspector_heading("".into());
            ui.set_props(ModelRc::default());
            ui.set_events(ModelRc::default());
            return;
        };
        let (heading, rows) = views::prop_rows(&doc.form, doc.selected);
        ui.set_inspector_heading(heading.into());
        ui.set_props(ModelRc::new(VecModel::from(rows)));
        ui.set_events(ModelRc::new(VecModel::from(views::event_rows(&doc.form, doc.selected))));
    }

    fn form_changed(&mut self) {
        if let Some(doc) = self.form.as_mut() {
            doc.dirty = true;
        }
        self.refresh_designer();
        self.refresh_tabs();
    }

    pub fn palette_pick(&mut self, kind: &str) {
        let ui = self.ui();
        let current = ui.get_active_tool();
        ui.set_active_tool(if current == kind { "".into() } else { kind.into() });
        if self.view != View::Design {
            self.show_design();
        }
        self.status(format!("Click on the form to drop a {kind}"));
    }

    /// Double-clicking a palette entry drops the control at a cascading spot.
    pub fn palette_add(&mut self, kind: &str) {
        let Some(doc) = self.form.as_mut() else { return };
        let n = doc.form.controls.len() as i32;
        let at = 16 + (n % 8) * 2 * GRID;
        let added = doc.form.add_control(kind, at, at);
        if let Ok(i) = added {
            doc.selected = Some(i);
        }
        if let Err(e) = added {
            self.message(e.to_string(), "error");
        }
        self.ui().set_active_tool("".into());
        if self.view != View::Design {
            self.show_design();
        }
        self.form_changed();
    }

    pub fn select(&mut self, i: i32) {
        let Some(doc) = self.form.as_mut() else { return };
        doc.selected = usize::try_from(i).ok().filter(|&i| i < doc.form.controls.len());
        self.ui().set_selected(i);
        self.refresh_inspector();
    }

    pub fn drop_control(&mut self, x: i32, y: i32) {
        let kind = self.ui().get_active_tool().to_string();
        let Some(doc) = self.form.as_mut() else { return };
        if kind.is_empty() {
            return;
        }
        let added = doc.form.add_control(&kind, x, y).map(|i| {
            doc.selected = Some(i);
            doc.form.controls[i].name.clone()
        });
        match added {
            Ok(name) => self.status(format!("Added {name}")),
            Err(e) => self.message(e.to_string(), "error"),
        }
        self.ui().set_active_tool("".into());
        self.form_changed();
    }

    pub fn drag_control(&mut self, i: usize, dx: i32, dy: i32) {
        let Some(doc) = self.form.as_mut() else { return };
        if doc.form.drag_by(i, dx, dy).is_ok() {
            doc.selected = Some(i);
        }
        self.form_changed();
    }

    pub fn resize_control(&mut self, i: usize, dw: i32, dh: i32) {
        let Some(doc) = self.form.as_mut() else { return };
        let _ = doc.form.resize_by(i, dw, dh);
        self.form_changed();
    }

    pub fn resize_form(&mut self, dw: i32, dh: i32) {
        let Some(doc) = self.form.as_mut() else { return };
        doc.form.width = snap(doc.form.width + dw).max(8 * GRID);
        doc.form.height = snap(doc.form.height + dh).max(6 * GRID);
        self.form_changed();
    }

    pub fn nudge(&mut self, dx: i32, dy: i32) {
        let Some(doc) = self.form.as_mut() else { return };
        let Some(i) = doc.selected else { return };
        let _ = doc.form.drag_by(i, dx * GRID, dy * GRID);
        self.form_changed();
    }

    pub fn delete_selected(&mut self) {
        let Some(doc) = self.form.as_mut() else { return };
        let Some(i) = doc.selected else { return };
        let removed = doc.form.remove_control(i);
        doc.selected = None;
        if let Ok(c) = removed {
            self.status(format!("Deleted {}; empty handlers are removed when you save", c.name));
        }
        self.form_changed();
    }

    pub fn prop_edited(&mut self, name: &str, value: &str) {
        let Some(doc) = self.form.as_mut() else { return };
        let form = &mut doc.form;
        let result = match doc.selected {
            None => match name {
                "Name" if value.trim() != form.name => Err(
                    "Renaming a form also renames its files and main.rs; that arrives with multi-form support".into(),
                ),
                "title" => {
                    form.title = value.to_string();
                    Ok(())
                }
                "width" | "height" => match value.trim().trim_end_matches("px").parse::<f64>() {
                    Ok(v) if v >= 64.0 => {
                        let v = snap(v as i32);
                        if name == "width" { form.width = v } else { form.height = v }
                        Ok(())
                    }
                    _ => Err(format!("{name} must be a number of pixels, at least 64")),
                },
                _ => Ok(()),
            },
            Some(i) => match name {
                "Name" => form.rename_control(i, value.trim()).map_err(|e| e.to_string()),
                "x" | "y" | "width" | "height" => match value.trim().trim_end_matches("px").parse::<f64>() {
                    Ok(v) => {
                        let c = &form.controls[i];
                        let (mut x, mut y, mut w, mut h) = (c.x, c.y, c.width, c.height);
                        let v = v.round() as i32;
                        match name {
                            "x" => x = v,
                            "y" => y = v,
                            "width" => w = v,
                            _ => h = v,
                        }
                        form.set_bounds(i, x, y, w, h).map_err(|e| e.to_string())
                    }
                    Err(_) => Err(format!("{name} must be a number of pixels")),
                },
                prop => form.set_prop(i, prop, value).map_err(|e| e.to_string()),
            },
        };
        if let Err(e) = result {
            self.status(&e);
            self.message(e, "warning");
        }
        self.form_changed();
    }

    /// Double-click on a control: create or open its default event handler.
    pub fn default_event(&mut self, i: usize) {
        let Some(doc) = self.form.as_mut() else { return };
        doc.selected = Some(i);
        let Some(kind) = doc.form.controls.get(i).map(|c| c.kind.clone()) else { return };
        let Some(spec) = kind_spec(&kind) else { return };
        match spec.callbacks.first() {
            Some(cb) => self.create_handler(i, cb.name),
            None => {
                self.refresh_designer();
                self.status(format!("{kind} has no events"));
            }
        }
    }

    pub fn event_activated(&mut self, callback: &str) {
        let Some(i) = self.form.as_ref().and_then(|d| d.selected) else { return };
        self.create_handler(i, callback);
    }

    /// Connects a callback to a handler, saves the form so the wiring and the
    /// stub exist on disk for rust-analyzer, and puts the caret in the stub.
    pub fn create_handler(&mut self, i: usize, callback: &str) {
        let Some(doc) = self.form.as_mut() else { return };
        let handler = doc.form.ensure_handler(i, callback);
        doc.dirty = true;
        let (form_name, form) = (doc.form.name.clone(), doc.form.clone());
        let events = doc.entry.events.clone();
        let handler = match handler {
            Ok(h) => h,
            Err(e) => {
                self.message(e.to_string(), "error");
                return;
            }
        };
        if let Some(p) = self.project.as_mut() {
            p.remember_handlers(&form_name, &form);
        }
        self.save_all();
        let Some(path) = self.project.as_ref().map(|p| p.path(&events)) else { return };
        let bi = match self.open_buffer(&path) {
            Ok(bi) => bi,
            Err(e) => {
                self.message(e, "error");
                return;
            }
        };
        if let Some(off) = handler_body_offset(&self.buffers[bi].text, &handler) {
            self.buffers[bi].caret = off;
        }
        self.show_buffer(bi);
        self.status(format!("{handler} is ready for your code"));
    }
}
