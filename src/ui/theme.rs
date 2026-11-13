use eframe::egui::{self, Color32, CornerRadius};

const BG: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
const PANEL: Color32 = Color32::from_rgb(0x26, 0x26, 0x26);
const WIDGET: Color32 = Color32::from_rgb(0x33, 0x33, 0x33);
const HOVER: Color32 = Color32::from_rgb(0x44, 0x44, 0x44);
const ACCENT: Color32 = Color32::from_rgb(0xe8, 0x87, 0x3a);
const TEXT: Color32 = Color32::from_rgb(0xd8, 0xd8, 0xd8);
const LINE: Color32 = Color32::from_rgb(0x3d, 0x3d, 0x3d);

pub fn install(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);

    let r = CornerRadius::same(6);
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        v.dark_mode = true;
        v.panel_fill = PANEL;
        v.window_fill = BG;
        v.faint_bg_color = BG;
        v.extreme_bg_color = BG;
        v.override_text_color = Some(TEXT);
        v.selection.bg_fill = ACCENT.gamma_multiply(0.45);
        v.selection.stroke.color = TEXT;

        v.widgets.noninteractive.corner_radius = r;
        v.widgets.inactive.corner_radius = r;
        v.widgets.hovered.corner_radius = r;
        v.widgets.active.corner_radius = r;
        v.widgets.open.corner_radius = r;

        v.widgets.noninteractive.bg_fill = PANEL;
        v.widgets.noninteractive.bg_stroke.color = LINE;
        v.widgets.inactive.bg_fill = WIDGET;
        v.widgets.inactive.bg_stroke.color = LINE;
        v.widgets.hovered.bg_fill = HOVER;
        v.widgets.active.bg_fill = ACCENT;
    });
}
