//! The egui window. It only reads snapshots from `Fleet` and submits jobs;
//! all device work happens on the Tokio runtime, so the UI never blocks.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Sense, Stroke, vec2};
use egui_extras::{Column, TableBuilder};
use fleet_core::firmware::Firmware;
use fleet_core::model::now_secs;
use fleet_core::{Device, DeviceKey, DeviceMode, ErrorClass, Fleet, Health, JobId, JobKind, JobState, JobView, Lookup, PairState};

use crate::theme::{self, Palette};
use crate::wizard::{self, Wizard};

/// Extras used to produce screenshots for the README and for checking the UI.
#[derive(Default, Clone)]
pub struct Options {
    pub screenshot: Option<PathBuf>,
    pub tab: Option<String>,
    pub lookup: Option<String>,
    pub theme: Option<String>,
    pub select_job: bool,
    /// Open the guided reset at this step (screenshots).
    pub wizard: Option<String>,
    /// Seconds to wait before taking a screenshot.
    pub wait: f32,
}

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

#[derive(Clone, Copy)]
enum Action {
    Pair,
    Reset,
    Backup,
    EnterRecovery,
    ExitRecovery,
}

enum GuideAction {
    Wizard,
    Download(String),
}

struct Guide {
    title: String,
    text: String,
    tone: u8, // 0 info, 1 warning, 2 problem
    button: Option<(String, GuideAction)>,
}

pub struct FleetApp {
    wizard: Option<Wizard>,
    dismissed_update: Option<String>,
    cache_confirm: Option<Instant>,
    storage_polled: Option<Instant>,
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
    opts: Options,
    frames: u32,
    shot_requested: bool,
}

impl FleetApp {
    pub fn new(cc: &eframe::CreationContext<'_>, fleet: Arc<Fleet>, opts: Options) -> Self {
        theme::apply(&cc.egui_ctx);
        match opts.theme.as_deref() {
            Some("dark") => cc.egui_ctx.set_theme(egui::ThemePreference::Dark),
            Some("light") => cc.egui_ctx.set_theme(egui::ThemePreference::Light),
            _ => {}
        }
        let tab = match opts.tab.as_deref() {
            Some("firmware") => Tab::Firmware,
            Some("backups") => Tab::Backups,
            _ => Tab::Devices,
        };
        let mut app = Self {
            wizard: None,
            dismissed_update: None,
            cache_confirm: None,
            storage_polled: None,
            crashed: None,
            tab,
            identifier: String::new(),
            lookup: Arc::new(Mutex::new(Lookup::Idle)),
            fleet,
            revision: 0,
            devices: Vec::new(),
            jobs: Vec::new(),
            health: (Health::Starting, Health::Starting),
            selected_job: None,
            notice: None,
            opts,
            frames: 0,
            shot_requested: false,
        };
        if let Some(id) = app.opts.lookup.clone() {
            app.identifier = id.clone();
            app.fleet.start_lookup(&id, app.lookup.clone());
        }
        app.refresh();
        if let Some(step) = app.opts.wizard.clone() {
            let step = match step.as_str() {
                "firmware" => wizard::Step::Firmware,
                "review" => wizard::Step::Review,
                "progress" => wizard::Step::Progress,
                "done" => wizard::Step::Done,
                _ => wizard::Step::Devices,
            };
            app.wizard = Some(Wizard::demo(step, &app.devices, &app.fleet));
        }
        app
    }

    fn update_banner(&mut self, ui: &mut egui::Ui, p: Palette) {
        let fleet_core::update::UpdateState::Available(info) = self.fleet.update_state() else { return };
        if self.dismissed_update.as_deref() == Some(info.version.as_str()) {
            return;
        }
        let deb_name = info.deb_url.as_deref().and_then(|u| fleet_core::update::check_update_url(u).ok());
        let downloaded = deb_name.as_deref().is_some_and(|n| self.fleet.paths.updates.join(n).is_file());
        let job = self.jobs.iter().find(|j| matches!(j.kind, JobKind::Download | JobKind::Install) && j.title.contains("update") && j.state.is_active()).cloned();
        egui::Frame::new().fill(p.accent.gamma_multiply(0.12)).stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.6))).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(14, 9)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("Version {} is available", info.version)).strong());
                    let first = info.notes.lines().find(|l| !l.trim().is_empty()).unwrap_or("A new version of iDevice Fleet is ready.");
                    ui.add(egui::Label::new(RichText::new(first).small().color(p.weak)).truncate());
                    if let Some(j) = &job {
                        ui.horizontal(|ui| {
                            theme::progress_bar(ui, p, j.progress.map(|x| x / 100.0), p.accent, 260.0);
                            ui.label(RichText::new(j.stage.clone().unwrap_or_default()).small().color(p.weak));
                        });
                    } else if downloaded {
                        ui.label(RichText::new("Downloaded and verified. Installing asks for your password and keeps your settings and data.").small().color(p.weak));
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Later").clicked() {
                        self.dismissed_update = Some(info.version.clone());
                    }
                    if ui.small_button("Release notes").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&info.page_url));
                    }
                    if job.is_some() {
                        return;
                    }
                    if downloaded {
                        if ui.add(theme::primary_button(p, "Install update")).clicked() {
                            let r = self.fleet.install_update(deb_name.as_deref().unwrap_or(""));
                            self.notify(r, "Installing the update");
                        }
                    } else if info.deb_url.is_some() && ui.add(theme::primary_button(p, "Download update")).clicked() {
                        let r = self.fleet.download_update(&info);
                        self.notify(r, "Downloading the update");
                    }
                });
            });
        });
    }

    fn guide(&self) -> Guide {
        let g = |tone: u8, title: &str, text: &str, button: Option<(&str, GuideAction)>| Guide { title: title.into(), text: text.into(), tone, button: button.map(|(l, a)| (l.to_string(), a)) };
        if let Health::Down(why) = &self.health.0 {
            return g(2, "The device service isn't running", &format!("{why}. Without it, connected devices can't be seen."), None);
        }
        if self.devices.is_empty() {
            return g(0, "Connect a device", "Plug in an iPhone or iPad with a USB cable. If it asks, unlock it and tap Trust. A device that's already in recovery mode shows up here too.", None);
        }
        if let Some(d) = self.devices.iter().find(|d| d.mode == DeviceMode::Normal && d.pair_state == PairState::NotPaired) {
            return g(1, &format!("Trust this computer on {}", device_title(d)), "Unlock the device, then use Trust below and tap Trust on its screen. It's only needed to back up or to put it into recovery mode for you. A reset also works if you put the device into recovery mode by hand.", None);
        }
        let lib = self.fleet.library();
        if let Some(m) = self.devices.iter().filter_map(|d| d.product_type.clone()).find(|m| fleet_core::firmware::matching(&lib, m).is_empty()) {
            return g(1, &format!("Get firmware for {m}"), "A reset installs fresh firmware, so it has to be downloaded first. One click fetches the newest version Apple still signs (about 10 GB, resumes if interrupted).", Some(("Download firmware", GuideAction::Download(m))));
        }
        g(0, "Ready to go", "Everything is in place. Reset & reinstall walks you through wiping devices step by step. If the data on a device matters, use Back up on it first.", Some(("Reset & reinstall…", GuideAction::Wizard)))
    }

    fn refresh(&mut self) {
        let rev = self.fleet.revision();
        if rev != self.revision {
            self.revision = rev;
            self.devices = self.fleet.devices();
            self.jobs = self.fleet.engine.snapshot();
            self.health = self.fleet.health();
            if self.opts.select_job && self.selected_job.is_none() {
                self.selected_job = self.jobs.iter().find(|j| j.state.is_active() && j.kind == JobKind::Test).map(|j| j.id.clone());
            }
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

    // ---- header ------------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui, p: Palette) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            // Logo: a rounded accent square with a tiny device outline.
            let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(8), p.accent);
            let dev = egui::Rect::from_center_size(rect.center(), vec2(11.0, 18.0));
            ui.painter().rect_stroke(dev, CornerRadius::same(3), Stroke::new(1.8, p.accent_text), egui::StrokeKind::Inside);
            ui.painter().circle_filled(egui::pos2(dev.center().x, dev.bottom() - 3.0), 1.1, p.accent_text);
            ui.add_space(2.0);
            ui.label(RichText::new("iDevice Fleet").size(18.0).strong());
            ui.add_space(18.0);
            for (tab, label) in [(Tab::Devices, "Devices"), (Tab::Firmware, "Firmware"), (Tab::Backups, "Backups")] {
                if theme::tab_button(ui, p, self.tab == tab, label).clicked() {
                    self.tab = tab;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(4.0);
                health_chip(ui, p, "USB", &self.health.1);
                health_chip(ui, p, "usbmuxd", &self.health.0);
            });
        });
    }

    // ---- devices -------------------------------------------------------------

    fn devices_tab(&mut self, ui: &mut egui::Ui, p: Palette) {
        let busy = self.jobs.iter().filter(|j| j.state.is_active()).filter_map(|j| j.device.as_ref()).collect::<HashSet<_>>().len();
        let attention = self.devices.iter().filter(|d| d.mode != DeviceMode::Normal || d.pair_state == PairState::NotPaired).count();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{} device{}", self.devices.len(), if self.devices.len() == 1 { "" } else { "s" })).size(16.0).strong());
            if busy > 0 {
                theme::pill(ui, &format!("{busy} busy"), p.info);
            }
            if attention > 0 {
                theme::pill(ui, &format!("{attention} need attention"), p.warn);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!self.devices.is_empty(), theme::primary_button(p, "Reset & reinstall…")).on_hover_text("A guided reset of one or more devices").clicked() {
                    self.wizard = Some(Wizard::new(None));
                }
            });
        });
        if self.devices.is_empty() {
            theme::empty_state(ui, p, "No devices connected", "Connect an iPhone or iPad by USB. Devices in recovery or DFU mode show up here too.");
            return;
        }
        let mut action: Option<(Action, DeviceKey)> = None;
        let devices = self.devices.clone();
        let guide = self.guide();
        let mut guide_click = false;
        let tone = [p.info, p.warn, p.bad][guide.tone as usize];
        egui::Frame::new().fill(tone.gamma_multiply(0.10)).stroke(Stroke::new(1.0, tone.gamma_multiply(0.5))).corner_radius(CornerRadius::same(10)).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&guide.title).strong().size(15.0));
                    ui.add(egui::Label::new(RichText::new(&guide.text).color(p.text)).wrap());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some((label, _)) = &guide.button {
                        guide_click = ui.add(theme::primary_button(p, label)).clicked();
                    }
                });
            });
        });
        if guide_click && let Some((_, a)) = guide.button {
            match a {
                GuideAction::Wizard => self.wizard = Some(Wizard::new(None)),
                GuideAction::Download(m) => {
                    self.fleet.download_latest_signed(&m);
                    self.notice = Some(Notice { text: format!("Looking up the newest signed firmware for {m}, the download starts automatically"), error: false, until: Instant::now() + Duration::from_secs(6) });
                }
            }
        }
        egui::ScrollArea::vertical().id_salt("devices").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            for d in &devices {
                let job = self.active_job_for(&d.key).cloned();
                if let Some(a) = device_card(ui, p, d, job.as_ref()) {
                    action = Some((a, d.key.clone()));
                }
            }
            ui.add_space(6.0);
        });
        if let Some((a, key)) = action {
            if matches!(a, Action::Reset) {
                self.wizard = Some(Wizard::new(Some(key)));
                return;
            }
            let (r, what) = match a {
                Action::Reset => unreachable!("handled above"),
                Action::Pair => (self.fleet.pair(&key), "Pairing"),
                Action::Backup => (self.fleet.backup(&key), "Backup"),
                Action::EnterRecovery => (self.fleet.enter_recovery(&key), "Entering recovery"),
                Action::ExitRecovery => (self.fleet.exit_recovery(&key), "Exiting recovery"),
            };
            self.notify(r, what);
        }
    }

    // ---- firmware ------------------------------------------------------------

    fn firmware_tab(&mut self, ui: &mut egui::Ui, p: Palette) {
        if self.identifier.is_empty()
            && let Some(m) = self.devices.iter().find_map(|d| d.product_type.clone())
        {
            self.identifier = m;
        }
        egui::ScrollArea::vertical().id_salt("firmware").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 12.0;
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Get firmware").size(16.0).strong());
                ui.label(RichText::new("Look up a model to see which versions Apple still signs. Only signed firmware can be restored.").color(p.weak));
                ui.horizontal(|ui| {
                    let edit = ui.add(egui::TextEdit::singleline(&mut self.identifier).hint_text("iPad13,18").desired_width(150.0));
                    let loading = matches!(*self.lookup.lock().unwrap_or_else(|e| e.into_inner()), Lookup::Loading);
                    let go = ui.add_enabled(!loading && !self.identifier.trim().is_empty(), theme::primary_button(p, "Look up")).clicked()
                        || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !loading);
                    if go {
                        self.fleet.start_lookup(&self.identifier, self.lookup.clone());
                    }
                    let models: Vec<String> = self.devices.iter().filter_map(|d| d.product_type.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                    if !models.is_empty() {
                        ui.label(RichText::new("Connected:").color(p.weak));
                    }
                    for m in models {
                        if ui.small_button(&m).clicked() {
                            self.identifier = m.clone();
                            self.fleet.start_lookup(&m, self.lookup.clone());
                        }
                    }
                });
                let lookup = self.lookup.lock().unwrap_or_else(|e| e.into_inner()).clone();
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
                        ui.colored_label(p.bad, e);
                    }
                    Lookup::Done(cat) => {
                        let signed = cat.firmwares.iter().filter(|f| f.signed).count();
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(if cat.name.is_empty() { cat.identifier.clone() } else { cat.name.clone() }).strong());
                            theme::pill(ui, &format!("{signed} signed"), p.ok);
                            ui.label(RichText::new(format!("of {} versions", cat.firmwares.len())).color(p.weak));
                        });
                        let mut to_download: Option<Firmware> = None;
                        egui::ScrollArea::vertical().id_salt("catalog").max_height(250.0).show(ui, |ui| {
                            TableBuilder::new(ui)
                                .id_salt("catalog_table")
                                .striped(true)
                                .column(Column::exact(90.0))
                                .column(Column::exact(90.0))
                                .column(Column::exact(100.0))
                                .column(Column::exact(100.0))
                                .column(Column::remainder())
                                .header(24.0, |mut h| {
                                    for t in ["Version", "Build", "Size", "Status", ""] {
                                        h.col(|ui| {
                                            ui.label(RichText::new(t).small().strong().color(p.weak));
                                        });
                                    }
                                })
                                .body(|mut body| {
                                    for f in cat.firmwares.iter().take(40) {
                                        let have = library.iter().any(|l| l.build.as_deref() == Some(f.build.as_str()) && l.product_types.contains(&cat.identifier));
                                        body.row(30.0, |mut row| {
                                            row.col(|ui| {
                                                ui.label(RichText::new(&f.version).strong());
                                            });
                                            row.col(|ui| {
                                                ui.monospace(&f.build);
                                            });
                                            row.col(|ui| {
                                                ui.label(fmt_size(f.size));
                                            });
                                            row.col(|ui| {
                                                theme::pill(ui, if f.signed { "signed" } else { "not signed" }, if f.signed { p.ok } else { p.weak });
                                            });
                                            row.col(|ui| {
                                                if have {
                                                    ui.label(RichText::new("In library").color(p.weak));
                                                } else {
                                                    let b = if f.signed { theme::primary_button(p, "Download") } else { egui::Button::new("Download") };
                                                    if ui.add(b).on_hover_text(if f.signed { "" } else { "Apple no longer signs this version, so it can't be restored" }).clicked() {
                                                        to_download = Some(f.clone());
                                                    }
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
            });
            let library = self.fleet.library();
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Library").size(16.0).strong());
                    theme::pill(ui, &format!("{} file{}", library.len(), if library.len() == 1 { "" } else { "s" }), p.weak);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Rescan").clicked() {
                            self.fleet.refresh_library();
                        }
                        ui.label(RichText::new(self.fleet.paths.firmware.display().to_string()).small().color(p.weak));
                    });
                });
                if library.is_empty() {
                    ui.label(RichText::new("No firmware yet. Look up a model above and download its signed version, or copy .ipsw files into the folder.").color(p.weak));
                }
                for l in &library {
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(&l.file).monospace()).truncate());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| match &l.error {
                            Some(e) => {
                                ui.colored_label(p.bad, e);
                            }
                            None => {
                                ui.label(RichText::new(fmt_size(l.size)).color(p.weak));
                                theme::pill(ui, &format!("{} model{}", l.product_types.len(), if l.product_types.len() == 1 { "" } else { "s" }), p.weak);
                                theme::pill(ui, &format!("{} · {}", l.version.as_deref().unwrap_or("?"), l.build.as_deref().unwrap_or("?")), p.accent);
                            }
                        });
                    });
                }
            });
            self.storage_card(ui, p);
        });
    }

    fn storage_card(&mut self, ui: &mut egui::Ui, p: Palette) {
        if self.storage_polled.is_none_or(|t| t.elapsed() > Duration::from_secs(5)) {
            self.storage_polled = Some(Instant::now());
            self.fleet.refresh_storage();
        }
        let st = self.fleet.storage();
        theme::card(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Storage").size(16.0).strong());
            let rows = [
                ("Firmware library", st.firmware, "Kept. Downloaded firmware used for resets."),
                ("Backups", st.backups, "Kept. Your device backups."),
                ("Logs", st.logs, "Job logs and the app log."),
            ];
            for (name, bytes, note) in rows {
                ui.horizontal(|ui| {
                    ui.add_sized([150.0, 20.0], egui::Label::new(RichText::new(name).strong()));
                    ui.add_sized([90.0, 20.0], egui::Label::new(fmt_size(bytes)));
                    ui.label(RichText::new(note).small().color(p.weak));
                });
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.add_sized([150.0, 20.0], egui::Label::new(RichText::new("Cache").strong()));
                ui.add_sized([90.0, 20.0], egui::Label::new(fmt_size(st.cache_total())));
                ui.label(RichText::new(format!("Unfinished downloads {} · restore working files {}", fmt_size(st.partial_downloads), fmt_size(st.restore_cache))).small().color(p.weak));
            });
            ui.horizontal(|ui| {
                let armed = self.cache_confirm.is_some_and(|t| t.elapsed() < Duration::from_secs(4));
                let label = if armed { "Click again to delete the cache" } else { "Delete cache" };
                let btn = if armed { egui::Button::new(RichText::new(label).color(egui::Color32::WHITE).strong()).fill(p.bad) } else { egui::Button::new(label) };
                if ui.add_enabled(st.cache_total() > 0, btn).on_hover_text("Deletes unfinished downloads and leftover restore files. Downloaded firmware and backups are kept.").clicked() {
                    if armed {
                        self.cache_confirm = None;
                        match self.fleet.clear_cache() {
                            Ok(freed) => self.notice = Some(Notice { text: format!("Deleted the cache and freed {}", fmt_size(freed)), error: false, until: Instant::now() + Duration::from_secs(6) }),
                            Err(e) => self.notice = Some(Notice { text: e.message, error: true, until: Instant::now() + Duration::from_secs(8) }),
                        }
                        self.fleet.refresh_storage();
                        self.storage_polled = Some(Instant::now());
                    } else {
                        self.cache_confirm = Some(Instant::now());
                    }
                }
                ui.label(RichText::new("A restore removes its own working files when it ends, so this is usually small.").small().color(p.weak));
            });
        });
        theme::card(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("About").size(16.0).strong());
            ui.horizontal(|ui| {
                ui.label(format!("iDevice Fleet {}", env!("CARGO_PKG_VERSION")));
                match self.fleet.update_state() {
                    fleet_core::update::UpdateState::Checking => {
                        ui.spinner();
                        ui.label("Checking for updates…");
                    }
                    fleet_core::update::UpdateState::UpToDate => {
                        theme::pill(ui, "up to date", p.ok);
                    }
                    fleet_core::update::UpdateState::Available(i) => {
                        theme::pill(ui, &format!("{} available", i.version), p.accent);
                    }
                    fleet_core::update::UpdateState::Failed(e) => {
                        ui.label(RichText::new(format!("Couldn't check: {e}")).small().color(p.warn));
                    }
                    fleet_core::update::UpdateState::Unknown => {}
                }
                if ui.button("Check for updates").clicked() {
                    self.dismissed_update = None;
                    self.fleet.check_for_updates();
                }
            });
        });
    }

    // ---- backups -------------------------------------------------------------

    fn backups_tab(&mut self, ui: &mut egui::Ui, p: Palette) {
        let backups = self.fleet.backups();
        theme::card(p).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Backups").size(16.0).strong());
                theme::pill(ui, &format!("{}", backups.len()), p.weak);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Rescan").clicked() {
                        self.fleet.refresh_backups();
                    }
                    ui.label(RichText::new(self.fleet.paths.backups.display().to_string()).small().color(p.weak));
                });
            });
            ui.label(RichText::new("Full device backups made with Back up. Turn on backup encryption on the device first, or saved passwords, Health and Wi-Fi data are left out.").color(p.weak));
            if backups.is_empty() {
                theme::empty_state(ui, p, "No backups yet", "Use Back up on a trusted device.");
                return;
            }
            ui.add_space(4.0);
            TableBuilder::new(ui)
                .id_salt("backups")
                .striped(true)
                .column(Column::initial(200.0).at_least(100.0))
                .column(Column::initial(110.0))
                .column(Column::initial(70.0))
                .column(Column::initial(120.0))
                .column(Column::initial(110.0))
                .column(Column::initial(100.0))
                .column(Column::initial(100.0))
                .column(Column::remainder())
                .header(24.0, |mut h| {
                    for t in ["Device", "Model", "OS", "Serial", "Last backup", "Encrypted", "Status", "Size"] {
                        h.col(|ui| {
                            ui.label(RichText::new(t).small().strong().color(p.weak));
                        });
                    }
                })
                .body(|mut body| {
                    for b in &backups {
                        body.row(32.0, |mut row| {
                            row.col(|ui| {
                                ui.label(RichText::new(b.device_name.as_deref().unwrap_or(&b.folder)).strong());
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
                                    Some(true) => theme::pill(ui, "encrypted", p.ok),
                                    Some(false) => theme::pill(ui, "not encrypted", p.warn),
                                    None => theme::pill(ui, "unknown", p.weak),
                                };
                            });
                            row.col(|ui| {
                                if b.complete { theme::pill(ui, "complete", p.ok) } else { theme::pill(ui, "unfinished", p.bad) };
                            });
                            row.col(|ui| {
                                ui.label(fmt_size(b.size));
                            });
                        });
                    }
                });
        });
    }

    // ---- jobs ----------------------------------------------------------------

    fn jobs_panel(&mut self, ui: &mut egui::Ui, p: Palette) {
        let active = self.jobs.iter().filter(|j| j.state.is_active()).count();
        let failed = self.jobs.iter().filter(|j| matches!(j.state, JobState::Failed | JobState::Interrupted)).count();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Jobs").size(16.0).strong());
            if active > 0 {
                theme::pill(ui, &format!("{active} running"), p.info);
            }
            if failed > 0 {
                theme::pill(ui, &format!("{failed} need a look"), p.bad);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(self.jobs.iter().any(|j| !j.state.is_active()), egui::Button::new("Clear finished")).clicked() {
                    if let Err(e) = self.fleet.engine.clear_finished() {
                        self.notice = Some(Notice { text: e.message, error: true, until: Instant::now() + Duration::from_secs(8) });
                    }
                    self.revision = 0;
                }
            });
        });
        if self.jobs.is_empty() {
            ui.label(RichText::new("Nothing has run yet. Jobs you start appear here with their progress and logs.").color(p.weak));
            return;
        }
        let jobs = self.jobs.clone();
        ui.columns(2, |cols| {
            egui::ScrollArea::vertical().id_salt("jobs").auto_shrink([false, false]).show(&mut cols[0], |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                for j in &jobs {
                    self.job_card(ui, p, j);
                }
            });
            self.log_view(&mut cols[1], p);
        });
    }

    fn job_card(&mut self, ui: &mut egui::Ui, p: Palette, j: &JobView) {
        let selected = self.selected_job.as_deref() == Some(j.id.as_str());
        let (state_color, state_label) = job_state(p, j);
        let frame = egui::Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(if selected { 1.5 } else { 1.0 }, if selected { p.accent } else { p.line }))
            .corner_radius(CornerRadius::same(9))
            .inner_margin(egui::Margin::symmetric(11, 9));
        let mut clicked_inside = false;
        let resp = frame
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    theme::pill(ui, state_label, state_color);
                    ui.add(egui::Label::new(RichText::new(&j.title).strong()).truncate());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if j.state.is_active() {
                            let label = if j.cancelling { "Cancelling…" } else { "Cancel" };
                            if ui.add_enabled(!j.cancelling, egui::Button::new(label).small()).clicked() {
                                clicked_inside = true;
                                self.fleet.engine.cancel(&j.id);
                                self.revision = 0;
                            }
                        } else if j.state != JobState::Succeeded && ui.small_button("Run again").clicked() {
                            clicked_inside = true;
                            let r = self.fleet.engine.run_again(&j.id);
                            self.notify(r, "Job");
                        }
                    });
                });
                if j.state.is_active() {
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 150.0).max(80.0);
                        theme::progress_bar(ui, p, j.progress.map(|x| x / 100.0).or(if j.state == JobState::Running { None } else { Some(0.0) }), state_color, w);
                        let mut t = Vec::new();
                        if let Some(x) = j.progress {
                            t.push(format!("{x:.0}%"));
                        }
                        if let (JobState::Retrying, Some(at)) = (j.state, j.next_retry_at) {
                            t.push(format!("retry in {}", fmt_secs(at.saturating_sub(now_secs()))));
                        }
                        ui.label(RichText::new(t.join(" · ")).small().color(p.weak));
                    });
                }
                let mut meta = Vec::new();
                if let Some(s) = &j.stage {
                    meta.push(s.clone());
                }
                if j.attempt > 1 {
                    meta.push(format!("attempt {}", j.attempt));
                }
                meta.push(ago(j.created_at));
                ui.add(egui::Label::new(RichText::new(meta.join(" · ")).small().color(p.weak)).truncate());
                if let Some(e) = &j.error {
                    let color = match e.class {
                        ErrorClass::NeedsUser => p.warn,
                        ErrorClass::Transient if j.state == JobState::Retrying => p.warn,
                        _ => p.bad,
                    };
                    ui.add(egui::Label::new(RichText::new(&e.message).small().color(color)).wrap());
                }
            })
            .response;
        // Select the card on click. Checked from the pointer state rather than by
        // laying a click area over the card, which would swallow the buttons' clicks.
        if !clicked_inside && ui.rect_contains_pointer(resp.rect) && ui.input(|i| i.pointer.primary_clicked()) {
            self.selected_job = Some(j.id.clone());
        }
    }

    fn log_view(&mut self, ui: &mut egui::Ui, p: Palette) {
        let Some(id) = self.selected_job.clone() else {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.label(RichText::new("Select a job to see its log").color(p.weak)));
            return;
        };
        let lines = self.fleet.engine.log_tail(&id, 1000);
        let frame = egui::Frame::new().fill(ui.visuals().extreme_bg_color).stroke(Stroke::new(1.0, p.line)).corner_radius(CornerRadius::same(9)).inner_margin(egui::Margin::same(10));
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Log").strong());
                ui.add(egui::Label::new(RichText::new(self.fleet.engine.log_path(&id).display().to_string()).small().color(p.weak)).truncate());
            });
            egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                for l in &lines {
                    ui.label(RichText::new(l).monospace().small().color(p.text));
                }
            });
        });
    }

    // ---- screenshots (for the README and for checking the UI) --------------------

    fn screenshot_tick(&mut self, ctx: &egui::Context) {
        let Some(path) = self.opts.screenshot.clone() else { return };
        self.frames += 1;
        ctx.request_repaint();
        let waiting = matches!(*self.lookup.lock().unwrap_or_else(|e| e.into_inner()), Lookup::Loading);
        let wait_frames = 90 + (self.opts.wait * 60.0) as u32;
        if !self.shot_requested && self.frames > wait_frames && !waiting {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
            if let Some(buf) = image::RgbaImage::from_raw(img.width() as u32, img.height() as u32, bytes)
                && let Err(e) = buf.save(&path)
            {
                tracing::error!("could not save screenshot: {e}");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn device_card(ui: &mut egui::Ui, p: Palette, d: &Device, job: Option<&JobView>) -> Option<Action> {
    let mut action = None;
    let mode_color = match d.mode {
        DeviceMode::Normal => p.ok,
        DeviceMode::Recovery => p.warn,
        DeviceMode::Dfu => p.bad,
        DeviceMode::Reconnecting => p.info,
    };
    let tablet = d.product_type.as_deref().is_some_and(|t| t.starts_with("iPad"));
    let busy = job.is_some();
    theme::card(p)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                theme::device_icon(ui, tablet, mode_color);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(device_title(d)).size(16.0).strong());
                        theme::pill(ui, d.mode.label(), mode_color);
                        if d.mode == DeviceMode::Normal {
                            match d.pair_state {
                                PairState::Paired => {
                                    theme::pill(ui, d.activation_state.as_deref().unwrap_or("Trusted"), p.ok);
                                }
                                PairState::NotPaired => {
                                    theme::pill(ui, "Not trusted", p.warn);
                                }
                                PairState::Unknown => {
                                    theme::pill(ui, "Reading…", p.weak);
                                }
                            }
                        }
                        if let Some(b) = d.battery_percent {
                            theme::pill(ui, &format!("{b}%"), if b < 20 { p.bad } else if b < 40 { p.warn } else { p.weak });
                        }
                    });
                    let mut sub = Vec::new();
                    sub.extend(d.product_type.clone());
                    sub.extend(d.os_version.as_ref().map(|v| format!("iOS {v}")));
                    sub.extend(d.serial.clone());
                    if let Some(e) = d.ecid {
                        sub.push(format!("ECID {e:#x}"));
                    }
                    ui.label(RichText::new(sub.join("  ·  ")).small().color(p.weak));
                    if let Some(prob) = &d.problem {
                        ui.add(egui::Label::new(RichText::new(prob).small().color(p.warn)).wrap());
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let paired = d.pair_state == PairState::Paired;
                    let free = !busy;
                    match d.mode {
                        DeviceMode::Normal => {
                            if ui.add_enabled(free, egui::Button::new("Reset…")).on_hover_text("Erase and reinstall, guided").clicked() {
                                action = Some(Action::Reset);
                            }
                            if ui.add_enabled(free && paired, egui::Button::new("Recovery mode")).clicked() {
                                action = Some(Action::EnterRecovery);
                            }
                            if ui.add_enabled(free && paired, theme::primary_button(p, "Back up")).on_hover_text("Full backup into the backup folder").clicked() {
                                action = Some(Action::Backup);
                            }
                            if d.pair_state == PairState::NotPaired && ui.add_enabled(free, theme::primary_button(p, "Trust")).on_hover_text("Unlock the device and tap Trust when asked").clicked() {
                                action = Some(Action::Pair);
                            }
                        }
                        DeviceMode::Recovery | DeviceMode::Dfu => {
                            if ui.add_enabled(free, theme::primary_button(p, "Reset…")).on_hover_text("Erase and reinstall, guided").clicked() {
                                action = Some(Action::Reset);
                            }
                            if ui.add_enabled(free, egui::Button::new("Exit recovery")).on_hover_text("Restart into normal mode").clicked() {
                                action = Some(Action::ExitRecovery);
                            }
                        }
                        DeviceMode::Reconnecting => {
                            ui.label(RichText::new("Waiting for the device to come back…").small().color(p.weak));
                        }
                    }
                });
            });
            if let Some(j) = job {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 280.0).max(100.0);
                    theme::progress_bar(ui, p, j.progress.map(|x| x / 100.0), p.accent, w);
                    let mut t = vec![j.stage.clone().unwrap_or_else(|| j.state.as_str().to_string())];
                    if let Some(x) = j.progress {
                        t.insert(0, format!("{x:.0}%"));
                    }
                    ui.add(egui::Label::new(RichText::new(t.join(" · ")).small().color(p.weak)).truncate());
                });
            }
        });
    action
}

pub(crate) fn device_title(d: &Device) -> String {
    if d.name.is_some() || d.product_type.is_some() || d.serial.is_some() {
        d.label()
    } else {
        "Unknown device".to_string()
    }
}

pub(crate) fn job_state(p: Palette, j: &JobView) -> (Color32, &'static str) {
    if j.cancelling && j.state.is_active() {
        return (p.warn, "Cancelling");
    }
    match j.state {
        JobState::Succeeded => (p.ok, "Done"),
        JobState::Running => (p.info, "Running"),
        JobState::Queued => (p.info, "Queued"),
        JobState::Retrying => (p.warn, "Retrying"),
        JobState::Interrupted => (p.warn, "Interrupted"),
        JobState::Failed => (p.bad, "Failed"),
        JobState::Cancelled => (p.weak, "Cancelled"),
    }
}

fn health_chip(ui: &mut egui::Ui, p: Palette, name: &str, h: &Health) {
    let (color, tip) = match h {
        Health::Ok => (p.ok, "Working".to_string()),
        Health::Starting => (p.warn, "Starting".to_string()),
        Health::Down(why) => (p.bad, why.clone()),
    };
    theme::status_chip(ui, name, color).on_hover_text(tip);
}

pub(crate) fn fmt_size(b: u64) -> String {
    match b {
        0..=999_999 => format!("{:.0} KB", b as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.0} MB", b as f64 / 1e6),
        _ => format!("{:.2} GB", b as f64 / 1e9),
    }
}

fn fmt_secs(s: u64) -> String {
    if s >= 3600 {
        format!("{}h {}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

fn ago(t: u64) -> String {
    let s = now_secs().saturating_sub(t);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86400),
    }
}

impl eframe::App for FleetApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = theme::palette(ui);
        // A bug in drawing code must not take the window down with it. Jobs live
        // in the engine and keep running regardless.
        if let Some(msg) = self.crashed.clone() {
            let mut retry = false;
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Something went wrong in the window").size(20.0).strong());
                    ui.label(RichText::new("Your jobs are still running in the background. This is a display problem only.").color(p.weak));
                    ui.add_space(6.0);
                    ui.colored_label(p.bad, &msg);
                    ui.add_space(10.0);
                    retry = ui.add(theme::primary_button(p, "Try again")).clicked();
                    ui.label(RichText::new(format!("Details are in {}", self.fleet.paths.logs.join("app.log").display())).small().color(p.weak));
                });
            });
            if retry {
                self.crashed = None;
                self.revision = 0;
            }
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            return;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.draw(ui, p)));
        if let Err(payload) = result {
            let msg = payload.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| payload.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown error".into());
            tracing::error!("UI panic caught: {msg}");
            self.crashed = Some(msg);
        }
    }
}

impl FleetApp {
    fn draw(&mut self, ui: &mut egui::Ui, p: Palette) {
        self.refresh();
        egui::Panel::top("top").frame(egui::Frame::new().fill(p.card).inner_margin(egui::Margin::symmetric(14, 8)).stroke(Stroke::new(1.0, p.line))).show(ui, |ui| {
            self.header(ui, p);
        });
        egui::Panel::bottom("jobs")
            .resizable(true)
            .default_size(235.0)
            .min_size(150.0)
            .frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(16, 10)).stroke(Stroke::new(1.0, p.line)))
            .show(ui, |ui| {
                self.jobs_panel(ui, p);
            });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(egui::Margin::symmetric(16, 14))).show(ui, |ui| {
            self.update_banner(ui, p);
            if let Some(n) = &self.notice {
                if Instant::now() < n.until {
                    let c = if n.error { p.bad } else { p.ok };
                    egui::Frame::new().fill(c.gamma_multiply(0.15)).stroke(Stroke::new(1.0, c.gamma_multiply(0.6))).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&n.text).color(c).strong());
                    });
                } else {
                    self.notice = None;
                }
            }
            match self.tab {
                Tab::Devices => self.devices_tab(ui, p),
                Tab::Firmware => self.firmware_tab(ui, p),
                Tab::Backups => self.backups_tab(ui, p),
            }
        });
        if let Some(mut w) = self.wizard.take() {
            let devices = self.devices.clone();
            let jobs = self.jobs.clone();
            match w.show(ui.ctx(), &self.fleet, &devices, &jobs, p) {
                wizard::Outcome::Open => self.wizard = Some(w),
                wizard::Outcome::Close => self.revision = 0,
                wizard::Outcome::GoFirmware => {
                    self.tab = Tab::Firmware;
                    self.revision = 0;
                }
            }
        }
        self.screenshot_tick(ui.ctx());
        // Device and job changes arrive from other threads; poll cheaply.
        ui.ctx().request_repaint_after(Duration::from_millis(400));
    }
}

/// Shown instead of the main window when the app can't start.
pub struct StartupError(pub String);

impl eframe::App for StartupError {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let p = theme::palette(ui);
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(60.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("iDevice Fleet couldn't start").size(22.0).strong());
                ui.add_space(8.0);
                ui.colored_label(p.bad, &self.0);
            });
        });
    }
}
