//! The Maschine MK3 configuration window.
//!
//! A separate process from the driver on purpose. The driver holds a
//! real-time input thread; a window that is being resized, or that has just
//! been asked to open a file dialog, must not be able to interfere with it.
//! They talk over a Unix socket.
//!
//! Opening this starts the driver if it is not already running, because that
//! is what someone double-clicking a configuration app expects. The reverse
//! does not hold: the driver never opens a window.
//!
//! Edits are applied to the config file through `toml_edit`, so the comments
//! explaining every setting survive being saved from a GUI.

use anyhow::{Context, Result};
use eframe::egui;
use maschine_mk3::config::Config;
use maschine_mk3::ipc::{Client, Event, Outbound, PresetEntry, Reply, Request};
use maschine_mk3::profile::{Control, ControlKind, Profile};
use std::collections::HashMap;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!(
            "mk3-gui [--no-spawn]\n\n\
             Configuration window for the Maschine MK3 driver.\n\
             Starts the driver if it is not already running; --no-spawn disables that."
        );
        return Ok(());
    }
    let spawn = !args.iter().any(|a| a == "--no-spawn");

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("Maschine MK3"),
        ..Default::default()
    };
    eframe::run_native(
        "Maschine MK3",
        native,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Box::new(App::new(spawn))
        }),
    )
    .map_err(|e| anyhow::anyhow!("starting the window: {e}"))
}

/// What the inspector is currently editing.
#[derive(Clone, PartialEq)]
enum Selection {
    None,
    Button(String),
    Pads,
    Knobs,
    Encoder,
    Strip,
    General,
}

struct App {
    client: Option<Client>,
    status: String,
    status_bad: bool,
    /// The config file's text, kept so comments survive a save.
    doc: Option<toml_edit::DocumentMut>,
    /// The same config parsed, for reading values.
    cfg: Option<Config>,
    /// The hardware description, which supplies the panel map.
    profile: Option<Profile>,
    path: String,
    dirty: bool,
    selection: Selection,
    /// Controls lit by live hardware activity, and when they were last touched.
    active: HashMap<String, Instant>,
    knob_values: [u8; 8],
    encoder_value: u8,
    strip_value: u8,
    pad_values: [u8; 16],
    last_connect_attempt: Instant,
    should_spawn: bool,
    /// Whether hardware activity moves the inspector to the control touched.
    follow_hardware: bool,
    raw_editor: bool,
    raw_text: String,
    presets: Vec<PresetEntry>,
    presets_dir: String,
    active_preset: Option<String>,
    /// Set while the "save as" popup is open.
    save_as: Option<(String, String)>,
    /// Set while the import popup is open.
    import: Option<(String, String)>,
}

impl App {
    fn new(should_spawn: bool) -> Self {
        let mut app = Self {
            client: None,
            status: "connecting".into(),
            status_bad: false,
            doc: None,
            cfg: None,
            profile: None,
            path: String::new(),
            dirty: false,
            selection: Selection::None,
            active: HashMap::new(),
            knob_values: [0; 8],
            encoder_value: 0,
            strip_value: 0,
            pad_values: [0; 16],
            last_connect_attempt: Instant::now() - Duration::from_secs(10),
            should_spawn,
            follow_hardware: true,
            raw_editor: false,
            raw_text: String::new(),
            presets: Vec::new(),
            presets_dir: String::new(),
            active_preset: None,
            save_as: None,
            import: None,
        };
        app.connect();
        app
    }

    fn connect(&mut self) {
        self.last_connect_attempt = Instant::now();
        let path = maschine_mk3::ipc::socket_path();
        match Client::connect(&path) {
            Ok(c) => {
                self.client = Some(c);
                self.status = "connected".into();
                self.status_bad = false;
                self.load();
                self.refresh_presets();
            }
            Err(_) if self.should_spawn => {
                // Only try once: repeatedly spawning a driver that is failing
                // to start would bury the real error under a pile of children.
                self.should_spawn = false;
                match spawn_driver() {
                    Ok(()) => self.status = "starting the driver...".into(),
                    Err(e) => {
                        self.status = format!("could not start the driver: {e:#}");
                        self.status_bad = true;
                    }
                }
            }
            Err(_) => {
                self.status = "driver not running".into();
                self.status_bad = true;
            }
        }
    }

    fn load(&mut self) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        // The profile is what the panel map is drawn from, so it has to arrive
        // before anything can be displayed.
        if let Ok(Reply::Profile { toml, .. }) =
            client.request(&Request::GetProfile, Duration::from_secs(2))
        {
            match Profile::parse(&toml) {
                Ok(p) => self.profile = Some(p),
                Err(e) => {
                    self.status = format!("device profile will not parse: {e:#}");
                    self.status_bad = true;
                }
            }
        }
        let Some(client) = self.client.as_mut() else {
            return;
        };
        match client.request(&Request::GetConfig, Duration::from_secs(2)) {
            Ok(Reply::Config { toml, path }) => match toml.parse::<toml_edit::DocumentMut>() {
                Ok(doc) => {
                    self.cfg = toml::from_str(&toml).ok();
                    self.raw_text = toml.clone();
                    self.doc = Some(doc);
                    self.path = path;
                    self.dirty = false;
                    self.status = "loaded".into();
                    self.status_bad = false;
                }
                Err(e) => {
                    self.status = format!("config will not parse: {e}");
                    self.status_bad = true;
                }
            },
            Ok(Reply::Error { message }) => {
                self.status = message;
                self.status_bad = true;
            }
            Ok(_) => {}
            Err(e) => {
                self.status = format!("{e:#}");
                self.status_bad = true;
                self.client = None;
            }
        }
    }

    /// Ask the driver what presets exist.
    fn refresh_presets(&mut self) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        if let Ok(Reply::Presets {
            entries,
            dir,
            active,
        }) = client.request(&Request::ListPresets, Duration::from_secs(2))
        {
            self.presets = entries;
            self.presets_dir = dir;
            self.active_preset = active;
        }
    }

    /// Send a preset request and report the outcome in the status line.
    fn preset_request(&mut self, req: Request, done: &str) {
        let Some(client) = self.client.as_mut() else {
            return;
        };
        match client.request(&req, Duration::from_secs(3)) {
            Ok(Reply::Ok) => {
                self.status = done.to_string();
                self.status_bad = false;
                self.load();
                self.refresh_presets();
            }
            Ok(Reply::Error { message }) => {
                self.status = message;
                self.status_bad = true;
            }
            Ok(_) => {}
            Err(e) => {
                self.status = format!("{e:#}");
                self.status_bad = true;
                self.client = None;
            }
        }
    }

    fn save(&mut self) {
        let text = if self.raw_editor {
            self.raw_text.clone()
        } else {
            match self.doc.as_ref() {
                Some(d) => d.to_string(),
                None => return,
            }
        };
        let Some(client) = self.client.as_mut() else {
            self.status = "not connected".into();
            self.status_bad = true;
            return;
        };
        match client.request(&Request::SetConfig { toml: text }, Duration::from_secs(3)) {
            Ok(Reply::Ok) => {
                self.dirty = false;
                self.status = "applied".into();
                self.status_bad = false;
                self.load();
            }
            Ok(Reply::Error { message }) => {
                self.status = message;
                self.status_bad = true;
            }
            Ok(_) => {}
            Err(e) => {
                self.status = format!("{e:#}");
                self.status_bad = true;
                self.client = None;
            }
        }
    }

    fn pump(&mut self) {
        let mut lost = false;
        if let Some(client) = self.client.as_mut() {
            for msg in client.poll() {
                match msg {
                    Outbound::Event(ev) => self.on_event(ev),
                    Outbound::Reply(Reply::Error { message }) => {
                        self.status = message;
                        self.status_bad = true;
                    }
                    Outbound::Reply(_) => {}
                }
            }
        } else {
            lost = true;
        }
        if lost && self.last_connect_attempt.elapsed() > Duration::from_secs(2) {
            self.connect();
        }
        // Let a highlight fade rather than vanish, so a quick tap is still
        // visible on the next frame.
        self.active
            .retain(|_, t| t.elapsed() < Duration::from_millis(400));
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Button { bit, down } => {
                let Some(profile) = self.profile.as_ref() else { return };
                let Some((name, _)) = profile.button_at_bit(bit) else {
                    return;
                };
                let name = name.clone();
                if down {
                    self.active.insert(name.clone(), Instant::now());
                    if self.follow_hardware {
                        self.selection = Selection::Button(name);
                    }
                } else {
                    self.active.insert(name, Instant::now());
                }
            }
            Event::Pad { pad, down, value } => {
                if (pad as usize) < 16 {
                    self.pad_values[pad as usize] = if down { value } else { 0 };
                }
                self.active
                    .insert(format!("pad {pad}"), Instant::now());
                if down && self.follow_hardware {
                    self.selection = Selection::Pads;
                }
            }
            Event::Knob { knob, value } => {
                if knob < 8 {
                    self.knob_values[knob] = value;
                }
                self.active
                    .insert(format!("knob {}", knob + 1), Instant::now());
                if self.follow_hardware {
                    self.selection = Selection::Knobs;
                }
            }
            Event::Encoder { value } => {
                self.encoder_value = value;
                self.active.insert("encoder".into(), Instant::now());
                if self.follow_hardware {
                    self.selection = Selection::Encoder;
                }
            }
            Event::Strip { value } => {
                self.strip_value = value;
                self.active.insert("touch strip".into(), Instant::now());
                if self.follow_hardware {
                    self.selection = Selection::Strip;
                }
            }
        }
    }

    /// Read a value out of the live document.
    fn get_str(&self, table: &str, key: &str) -> String {
        self.doc
            .as_ref()
            .and_then(|d| d.get(table))
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn get_int(&self, table: &str, key: &str) -> i64 {
        self.doc
            .as_ref()
            .and_then(|d| d.get(table))
            .and_then(|t| t.get(key))
            .and_then(|v| v.as_integer())
            .unwrap_or(0)
    }

    fn set_int(&mut self, table: &str, key: &str, v: i64) {
        if let Some(d) = self.doc.as_mut() {
            if let Some(t) = d.get_mut(table) {
                t[key] = toml_edit::value(v);
                self.dirty = true;
            }
        }
    }

    fn set_str(&mut self, table: &str, key: &str, v: &str) {
        if let Some(d) = self.doc.as_mut() {
            if let Some(t) = d.get_mut(table) {
                t[key] = toml_edit::value(v);
                self.dirty = true;
            }
        }
    }




}

/// Start the driver as a detached child.
fn spawn_driver() -> Result<()> {
    // Prefer a sibling of this binary, so a build directory works without
    // anything being installed.
    let own = std::env::current_exe().ok();
    let sibling = own.as_ref().and_then(|p| p.parent()).map(|d| d.join("mk3d"));
    let candidates: Vec<std::path::PathBuf> = sibling
        .into_iter()
        .filter(|p| p.exists())
        .chain(std::iter::once(std::path::PathBuf::from("mk3d")))
        .collect();

    let mut last = None;
    for exe in candidates {
        match std::process::Command::new(&exe)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap()).context("mk3d not found next to this binary or on PATH")
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        // The panel shows live hardware state, so it has to redraw without
        // waiting for mouse input.
        ctx.request_repaint_after(Duration::from_millis(33));

        egui::TopBottomPanel::top("bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("MASCHINE MK3");
                ui.separator();
                let colour = if self.status_bad {
                    egui::Color32::from_rgb(255, 107, 107)
                } else if self.client.is_some() {
                    egui::Color32::from_rgb(78, 201, 122)
                } else {
                    egui::Color32::GRAY
                };
                ui.colored_label(colour, &self.status);
                if self.dirty {
                    ui.colored_label(egui::Color32::from_rgb(255, 138, 0), "• unsaved");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Save & apply").clicked() {
                        self.save();
                    }
                    if ui.button("Reload").clicked() {
                        self.load();
                    }
                    ui.checkbox(&mut self.raw_editor, "Raw TOML");
                    ui.checkbox(&mut self.follow_hardware, "Follow hardware")
                        .on_hover_text(
                            "Pressing a control on the MK3 selects it here",
                        );
                });
            });
        });

        self.preset_bar(ctx);

        egui::SidePanel::right("inspector")
            .resizable(true)
            .default_width(340.0)
            .show(ctx, |ui| self.inspector(ui));

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.raw_editor {
                ui.label(format!("Editing {} directly", self.path));
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut self.raw_text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(40),
                    );
                    if r.changed() {
                        self.dirty = true;
                    }
                });
            } else {
                self.draw_panel(ui);
            }
        });
    }
}

impl App {
    /// The preset strip under the toolbar.
    ///
    /// Presets are whole configurations, so switching one is a bigger step
    /// than editing a field: it replaces the file. The driver keeps the
    /// previous config beside it as `.toml.prev`, and the bar says so, because
    /// an unlabelled button that silently discards an evening's work is not
    /// something anyone should have to trust.
    fn preset_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("presets").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Preset:");
                let current = self
                    .active_preset
                    .clone()
                    .unwrap_or_else(|| "(unsaved)".into());
                let mut chosen: Option<String> = None;
                egui::ComboBox::from_id_source("preset-picker")
                    .selected_text(&current)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for p in &self.presets {
                            let label = if p.builtin {
                                format!("{}  ·  built-in", p.name)
                            } else if p.shadows_builtin {
                                format!("{}  ·  yours (shadows built-in)", p.name)
                            } else {
                                format!("{}  ·  yours", p.name)
                            };
                            if ui
                                .selectable_label(current == p.name, label)
                                .on_hover_text(&p.description)
                                .clicked()
                            {
                                chosen = Some(p.name.clone());
                            }
                        }
                    });
                if let Some(name) = chosen {
                    if name != current {
                        self.preset_request(
                            Request::LoadPreset { name: name.clone() },
                            &format!("loaded `{name}` (previous config kept as config.toml.prev)"),
                        );
                    }
                }

                if let Some(p) = self.presets.iter().find(|p| p.name == current) {
                    ui.label(egui::RichText::new(&p.description).weak());
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Folder").on_hover_text(&self.presets_dir).clicked() {
                        // Opening the directory is how a preset gets shared:
                        // they are ordinary files, so sending one is sending a
                        // file and receiving one is dropping it in here.
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&self.presets_dir)
                            .spawn();
                    }
                    if ui.button("Import...").clicked() {
                        self.import = Some((String::new(), String::new()));
                    }
                    let deletable = self
                        .presets
                        .iter()
                        .any(|p| p.name == current && !p.builtin);
                    if ui
                        .add_enabled(deletable, egui::Button::new("Delete"))
                        .on_hover_text(if deletable {
                            "Remove this preset file"
                        } else {
                            "Built-in presets cannot be deleted, only shadowed by one of yours"
                        })
                        .clicked()
                    {
                        self.preset_request(
                            Request::DeletePreset {
                                name: current.clone(),
                            },
                            &format!("deleted `{current}`"),
                        );
                    }
                    if ui.button("Save as...").clicked() {
                        self.save_as = Some((
                            if current == "(unsaved)" {
                                String::new()
                            } else {
                                current.clone()
                            },
                            self.presets
                                .iter()
                                .find(|p| p.name == current)
                                .map(|p| p.description.clone())
                                .unwrap_or_default(),
                        ));
                    }
                });
            });
        });

        self.save_as_window(ctx);
        self.import_window(ctx);
    }

    fn save_as_window(&mut self, ctx: &egui::Context) {
        let Some((mut name, mut desc)) = self.save_as.clone() else {
            return;
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new("Save preset")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut name);
                ui.small("Letters, digits, `-` and `_`. This becomes the file name.");
                ui.add_space(6.0);
                ui.label("What is it for?");
                ui.text_edit_singleline(&mut desc);
                ui.small("Shown in the chooser, and to whoever you send it to.");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!name.is_empty(), egui::Button::new("Save"))
                        .clicked()
                    {
                        go = true;
                    }
                    if self.dirty {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 138, 0),
                            "unsaved edits are not included — apply them first",
                        );
                    }
                });
            });
        if go {
            self.preset_request(
                Request::SavePreset {
                    name: name.clone(),
                    description: desc.clone(),
                },
                &format!("saved `{name}`"),
            );
            self.save_as = None;
        } else if !open {
            self.save_as = None;
        } else {
            self.save_as = Some((name, desc));
        }
    }

    fn import_window(&mut self, ctx: &egui::Context) {
        let Some((mut name, mut text)) = self.import.clone() else {
            return;
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new("Import a preset")
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(
                    "Paste a preset someone sent you, or drop the .toml file into the                      presets folder and it will appear in the list.",
                );
                ui.add_space(6.0);
                ui.label("Name");
                ui.text_edit_singleline(&mut name);
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY)
                            .desired_rows(16),
                    );
                });
                ui.add_space(6.0);
                if ui
                    .add_enabled(
                        !name.is_empty() && !text.trim().is_empty(),
                        egui::Button::new("Import"),
                    )
                    .clicked()
                {
                    go = true;
                }
                ui.small("It is checked against this device before anything is written.");
            });
        if go {
            self.preset_request(
                Request::ImportPreset {
                    name: name.clone(),
                    toml: text.clone(),
                },
                &format!("imported `{name}`"),
            );
            self.import = None;
        } else if !open {
            self.import = None;
        } else {
            self.import = Some((name, text));
        }
    }

    fn draw_panel(&mut self, ui: &mut egui::Ui) {
        let Some(profile) = self.profile.clone() else {
            ui.label("Waiting for the device profile...");
            return;
        };
        let avail = ui.available_size();
        let scale = (avail.x / profile.layout.panel_width)
            .min(avail.y / profile.layout.panel_height);
        let origin = ui.min_rect().min;
        let _ = ui.allocate_space(egui::vec2(
            profile.layout.panel_width * scale,
            profile.layout.panel_height * scale,
        ));
        let painter = ui.painter().clone();

        let mut clicked: Option<Selection> = None;

        // Draw in a stable order so overlapping regions do not flicker.
        let mut slots: Vec<(&String, &Control)> = profile.placed().collect();
        slots.sort_by_key(|(n, _)| n.as_str());

        for (name, c) in slots {
            let rect = egui::Rect::from_min_size(
                origin + egui::vec2(c.x.unwrap_or(0.0) * scale, c.y.unwrap_or(0.0) * scale),
                egui::vec2(c.w.unwrap_or(2.0) * scale, c.h.unwrap_or(1.6) * scale),
            );
            let hot = self.active.contains_key(name.as_str());
            let selected = match (&self.selection, c.kind) {
                (Selection::Button(n), ControlKind::Button) => n == name,
                (Selection::Pads, ControlKind::Pad) => true,
                (Selection::Knobs, ControlKind::Knob) => true,
                (Selection::Encoder, ControlKind::Encoder) => true,
                (Selection::Strip, ControlKind::Strip) => true,
                _ => false,
            };
            // A button with no binding is drawn dimmed: it exists on the
            // hardware but the config says nothing about it.
            let bound = c.kind != ControlKind::Button
                || self
                    .cfg
                    .as_ref()
                    .is_some_and(|cf| cf.buttons.contains_key(name.as_str()));

            let base = match c.kind {
                ControlKind::Screen => egui::Color32::from_rgb(18, 20, 26),
                ControlKind::Pad => egui::Color32::from_rgb(38, 42, 52),
                _ if !bound => egui::Color32::from_rgb(28, 28, 32),
                _ => egui::Color32::from_rgb(46, 50, 60),
            };
            let fill = if hot {
                egui::Color32::from_rgb(255, 138, 0)
            } else {
                base
            };
            let stroke = if selected {
                egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 138, 0))
            } else if c.led_colour.is_some() {
                // Colour LEDs get a hint of one, so the mixed bank is visible.
                egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 90, 60))
            } else {
                egui::Stroke::new(1.0, egui::Color32::from_rgb(70, 74, 84))
            };
            let rounding = if c.kind == ControlKind::Knob {
                rect.width() / 2.0
            } else {
                3.0
            };
            painter.rect(rect, rounding, fill, stroke);

            let level = match c.kind {
                ControlKind::Knob => Some(self.knob_values[c.index.min(7)] as f32 / 127.0),
                ControlKind::Strip => Some(self.strip_value as f32 / 127.0),
                ControlKind::Encoder => Some(self.encoder_value as f32 / 127.0),
                ControlKind::Pad => Some(self.pad_values[c.index.min(15)] as f32 / 127.0),
                _ => None,
            };
            if let Some(level) = level {
                if level > 0.0 {
                    let inner = egui::Rect::from_min_max(
                        egui::pos2(
                            rect.left() + 2.0,
                            rect.bottom() - 4.0 - (rect.height() - 6.0) * level,
                        ),
                        egui::pos2(rect.right() - 2.0, rect.bottom() - 2.0),
                    );
                    painter.rect_filled(
                        inner,
                        2.0,
                        egui::Color32::from_rgb(255, 138, 0).gamma_multiply(0.8),
                    );
                }
            }

            let text = match c.kind {
                ControlKind::Knob => {
                    format!("{}\n{}", c.label, self.knob_values[c.index.min(7)])
                }
                ControlKind::Encoder => format!("{}\n{}", c.label, self.encoder_value),
                ControlKind::Strip => format!("{} {}", c.label, self.strip_value),
                ControlKind::Screen => String::new(),
                _ => c.label.clone(),
            };
            if !text.is_empty() {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional((scale * 0.42).clamp(7.0, 13.0)),
                    if hot {
                        egui::Color32::BLACK
                    } else if bound {
                        egui::Color32::from_rgb(220, 222, 228)
                    } else {
                        egui::Color32::from_rgb(110, 112, 120)
                    },
                );
            }

            let resp = ui.interact(rect, ui.id().with(name.as_str()), egui::Sense::click());
            if resp.clicked() {
                clicked = Some(match c.kind {
                    ControlKind::Button => Selection::Button(name.clone()),
                    ControlKind::Pad => Selection::Pads,
                    ControlKind::Knob => Selection::Knobs,
                    ControlKind::Encoder => Selection::Encoder,
                    ControlKind::Strip => Selection::Strip,
                    ControlKind::Screen => Selection::General,
                });
            }
            if resp.hovered() {
                let mut tip = name.clone();
                if let Some(b) = c.bit {
                    tip.push_str(&format!("\nbit {b}"));
                }
                match c.led {
                    Some(l) => tip.push_str(&format!("  LED slot {l}")),
                    None => tip.push_str("  no LED"),
                }
                if !bound {
                    tip.push_str("\n\nnot bound -- click to give it a MIDI message");
                }
                resp.on_hover_text(tip);
            }
        }

        if let Some(sel) = clicked {
            self.selection = sel;
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.selectable_label(matches!(self.selection, Selection::Pads), "Pads").clicked() {
                self.selection = Selection::Pads;
            }
            if ui.selectable_label(matches!(self.selection, Selection::Knobs), "Knobs").clicked() {
                self.selection = Selection::Knobs;
            }
            if ui.selectable_label(matches!(self.selection, Selection::General), "General").clicked() {
                self.selection = Selection::General;
            }
            if ui.selectable_label(matches!(self.selection, Selection::Strip), "Strip").clicked() {
                self.selection = Selection::Strip;
            }
        });
        ui.separator();

        if self.doc.is_none() {
            ui.label("No config loaded.");
            if ui.button("Retry").clicked() {
                self.connect();
            }
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| match self.selection.clone() {
            Selection::Button(name) => self.button_inspector(ui, &name),
            Selection::Pads => self.pads_inspector(ui),
            Selection::Knobs => self.knobs_inspector(ui),
            Selection::Encoder => self.encoder_inspector(ui),
            Selection::Strip => self.strip_inspector(ui),
            Selection::General => self.general_inspector(ui),
            Selection::None => {
                ui.label("Click a control, or press one on the MK3.");
            }
        });
    }

    fn button_inspector(&mut self, ui: &mut egui::Ui, name: &str) {
        ui.heading(name);

        // Where the control is wired is hardware, shown but not editable here.
        if let Some(c) = self.profile.as_ref().and_then(|p| p.get(name)) {
            let led = match c.led {
                Some(l) => format!("LED slot {l}"),
                None => "no LED".to_string(),
            };
            let colour = match c.led_colour {
                Some(p) => format!(", colour palette {p}"),
                None => String::new(),
            };
            ui.small(format!(
                "bit {}  ·  {led}{colour}",
                c.bit.map(|b| b.to_string()).unwrap_or_else(|| "-".into())
            ));
            if let Some(g) = &c.group {
                ui.small(format!("group: {g}"));
            }
            ui.small("Those come from the device profile, not your settings.");
        }
        ui.add_space(8.0);

        let bound = self
            .cfg
            .as_ref()
            .is_some_and(|c| c.buttons.contains_key(name));
        if !bound {
            ui.label("This control sends nothing.");
            if ui.button("Bind it").clicked() {
                self.set_binding(name, "none", "momentary", "follow");
            }
            return;
        }

        let mut send = self.binding_str(name, "send");
        ui.label("Send");
        if ui.text_edit_singleline(&mut send).changed() {
            let (m, l) = (self.binding_str(name, "mode"), self.binding_str(name, "led"));
            self.set_binding(name, &send, &m, &l);
        }
        ui.small("none | note CH N | cc CH N [VAL] | pc CH N | start | stop | continue");

        ui.add_space(8.0);
        let mode = self.binding_str(name, "mode");
        ui.label("Press behaviour");
        for opt in ["momentary", "toggle", "trigger"] {
            if ui.radio(mode == opt, opt).clicked() {
                let (s, l) = (self.binding_str(name, "send"), self.binding_str(name, "led"));
                self.set_binding(name, &s, opt, &l);
            }
        }

        ui.add_space(8.0);
        let led = self.binding_str(name, "led");
        ui.label("LED");
        for (opt, help) in [
            ("follow", "mirror the button"),
            ("midi", "mirror what the host sends back"),
            ("always", "always lit"),
            ("off", "never lit"),
        ] {
            if ui.radio(led == opt, opt).on_hover_text(help).clicked() {
                let (s, m) = (self.binding_str(name, "send"), self.binding_str(name, "mode"));
                self.set_binding(name, &s, &m, opt);
            }
        }

        ui.add_space(10.0);
        if ui.button("Unbind").clicked() {
            if let Some(d) = self.doc.as_mut() {
                if let Some(t) = d.get_mut("buttons").and_then(|b| b.as_table_mut()) {
                    t.remove(name);
                    self.dirty = true;
                }
            }
        }
    }

    /// Read one field of a binding, whatever form it is written in.
    fn binding_str(&self, name: &str, key: &str) -> String {
        let Some(item) = self
            .doc
            .as_ref()
            .and_then(|d| d.get("buttons"))
            .and_then(|b| b.get(name))
        else {
            return String::new();
        };
        // The shorthand form is a bare string meaning `send`.
        if let Some(s) = item.as_str() {
            return if key == "send" {
                s.to_string()
            } else if key == "mode" {
                "momentary".into()
            } else {
                "follow".into()
            };
        }
        item.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(match key {
                "mode" => "momentary",
                "led" => "follow",
                _ => "none",
            })
            .to_string()
    }

    /// Write a binding back, using the short form when the defaults apply.
    ///
    /// Keeping the file in its simplest form matters: someone reading it
    /// should see `play = "cc 1 118"`, not a table repeating two defaults.
    fn set_binding(&mut self, name: &str, send: &str, mode: &str, led: &str) {
        let Some(d) = self.doc.as_mut() else { return };
        if d.get("buttons").is_none() {
            d["buttons"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let Some(t) = d.get_mut("buttons").and_then(|b| b.as_table_mut()) else {
            return;
        };
        if mode == "momentary" && led == "follow" {
            t[name] = toml_edit::value(send);
        } else {
            let mut inline = toml_edit::InlineTable::new();
            inline.insert("send", send.into());
            if mode != "momentary" {
                inline.insert("mode", mode.into());
            }
            if led != "follow" {
                inline.insert("led", led.into());
            }
            t[name] = toml_edit::value(inline);
        }
        self.dirty = true;
    }

    fn pads_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pads");
        self.int_field(ui, "pads", "channel", 1, 16, "MIDI channel");
        self.enum_field(ui, "pads", "curve", &["linear", "soft", "hard", "fixed"], "Velocity curve");
        self.int_field(ui, "pads", "velocity_max", 1, 4095, "Raw hit for velocity 127");
        self.int_field(ui, "pads", "threshold", 0, 4095, "Ignore hits below");
        self.enum_field(ui, "pads", "aftertouch", &["off", "poly", "channel"], "Aftertouch");
        self.enum_field(ui, "pads", "aftertouch_curve", &["linear", "soft", "hard"], "Pressure curve");
        self.int_field(ui, "pads", "aftertouch_max", 1, 4095, "Raw pressure for 127");
        self.int_field(ui, "pads", "aftertouch_floor", 0, 4095, "Pressure noise floor");
        self.int_field(ui, "pads", "idle_colour", 0, 31, "Idle colour");
        self.int_field(ui, "pads", "active_colour", 0, 31, "Struck colour");
        self.int_field(ui, "pads", "idle_level", 0, 3, "Idle brightness step");

        ui.add_space(8.0);
        ui.label("Note per pad");
        ui.small("Laid out as the pads are; the device numbers them top-left first.");
        let notes: Vec<i64> = self
            .doc
            .as_ref()
            .and_then(|d| d.get("pads"))
            .and_then(|p| p.get("notes"))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_integer()).collect())
            .unwrap_or_default();
        if notes.len() == 16 {
            let mut changed: Option<(usize, i64)> = None;
            egui::Grid::new("padnotes").spacing([4.0, 4.0]).show(ui, |ui| {
                for row in 0..4 {
                    for col in 0..4 {
                        let i = row * 4 + col;
                        let mut v = notes[i];
                        if ui
                            .add(egui::DragValue::new(&mut v).clamp_range(0..=127))
                            .changed()
                        {
                            changed = Some((i, v));
                        }
                    }
                    ui.end_row();
                }
            });
            if let Some((i, v)) = changed {
                if let Some(d) = self.doc.as_mut() {
                    if let Some(arr) = d
                        .get_mut("pads")
                        .and_then(|p| p.get_mut("notes"))
                        .and_then(|n| n.as_array_mut())
                    {
                        if let Some(item) = arr.get_mut(i) {
                            *item = toml_edit::Value::from(v);
                            self.dirty = true;
                        }
                    }
                }
            }
        }
    }

    fn knobs_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("Knobs");
        ui.small(
            "The knobs are endless encoders: their raw position rolls over from \
             999 to 0. `accumulate` integrates movement here and clamps at the \
             ends, so they behave like knobs with end stops.",
        );
        ui.add_space(4.0);
        self.int_field(ui, "knobs", "channel", 1, 16, "MIDI channel");
        self.enum_field(ui, "knobs", "mode", &["accumulate", "absolute", "relative"], "Mode");
        self.int_field(ui, "knobs", "travel", 50, 8000, "Raw units for full range");
        self.int_field(ui, "knobs", "initial", 0, 127, "Value at startup");
        self.int_field(ui, "knobs", "deadband", 0, 100, "Jitter deadband");

        ui.add_space(8.0);
        ui.label("Live");
        for (i, v) in self.knob_values.iter().enumerate() {
            ui.add(
                egui::ProgressBar::new(*v as f32 / 127.0)
                    .text(format!("knob {}  {v:>3}", i + 1)),
            );
        }
    }

    fn encoder_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("4-D encoder");
        self.int_field(ui, "encoder", "channel", 1, 16, "MIDI channel");
        self.int_field(ui, "encoder", "cc", 0, 127, "CC");
        self.enum_field(ui, "encoder", "mode", &["absolute", "accumulate", "relative"], "Mode");
        self.int_field(ui, "encoder", "step", 1, 32, "Detents per step");
        self.int_field(ui, "encoder", "initial", 0, 127, "Value at startup");
        ui.add_space(8.0);
        ui.add(
            egui::ProgressBar::new(self.encoder_value as f32 / 127.0)
                .text(format!("{}", self.encoder_value)),
        );
    }

    fn strip_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("Touch strip");
        self.int_field(ui, "touchstrip", "channel", 1, 16, "MIDI channel");
        self.int_field(ui, "touchstrip", "cc", 0, 127, "CC");
        self.int_field(ui, "touchstrip", "led_value", 0, 127, "LED colour byte");
        ui.small(
            "The strip decodes colour differently from the pads, so this is a raw \
             byte rather than a palette index. `mk3-learn probe strip` lights all \
             25 LEDs in different colours so you can pick one.",
        );
        ui.add_space(8.0);
        ui.add(
            egui::ProgressBar::new(self.strip_value as f32 / 127.0)
                .text(format!("{}", self.strip_value)),
        );
    }

    fn general_inspector(&mut self, ui: &mut egui::Ui) {
        ui.heading("General");
        ui.small(&self.path);
        ui.add_space(6.0);
        for key in ["client_name", "out_port", "in_port"] {
            let mut v = self.get_str("general", key);
            ui.label(key);
            if ui.text_edit_singleline(&mut v).changed() {
                self.set_str("general", key, &v);
            }
        }
        ui.small("Port names take effect when the driver restarts.");
        ui.add_space(6.0);
        self.int_field(ui, "general", "realtime_priority", 0, 99, "Real-time priority");
        ui.add_space(6.0);
        self.int_field(ui, "display", "brightness", 0, 100, "Screen brightness");
        self.int_field(ui, "display", "contrast", 0, 100, "Screen contrast");
        self.int_field(ui, "leds", "button_idle", 0, 127, "LED idle brightness");
        self.int_field(ui, "leds", "button_active", 0, 127, "LED active brightness");
    }

    fn int_field(&mut self, ui: &mut egui::Ui, table: &str, key: &str, lo: i64, hi: i64, label: &str) {
        let mut v = self.get_int(table, key);
        if ui
            .add(egui::DragValue::new(&mut v).clamp_range(lo..=hi).prefix(format!("{label}: ")))
            .changed()
        {
            self.set_int(table, key, v);
        }
    }

    fn enum_field(&mut self, ui: &mut egui::Ui, table: &str, key: &str, opts: &[&str], label: &str) {
        let cur = self.get_str(table, key);
        ui.horizontal(|ui| {
            ui.label(label);
            for opt in opts {
                if ui.selectable_label(cur == *opt, *opt).clicked() {
                    self.set_str(table, key, opt);
                }
            }
        });
    }
}
