//! Design tokens and their mapping onto egui's visuals. Every colour the app draws comes from
//! [`Tokens`]; the light and dark sets are installed for both themes, so the window follows the
//! system appearance.
//!
//! Layout concept: a workbench. Quiet slate surfaces biased toward one cobalt accent, a centred
//! reading column, and tool activity printed like terminal log lines. Only the user's messages
//! sit on a card; everything else is text on the page.

use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme};
use fastframe_fonts::Weight;

#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    /// Window/panel ground.
    pub bg: Color32,
    /// Cards, inputs: one step up from the ground.
    pub surface: Color32,
    /// Windows, popups, modals.
    pub raised: Color32,
    /// Hairlines and borders.
    pub line: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub accent: Color32,
    /// Text on an accent fill.
    pub on_accent: Color32,
    pub code: Color32,
    // Semantic colours (state), separate from the accent.
    pub ok: Color32,
    pub warn: Color32,
    pub danger: Color32,
    /// The Magia agent's border gradient (a loop: the last colour blends back into the first).
    pub magic: [Color32; 4],
}

pub const DARK: Tokens = Tokens {
    bg: Color32::from_rgb(0x14, 0x17, 0x1c),
    surface: Color32::from_rgb(0x1c, 0x20, 0x27),
    raised: Color32::from_rgb(0x22, 0x27, 0x30),
    line: Color32::from_rgb(0x2e, 0x34, 0x40),
    fg: Color32::from_rgb(0xe4, 0xe7, 0xec),
    muted: Color32::from_rgb(0x8b, 0x93, 0xa1),
    accent: Color32::from_rgb(0x6e, 0x9b, 0xff),
    on_accent: Color32::from_rgb(0x0b, 0x12, 0x20),
    code: Color32::from_rgb(0x10, 0x13, 0x18),
    ok: Color32::from_rgb(0x4c, 0xc3, 0x8a),
    warn: Color32::from_rgb(0xe3, 0xad, 0x52),
    danger: Color32::from_rgb(0xf0, 0x76, 0x70),
    magic: [
        Color32::from_rgb(0xa7, 0x8b, 0xfa),
        Color32::from_rgb(0x6e, 0x9b, 0xff),
        Color32::from_rgb(0x5e, 0xe0, 0xd6),
        Color32::from_rgb(0xf4, 0x72, 0xb6),
    ],
};

pub const LIGHT: Tokens = Tokens {
    bg: Color32::from_rgb(0xf5, 0xf6, 0xf8),
    surface: Color32::from_rgb(0xff, 0xff, 0xff),
    raised: Color32::from_rgb(0xff, 0xff, 0xff),
    line: Color32::from_rgb(0xd9, 0xde, 0xe5),
    fg: Color32::from_rgb(0x1b, 0x1f, 0x26),
    muted: Color32::from_rgb(0x5d, 0x65, 0x72),
    accent: Color32::from_rgb(0x2f, 0x5f, 0xd0),
    on_accent: Color32::from_rgb(0xff, 0xff, 0xff),
    code: Color32::from_rgb(0xee, 0xf1, 0xf5),
    ok: Color32::from_rgb(0x17, 0x78, 0x4d),
    warn: Color32::from_rgb(0x93, 0x5c, 0x00),
    danger: Color32::from_rgb(0xc2, 0x3b, 0x34),
    magic: [
        Color32::from_rgb(0x7c, 0x3a, 0xed),
        Color32::from_rgb(0x2f, 0x5f, 0xd0),
        Color32::from_rgb(0x0e, 0x9f, 0x97),
        Color32::from_rgb(0xdb, 0x27, 0x77),
    ],
};

/// Type scale (points).
pub const SMALL: f32 = 12.0;
pub const BODY: f32 = 14.5;
pub const MONO: f32 = 13.0;
pub const HEADING: f32 = 18.0;

/// Width of the reading column.
pub const COLUMN: f32 = 780.0;

pub fn tokens(ui: &egui::Ui) -> &'static Tokens {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

pub fn semibold(size: f32) -> FontId {
    Weight::SemiBold.font_id(size)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// `a` blended toward `b` by `t` (0..1), opaque.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

/// Install both token sets (egui picks by the system theme) and the spacing/type scale.
pub fn install(ctx: &egui::Context) {
    for (theme, t) in [(Theme::Dark, &DARK), (Theme::Light, &LIGHT)] {
        ctx.style_mut_of(theme, |s| style(s, t));
    }
}

fn style(s: &mut egui::Style, t: &Tokens) {
    s.text_styles = [
        (TextStyle::Small, FontId::proportional(SMALL)),
        (TextStyle::Body, FontId::proportional(BODY)),
        (TextStyle::Button, FontId::proportional(BODY - 0.5)),
        (TextStyle::Monospace, mono(MONO)),
        (TextStyle::Heading, semibold(HEADING)),
    ]
    .into();

    let sp = &mut s.spacing;
    sp.item_spacing = egui::vec2(8.0, 6.0);
    sp.button_padding = egui::vec2(10.0, 5.0);
    sp.interact_size.y = 26.0;
    sp.window_margin = Margin::same(16);
    sp.menu_margin = Margin::same(6);
    sp.indent = 16.0;

    let v = &mut s.visuals;
    let radius = CornerRadius::same(6);
    v.panel_fill = t.bg;
    v.window_fill = t.raised;
    v.window_stroke = Stroke::new(1.0, t.line);
    v.window_corner_radius = CornerRadius::same(10);
    v.menu_corner_radius = CornerRadius::same(8);
    let shadow_alpha = if v.dark_mode { 110 } else { 28 };
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    v.faint_bg_color = t.surface;
    v.extreme_bg_color = t.surface;
    v.text_edit_bg_color = Some(t.surface);
    v.code_bg_color = t.code;
    v.hyperlink_color = t.accent;
    v.warn_fg_color = t.warn;
    v.error_fg_color = t.danger;
    v.override_text_color = None;
    v.weak_text_color = Some(t.muted);
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = true;
    v.selection.bg_fill = mix(t.surface, t.accent, if v.dark_mode { 0.35 } else { 0.22 });
    v.selection.stroke = Stroke::new(1.5, t.accent); // also the focus ring of text boxes

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.surface;
    w.noninteractive.weak_bg_fill = t.surface;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.line);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.fg);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = t.surface;
    w.inactive.weak_bg_fill = t.surface;
    w.inactive.bg_stroke = Stroke::new(1.0, t.line);
    w.inactive.fg_stroke = Stroke::new(1.0, t.fg);
    w.inactive.corner_radius = radius;

    let hover = mix(t.surface, t.accent, 0.12);
    w.hovered.bg_fill = hover;
    w.hovered.weak_bg_fill = hover;
    w.hovered.bg_stroke = Stroke::new(1.0, mix(t.line, t.accent, 0.6));
    w.hovered.fg_stroke = Stroke::new(1.0, t.fg);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;

    let press = mix(t.surface, t.accent, 0.22);
    w.active.bg_fill = press;
    w.active.weak_bg_fill = press;
    w.active.bg_stroke = Stroke::new(1.0, t.accent);
    w.active.fg_stroke = Stroke::new(1.0, t.fg);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;

    w.open = w.hovered;
}

/// The one filled button per view (Send, Allow, Answer…).
pub fn primary(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let t = tokens(ui);
    let label = egui::RichText::new(text)
        .font(semibold(BODY - 0.5))
        .color(t.on_accent);
    ui.add_enabled(
        enabled,
        egui::Button::new(label)
            .fill(t.accent)
            .stroke(Stroke::NONE)
            .min_size(egui::vec2(72.0, 28.0)),
    )
}

/// A quiet outlined button (Deny, Skip, Stop…), optionally tinted.
pub fn secondary(ui: &mut egui::Ui, text: &str, tint: Option<Color32>) -> egui::Response {
    let t = tokens(ui);
    let c = tint.unwrap_or(t.fg);
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(c))
            .fill(t.surface)
            .stroke(Stroke::new(
                1.0,
                tint.map_or(t.line, |c| mix(t.line, c, 0.6)),
            ))
            .min_size(egui::vec2(72.0, 28.0)),
    )
}

/// Small rounded label marking a state (permission mode, image, …).
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    let t = tokens(ui);
    egui::Frame::new()
        .fill(mix(t.surface, color, 0.14))
        .stroke(Stroke::new(1.0, mix(t.line, color, 0.45)))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).font(semibold(SMALL)).color(color))
        })
        .inner
}

/// Section label above a block ("You", "Assistant").
pub fn role(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.label(egui::RichText::new(text).font(semibold(SMALL)).color(color));
}

/// Short human form of a token count: 950, 12.3k, 1.2M.
pub fn count(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance contrast.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let lum = |c: Color32| {
            let f = |v: u8| {
                let v = v as f32 / 255.0;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        };
        let (x, y) = (lum(a) + 0.05, lum(b) + 0.05);
        x.max(y) / x.min(y)
    }

    #[test]
    fn text_is_readable_in_both_themes() {
        for t in [DARK, LIGHT] {
            for bg in [t.bg, t.surface, t.raised] {
                assert!(contrast(t.fg, bg) >= 7.0, "body text");
                assert!(contrast(t.muted, bg) >= 4.5, "muted text");
                for c in [t.accent, t.ok, t.warn, t.danger] {
                    assert!(contrast(c, bg) >= 4.5, "{c:?} on {bg:?}");
                }
            }
            assert!(contrast(t.on_accent, t.accent) >= 4.5, "button label");
            assert!(contrast(t.fg, t.code) >= 7.0, "code");
        }
    }

    #[test]
    fn counts() {
        assert_eq!(count(950), "950");
        assert_eq!(count(12_345), "12.3k");
        assert_eq!(count(1_200_000), "1.2M");
    }
}
