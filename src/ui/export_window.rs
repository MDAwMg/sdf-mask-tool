//! 导出窗口。
//!
//! 布局自上而下：
//! 1. **预览区**（两张图并排：SDF 本体 + 该角度还原遮罩）
//! 2. **选项区**（位深、反向、角度、通道）
//! 3. **路径与按钮**（内嵌目录/文件名输入框 + 导出按钮）
//!
//! 路径直接在窗口里填，**不弹系统文件对话框**。
//! 预览和实际导出走同一条采样路径，所见即所得。

use eframe::egui::{
    self, Color32, ColorImage, Rect, RichText, TextureHandle, TextureOptions, Ui, Vec2,
};

use sdf_mask_tool::io::{Baked, BitDepth, ExportOptions, bake_channel};
use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::ChannelMode;
use sdf_mask_tool::model::Doc;
use sdf_mask_tool::model::nesting::{self, NestDir, Violations};
use sdf_mask_tool::sdf;

const LABEL_GRAY: Color32 = Color32::from_gray(0x99);
const SECTION: Color32 = Color32::from_rgb(0xb8, 0xb8, 0xb8);

pub struct ExportWindow {
    pub open: bool,
    pub opts: ExportOptions,
    pub angle: f32,
    pub channel: usize,
    /// 导出目录（可直接编辑，不弹系统框）
    pub dir_input: String,
    /// 文件名（不含扩展名）
    pub name_input: String,
    /// 批量导出时每个图层用各自的图层名，此处只作为前缀/目录
    pub export_all_layers: bool,

    baked: Option<Baked>,
    baked_key: Option<u64>,
    tex: Option<TextureHandle>,
    tex_key: Option<(u64, i32, i32)>,
    tex_sdf: Option<TextureHandle>,
    tex_sdf_key: Option<(u64, i32)>,
    /// 违规检测结果（缓存，避免每帧重算）
    violations: Option<Violations>,
    violations_key: Option<u64>,
    pub message: String,
    message_ok: bool,
    /// 请求外部执行"一键修复"（由 App 处理，因为修复要进撤销栈）
    pub repair_requested: Option<NestDir>,
    /// 修复建议的方向（检测时算出来的）
    repair_dir_hint: Option<NestDir>,
}

impl Default for ExportWindow {
    fn default() -> Self {
        Self {
            open: false,
            opts: ExportOptions::default(),
            angle: 90.0,
            channel: 0,
            dir_input: String::new(),
            name_input: String::new(),
            export_all_layers: false,
            baked: None,
            baked_key: None,
            tex: None,
            tex_key: None,
            tex_sdf: None,
            tex_sdf_key: None,
            violations: None,
            violations_key: None,
            message: String::new(),
            message_ok: false,
            repair_requested: None,
            repair_dir_hint: None,
        }
    }
}

impl ExportWindow {
    pub fn open_window(&mut self, doc: &Doc) {
        self.open = true;
        self.message.clear();
        // 首次打开时给一个合理默认：桌面/导出
        if self.dir_input.trim().is_empty() {
            self.dir_input = default_export_dir();
        }
        if self.name_input.trim().is_empty() {
            self.name_input = doc
                .layers
                .get(doc.active_layer)
                .map(|l| sanitize(&l.name))
                .unwrap_or_else(|| "sdf_mask".to_string());
        }
    }

    pub fn invalidate(&mut self) {
        self.baked = None;
        self.baked_key = None;
        self.tex = None;
        self.tex_key = None;
        self.tex_sdf = None;
        self.tex_sdf_key = None;
        self.violations = None;
        self.violations_key = None;
    }

    /// 当前检测到的违规。
    ///
    /// 画布的红标记和画笔栏按钮现在用 App 层自己那份检测结果，
    /// 这个方法保留给导出窗口内部检查用。
    #[allow(dead_code)]
    pub fn violations(&self) -> Option<&Violations> {
        self.violations.as_ref()
    }
}

fn default_export_dir() -> String {
    // 优先桌面目录，退回到当前目录
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let p = std::path::PathBuf::from(home).join("Desktop");
        if p.is_dir() {
            return p.display().to_string();
        }
    }
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    if cleaned.trim().is_empty() {
        "sdf_mask".to_string()
    } else {
        cleaned
    }
}

fn channel_fingerprint(ch: &sdf_mask_tool::model::Channel) -> u64 {
    let mut acc: u64 = ch.frames.len() as u64;
    for f in ch.frames.iter() {
        acc = acc.wrapping_mul(1099511628211) ^ f.mask.fingerprint();
    }
    acc
}

pub fn show(ui: &mut Ui, win: &mut ExportWindow, doc: &Doc) {
    if !win.open {
        return;
    }
    // 嵌套检测已经移到 App 层（画笔栏的「自动纠错」用的就是这份结果）。
    // 这里如果发现检测结果没了，就地补算一次，避免窗口独立使用时没有数据。
    win.ensure_violations(doc);

    let mut open = win.open;
    egui::Window::new("导出")
        .collapsible(false)
        .resizable(true)
        .default_size([760.0, 700.0])
        .min_size([560.0, 480.0])
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            // ========== 1. 预览（上方）==========
            let shown = draw_preview(ui, win, doc);

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(4.0);

            // ========== 2. 选项（下方）==========
            draw_options(ui, win, doc, shown);

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(4.0);

            // ========== 3. 路径与按钮 ==========
            draw_path_and_buttons(ui, win, doc, shown);
        });
    win.open = open;
}

impl ExportWindow {
    /// 确保嵌套检测结果是最新的（导出窗口自用，App 也可读）。
    pub fn ensure_violations(&mut self, doc: &Doc) {
        let Some(layer) = doc.layers.get(doc.active_layer) else {
            return;
        };
        let ci = if layer.channel_mode == ChannelMode::Rgba {
            self.channel.min(3)
        } else {
            0
        };
        let ch = &layer.channels[ci];
        if ch.frames.len() < 2 {
            self.violations = None;
            self.violations_key = None;
            return;
        }
        let key = channel_fingerprint(ch);
        if self.violations_key == Some(key) && self.violations.is_some() {
            return;
        }
        let refs: Vec<&Mask> = ch.frames.iter().map(|f| &f.mask).collect();
        let plan = nesting::plan(&refs, doc.width as usize, doc.height as usize);
        self.violations = Some(plan.violations);
        self.violations_key = Some(key);
        self.repair_dir_hint = Some(plan.dir);
    }
}

/// 有违规时显示的面板：说明问题、给出一键修复。
fn draw_violation_panel(
    ui: &mut Ui,
    win: &mut ExportWindow,
    doc: &Doc,
    v: &Violations,
    ch: &sdf_mask_tool::model::Channel,
) {
    let dir = win.repair_dir_hint.unwrap_or(NestDir::Grows);

    ui.label(
        RichText::new("无法导出：这个通道的遮罩没有层层嵌套")
            .strong()
            .color(Color32::from_rgb(0xe0, 0x70, 0x50)),
    );
    ui.add_space(4.0);
    ui.colored_label(
        Color32::from_rgb(0xc0, 0x90, 0x70),
        format!(
            "有 {} 个像素（{:.1}%）不符合「{}」的单调变化。\n\
             简单说：这些区域在某一帧出现了、但后面的帧里消失了（或反之），\n\
             形状前后不包含，单通道 SDF 表达不了这种变化。",
            v.count,
            v.ratio() * 100.0,
            dir.label()
        ),
    );

    ui.add_space(6.0);

    // 用一张小图示意哪里有问题：红=违规，深灰=被遮，浅灰=受光
    let cell = (ui.available_size().x * 0.5).min(260.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(cell), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, Color32::from_rgb(0x18, 0x18, 0x18));
    // 采样绘制违规分布（每格若干像素取众数，避免 1M 次 draw call）
    let (w, h) = (doc.width as usize, doc.height as usize);
    let step = ((w.max(h) / 96).max(1)) as usize;
    let cols = w.div_ceil(step);
    let rows = h.div_ceil(step);
    let cw = rect.width() / cols as f32;
    let chh = rect.height() / rows as f32;
    for ry in 0..rows {
        for rx in 0..cols {
            let x = rx * step;
            let y = ry * step;
            let i = y * w + x;
            if i >= w * h {
                continue;
            }
            let color = if v.bad[i] {
                Color32::from_rgb(0xe6, 0x3c, 0x3c)
            } else if ch
                .frames
                .first()
                .map(|f| f.mask.data()[i] != 0)
                .unwrap_or(false)
            {
                Color32::from_rgb(0x2a, 0x2a, 0x2a)
            } else {
                Color32::from_rgb(0xc0, 0xc0, 0xc0)
            };
            let r = Rect::from_min_size(
                egui::pos2(rect.min.x + rx as f32 * cw, rect.min.y + ry as f32 * chh),
                Vec2::new(cw + 0.5, chh + 0.5),
            );
            p.rect_filled(r, 0.0, color);
        }
    }
    p.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, Color32::from_rgb(0x44, 0x44, 0x44)),
        egui::StrokeKind::Outside,
    );

    ui.add_space(4.0);
    ui.label(
        RichText::new("红 = 违规区域（画布上也会标红）")
            .size(11.0)
            .color(LABEL_GRAY),
    );

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        // 修复入口已经移到笔刷栏的「自动纠错」按钮。这里保留一个跳转提示，
        // 避免误判为"没救了"。仍然直接给按钮，因为光标就在导出窗口里。
        if ui
            .button(RichText::new(
                format!("一键修复为「{}」", dir.label()).to_string(),
            ))
            .on_hover_text(
                "把每一处改成层层包含：\n\
                 • 扩张：后面的点补上前面的点的遮罩，阴影范围只增不减\n\
                 • 收缩：后面的点挖掉前面的点没有的部分，阴影范围只减不增\n\
                 修复会改动你的绘制内容，可 Ctrl+Z 撤销\n\
                 （同样的按钮也在画笔栏上，叫「自动纠错」）",
            )
            .clicked()
        {
            win.repair_requested = Some(dir);
        }

        // 另选方向
        let other = match dir {
            NestDir::Grows => NestDir::Shrinks,
            NestDir::Shrinks => NestDir::Grows,
        };
        if ui
            .button(format!("或改用「{}」修复", other.label()))
            .on_hover_text("换另一个方向修复，改动量可能更大")
            .clicked()
        {
            win.repair_requested = Some(other);
        }
    });

    // 说清楚修复的代价：修复会真的改动绘制内容。
    //
    // 只描述"对遮罩做了什么"，不再补一句"所以遮罩会变大/变小"——
    // 那是同一件事的第二种说法，两种并列反而让人以为有区别。
    ui.add_space(6.0);
    ui.label(
        RichText::new(match dir {
            NestDir::Shrinks => "「收缩」会从后面的点里挖掉那些前面的点没有的部分。",
            NestDir::Grows => "「扩张」会把前面点的遮罩补进后面的点。",
        })
        .size(11.0)
        .color(Color32::from_rgb(0xb0, 0xa0, 0x70)),
    );
    ui.label(
        RichText::new("修复后这段区域的绘制结果会变，请先确认能接受再点。可 Ctrl+Z 撤销。")
            .size(11.0)
            .color(LABEL_GRAY),
    );
}
fn draw_preview(ui: &mut Ui, win: &mut ExportWindow, doc: &Doc) -> bool {
    let Some(layer) = doc.layers.get(doc.active_layer) else {
        ui.label("没有图层");
        return false;
    };

    // RGBA 模式下选看哪个通道；灰度模式固定 R
    let ci = if layer.channel_mode == ChannelMode::Rgba {
        win.channel.min(3)
    } else {
        0
    };
    let ch = &layer.channels[ci];

    if ch.frames.len() < 2 {
        ui.colored_label(
            Color32::from_rgb(0xd0, 0x90, 0x50),
            "这个通道少于两个参数点，无法导出。先在参数轴上至少画两处。",
        );
        return false;
    }

    let key = channel_fingerprint(ch);

    // ---- 嵌套检测：能不能导出的前提。结果由 ensure_violations 缓存维护 ----
    let violations = win.violations.clone().unwrap_or_else(|| Violations {
        bad: vec![false; (doc.width * doc.height) as usize],
        count: 0,
        total: (doc.width * doc.height) as usize,
    });

    // 有违规就不烘焙（烘焙必然不精确），只显示问题和修复入口
    if !violations.is_clean() {
        draw_violation_panel(ui, win, doc, &violations, ch);
        return false;
    }

    if win.baked_key != Some(key) || win.baked.is_none() {
        match bake_channel(ch, doc.width, doc.height) {
            Ok(b) => {
                win.baked = Some(b);
                win.baked_key = Some(key);
                win.tex = None;
                win.tex_key = None;
                win.tex_sdf = None;
                win.tex_sdf_key = None;
                win.message.clear();
            }
            Err(e) => {
                win.message = e;
                win.message_ok = false;
                win.baked = None;
            }
        }
    }

    if win.baked.is_none() {
        if !win.message.is_empty() {
            ui.colored_label(Color32::from_rgb(0xe0, 0x80, 0x60), &win.message);
        }
        return false;
    }

    let baked = win.baked.as_ref().unwrap();

    // ---- 纹理 ----
    let sdf_key = (win.baked_key.unwrap_or(0), win.opts.invert as i32);
    if win.tex_sdf.is_none() || win.tex_sdf_key != Some(sdf_key) {
        let ci_img = sdf_to_colorimage(baked, win.opts.invert);
        match &mut win.tex_sdf {
            Some(t) => t.set(ci_img, TextureOptions::NEAREST),
            None => {
                win.tex_sdf = Some(ui.ctx().load_texture(
                    "export_sdf",
                    ci_img,
                    TextureOptions::NEAREST,
                ))
            }
        }
        win.tex_sdf_key = Some(sdf_key);
    }

    let tkey = (
        win.baked_key.unwrap_or(0),
        (win.angle * 4.0) as i32,
        win.opts.invert as i32,
    );
    if win.tex.is_none() || win.tex_key != Some(tkey) {
        let mut mask = Mask::new(doc.width, doc.height);
        // 用「导出语义」采样，而不是后置取反：
        // 写盘时的「反向」是把 texel 换成 FULL - v（参数轴明暗对调），
        // 后置取反是同一时刻明暗互换，两者在轴中间给出不同结果。
        // 预览必须复刻写盘那一条，否则预览会预示一张不存在的图。
        sdf::sample_texels_as_exported(
            &baked.texels,
            win.angle / 90.0,
            baked.direction,
            win.opts.invert,
            &mut mask,
        );
        let img = mask_to_rgba(&mask, [1.0, 1.0, 1.0], 0.55);
        let ci_img =
            ColorImage::from_rgba_unmultiplied([doc.width as usize, doc.height as usize], &img);
        match &mut win.tex {
            Some(t) => t.set(ci_img, TextureOptions::NEAREST),
            None => {
                win.tex = Some(ui.ctx().load_texture(
                    "export_prev",
                    ci_img,
                    TextureOptions::NEAREST,
                ))
            }
        }
        win.tex_key = Some(tkey);
    }

    // ---- 并排显示 ----
    //
    // # 这里曾经有个正反馈循环，导致窗口越开越大
    //
    // 原来的写法是 `cell = (avail.x * 0.45).min(avail.y * 0.55)`，
    // 也就是"占当前可用空间的 45%"。问题在于 `avail` 会随窗口尺寸变：
    //   窗口变大 → avail 变大 → cell 变大 → 内容需要更多空间
    //   → egui 自动把窗口撑大 → avail 又变大 → ...
    // 每帧放大一点，窗口会持续膨胀下去（实测缺陷）。
    //
    // 修法：**给 cell 一个绝对上限**。窗口再大，预览也不超过固定像素数。
    // 这样内容尺寸与窗口尺寸解耦，反馈循环被切断。
    const CELL_MAX: f32 = 260.0;
    const CELL_MIN: f32 = 90.0;
    let avail = ui.available_size();
    let cell = (avail.x * 0.42)
        .min(avail.y * 0.50)
        .clamp(CELL_MIN, CELL_MAX);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new("SDF 贴图（实际导出内容）")
                    .size(11.0)
                    .color(LABEL_GRAY),
            );
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(cell), egui::Sense::hover());
            draw_image(
                ui,
                rect,
                win.tex_sdf.as_ref(),
                Color32::from_rgb(0x18, 0x18, 0x18),
            );
        });
        ui.add_space(10.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new(format!("还原遮罩（位置 {:.3}）", win.angle / 90.0))
                    .size(11.0)
                    .color(LABEL_GRAY),
            );
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(cell), egui::Sense::hover());
            draw_image(
                ui,
                rect,
                win.tex.as_ref(),
                Color32::from_rgb(0x10, 0x10, 0x10),
            );
        });
    });

    true
}

fn draw_options(ui: &mut Ui, win: &mut ExportWindow, doc: &Doc, baked_ok: bool) {
    ui.label(RichText::new("选项").strong().color(SECTION));
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.label("位深");
        for d in [BitDepth::Bit8, BitDepth::Bit16] {
            if ui
                .selectable_label(win.opts.bit_depth == d, d.label())
                .on_hover_text(match d {
                    BitDepth::Bit8 => "8 位单通道，体积小，多数项目够用",
                    BitDepth::Bit16 => "16 位单通道，角度精度是 8 位的 256 倍",
                })
                .clicked()
            {
                win.opts.bit_depth = d;
            }
        }

        ui.add_space(12.0);
        ui.checkbox(&mut win.opts.invert, "反向")
            .on_hover_text("把整条参数轴的明暗反过来");

        ui.add_space(12.0);
        ui.add_enabled_ui(baked_ok, |ui| {
            // 预览位置也用 0..1 表述，与参数轴一致。
            // 内部 `win.angle` 仍是 0..90（烘焙/采样都按角度），
            // 这里只做显示层的换算，避免把内部表示也改掉。
            let mut t = win.angle / 90.0;
            if ui
                .add(
                    egui::Slider::new(&mut t, 0.0..=1.0)
                        .text("预览位置")
                        .fixed_decimals(3),
                )
                .changed()
            {
                win.angle = t * 90.0;
            }
        });
    });

    // 通道选择只在 RGBA 模式出现
    if let Some(layer) = doc.layers.get(doc.active_layer)
        && layer.channel_mode == ChannelMode::Rgba
    {
        ui.horizontal(|ui| {
            ui.label("预览通道");
            for (i, name) in ["R", "G", "B", "A"].iter().enumerate() {
                if ui.selectable_label(win.channel == i, *name).clicked() {
                    win.channel = i;
                    win.tex = None;
                    win.tex_key = None;
                }
            }
            ui.label(
                RichText::new("（RGBA 导出固定 8bit）")
                    .size(10.0)
                    .color(Color32::from_gray(0x77)),
            );
        });
    }

    // 统计信息
    if baked_ok && let Some(baked) = &win.baked {
        let n_trans = baked
            .texels
            .iter()
            .filter(|&&v| (v as u32) < sdf::TEXEL_FULL)
            .count();
        let total = (doc.width * doc.height) as usize;

        // 编码方向决定着色器用哪条比较式，方向不同结果完全相反。
        // 该标记同时写进 PNG 元数据，但界面也必须当场显示——
        // 否则导出的贴图在引擎里"整片是反的"，而工具侧看不出任何异常。
        let dir_color = match baked.direction {
            sdf::Direction::ShadowGrows => Color32::from_rgb(0x7a, 0xc0, 0x8a),
            sdf::Direction::ShadowShrinks => Color32::from_rgb(0xd0, 0x90, 0x50),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("编码方向").size(11.0).color(LABEL_GRAY));
            ui.label(
                RichText::new(baked.direction.label())
                    .size(11.0)
                    .strong()
                    .color(dir_color),
            );
            ui.label(
                RichText::new(match baked.direction {
                    sdf::Direction::ShadowGrows => "着色器：lit = texel > t",
                    sdf::Direction::ShadowShrinks => "着色器：lit = texel < t",
                })
                .size(11.0)
                .color(LABEL_GRAY),
            )
            .on_hover_text(
                "方向已写入 PNG 的 sdf-direction 元数据。\
                 两种方向的比较式相反，用错会让整张贴图明暗颠倒。",
            );
        });
        ui.label(
            RichText::new(format!(
                "过渡像素：{} / {}（{:.1}%）",
                n_trans,
                total,
                n_trans as f32 / total as f32 * 100.0
            ))
            .size(11.0)
            .color(LABEL_GRAY),
        );
    }
}

fn draw_path_and_buttons(ui: &mut Ui, win: &mut ExportWindow, doc: &Doc, baked_ok: bool) {
    ui.label(RichText::new("输出").strong().color(SECTION));
    ui.add_space(4.0);

    egui::Grid::new("export_path_grid")
        .num_columns(2)
        .spacing([8.0, 6.0])
        .show(ui, |ui| {
            ui.label("目录");
            ui.horizontal(|ui| {
                // 手输框占满剩余宽度，浏览按钮固定在最右
                let btn_w = 68.0;
                // **给输入框宽度一个上限**。
                // 用纯 `available_width()` 会参与"窗口变宽 → 输入框变宽
                // → 内容更宽"的正反馈，和预览区那个 bug 同源。
                let edit_w = (ui.available_width() - btn_w - 6.0).clamp(120.0, 420.0);
                ui.add(
                    egui::TextEdit::singleline(&mut win.dir_input)
                        .desired_width(edit_w)
                        .hint_text("例如 C:\\Users\\Public\\Desktop\\export"),
                );
                // 可视化选择目录。手输仍然可用，两者共存。
                // pick_folder 是阻塞调用，和「导入底图」「打开工程」的既有行为一致。
                if ui
                    .add(egui::Button::new("浏览…").min_size(Vec2::new(btn_w, 0.0)))
                    .on_hover_text("打开系统目录选择框")
                    .clicked()
                {
                    let mut dlg = rfd::FileDialog::new().set_title("选择导出目录");
                    // 从当前输入框内容起步，省得每次从头找
                    let cur = win.dir_input.trim();
                    if !cur.is_empty() && std::path::Path::new(cur).is_dir() {
                        dlg = dlg.set_directory(cur);
                    }
                    if let Some(p) = dlg.pick_folder() {
                        win.dir_input = p.display().to_string();
                        log::info!("导出目录 → {}", win.dir_input);
                    }
                }
            });
            ui.end_row();

            ui.label("文件名");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut win.name_input)
                        .desired_width(200.0)
                        .hint_text("不含扩展名"),
                );
                ui.label(".png");
            });
            ui.end_row();
        });

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.checkbox(&mut win.export_all_layers, "导出全部可见图层")
            .on_hover_text("文件名取图层名，写在上面指定的目录里");

        ui.add_space(10.0);
        let label = if win.export_all_layers {
            "导出全部"
        } else {
            "导出"
        };
        if ui
            .add_enabled(
                baked_ok,
                egui::Button::new(label).min_size(Vec2::new(80.0, 24.0)),
            )
            .clicked()
        {
            if win.export_all_layers {
                do_export_all(win, doc);
            } else {
                do_export_one(win, doc);
            }
        }

        if ui.button("关闭").clicked() {
            win.open = false;
        }
    });

    if !win.message.is_empty() {
        ui.add_space(4.0);
        let color = if win.message_ok {
            Color32::from_rgb(0x7a, 0xc0, 0x7a)
        } else {
            Color32::from_rgb(0xe0, 0x80, 0x60)
        };
        ui.colored_label(color, &win.message);
    }
}

fn do_export_one(win: &mut ExportWindow, doc: &Doc) {
    let dir = win.dir_input.trim();
    if dir.is_empty() {
        win.message = "请填写输出目录".into();
        win.message_ok = false;
        return;
    }
    let name = sanitize(win.name_input.trim());
    let path = std::path::Path::new(dir).join(format!("{name}.png"));

    match sdf_mask_tool::io::export_layer_opts(doc, doc.active_layer, &path, &win.opts) {
        Ok(()) => {
            log::info!("导出图层 → {}", path.display());
            win.message = format!("已导出：{}", path.display());
            win.message_ok = true;
        }
        Err(e) => {
            log::warn!("导出失败：{e}");
            win.message = e;
            win.message_ok = false;
        }
    }
}

fn do_export_all(win: &mut ExportWindow, doc: &Doc) {
    let dir = win.dir_input.trim();
    if dir.is_empty() {
        win.message = "请填写输出目录".into();
        win.message_ok = false;
        return;
    }
    let dir = std::path::PathBuf::from(dir);
    match sdf_mask_tool::io::export_all_opts(doc, &dir, &win.opts) {
        Ok(n) => {
            log::info!("批量导出 {n} 个图层 → {}", dir.display());
            win.message = format!("已导出 {n} 个图层到 {}", dir.display());
            win.message_ok = true;
        }
        Err(e) => {
            log::warn!("批量导出失败：{e}");
            win.message = e;
            win.message_ok = false;
        }
    }
}

/// 把 texel 图直接做成灰度纹理——**这就是最终导出文件里存的像素**。
fn sdf_to_colorimage(baked: &Baked, invert: bool) -> ColorImage {
    let mut rgba = Vec::with_capacity(baked.texels.len() * 4);
    for &v in &baked.texels {
        let t = if invert {
            (sdf::TEXEL_FULL as u16).saturating_sub(v)
        } else {
            v
        };
        let g = sdf::texel_to_u8(t);
        rgba.extend_from_slice(&[g, g, g, 255]);
    }
    ColorImage::from_rgba_unmultiplied([baked.width as usize, baked.height as usize], &rgba)
}

fn mask_to_rgba(mask: &Mask, rgb: [f32; 3], alpha: f32) -> Vec<u8> {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    let r = (rgb[0].clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (rgb[1].clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (rgb[2].clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut out = Vec::with_capacity(mask.data().len() * 4);
    for &v in mask.data() {
        if v != 0 {
            out.extend_from_slice(&[r, g, b, a]);
        } else {
            out.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    out
}

fn draw_image(ui: &mut Ui, rect: Rect, tex: Option<&TextureHandle>, bg: Color32) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, bg);
    if let Some(t) = tex {
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        p.image(t.id(), rect, uv, Color32::WHITE);
    }
    p.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, Color32::from_rgb(0x44, 0x44, 0x44)),
        egui::StrokeKind::Outside,
    );
}
