//! Look and feel: palette, spacing, and a few small widgets (pills, progress
//! bars, device icons) painted by hand so they look the same everywhere.

use eframe::egui::{self, Color32, CornerRadius, FontId, Margin, Rect, RichText, Sense, Stroke, StrokeKind, TextStyle, Theme, vec2};

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub line: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub bad: Color32,
    pub info: Color32,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: Color32::from_rgb(0xF2, 0xF5, 0xF6),
    card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    card_hover: Color32::from_rgb(0xF7, 0xFA, 0xFA),
    line: Color32::from_rgb(0xDB, 0xE3, 0xE6),
    text: Color32::from_rgb(0x15, 0x21, 0x2A),
    weak: Color32::from_rgb(0x5E, 0x6E, 0x79),
    accent: Color32::from_rgb(0x0B, 0x7A, 0x84),
    accent_text: Color32::WHITE,
    ok: Color32::from_rgb(0x1E, 0x8A, 0x4C),
    warn: Color32::from_rgb(0xB4, 0x72, 0x00),
    bad: Color32::from_rgb(0xC6, 0x3B, 0x30),
    info: Color32::from_rgb(0x2A, 0x6F, 0xC4),
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: Color32::from_rgb(0x0E, 0x14, 0x18),
    card: Color32::from_rgb(0x17, 0x21, 0x27),
    card_hover: Color32::from_rgb(0x1C, 0x28, 0x2F),
    line: Color32::from_rgb(0x2A, 0x3A, 0x44),
    text: Color32::from_rgb(0xE4, 0xEB, 0xEF),
    weak: Color32::from_rgb(0x8F, 0xA1, 0xAC),
    accent: Color32::from_rgb(0x4F, 0xB3, 0xBD),
    accent_text: Color32::from_rgb(0x05, 0x24, 0x28),
    ok: Color32::from_rgb(0x5C, 0xCF, 0x8A),
    warn: Color32::from_rgb(0xE7, 0xB0, 0x4F),
    bad: Color32::from_rgb(0xF0, 0x8B, 0x83),
    info: Color32::from_rgb(0x8D, 0xB1, 0xF0),
};

pub fn palette(ui: &egui::Ui) -> Palette {
    if ui.visuals().dark_mode { DARK } else { LIGHT }
}

fn visuals(p: Palette) -> egui::Visuals {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = if p.dark { Color32::from_rgb(0x0A, 0x0F, 0x12) } else { Color32::from_rgb(0xEA, 0xEF, 0xF1) };
    v.faint_bg_color = p.card_hover;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.30);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    let r = CornerRadius::same(7);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.bg_fill = if p.dark { Color32::from_rgb(0x21, 0x2E, 0x36) } else { Color32::from_rgb(0xE9, 0xEF, 0xF1) };
    v.widgets.inactive.weak_bg_fill = v.widgets.inactive.bg_fill;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.line);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.bg_fill = if p.dark { Color32::from_rgb(0x2A, 0x3A, 0x44) } else { Color32::from_rgb(0xDD, 0xE7, 0xEA) };
    v.widgets.hovered.weak_bg_fill = v.widgets.hovered.bg_fill;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.active.bg_fill = p.accent.gamma_multiply(0.35);
    v.widgets.active.weak_bg_fill = v.widgets.active.bg_fill;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.accent);
    v
}

pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 8.0);
        s.spacing.button_padding = vec2(11.0, 5.0);
        s.spacing.interact_size.y = 26.0;
        s.spacing.scroll.bar_width = 8.0;
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(21.0));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(13.5));
        s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
        s.text_styles.insert(TextStyle::Monospace, FontId::monospace(12.5));
    });
    ctx.style_mut_of(Theme::Light, |s| s.visuals = visuals(LIGHT));
    ctx.style_mut_of(Theme::Dark, |s| s.visuals = visuals(DARK));
}

/// A rounded surface that groups related content.
pub fn card(p: Palette) -> egui::Frame {
    egui::Frame::new().fill(p.card).stroke(Stroke::new(1.0, p.line)).corner_radius(CornerRadius::same(10)).inner_margin(Margin::same(12))
}

pub fn primary_button(p: Palette, text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(p.accent_text).strong()).fill(p.accent).stroke(Stroke::NONE)
}

/// Small rounded label with a tinted background.
pub fn pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let font = FontId::proportional(12.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let size = galley.size() + vec2(16.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(255), color.gamma_multiply(if ui.visuals().dark_mode { 0.22 } else { 0.14 }));
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, color);
    resp
}

/// Pill with a status dot (drawn, since the default font has no dot glyph).
pub fn status_chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::proportional(12.0), color);
    let size = galley.size() + vec2(28.0, 6.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(255), color.gamma_multiply(if ui.visuals().dark_mode { 0.22 } else { 0.14 }));
    ui.painter().circle_filled(egui::pos2(rect.left() + 11.0, rect.center().y), 3.2, color);
    ui.painter().galley(egui::pos2(rect.left() + 19.0, rect.center().y - galley.size().y / 2.0), galley, color);
    resp
}

/// A slim progress bar. `None` shows a moving segment (work of unknown length).
pub fn progress_bar(ui: &mut egui::Ui, p: Palette, fraction: Option<f32>, color: Color32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 8.0), Sense::hover());
    let track = if p.dark { Color32::from_rgb(0x26, 0x34, 0x3C) } else { Color32::from_rgb(0xE0, 0xE7, 0xEA) };
    ui.painter().rect_filled(rect, CornerRadius::same(4), track);
    match fraction {
        Some(f) => {
            let w = (rect.width() * f.clamp(0.0, 1.0)).max(if f > 0.0 { 8.0 } else { 0.0 });
            if w > 0.0 {
                ui.painter().rect_filled(Rect::from_min_size(rect.min, vec2(w, rect.height())), CornerRadius::same(4), color);
            }
        }
        None => {
            let t = ui.input(|i| i.time) as f32;
            let seg = rect.width() * 0.3;
            let x = rect.left() + ((t * 0.9).fract() * (rect.width() + seg)) - seg;
            let a = x.max(rect.left());
            let b = (x + seg).min(rect.right());
            if b > a {
                ui.painter().rect_filled(Rect::from_min_max(egui::pos2(a, rect.top()), egui::pos2(b, rect.bottom())), CornerRadius::same(4), color);
            }
            ui.ctx().request_repaint();
        }
    }
}

/// Phone or tablet outline, tinted by state.
pub fn device_icon(ui: &mut egui::Ui, tablet: bool, color: Color32) {
    let size = if tablet { vec2(40.0, 48.0) } else { vec2(26.0, 48.0) };
    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 48.0), Sense::hover());
    let body = Rect::from_center_size(rect.center(), size);
    let p = ui.painter();
    p.rect_filled(body, CornerRadius::same(if tablet { 6 } else { 7 }), color.gamma_multiply(0.14));
    p.rect_stroke(body, CornerRadius::same(if tablet { 6 } else { 7 }), Stroke::new(2.0, color), StrokeKind::Inside);
    if tablet {
        p.circle_filled(egui::pos2(body.center().x, body.bottom() - 4.5), 1.6, color);
    } else {
        p.rect_filled(Rect::from_center_size(egui::pos2(body.center().x, body.top() + 5.0), vec2(8.0, 2.5)), CornerRadius::same(2), color);
    }
}

pub fn tab_button(ui: &mut egui::Ui, p: Palette, selected: bool, label: &str) -> egui::Response {
    let font = FontId::proportional(14.5);
    let color = if selected { p.text } else { p.weak };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, color);
    let size = vec2(galley.size().x + 24.0, 34.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let c = if selected || resp.hovered() { p.text } else { p.weak };
    ui.painter().galley(rect.center() - galley.size() / 2.0 - vec2(0.0, 1.0), galley, c);
    if selected {
        ui.painter().rect_filled(Rect::from_min_size(egui::pos2(rect.left() + 8.0, rect.bottom() - 3.0), vec2(rect.width() - 16.0, 3.0)), CornerRadius::same(2), p.accent);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Large, quiet message for empty lists.
pub fn empty_state(ui: &mut egui::Ui, p: Palette, title: &str, hint: &str) {
    ui.add_space(36.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
        ui.painter().circle_stroke(rect.center(), 24.0, Stroke::new(2.0, p.line));
        ui.painter().circle_filled(rect.center(), 5.0, p.line);
        ui.add_space(6.0);
        ui.label(RichText::new(title).size(17.0).strong());
        ui.label(RichText::new(hint).color(p.weak));
    });
}
