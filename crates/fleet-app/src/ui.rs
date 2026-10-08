//! The egui window. It only reads snapshots from `Fleet` and submits jobs;
//! all device work happens on the Tokio runtime, so the UI never blocks.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};
use fleet_core::model::now_secs;
use fleet_core::{Device, DeviceKey, DeviceMode, ErrorClass, Fleet, Health, JobId, JobState, JobView, PairState};

const OK: Color32 = Color32::from_rgb(0x2f, 0x9e, 0x5b);
const WARN: Color32 = Color32::from_rgb(0xd0, 0x8a, 0x10);
const BAD: Color32 = Color32::from_rgb(0xd0, 0x45, 0x3a);
const INFO: Color32 = Color32::from_rgb(0x3b, 0x82, 0xd6);

struct Notice {
    text: String,
    error: bool,
    until: Instant,
}

pub struct FleetApp {
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
                Action::EnterRecovery => (self.fleet.enter_recovery(&key), "Entering recovery"),
                Action::ExitRecovery => (self.fleet.exit_recovery(&key), "Exiting recovery"),
            };
            self.notify(r, what);
        }
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
    EnterRecovery,
    ExitRecovery,
}

impl eframe::App for FleetApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
            self.devices_table(ui);
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
