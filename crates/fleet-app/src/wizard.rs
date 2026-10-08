//! The guided "Reset & reinstall" flow: pick devices, pick firmware, review the
//! checks, watch it run, see what to do next. Every step says in plain words what
//! is happening and what (if anything) the person has to do.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use eframe::egui::{self, Align, Layout, RichText, Sense, Stroke, vec2};
use fleet_core::{Check, Device, DeviceKey, DeviceMode, Engine, Fleet, JobId, JobState, JobView, LatestState, Level, PairState, RestoreTarget};

use crate::theme::{self, Palette};
use crate::ui::{device_title, fmt_size, job_state};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Devices,
    Firmware,
    Review,
    Progress,
    Done,
}

impl Step {
    fn index(self) -> usize {
        self as usize
    }
}

pub enum Outcome {
    Open,
    Close,
    /// Close and show the Firmware tab.
    GoFirmware,
}

#[derive(Clone, Copy, PartialEq)]
enum Hand {
    FaceId,
    HomeButton,
    Iphone7,
}

pub struct Wizard {
    pub step: Step,
    selected: BTreeSet<DeviceKey>,
    choice: HashMap<DeviceKey, String>,
    erase: bool,
    engine: Engine,
    confirm: String,
    started: Vec<(DeviceKey, Result<JobId, String>)>,
    manual: bool,
    hand: Hand,
    error: Option<String>,
}

impl Wizard {
    pub fn new(preselect: Option<DeviceKey>) -> Self {
        Self {
            step: Step::Devices,
            selected: preselect.into_iter().collect(),
            choice: HashMap::new(),
            erase: true,
            engine: Engine::Auto,
            confirm: String::new(),
            started: Vec::new(),
            manual: false,
            hand: Hand::FaceId,
            error: None,
        }
    }

    /// For screenshots: jump straight to a step with the first few devices chosen.
    pub fn demo(step: Step, devices: &[Device], fleet: &Arc<Fleet>) -> Self {
        let mut w = Self::new(None);
        let busy = fleet.engine.busy_devices();
        w.selected = devices.iter().filter(|d| d.ecid.is_some() && d.product_type.is_some() && !busy.contains(&d.key) && d.battery_percent.is_none_or(|b| b >= 20)).take(3).map(|d| d.key.clone()).collect();
        w.step = Step::Devices;
        if step != Step::Devices {
            w.enter_firmware(fleet, devices);
            w.step = Step::Firmware;
        }
        if matches!(step, Step::Review | Step::Progress | Step::Done) {
            w.step = Step::Review;
            w.confirm = "ERASE".into();
        }
        if matches!(step, Step::Progress | Step::Done) {
            w.start(fleet);
        }
        w
    }

    fn enter_firmware(&mut self, fleet: &Arc<Fleet>, devices: &[Device]) {
        let lib = fleet.library();
        for d in devices.iter().filter(|d| self.selected.contains(&d.key)) {
            if let Some(m) = &d.product_type {
                fleet.ensure_catalog(m);
            }
            if self.choice.contains_key(&d.key) {
                continue;
            }
            if let Some(f) = default_choice(fleet, d, &lib) {
                self.choice.insert(d.key.clone(), f);
            }
        }
    }

    fn start(&mut self, fleet: &Arc<Fleet>) {
        let targets: Vec<RestoreTarget> = self.selected.iter().filter_map(|k| self.choice.get(k).map(|f| RestoreTarget { key: k.clone(), ipsw_file: f.clone() })).collect();
        self.started = fleet.restore(&targets, self.erase, self.engine).into_iter().map(|(k, r)| (k, r.map_err(|e| e.message))).collect();
        if self.started.iter().any(|(_, r)| r.is_ok()) {
            self.step = Step::Progress;
        } else {
            self.error = self.started.iter().find_map(|(_, r)| r.clone().err());
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, fleet: &Arc<Fleet>, devices: &[Device], jobs: &[JobView], p: Palette) -> Outcome {
        let mut outcome = Outcome::Open;
        // Move to the result as soon as everything has stopped.
        if self.step == Step::Progress {
            let active = self.started.iter().filter_map(|(_, r)| r.as_ref().ok()).any(|id| jobs.iter().any(|j| &j.id == id && j.state.is_active()));
            if !active {
                self.step = Step::Done;
            }
        }
        let modal = egui::Modal::new(egui::Id::new("reset_wizard")).frame(theme::card(p).inner_margin(egui::Margin::same(20)).corner_radius(egui::CornerRadius::same(14))).show(ctx, |ui| {
            ui.set_width(800.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            self.steps_header(ui, p);
            ui.separator();
            egui::ScrollArea::vertical().id_salt("wizard_body").max_height(430.0).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_width(780.0);
                match self.step {
                    Step::Devices => self.step_devices(ui, p, devices),
                    Step::Firmware => {
                        if let Some(o) = self.step_firmware(ui, p, fleet, devices, jobs) {
                            outcome = o;
                        }
                    }
                    Step::Review => self.step_review(ui, p, fleet, devices),
                    Step::Progress => self.step_progress(ui, p, devices, jobs, fleet),
                    Step::Done => self.step_done(ui, p, fleet, devices, jobs),
                }
            });
            ui.separator();
            self.footer(ui, p, fleet, devices, jobs, &mut outcome);
        });
        if modal.should_close() && matches!(self.step, Step::Devices | Step::Firmware | Step::Review | Step::Done) {
            outcome = Outcome::Close;
        }
        outcome
    }

    fn steps_header(&self, ui: &mut egui::Ui, p: Palette) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Reset & reinstall").size(20.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for (i, name) in ["Devices", "Firmware", "Review", "Reset"].iter().enumerate().rev() {
                    let cur = self.step.index().min(3) == i && self.step != Step::Done;
                    let done = self.step.index() > i || self.step == Step::Done;
                    let color = if cur { p.accent } else if done { p.ok } else { p.weak };
                    ui.label(RichText::new(*name).color(color).strong());
                    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 10.0, if cur || done { color } else { color.gamma_multiply(0.25) });
                    if done {
                        let c = rect.center();
                        let st = Stroke::new(2.0, p.accent_text);
                        ui.painter().line_segment([c + vec2(-4.0, 0.0), c + vec2(-1.0, 3.5)], st);
                        ui.painter().line_segment([c + vec2(-1.0, 3.5), c + vec2(5.0, -3.5)], st);
                    } else {
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, (i + 1).to_string(), egui::FontId::proportional(12.0), if cur { p.accent_text } else { p.weak });
                    }
                    if i > 0 {
                        ui.add_space(6.0);
                    }
                }
            });
        });
    }

    // ---- step 1 --------------------------------------------------------------------------

    fn step_devices(&mut self, ui: &mut egui::Ui, p: Palette, devices: &[Device]) {
        ui.label(RichText::new("Which devices should be reset?").size(16.0).strong());
        ui.label(RichText::new("Each selected device is wiped and gets fresh firmware, like a new device. You'll see exactly what will happen before anything starts.").color(p.weak));
        if devices.is_empty() {
            theme::empty_state(ui, p, "No devices connected", "Connect an iPhone or iPad with a USB cable. Unlock it and tap Trust if asked.");
        }
        for d in devices {
            let eligible = d.ecid.is_some() && d.mode != DeviceMode::Reconnecting;
            let mut on = self.selected.contains(&d.key);
            theme::card(p).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    if ui.add_enabled(eligible, egui::Checkbox::without_text(&mut on)).changed() {
                        if on {
                            self.selected.insert(d.key.clone());
                        } else {
                            self.selected.remove(&d.key);
                        }
                    }
                    ui.label(RichText::new(device_title(d)).strong());
                    theme::pill(ui, d.mode.label(), match d.mode {
                        DeviceMode::Normal => p.ok,
                        DeviceMode::Recovery => p.warn,
                        DeviceMode::Dfu => p.bad,
                        DeviceMode::Reconnecting => p.info,
                    });
                    if let Some(m) = &d.product_type {
                        ui.label(RichText::new(m).small().color(p.weak));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !eligible {
                            ui.label(RichText::new(if d.mode == DeviceMode::Reconnecting { "busy" } else { "waiting for its ID…" }).small().color(p.weak));
                        } else if d.mode == DeviceMode::Normal && d.pair_state == PairState::NotPaired {
                            theme::pill(ui, "not trusted", p.warn).on_hover_text("It can still be reset, but you must put it into recovery mode by hand");
                        } else if let Some(b) = d.battery_percent {
                            theme::pill(ui, &format!("{b}%"), if b < 20 { p.bad } else if b < 40 { p.warn } else { p.weak });
                        }
                    });
                });
            });
        }
        ui.add_space(4.0);
        let label = if self.manual { "Hide: put a device into recovery mode by hand" } else { "A device isn't listed? Put it into recovery mode by hand" };
        if ui.link(label).clicked() {
            self.manual = !self.manual;
        }
        if self.manual {
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Put a device into recovery mode").strong());
                ui.label(RichText::new("Keep the USB cable connected the whole time. The device shows up in the list within a few seconds, and its screen shows a computer or cable picture.").color(p.weak));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.hand, Hand::FaceId, "iPad without Home button, iPhone 8 and newer");
                    ui.selectable_value(&mut self.hand, Hand::HomeButton, "iPad with Home button, iPhone 6s and older, SE (1st gen)");
                    ui.selectable_value(&mut self.hand, Hand::Iphone7, "iPhone 7");
                });
                ui.label(match self.hand {
                    Hand::FaceId => "1. Press and quickly release Volume Up.  2. Press and quickly release Volume Down.  3. Press and hold the Top (side) button until the recovery screen appears. Ignore the Apple logo, keep holding.",
                    Hand::HomeButton => "Press and hold the Home button and the Top (or Side) button together. Keep holding through the Apple logo until the recovery screen appears.",
                    Hand::Iphone7 => "Press and hold the Volume Down button and the Side button together. Keep holding through the Apple logo until the recovery screen appears.",
                });
            });
        }
    }

    // ---- step 2 --------------------------------------------------------------------------

    fn step_firmware(&mut self, ui: &mut egui::Ui, p: Palette, fleet: &Arc<Fleet>, devices: &[Device], jobs: &[JobView]) -> Option<Outcome> {
        let lib = fleet.library();
        ui.label(RichText::new("Which firmware should be installed?").size(16.0).strong());
        ui.label(RichText::new("Only firmware that Apple still signs can be installed. The newest signed version is preselected.").color(p.weak));
        let mut models_missing: Vec<String> = Vec::new();
        for d in devices.iter().filter(|d| self.selected.contains(&d.key)) {
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(device_title(d)).strong());
                    if let Some(m) = &d.product_type {
                        ui.label(RichText::new(m).small().color(p.weak));
                    }
                });
                let options: Vec<&fleet_core::firmware::LocalIpsw> = match &d.product_type {
                    Some(m) => fleet_core::firmware::matching(&lib, m),
                    None => lib.iter().filter(|l| l.error.is_none()).collect(),
                };
                if options.is_empty() {
                    let model = d.product_type.clone().unwrap_or_default();
                    ui.label(RichText::new(format!("There's no firmware for {} in the library yet.", if model.is_empty() { "this device" } else { &model })).color(p.warn));
                    if !model.is_empty() {
                        models_missing.push(model.clone());
                        match fleet.latest_state(&model) {
                            None | Some(LatestState::Failed(_)) => {
                                if let Some(LatestState::Failed(e)) = fleet.latest_state(&model) {
                                    ui.colored_label(p.bad, e);
                                }
                                if ui.add(theme::primary_button(p, "Download the newest signed firmware")).clicked() {
                                    fleet.download_latest_signed(&model);
                                }
                            }
                            Some(LatestState::Looking) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Finding the newest version Apple signs…");
                                });
                            }
                            Some(LatestState::Downloading(id)) => {
                                if let Some(j) = jobs.iter().find(|j| j.id == id) {
                                    ui.horizontal(|ui| {
                                        theme::progress_bar(ui, p, j.progress.map(|x| x / 100.0), p.accent, 360.0);
                                        ui.label(RichText::new(format!("{} · {}", j.progress.map(|x| format!("{x:.0}%")).unwrap_or_default(), j.stage.clone().unwrap_or_default())).small().color(p.weak));
                                    });
                                    if j.state == JobState::Failed {
                                        ui.colored_label(p.bad, j.error.as_ref().map(|e| e.message.clone()).unwrap_or_default());
                                    }
                                }
                                ui.label(RichText::new("Downloads are about 10 GB and resume if the connection drops. You can wait here.").small().color(p.weak));
                            }
                            Some(LatestState::Have(file)) => {
                                self.choice.insert(d.key.clone(), file);
                            }
                        }
                    }
                } else {
                    let cur = self.choice.get(&d.key).cloned().filter(|c| options.iter().any(|o| &o.file == c)).unwrap_or_else(|| options[0].file.clone());
                    self.choice.insert(d.key.clone(), cur.clone());
                    let label_of = |o: &fleet_core::firmware::LocalIpsw| {
                        let sig = match (&d.product_type, &o.build) {
                            (Some(m), Some(b)) => match fleet.is_signed(m, b) {
                                Some(true) => "signed",
                                Some(false) => "NOT signed",
                                None => "signing unknown",
                            },
                            _ => "",
                        };
                        format!("{} ({}) · {} · {}", o.version.as_deref().unwrap_or("?"), o.build.as_deref().unwrap_or("?"), fmt_size(o.size), sig)
                    };
                    let selected_text = options.iter().find(|o| o.file == cur).map(|o| label_of(o)).unwrap_or_default();
                    egui::ComboBox::from_id_salt(("fw", &d.key)).width(520.0).selected_text(selected_text).show_ui(ui, |ui| {
                        for o in &options {
                            if ui.selectable_label(o.file == cur, label_of(o)).clicked() {
                                self.choice.insert(d.key.clone(), o.file.clone());
                            }
                        }
                    });
                }
            });
        }
        // A model with several devices only needs one download; keep the list tidy.
        let _ = models_missing;
        if ui.link("Browse all firmware versions").clicked() {
            return Some(Outcome::GoFirmware);
        }
        None
    }

    // ---- step 3 --------------------------------------------------------------------------

    fn step_review(&mut self, ui: &mut egui::Ui, p: Palette, fleet: &Arc<Fleet>, devices: &[Device]) {
        let n = self.selected.len();
        ui.label(RichText::new("Review before anything is erased").size(16.0).strong());
        ui.horizontal(|ui| {
            ui.label("What should happen:");
            ui.selectable_value(&mut self.erase, true, "Reset (erase everything)");
            ui.selectable_value(&mut self.erase, false, "Update only (keep data)");
        });
        let external = fleet.external_engine_available();
        ui.horizontal(|ui| {
            ui.label("Installer:");
            ui.selectable_value(&mut self.engine, Engine::Auto, "Automatic (recommended)");
            ui.selectable_value(&mut self.engine, Engine::Native, "Built-in only");
            ui.add_enabled_ui(external, |ui| {
                ui.selectable_value(&mut self.engine, Engine::IdeviceRestore, "idevicerestore");
            });
            if !external {
                ui.label(RichText::new("idevicerestore isn't installed").small().color(p.weak)).on_hover_text("sudo apt install idevicerestore");
            }
        });
        let lib = fleet.library();
        let mut blocked = false;
        for d in devices.iter().filter(|d| self.selected.contains(&d.key)) {
            let file = self.choice.get(&d.key).cloned();
            let checks: Vec<Check> = fleet.check_restore(&d.key, file.as_deref(), self.erase, n);
            blocked |= fleet_core::restore::worst(&checks) == Level::Block;
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(device_title(d)).strong());
                    if let Some(f) = file.as_ref().and_then(|f| lib.iter().find(|l| &l.file == f)) {
                        theme::pill(ui, &format!("iOS {}", f.version.as_deref().unwrap_or("?")), p.accent);
                    }
                });
                for c in checks {
                    ui.horizontal_top(|ui| {
                        let (color, tag) = match c.level {
                            Level::Ok => (p.ok, "OK"),
                            Level::Warn => (p.warn, "Check"),
                            Level::Block => (p.bad, "Fix this"),
                        };
                        theme::pill(ui, tag, color);
                        ui.add(egui::Label::new(RichText::new(c.text).color(p.text)).wrap());
                    });
                }
            });
        }
        theme::card(p).fill(p.accent.gamma_multiply(0.10)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("While it runs").strong());
            ui.label(if self.erase {
                "Each device restarts several times and shows an Apple logo with a progress bar. A reset takes roughly 15 to 30 minutes. Don't unplug anything. If the data matters, cancel now and make a backup first."
            } else {
                "Each device restarts several times and shows an Apple logo with a progress bar. An update takes roughly 15 to 30 minutes and keeps the data. Don't unplug anything."
            });
        });
        if let Some(e) = &self.error {
            ui.colored_label(p.bad, e);
        }
        let word = if self.erase { "ERASE" } else { "UPDATE" };
        ui.horizontal(|ui| {
            ui.label(format!("Type {word} to confirm:"));
            ui.add(egui::TextEdit::singleline(&mut self.confirm).desired_width(120.0));
            if blocked {
                ui.colored_label(p.bad, "Fix the items marked \"Fix this\" first.");
            }
        });
    }

    // ---- step 4 --------------------------------------------------------------------------

    fn step_progress(&mut self, ui: &mut egui::Ui, p: Palette, devices: &[Device], jobs: &[JobView], fleet: &Arc<Fleet>) {
        ui.label(RichText::new("Resetting…").size(16.0).strong());
        ui.label(RichText::new("Keep every cable connected and the devices charging. It's normal for the screens to go black, show a computer picture, then an Apple logo with a progress bar.").color(p.weak));
        for (key, r) in &self.started {
            let name = devices.iter().find(|d| &d.key == key).map(device_title).unwrap_or_else(|| key.to_string());
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                match r {
                    Err(e) => {
                        ui.label(RichText::new(name).strong());
                        ui.colored_label(p.bad, e);
                    }
                    Ok(id) => {
                        let Some(j) = jobs.iter().find(|j| &j.id == id) else { return };
                        let (color, label) = job_state(p, j);
                        ui.horizontal(|ui| {
                            theme::pill(ui, label, color);
                            ui.label(RichText::new(name).strong());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if j.state.is_active() && ui.add_enabled(!j.cancelling, egui::Button::new(if j.cancelling { "Cancelling…" } else { "Cancel" }).small()).clicked() {
                                    fleet.engine.cancel(id);
                                }
                            });
                        });
                        ui.horizontal(|ui| {
                            theme::progress_bar(ui, p, j.progress.map(|x| x / 100.0), color, 460.0);
                            ui.label(RichText::new(j.progress.map(|x| format!("{x:.0}%")).unwrap_or_default()).small().color(p.weak));
                        });
                        let stage = j.stage.clone().unwrap_or_else(|| "Starting".into());
                        ui.label(RichText::new(&stage).small().color(p.weak));
                        if let Some(hint) = stage_hint(&stage) {
                            ui.label(RichText::new(hint).small().color(p.info));
                        }
                        if let (JobState::Retrying, Some(e)) = (j.state, &j.error) {
                            ui.colored_label(p.warn, format!("Had a problem, trying again: {}", e.message));
                        }
                    }
                }
            });
        }
    }

    // ---- step 5 --------------------------------------------------------------------------

    fn step_done(&mut self, ui: &mut egui::Ui, p: Palette, fleet: &Arc<Fleet>, devices: &[Device], jobs: &[JobView]) {
        let results: Vec<(&DeviceKey, Option<&JobView>, Option<&String>)> = self
            .started
            .iter()
            .map(|(k, r)| match r {
                Ok(id) => (k, jobs.iter().find(|j| &j.id == id), None),
                Err(e) => (k, None, Some(e)),
            })
            .collect();
        let ok = results.iter().filter(|(_, j, _)| j.is_some_and(|j| j.state == JobState::Succeeded)).count();
        ui.label(RichText::new(if ok == results.len() { "All done" } else { "Finished, with some problems" }).size(18.0).strong());
        ui.label(RichText::new(format!("{ok} of {} succeeded.", results.len())).color(p.weak));
        let mut retry: Option<(String, Option<Engine>)> = None;
        let mut exit: Option<DeviceKey> = None;
        for (key, job, err) in results {
            let dev = devices.iter().find(|d| &d.key == key);
            let name = dev.map(device_title).unwrap_or_else(|| key.to_string());
            theme::card(p).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    match job {
                        Some(j) => {
                            let (c, l) = job_state(p, j);
                            theme::pill(ui, l, c);
                        }
                        None => {
                            theme::pill(ui, "Not started", p.bad);
                        }
                    }
                    ui.label(RichText::new(name).strong());
                });
                match (job, err) {
                    (Some(j), _) if j.state == JobState::Succeeded => {
                        ui.label("The device restarts and shows the setup screen (\"Hello\"). Connect it to Wi-Fi to continue: a device that's in your Apple Business Manager enrolls itself in the new MDM during setup.");
                    }
                    (Some(j), _) => {
                        if let Some(e) = &j.error {
                            ui.colored_label(p.bad, &e.message);
                        }
                        ui.label(RichText::new(what_next(j)).color(p.text));
                        ui.horizontal(|ui| {
                            if j.state != JobState::Cancelled && ui.add(theme::primary_button(p, "Try again")).clicked() {
                                retry = Some((j.id.clone(), None));
                            }
                            if fleet.external_engine_available() && j.state == JobState::Failed && ui.button("Try with idevicerestore").clicked() {
                                retry = Some((j.id.clone(), Some(Engine::IdeviceRestore)));
                            }
                            if dev.is_some_and(|d| matches!(d.mode, DeviceMode::Recovery | DeviceMode::Dfu)) && ui.button("Restart it normally").on_hover_text("Exit recovery mode without restoring").clicked() {
                                exit = Some(key.clone());
                            }
                        });
                    }
                    (None, Some(e)) => {
                        ui.colored_label(p.bad, e);
                    }
                    _ => {}
                }
            });
        }
        if let Some((id, engine)) = retry {
            let r = match engine {
                Some(e) => fleet.retry_restore_with(&id, e),
                None => fleet.engine.run_again(&id),
            };
            match r {
                Ok(new) => {
                    for (_, slot) in self.started.iter_mut() {
                        if slot.as_ref().is_ok_and(|x| x == &id) {
                            *slot = Ok(new.clone());
                        }
                    }
                    self.step = Step::Progress;
                }
                Err(e) => self.error = Some(e.message),
            }
        }
        if let Some(k) = exit {
            let _ = fleet.exit_recovery(&k);
        }
        if let Some(e) = &self.error {
            ui.colored_label(p.bad, e);
        }
    }

    // ---- footer --------------------------------------------------------------------------

    fn footer(&mut self, ui: &mut egui::Ui, p: Palette, fleet: &Arc<Fleet>, devices: &[Device], jobs: &[JobView], outcome: &mut Outcome) {
        ui.horizontal(|ui| {
            match self.step {
                Step::Devices => {
                    if ui.button("Cancel").clicked() {
                        *outcome = Outcome::Close;
                    }
                }
                Step::Firmware | Step::Review => {
                    if ui.button("Back").clicked() {
                        self.error = None;
                        self.step = if self.step == Step::Firmware { Step::Devices } else { Step::Firmware };
                    }
                }
                Step::Progress => {
                    if ui.button("Hide this window").on_hover_text("The resets keep running. Progress stays in the Jobs list").clicked() {
                        *outcome = Outcome::Close;
                    }
                }
                Step::Done => {}
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.step {
                Step::Devices => {
                    let n = self.selected.len();
                    if ui.add_enabled(n > 0, theme::primary_button(p, "Next: choose firmware")).clicked() {
                        self.enter_firmware(fleet, devices);
                        self.step = Step::Firmware;
                    }
                    if n == 0 {
                        ui.label(RichText::new("Select at least one device").small().color(p.weak));
                    }
                }
                Step::Firmware => {
                    let downloading = self.selected.iter().any(|k| {
                        let model = devices.iter().find(|d| &d.key == k).and_then(|d| d.product_type.clone());
                        model.and_then(|m| fleet.latest_state(&m)).is_some_and(|s| matches!(s, LatestState::Downloading(id) if jobs.iter().any(|j| j.id == id && j.state.is_active())))
                    });
                    let ready = self.selected.iter().all(|k| self.choice.contains_key(k)) && !downloading;
                    if ui.add_enabled(ready, theme::primary_button(p, "Next: review")).clicked() {
                        self.confirm.clear();
                        self.step = Step::Review;
                    }
                    if !ready {
                        ui.label(RichText::new(if downloading { "Waiting for the download…" } else { "Choose firmware for every device" }).small().color(p.weak));
                    }
                }
                Step::Review => {
                    let word = if self.erase { "ERASE" } else { "UPDATE" };
                    let blocked = self.selected.iter().any(|k| fleet_core::restore::worst(&fleet.check_restore(k, self.choice.get(k).map(String::as_str), self.erase, self.selected.len())) == Level::Block);
                    let ok = !blocked && self.confirm.trim().eq_ignore_ascii_case(word);
                    let label = format!("{} {} device{}", if self.erase { "Reset" } else { "Update" }, self.selected.len(), if self.selected.len() == 1 { "" } else { "s" });
                    let btn = egui::Button::new(RichText::new(label).color(egui::Color32::WHITE).strong()).fill(p.bad);
                    if ui.add_enabled(ok, btn).clicked() {
                        self.error = None;
                        self.start(fleet);
                    }
                }
                Step::Progress => {}
                Step::Done => {
                    if ui.add(theme::primary_button(p, "Close")).clicked() {
                        *outcome = Outcome::Close;
                    }
                }
            });
        });
    }
}

/// Newest signed firmware for the device's model, falling back to the newest file.
fn default_choice(fleet: &Fleet, d: &Device, lib: &[fleet_core::firmware::LocalIpsw]) -> Option<String> {
    let model = d.product_type.as_deref()?;
    let all = fleet_core::firmware::matching(lib, model);
    all.iter().find(|o| o.build.as_deref().is_some_and(|b| fleet.is_signed(model, b) == Some(true))).or_else(|| all.first()).map(|o| o.file.clone())
}

/// A calm explanation of what the device is doing right now.
fn stage_hint(stage: &str) -> Option<&'static str> {
    let s = stage.to_lowercase();
    if s.contains("recovery") || s.contains("bootloader") {
        Some("The screen may go black or show a computer picture. That's expected.")
    } else if s.contains("restoring") || s.contains("flashing") || s.contains("system image") {
        Some("The device shows an Apple logo with a progress bar. Don't unplug it.")
    } else if s.contains("apple to approve") {
        Some("Apple checks that this firmware may be installed on this exact device.")
    } else {
        None
    }
}

/// What a person should do after a restore that didn't finish.
fn what_next(j: &JobView) -> String {
    let in_recovery = "The device is probably in recovery mode now, which is safe.";
    match (&j.error, j.state) {
        (_, JobState::Cancelled) => format!("{in_recovery} Start the reset again from the Devices tab to finish it, or use \"Restart it normally\" to leave it as it was."),
        (Some(e), _) if e.class == fleet_core::ErrorClass::NeedsUser => "Do what the message says, then try again.".into(),
        (Some(e), _) if e.message.to_lowercase().contains("no longer signs") => "Go back and choose a newer firmware version. Apple only installs versions it currently signs.".into(),
        (Some(e), _) if e.fallback_suggested => format!("{in_recovery} The built-in installer can't handle this firmware; try idevicerestore."),
        (Some(e), _) if e.class == fleet_core::ErrorClass::Transient => format!("{in_recovery} Check the cable (use one directly in the computer, not a hub) and try again."),
        _ => format!("{in_recovery} Try again. If it keeps failing, read the job's log for details."),
    }
}
