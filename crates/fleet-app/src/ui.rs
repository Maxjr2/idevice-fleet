//! The egui window. It only reads snapshots from `Fleet` and submits jobs;
//! all device work happens on the Tokio runtime, so the UI never blocks.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use fleet_core::model::now_secs;
use fleet_core::firmware::Firmware;
use fleet_core::{Device, Lookup, DeviceKey, DeviceMode, ErrorClass, Fleet, Health, JobId, JobState, JobView, PairState};

const OK: Color32 = Color32::from_rgb(0x2f, 0x9e, 0x5b);
const WARN: Color32 = Color32::from_rgb(0xd0, 0x8a, 0x10);
const BAD: Color32 = Color32::from_rgb(0xd0, 0x45, 0x3a);
const INFO: Color32 = Color32::from_rgb(0x3b, 0x82, 0xd6);

struct Notice {
    text: String,
    error: bool,
    until: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Devices,
    Firmware,
    Backups,
}

pub struct FleetApp {
    /// Set when drawing the UI panicked; the window shows this instead of crashing.
    crashed: Option<String>,
    tab: Tab,
    identifier: String,
    lookup: Arc<Mutex<Lookup>>,
    fleet: Arc<Fleet>,
    revision: u64,
    devices: Vec<Device>,
    jobs: Vec<JobView>,
    health: (Health, Health),
    selected_job: Option<JobId>,
    notice: Option<Notice>,
}

impl FleetApp {
    pub fn new(cc: &eframe::CreationContext<'_>, fleet: Arc<Fleet>) -> Self {
        cc.egui_ctx.set_zoom_factor(1.0);
        let mut app = Self {
            crashed: None,
            tab: Tab::Devices,
            identifier: String::new(),
            lookup: Arc::new(Mutex::new(Lookup::Idle)),
            fleet,
            revision: 0,
            devices: Vec::new(),
            jobs: Vec::new(),
            health: (Health::Starting, Health::Starting),
            selected_job: None,
            notice: None,
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        let rev = self.fleet.revision();
        if rev != self.revision {
            self.revision = rev;
            self.devices = self.fleet.devices();
            self.jobs = self.fleet.engine.snapshot();
            self.health = self.fleet.health();
        }
    }

    fn notify(&mut self, result: fleet_core::Result<JobId>, what: &str) {
        match result {
            Ok(id) => {
                self.selected_job = Some(id);
                self.notice = Some(Notice { text: format!("{what} started"), error: false, until: Instant::now() + Duration::from_secs(4) });
            }
            Err(e) => {
                self.notice = Some(Notice { text: e.message, error: true, until: Instant::now() + Duration::from_secs(10) });
            }
        }
        self.revision = 0;
    }

    fn active_job_for(&self, key: &DeviceKey) -> Option<&JobView> {
        self.jobs.iter().find(|j| j.state.is_active() && j.device.as_ref() == Some(key))
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("iDevice Fleet");
            ui.add_space(12.0);
            health_chip(ui, "usbmuxd", &self.health.0);
            health_chip(ui, "USB", &self.health.1);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let busy: HashSet<_> = self.jobs.iter().filter(|j| j.state.is_active()).filter_map(|j| j.device.clone()).collect();
                ui.label(format!("{} devices · {} busy", self.devices.len(), busy.len()));
            });
        });
        if let Some(n) = &self.notice {
            if Instant::now() < n.until {
                ui.colored_label(if n.error { BAD } else { OK }, &n.text);
            } else {
                self.notice = None;
            }
        }
    }

    fn devices_table(&mut self, ui: &mut egui::Ui) {
        if self.devices.is_empty() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No devices connected").size(18.0));
                ui.label("Connect an iPhone or iPad by USB. Devices in recovery or DFU mode show up here too.");
            });
            return;
        }
        let mut action: Option<(Action, DeviceKey)> = None;
        let devices = self.devices.clone();
        TableBuilder::new(ui)
            .id_salt("devices")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(200.0).at_least(120.0))
            .column(Column::initial(100.0))
            .column(Column::initial(70.0))
            .column(Column::initial(120.0))
            .column(Column::initial(140.0))
            .column(Column::initial(60.0))
            .column(Column::initial(200.0).at_least(120.0))
            .column(Column::remainder().at_least(160.0))
            .header(22.0, |mut h| {
                for t in ["Device", "Mode", "OS", "Serial", "ECID", "Battery", "Status", "Actions"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|mut body| {
                for d in &devices {
                    let job = self.active_job_for(&d.key).cloned();
                    body.row(30.0, |mut row| {
                        row.col(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(d.label()).strong());
                                if let Some(p) = &d.product_type {
                                    ui.label(RichText::new(p).small().weak());
                                }
                            });
                        });
                        row.col(|ui| {
                            mode_badge(ui, d.mode);
                        });
                        row.col(|ui| {
                            ui.label(d.os_version.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.monospace(d.serial.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.monospace(d.ecid.map(|e| format!("{e:#x}")).unwrap_or_default());
                        });
                        row.col(|ui| {
                            if let Some(b) = d.battery_percent {
                                ui.colored_label(if b < 20 { BAD } else if b < 40 { WARN } else { ui.visuals().text_color() }, format!("{b}%"));
                            }
                        });
                        row.col(|ui| {
                            if let Some(j) = &job {
                                ui.add(progress_bar(j).desired_width(180.0));
                            } else if let Some(p) = &d.problem {
                                ui.colored_label(WARN, p).on_hover_text(p);
                            } else if d.mode == DeviceMode::Normal {
                                match d.pair_state {
                                    PairState::Paired => ui.colored_label(OK, d.activation_state.as_deref().unwrap_or("Trusted")),
                                    PairState::NotPaired => ui.colored_label(WARN, "Not trusted"),
                                    PairState::Unknown => ui.weak("Reading…"),
                                };
                            } else {
                                ui.weak("Ready");
                            }
                        });
                        row.col(|ui| {
                            let free = job.is_none();
                            if d.mode == DeviceMode::Normal {
                                if d.pair_state == PairState::NotPaired && ui.add_enabled(free, egui::Button::new("Trust")).on_hover_text("Pair: unlock the device and tap Trust").clicked() {
                                    action = Some((Action::Pair, d.key.clone()));
                                }
                                if ui.add_enabled(free && d.pair_state == PairState::Paired, egui::Button::new("Back up")).on_hover_text("Full backup into the backup folder").clicked() {
                                    action = Some((Action::Backup, d.key.clone()));
                                }
                                if ui.add_enabled(free && d.pair_state == PairState::Paired, egui::Button::new("Recovery mode")).clicked() {
                                    action = Some((Action::EnterRecovery, d.key.clone()));
                                }
                            }
                            if matches!(d.mode, DeviceMode::Recovery | DeviceMode::Dfu)
                                && ui.add_enabled(free, egui::Button::new("Exit recovery")).on_hover_text("Restart into normal mode").clicked()
                            {
                                action = Some((Action::ExitRecovery, d.key.clone()));
                            }
                        });
                    });
                }
            });
        if let Some((a, key)) = action {
            let (r, what) = match a {
                Action::Pair => (self.fleet.pair(&key), "Pairing"),
                Action::Backup => (self.fleet.backup(&key), "Backup"),
                Action::EnterRecovery => (self.fleet.enter_recovery(&key), "Entering recovery"),
                Action::ExitRecovery => (self.fleet.exit_recovery(&key), "Exiting recovery"),
            };
            self.notify(r, what);
        }
    }

    fn firmware_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Get firmware");
        ui.label("Look up a model to see which versions Apple still signs. Only signed firmware can be restored.");
        if self.identifier.is_empty()
            && let Some(p) = self.devices.iter().find_map(|d| d.product_type.clone())
        {
            self.identifier = p;
        }
        ui.horizontal(|ui| {
            ui.label("Model identifier");
            let edit = ui.add(egui::TextEdit::singleline(&mut self.identifier).hint_text("iPad13,18").desired_width(160.0));
            let loading = matches!(*self.lookup.lock().unwrap_or_else(|p| p.into_inner()), Lookup::Loading);
            let go = ui.add_enabled(!loading && !self.identifier.trim().is_empty(), egui::Button::new("Look up")).clicked()
                || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !loading);
            if go {
                self.fleet.start_lookup(&self.identifier, self.lookup.clone());
            }
            let models: Vec<String> = self.devices.iter().filter_map(|d| d.product_type.clone()).collect::<HashSet<_>>().into_iter().collect();
            for m in models {
                if ui.small_button(&m).clicked() {
                    self.identifier = m;
                    self.fleet.start_lookup(&self.identifier, self.lookup.clone());
                }
            }
        });
        let lookup = self.lookup.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let library = self.fleet.library();
        match lookup {
            Lookup::Idle => {}
            Lookup::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Looking up…");
                });
            }
            Lookup::Failed(e) => {
                ui.colored_label(BAD, e);
            }
            Lookup::Done(cat) => {
                let signed = cat.firmwares.iter().filter(|f| f.signed).count();
                ui.label(format!("{}: {} signed of {} versions", if cat.name.is_empty() { &cat.identifier } else { &cat.name }, signed, cat.firmwares.len()));
                let mut to_download: Option<Firmware> = None;
                egui::ScrollArea::vertical().id_salt("catalog").max_height(260.0).show(ui, |ui| {
                    TableBuilder::new(ui)
                        .id_salt("catalog_table")
                        .striped(true)
                        .column(Column::exact(80.0))
                        .column(Column::exact(80.0))
                        .column(Column::exact(90.0))
                        .column(Column::exact(80.0))
                        .column(Column::remainder())
                        .header(20.0, |mut h| {
                            for t in ["Version", "Build", "Size", "Signed", ""] {
                                h.col(|ui| {
                                    ui.strong(t);
                                });
                            }
                        })
                        .body(|mut body| {
                            for f in cat.firmwares.iter().take(40) {
                                let have = library.iter().any(|l| l.build.as_deref() == Some(f.build.as_str()) && l.product_types.contains(&cat.identifier));
                                body.row(24.0, |mut row| {
                                    row.col(|ui| {
                                        ui.label(&f.version);
                                    });
                                    row.col(|ui| {
                                        ui.monospace(&f.build);
                                    });
                                    row.col(|ui| {
                                        ui.label(format!("{:.2} GB", f.size as f64 / 1e9));
                                    });
                                    row.col(|ui| {
                                        ui.colored_label(if f.signed { OK } else { BAD }, if f.signed { "signed" } else { "not signed" });
                                    });
                                    row.col(|ui| {
                                        if have {
                                            ui.weak("In library");
                                        } else if ui.button("Download").on_hover_text(if f.signed { "" } else { "Apple no longer signs this version, so it can't be restored" }).clicked() {
                                            to_download = Some(f.clone());
                                        }
                                    });
                                });
                            }
                        });
                });
                if let Some(f) = to_download {
                    let r = self.fleet.download_firmware(&f);
                    self.notify(r, "Download");
                }
            }
        }
        ui.add_space(10.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Library");
            if ui.button("Refresh").clicked() {
                self.fleet.refresh_library();
            }
            ui.label(RichText::new(self.fleet.paths.firmware.display().to_string()).small().weak());
        });
        if library.is_empty() {
            ui.weak("No firmware yet. Look up a model above and download its signed version, or copy .ipsw files into the folder.");
        }
        egui::ScrollArea::vertical().id_salt("library").show(ui, |ui| {
            for l in &library {
                ui.horizontal(|ui| {
                    ui.monospace(&l.file);
                    match &l.error {
                        Some(e) => {
                            ui.colored_label(BAD, e);
                        }
                        None => {
                            ui.label(format!("{} ({}) · {} models · {:.2} GB", l.version.as_deref().unwrap_or("?"), l.build.as_deref().unwrap_or("?"), l.product_types.len(), l.size as f64 / 1e9));
                        }
                    }
                });
            }
        });
    }

    fn backups_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Backups");
            if ui.button("Refresh").clicked() {
                self.fleet.refresh_backups();
            }
            ui.label(RichText::new(self.fleet.paths.backups.display().to_string()).small().weak());
        });
        ui.label("Full device backups made with Back up. Turn on backup encryption on the device first, or saved passwords, Health and Wi-Fi data are left out.");
        let backups = self.fleet.backups();
        if backups.is_empty() {
            ui.add_space(20.0);
            ui.weak("No backups yet.");
            return;
        }
        TableBuilder::new(ui)
            .id_salt("backups")
            .striped(true)
            .column(Column::initial(200.0).at_least(100.0))
            .column(Column::initial(110.0))
            .column(Column::initial(70.0))
            .column(Column::initial(120.0))
            .column(Column::initial(150.0))
            .column(Column::initial(90.0))
            .column(Column::initial(90.0))
            .column(Column::remainder())
            .header(22.0, |mut h| {
                for t in ["Device", "Model", "OS", "Serial", "Last backup", "Encrypted", "Complete", "Size"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|mut body| {
                for b in &backups {
                    body.row(24.0, |mut row| {
                        row.col(|ui| {
                            ui.label(b.device_name.as_deref().unwrap_or(&b.folder));
                        });
                        row.col(|ui| {
                            ui.label(b.product_type.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.label(b.os_version.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.monospace(b.serial.as_deref().unwrap_or(""));
                        });
                        row.col(|ui| {
                            ui.label(b.date.map(ago).unwrap_or_default());
                        });
                        row.col(|ui| {
                            match b.encrypted {
                                Some(true) => ui.colored_label(OK, "yes"),
                                Some(false) => ui.colored_label(WARN, "no"),
                                None => ui.weak("?"),
                            };
                        });
                        row.col(|ui| {
                            if b.complete { ui.colored_label(OK, "yes") } else { ui.colored_label(BAD, "unfinished") };
                        });
                        row.col(|ui| {
                            ui.label(format!("{:.2} GB", b.size as f64 / 1e9));
                        });
                    });
                }
            });
    }

    fn jobs_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Jobs");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(self.jobs.iter().any(|j| !j.state.is_active()), egui::Button::new("Clear finished")).clicked() {
                    if let Err(e) = self.fleet.engine.clear_finished() {
                        self.notice = Some(Notice { text: e.message, error: true, until: Instant::now() + Duration::from_secs(8) });
                    }
                    self.revision = 0;
                }
            });
        });
        ui.separator();
        if self.jobs.is_empty() {
            ui.weak("Nothing has run yet. Jobs you start appear here, with their progress and logs.");
            return;
        }
        let jobs = self.jobs.clone();
        ui.columns(2, |cols| {
            egui::ScrollArea::vertical().id_salt("jobs").auto_shrink([false, false]).show(&mut cols[0], |ui| {
                for j in &jobs {
                    self.job_row(ui, j);
                }
            });
            self.log_view(&mut cols[1]);
        });
    }

    fn job_row(&mut self, ui: &mut egui::Ui, j: &JobView) {
        let selected = self.selected_job.as_deref() == Some(j.id.as_str());
        let frame = egui::Frame::group(ui.style()).fill(if selected { ui.visuals().selection.bg_fill.gamma_multiply(0.35) } else { ui.visuals().faint_bg_color });
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                state_badge(ui, j);
                if ui.add(egui::Label::new(RichText::new(&j.title).strong()).sense(egui::Sense::click())).clicked() {
                    self.selected_job = Some(j.id.clone());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if j.state.is_active() {
                        if ui.button("Cancel").clicked() {
                            self.fleet.engine.cancel(&j.id);
                        }
                    } else if j.state != JobState::Succeeded && ui.button("Run again").clicked() {
                        let r = self.fleet.engine.run_again(&j.id);
                        self.notify(r, "Job");
                    }
                    if ui.button("Log").clicked() {
                        self.selected_job = Some(j.id.clone());
                    }
                });
            });
            if j.state.is_active() {
                ui.add(progress_bar(j));
            }
            let mut meta = Vec::new();
            if j.max_attempts > 1 && j.attempt > 1 {
                meta.push(format!("attempt {}/{}", j.attempt, j.max_attempts));
            }
            if let (JobState::Retrying, Some(at)) = (j.state, j.next_retry_at) {
                meta.push(format!("next try in {}s", at.saturating_sub(now_secs())));
            }
            meta.push(ago(j.created_at));
            ui.label(RichText::new(meta.join(" · ")).small().weak());
            if let Some(e) = &j.error {
                let color = match e.class {
                    ErrorClass::Transient if j.state == JobState::Retrying => WARN,
                    ErrorClass::NeedsUser => WARN,
                    _ => BAD,
                };
                ui.colored_label(color, &e.message);
            }
        });
        ui.add_space(4.0);
    }

    fn log_view(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected_job.clone() else {
            ui.weak("Select a job to see its log.");
            return;
        };
        let lines = self.fleet.engine.log_tail(&id, 1000);
        ui.horizontal(|ui| {
            ui.strong("Log");
            let path = self.fleet.engine.log_path(&id);
            ui.label(RichText::new(path.display().to_string()).small().weak()).on_hover_text("Full log file");
        });
        egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            for l in &lines {
                ui.label(RichText::new(l).monospace().small());
            }
        });
    }
}

#[derive(Clone, Copy)]
enum Action {
    Pair,
    Backup,
    EnterRecovery,
    ExitRecovery,
}

impl eframe::App for FleetApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // A bug in drawing code must not take the window (and the running jobs'
        // view) down with it. Jobs live in the engine and keep running regardless.
        if let Some(msg) = self.crashed.clone() {
            let mut retry = false;
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_space(30.0);
                ui.vertical_centered(|ui| {
                    ui.heading("Something went wrong in the window");
                    ui.add_space(6.0);
                    ui.label("Your jobs are still running in the background. This is a display problem only.");
                    ui.add_space(6.0);
                    ui.colored_label(BAD, &msg);
                    ui.add_space(10.0);
                    retry = ui.button("Try again").clicked();
                    ui.label(RichText::new(format!("Details are in {}", self.fleet.paths.logs.join("app.log").display())).small().weak());
                });
            });
            if retry {
                self.crashed = None;
                self.revision = 0;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            return;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.draw(ui)));
        if let Err(payload) = result {
            let msg = payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into());
            tracing::error!("UI panic caught: {msg}");
            self.crashed = Some(msg);
        }
    }
}

impl FleetApp {
    fn draw(&mut self, ui: &mut egui::Ui) {
        self.refresh();
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            self.top_bar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("jobs").resizable(true).default_size(280.0).min_size(140.0).show(ui, |ui| {
            ui.add_space(6.0);
            self.jobs_panel(ui);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Devices, "Devices");
                ui.selectable_value(&mut self.tab, Tab::Firmware, "Firmware");
                ui.selectable_value(&mut self.tab, Tab::Backups, "Backups");
            });
            ui.separator();
            match self.tab {
                Tab::Devices => self.devices_table(ui),
                Tab::Firmware => self.firmware_tab(ui),
                Tab::Backups => self.backups_tab(ui),
            }
        });
        // Device and job changes arrive from other threads; poll cheaply.
        ui.ctx().request_repaint_after(Duration::from_millis(400));
    }
}

fn health_chip(ui: &mut egui::Ui, name: &str, h: &Health) {
    let (color, text, tip) = match h {
        Health::Ok => (OK, format!("● {name}"), "Working".to_string()),
        Health::Starting => (WARN, format!("● {name}"), "Starting".to_string()),
        Health::Down(why) => (BAD, format!("● {name}"), why.clone()),
    };
    ui.colored_label(color, text).on_hover_text(tip);
}

fn mode_badge(ui: &mut egui::Ui, m: DeviceMode) {
    let color = match m {
        DeviceMode::Normal => OK,
        DeviceMode::Recovery => WARN,
        DeviceMode::Dfu => BAD,
        DeviceMode::Reconnecting => INFO,
    };
    ui.colored_label(color, RichText::new(m.label()).strong());
}

fn state_badge(ui: &mut egui::Ui, j: &JobView) {
    let color = match j.state {
        JobState::Succeeded => OK,
        JobState::Running | JobState::Queued => INFO,
        JobState::Retrying | JobState::Interrupted => WARN,
        JobState::Failed => BAD,
        JobState::Cancelled => ui.visuals().weak_text_color(),
    };
    ui.colored_label(color, RichText::new(j.state.as_str()).strong());
}

fn progress_bar(j: &JobView) -> egui::ProgressBar {
    let text = match (&j.stage, j.progress) {
        (Some(s), Some(p)) => format!("{s} · {p:.0}%"),
        (Some(s), None) => s.clone(),
        (None, Some(p)) => format!("{p:.0}%"),
        (None, None) => j.state.as_str().to_string(),
    };
    match j.progress {
        Some(p) => egui::ProgressBar::new(p / 100.0).text(text),
        None => egui::ProgressBar::new(0.0).animate(j.state == JobState::Running).text(text),
    }
}

fn ago(t: u64) -> String {
    let s = now_secs().saturating_sub(t);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// Shown instead of the main window when the app can't start.
pub struct StartupError(pub String);

impl eframe::App for StartupError {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.heading("iDevice Fleet couldn't start");
                ui.add_space(8.0);
                ui.colored_label(BAD, &self.0);
            });
        });
    }
}
