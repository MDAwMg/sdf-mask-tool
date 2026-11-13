//! 叠加层（遮罩）颜色选择器。
//!
//! 支持自由选择颜色，**包括 RGB / HSL / 十六进制**，
//! 而不是原来那个点击循环切换的绿/白/红方块。
//!
//! 这里自己画一套面板而不是用 `egui::color_picker`，原因有两个：
//! 1. egui 内置选择器只有 HSV 二维块 + 滑条，没有 HEX 输入框；
//! 2. 内置的数值输入走 `numeric_color_space` 全局风格，切换会影响别处。
//!
//! 面板结构（自上而下）：
//! - 顶部：色相条（横向渐变）
//! - 中部：饱和度 × 明度 二维块
//! - 下部：三个标签页 RGB / HSL / HEX，都是可编辑数字框
//!
//! 内部统一用 HSV 存（因为二维块就是 S×V），显示时按当前标签换算。

use eframe::egui::{self, Color32, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

/// 面板显示的数值空间
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorSpace {
    Rgb,
    Hsl,
    Hex,
}

impl ColorSpace {
    pub fn label(self) -> &'static str {
        match self {
            ColorSpace::Rgb => "RGB",
            ColorSpace::Hsl => "HSL",
            ColorSpace::Hex => "HEX",
        }
    }
}

/// 颜色选择器的持久状态（面板开合、当前标签页）
pub struct ColorPickerState {
    pub open: bool,
    pub space: ColorSpace,
    /// HEX 输入框的文本。单独存是因为输入到一半可能是非法串，
    /// 不能立刻打回色值，否则会吃掉正在输入的字符。
    pub hex_buf: String,
    /// 上一次同步 hex_buf 时的颜色。颜色从别处变了才重置输入框，
    /// 否则每敲一个字符都会被覆盖。
    pub hex_synced_from: (u8, u8, u8),
    /// 色相条/二维块的拖拽是否正在进行（避免每帧重建纹理）
    pub hsv: Hsv,
}

impl Default for ColorPickerState {
    fn default() -> Self {
        Self {
            open: false,
            space: ColorSpace::Hex,
            hex_buf: "#FFFFFF".to_string(),
            hex_synced_from: (255, 255, 255),
            hsv: Hsv::from_rgb([1.0, 1.0, 1.0]),
        }
    }
}

/// HSV（h/s/v 都是 0..1，h 是 0..360 的归一化）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl Hsv {
    pub fn from_rgb(rgb: [f32; 3]) -> Self {
        let (r, g, b) = (
            rgb[0].clamp(0.0, 1.0),
            rgb[1].clamp(0.0, 1.0),
            rgb[2].clamp(0.0, 1.0),
        );
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;

        let h = if d.abs() < 1e-6 {
            0.0
        } else if max == r {
            ((g - b) / d).rem_euclid(6.0) / 6.0
        } else if max == g {
            ((b - r) / d + 2.0) / 6.0
        } else {
            ((r - g) / d + 4.0) / 6.0
        };
        let s = if max.abs() < 1e-6 { 0.0 } else { d / max };
        Hsv {
            h: h.rem_euclid(1.0),
            s,
            v: max,
        }
    }

    pub fn to_rgb(self) -> [f32; 3] {
        let h6 = self.h.rem_euclid(1.0) * 6.0;
        let s = self.s.clamp(0.0, 1.0);
        let v = self.v.clamp(0.0, 1.0);
        let c = v * s;
        let x = c * (1.0 - ((h6 % 2.0) - 1.0).abs());
        let m = v - c;
        let (r, g, b) = match h6 as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        [r + m, g + m, b + m]
    }

    pub fn to_rgb8(self) -> (u8, u8, u8) {
        let c = self.to_rgb();
        (
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        )
    }

    pub fn to_color32(self) -> Color32 {
        let (r, g, b) = self.to_rgb8();
        Color32::from_rgb(r, g, b)
    }
}

/// RGB→HSL（返回 h 0..1、s 0..1、l 0..1）
pub fn rgb_to_hsl(rgb: [f32; 3]) -> (f32, f32, f32) {
    let (r, g, b) = (rgb[0], rgb[1], rgb[2]);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h.rem_euclid(1.0), s, l)
}

/// HSL→RGB（h 0..1、s 0..1、l 0..1）
pub fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    if s.abs() < 1e-6 {
        return [l, l, l];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let f = |t: f32| -> f32 {
        let t = t.rem_euclid(1.0);
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0)]
}

/// 解析 `#RRGGBB` / `RRGGBB` / `#RGB`，失败返回 None
pub fn parse_hex(s: &str) -> Option<[f32; 3]> {
    let t = s.trim().trim_start_matches('#');
    let v = match t.len() {
        6 => u32::from_str_radix(t, 16).ok()?,
        3 => {
            let mut out = 0u32;
            for ch in t.chars() {
                let d = ch.to_digit(16)?;
                out = (out << 4) | d;
                out = (out << 4) | d;
            }
            out
        }
        _ => return None,
    };
    let r = ((v >> 16) & 0xff) as f32 / 255.0;
    let g = ((v >> 8) & 0xff) as f32 / 255.0;
    let b = (v & 0xff) as f32 / 255.0;
    Some([r, g, b])
}

/// 格式化成 `#RRGGBB`（大写）
pub fn to_hex(rgb: [f32; 3]) -> String {
    format!(
        "#{:02X}{:02X}{:02X}",
        (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8
    )
}

/// 绘制颜色选择面板。返回 `true` 表示颜色被改动。
pub fn show(
    ui: &mut Ui,
    state: &mut ColorPickerState,
    color: &mut [f32; 3],
    alpha: &mut f32,
) -> bool {
    let mut changed = false;

    // 颜色从别处变了（比如换了预设），同步内部 HSV 和 HEX 输入框
    let (r8, g8, b8) = {
        let c = *color;
        (
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        )
    };
    if state.hex_synced_from != (r8, g8, b8) {
        state.hsv = Hsv::from_rgb(*color);
        state.hex_buf = to_hex(*color);
        state.hex_synced_from = (r8, g8, b8);
    }

    ui.label(RichText::new("叠加层颜色").strong());
    ui.add_space(2.0);

    // ---------- 色相条 ----------
    let w = 220.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 16.0), Sense::click_and_drag());
    paint_hue_bar(ui, rect);
    {
        let x = rect.min.x + state.hsv.h.clamp(0.0, 1.0) * rect.width();
        ui.painter().line_segment(
            [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
            Stroke::new(2.0, Color32::WHITE),
        );
        ui.painter().line_segment(
            [
                egui::pos2(x + 1.0, rect.min.y),
                egui::pos2(x + 1.0, rect.max.y),
            ],
            Stroke::new(1.0, Color32::BLACK),
        );
    }
    if resp.dragged() || resp.clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            let nh = ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
            if (nh - state.hsv.h).abs() > 1e-4 {
                state.hsv.h = nh;
                *color = state.hsv.to_rgb();
                changed = true;
            }
        }
    }

    ui.add_space(4.0);

    // ---------- 饱和度 × 明度 二维块 ----------
    let (rect2, resp2) = ui.allocate_exact_size(Vec2::new(w, 120.0), Sense::click_and_drag());
    paint_sv_square(ui, rect2, state.hsv.h);
    {
        let px = rect2.min.x + state.hsv.s.clamp(0.0, 1.0) * rect2.width();
        let py = rect2.max.y - state.hsv.v.clamp(0.0, 1.0) * rect2.height();
        let c = egui::pos2(px, py);
        ui.painter()
            .circle_stroke(c, 4.5, Stroke::new(1.5, Color32::WHITE));
        ui.painter()
            .circle_stroke(c, 5.5, Stroke::new(1.0, Color32::BLACK));
    }
    if resp2.dragged() || resp2.clicked() {
        if let Some(p) = resp2.interact_pointer_pos() {
            let ns = ((p.x - rect2.min.x) / rect2.width()).clamp(0.0, 1.0);
            let nv = ((rect2.max.y - p.y) / rect2.height()).clamp(0.0, 1.0);
            if (ns - state.hsv.s).abs() > 1e-4 || (nv - state.hsv.v).abs() > 1e-4 {
                state.hsv.s = ns;
                state.hsv.v = nv;
                *color = state.hsv.to_rgb();
                changed = true;
            }
        }
    }

    ui.add_space(6.0);

    // ---------- 数值输入：三个标签页 ----------
    ui.horizontal(|ui| {
        for sp in [ColorSpace::Hex, ColorSpace::Rgb, ColorSpace::Hsl] {
            if ui.selectable_label(state.space == sp, sp.label()).clicked() {
                state.space = sp;
                // 换标签页时把输入框刷成当前颜色
                state.hex_buf = to_hex(*color);
            }
        }
    });
    ui.add_space(3.0);

    match state.space {
        ColorSpace::Hex => {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.hex_buf)
                        .desired_width(100.0)
                        .hint_text("#RRGGBB"),
                );
                // 只在输入合法时才生效，非法串保留在框里以便继续编辑
                if let Some(c) = parse_hex(&state.hex_buf) {
                    let (nr, ng, nb) = (
                        (c[0] * 255.0).round() as u8,
                        (c[1] * 255.0).round() as u8,
                        (c[2] * 255.0).round() as u8,
                    );
                    if state.hex_synced_from != (nr, ng, nb) {
                        *color = c;
                        state.hsv = Hsv::from_rgb(c);
                        state.hex_synced_from = (nr, ng, nb);
                        changed = true;
                    }
                } else {
                    ui.colored_label(Color32::from_rgb(0xd0, 0x80, 0x60), "格式应为 #RRGGBB");
                }
                swatch(ui, *color);
            });
        }
        ColorSpace::Rgb => {
            let mut c8 = (
                (color[0] * 255.0).round() as u8,
                (color[1] * 255.0).round() as u8,
                (color[2] * 255.0).round() as u8,
            );
            ui.horizontal(|ui| {
                let mut any = false;
                for (label, v) in [("R", &mut c8.0), ("G", &mut c8.1), ("B", &mut c8.2)] {
                    ui.label(label);
                    any |= ui
                        .add_sized(
                            [44.0, 18.0],
                            egui::DragValue::new(v).range(0..=255).speed(0.6),
                        )
                        .changed();
                }
                if any {
                    *color = [
                        c8.0 as f32 / 255.0,
                        c8.1 as f32 / 255.0,
                        c8.2 as f32 / 255.0,
                    ];
                    state.hsv = Hsv::from_rgb(*color);
                    state.hex_synced_from = c8;
                    state.hex_buf = to_hex(*color);
                    changed = true;
                }
                swatch(ui, *color);
            });
        }
        ColorSpace::Hsl => {
            let (h0, s0, l0) = rgb_to_hsl(*color);
            let mut h_deg = h0 * 360.0;
            let mut s_pct = s0 * 100.0;
            let mut l_pct = l0 * 100.0;
            ui.horizontal(|ui| {
                let mut any = false;
                ui.label("H");
                any |= ui
                    .add_sized(
                        [52.0, 18.0],
                        egui::DragValue::new(&mut h_deg)
                            .range(0.0..=360.0)
                            .speed(1.0)
                            .suffix("°"),
                    )
                    .changed();
                ui.label("S");
                any |= ui
                    .add_sized(
                        [44.0, 18.0],
                        egui::DragValue::new(&mut s_pct)
                            .range(0.0..=100.0)
                            .speed(0.5),
                    )
                    .changed();
                ui.label("L");
                any |= ui
                    .add_sized(
                        [44.0, 18.0],
                        egui::DragValue::new(&mut l_pct)
                            .range(0.0..=100.0)
                            .speed(0.5),
                    )
                    .changed();
                if any {
                    let c = hsl_to_rgb(h_deg / 360.0, s_pct / 100.0, l_pct / 100.0);
                    *color = c;
                    state.hsv = Hsv::from_rgb(c);
                    state.hex_synced_from = (
                        (c[0] * 255.0).round() as u8,
                        (c[1] * 255.0).round() as u8,
                        (c[2] * 255.0).round() as u8,
                    );
                    state.hex_buf = to_hex(c);
                    changed = true;
                }
                swatch(ui, *color);
            });
        }
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("透明度");
        ui.add(
            egui::Slider::new(alpha, 0.05..=1.0)
                .show_value(true)
                .max_decimals(2),
        );
    });

    ui.add_space(4.0);
    ui.label(
        RichText::new("这是画布上的叠加显示色，不影响导出的贴图内容。")
            .size(10.0)
            .color(Color32::from_gray(0x80)),
    );

    changed
}

fn swatch(ui: &mut Ui, color: [f32; 3]) {
    let c = Color32::from_rgb(
        (color[0] * 255.0).round() as u8,
        (color[1] * 255.0).round() as u8,
        (color[2] * 255.0).round() as u8,
    );
    let (rect, _) = ui.allocate_exact_size(Vec2::new(28.0, 18.0), Sense::hover());
    ui.painter().rect_filled(rect, 4.0, c);
    ui.painter().rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(0x66)),
        StrokeKind::Outside,
    );
}

fn paint_hue_bar(ui: &Ui, rect: Rect) {
    // 12 段足够平滑，逐像素画会白烧几十次 draw call
    let n = 12;
    let p = ui.painter();
    for i in 0..n {
        let t0 = i as f32 / n as f32;
        let t1 = (i + 1) as f32 / n as f32;
        let c = Hsv {
            h: t0,
            s: 1.0,
            v: 1.0,
        }
        .to_color32();
        let r = Rect::from_min_max(
            egui::pos2(rect.min.x + t0 * rect.width(), rect.min.y),
            egui::pos2(rect.min.x + t1 * rect.width(), rect.max.y),
        );
        p.rect_filled(r, 0.0, c);
    }
    p.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(0x55)),
        StrokeKind::Outside,
    );
}

fn paint_sv_square(ui: &Ui, rect: Rect, h: f32) {
    let p = ui.painter();
    // 底色 = 该色相下的纯色
    let base = Hsv { h, s: 1.0, v: 1.0 }.to_color32();
    p.rect_filled(rect, 4.0, base);

    let tl = rect.left_top();
    let tr = rect.right_top();
    let bl = rect.left_bottom();
    let br = rect.right_bottom();

    // 从白色渐变到透明：左→右表达饱和度
    let white_a = Color32::from_rgba_unmultiplied(255, 255, 255, 255);
    let white_b = Color32::from_rgba_unmultiplied(255, 255, 255, 0);
    let mut m = egui::Mesh::default();
    m.vertices.push(egui::epaint::Vertex {
        pos: tl,
        uv: egui::epaint::WHITE_UV,
        color: white_a,
    });
    m.vertices.push(egui::epaint::Vertex {
        pos: tr,
        uv: egui::epaint::WHITE_UV,
        color: white_b,
    });
    m.vertices.push(egui::epaint::Vertex {
        pos: br,
        uv: egui::epaint::WHITE_UV,
        color: white_b,
    });
    m.vertices.push(egui::epaint::Vertex {
        pos: bl,
        uv: egui::epaint::WHITE_UV,
        color: white_a,
    });
    m.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    p.add(egui::Shape::mesh(m));

    // 纵向黑色渐变：上透明 → 下不透明，表达明度
    let mut m2 = egui::Mesh::default();
    let blk_a = Color32::from_rgba_unmultiplied(0, 0, 0, 0);
    let blk_b = Color32::from_rgba_unmultiplied(0, 0, 0, 255);
    m2.vertices.push(egui::epaint::Vertex {
        pos: tl,
        uv: egui::epaint::WHITE_UV,
        color: blk_a,
    });
    m2.vertices.push(egui::epaint::Vertex {
        pos: tr,
        uv: egui::epaint::WHITE_UV,
        color: blk_a,
    });
    m2.vertices.push(egui::epaint::Vertex {
        pos: br,
        uv: egui::epaint::WHITE_UV,
        color: blk_b,
    });
    m2.vertices.push(egui::epaint::Vertex {
        pos: bl,
        uv: egui::epaint::WHITE_UV,
        color: blk_b,
    });
    m2.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    p.add(egui::Shape::mesh(m2));

    p.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_gray(0x55)),
        StrokeKind::Outside,
    );
}

/// 触发颜色面板的按钮：一个色块，点了开面板。
pub fn color_button(ui: &mut Ui, color: [f32; 3], open: &mut bool) -> Response {
    let c = Color32::from_rgb(
        (color[0] * 255.0).round() as u8,
        (color[1] * 255.0).round() as u8,
        (color[2] * 255.0).round() as u8,
    );
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(30.0, 20.0), Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, c);
    p.rect_stroke(
        rect,
        4.0,
        Stroke::new(
            if resp.hovered() { 1.5 } else { 1.0 },
            if *open {
                Color32::from_rgb(0xe8, 0x87, 0x3a)
            } else if resp.hovered() {
                Color32::from_gray(0xb0)
            } else {
                Color32::from_gray(0x55)
            },
        ),
        StrokeKind::Outside,
    );
    if resp.clicked() {
        *open = !*open;
    }
    resp.on_hover_text("点击选择叠加层颜色")
}
