use eframe::egui::{self, Color32, FontId, Stroke};

pub const INK: Color32 = Color32::from_rgb(204, 204, 204);
pub const MUTED: Color32 = Color32::from_rgb(181, 181, 181);
pub const ACCENT: Color32 = Color32::from_rgb(0, 120, 212);
pub const ON_ACCENT: Color32 = Color32::WHITE;
pub const LINK: Color32 = Color32::from_rgb(77, 170, 252);
pub const LINE: Color32 = Color32::from_rgb(60, 60, 60);
pub const PAPER: Color32 = Color32::from_rgb(31, 31, 31);
pub const SIDEBAR: Color32 = Color32::from_rgb(24, 24, 24);
pub const INPUT: Color32 = Color32::from_rgb(49, 49, 49);
pub const PROMPT_SIZE: f32 = 20.0;
pub const PROMPT_CODE_SIZE: f32 = 18.0;
pub const PROMPT_HEADING_SIZE: f32 = 24.0;

pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    let mut fonts = egui::FontDefinitions::default();
    let fallback = fonts.families[&egui::FontFamily::Proportional].clone();
    fonts
        .families
        .insert(egui::FontFamily::Name("strong".into()), fallback);
    for (name, path, family) in [
        (
            "system-ui",
            "/System/Library/Fonts/SFNS.ttf",
            egui::FontFamily::Proportional,
        ),
        (
            "system-code",
            "/System/Library/Fonts/Menlo.ttc",
            egui::FontFamily::Monospace,
        ),
        (
            "system-bold",
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            egui::FontFamily::Name("strong".into()),
        ),
    ] {
        if let Ok(data) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.into(), egui::FontData::from_owned(data).into());
            fonts
                .families
                .get_mut(&family)
                .unwrap()
                .insert(0, name.into());
        }
    }
    ctx.set_fonts(fonts);
    let mut style = egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    };
    style.visuals.override_text_color = None;
    style.visuals.panel_fill = PAPER;
    style.visuals.window_fill = PAPER;
    style.visuals.extreme_bg_color = INPUT;
    style.visuals.hyperlink_color = LINK;
    style.visuals.selection.bg_fill = ACCENT;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, ON_ACCENT);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    style.visuals.widgets.inactive.weak_bg_fill = INPUT;
    style.visuals.widgets.inactive.bg_fill = INPUT;
    style.visuals.widgets.hovered.weak_bg_fill = LINE;
    style.visuals.widgets.hovered.bg_fill = LINE;
    style.visuals.widgets.active.weak_bg_fill = ACCENT;
    style.visuals.widgets.active.bg_fill = ACCENT;
    style.visuals.widgets.noninteractive.fg_stroke.color = INK;
    style.visuals.widgets.inactive.fg_stroke.color = INK;
    style.visuals.widgets.hovered.fg_stroke.color = ON_ACCENT;
    style.visuals.widgets.active.fg_stroke.color = ON_ACCENT;
    style.spacing.item_spacing = egui::vec2(8.0, 7.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    for font in style.text_styles.values_mut() {
        font.size += 5.0;
    }
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(19.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(18.0));
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f32 {
        let linear = |channel: u8| {
            let value = f32::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }

    #[test]
    fn controls_and_selection_keep_readable_foreground_background_pairs() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let style = ctx.style();
        let visuals = &style.visuals;
        let widgets = &visuals.widgets;
        let pairs = [
            (visuals.selection.stroke.color, visuals.selection.bg_fill),
            (widgets.inactive.fg_stroke.color, widgets.inactive.bg_fill),
            (widgets.hovered.fg_stroke.color, widgets.hovered.bg_fill),
            (widgets.active.fg_stroke.color, widgets.active.bg_fill),
            (ON_ACCENT, ACCENT),
            (INK, INPUT),
            (MUTED, PAPER),
        ];
        for (foreground, background) in pairs {
            let a = luminance(foreground);
            let b = luminance(background);
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(
                ratio >= 4.5,
                "Insufficient contrast: {foreground:?} on {background:?}: {ratio}"
            );
        }
        assert_eq!(visuals.selection.stroke.color, ON_ACCENT);
        assert_eq!(visuals.selection.bg_fill, ACCENT);
    }
}
