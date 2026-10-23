//! Visual design: a warm, light "paper" theme (default) and a matching dark
//! variant, plus the semantic colours the UI uses for statuses and tags.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme, Vec2,
};

/// Semantic colours, resolved for the active theme.
#[derive(Clone, Copy)]
pub struct Palette {
    pub accent: Color32,
    pub accent_soft: Color32,
    pub approved: Color32,
    pub generated: Color32,
    pub rejected: Color32,
    pub draft: Color32,
    pub warning: Color32,
    pub working: Color32,
    pub tag: Color32,
    pub tag_bg: Color32,
    pub muted: Color32,
    pub card: Color32,
    pub border: Color32,
    pub playing: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const LIGHT: Palette = Palette {
    accent: hex(0x2F6F73),
    accent_soft: hex(0xDCEBE8),
    approved: hex(0x2E8B57),
    generated: hex(0x3A6EA5),
    rejected: hex(0xC2452D),
    draft: hex(0xA29D93),
    warning: hex(0xB7791F),
    working: hex(0xC9932E),
    tag: hex(0x7E4E96),
    tag_bg: hex(0xF1E8F5),
    muted: hex(0x7A766E),
    card: hex(0xFFFFFF),
    border: hex(0xE4DFD5),
    playing: hex(0xFCEFC9),
};

pub const DARK: Palette = Palette {
    accent: hex(0x6FB7B3),
    accent_soft: hex(0x23393A),
    approved: hex(0x5CC08A),
    generated: hex(0x6E9FD6),
    rejected: hex(0xE0735C),
    draft: hex(0x8A867E),
    warning: hex(0xE0A84A),
    working: hex(0xE6BC5C),
    tag: hex(0xC79BDB),
    tag_bg: hex(0x3A2E40),
    muted: hex(0x9A968E),
    card: hex(0x24262A),
    border: hex(0x35383D),
    playing: hex(0x4A4128),
};

pub fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// Colour for a speaker badge, stable per key and readable on the theme.
pub fn speaker_color(key: &str, dark: bool) -> Color32 {
    if key == crate::model::NARRATOR {
        return if dark { hex(0xA8A49C) } else { hex(0x6B675F) };
    }
    let h = key
        .bytes()
        .fold(7u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    let (s, v) = if dark { (0.5, 0.85) } else { (0.65, 0.55) };
    egui::ecolor::Hsva::new((h % 360) as f32 / 360.0, s, v, 1.0).into()
}

fn light_visuals() -> egui::Visuals {
    let p = &LIGHT;
    let mut v = egui::Visuals::light();
    let ink = hex(0x2B2A27);
    v.panel_fill = hex(0xF7F5F0);
    v.window_fill = p.card;
    v.faint_bg_color = hex(0xF1EEE7);
    v.extreme_bg_color = p.card;
    v.text_edit_bg_color = Some(p.card);
    v.code_bg_color = hex(0xEFECE5);
    v.override_text_color = Some(ink);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.rejected;
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(28),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 12,
        spread: 0,
        color: Color32::from_black_alpha(24),
    };
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.card;
    w.noninteractive.weak_bg_fill = p.card;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    w.noninteractive.fg_stroke = Stroke::new(1.0, ink);
    w.inactive.weak_bg_fill = hex(0xEEEBE4);
    w.inactive.bg_fill = hex(0xE6E2D9);
    w.inactive.bg_stroke = Stroke::new(1.0, hex(0xE0DBD0));
    w.inactive.fg_stroke = Stroke::new(1.0, ink);
    w.hovered.weak_bg_fill = hex(0xE4EFEC);
    w.hovered.bg_fill = hex(0xE4EFEC);
    w.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    w.hovered.fg_stroke = Stroke::new(1.0, ink);
    w.active.weak_bg_fill = p.accent_soft;
    w.active.bg_fill = p.accent_soft;
    w.active.bg_stroke = Stroke::new(1.0, p.accent);
    w.active.fg_stroke = Stroke::new(1.0, ink);
    w.open.weak_bg_fill = p.accent_soft;
    for wv in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        wv.corner_radius = CornerRadius::same(6);
    }
    v
}

fn dark_visuals() -> egui::Visuals {
    let p = &DARK;
    let mut v = egui::Visuals::dark();
    v.panel_fill = hex(0x1C1D20);
    v.window_fill = p.card;
    v.extreme_bg_color = hex(0x17181A);
    v.selection.bg_fill = p.accent_soft;
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.window_corner_radius = CornerRadius::same(10);
    for wv in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        wv.corner_radius = CornerRadius::same(6);
    }
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    v
}

/// Install both themes and pick the active one.
pub fn apply(ctx: &egui::Context, dark: bool) {
    ctx.set_visuals_of(Theme::Light, light_visuals());
    ctx.set_visuals_of(Theme::Dark, dark_visuals());
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.window_margin = Margin::same(14);
        style.spacing.menu_margin = Margin::same(8);
        style.spacing.interact_size.y = 26.0;
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(21.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.5, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.5, FontFamily::Monospace),
            ),
        ]
        .into();
    });
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
}

/// A primary (accent-filled) button.
pub fn primary_button(ui: &egui::Ui, text: &str) -> egui::Button<'static> {
    let p = palette(ui);
    egui::Button::new(
        egui::RichText::new(text.to_string())
            .color(Color32::WHITE)
            .strong(),
    )
    .fill(p.accent)
    .stroke(Stroke::NONE)
}
