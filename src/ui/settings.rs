//! 设置面板：那些"会改、但不常改"的东西。
//!
//! 放置原则：把刻度数等低频参数从工具栏挪到这里，集中管理。
//! 判据是"改一次管很久"——放工具栏上既占地方又容易误碰。
//!
//! 面板分四组：时间轴 / 绘制 / 新建工程 / 纠错显示。
//! 立即生效的项（撤销上限、默认笔刷、显示开关）落地即用；
//! 影响工程结构的项（刻度数）要按「应用」，因为改刻度数会重排参数点。

use eframe::egui::{self, Color32, RichText, Ui};

use sdf_mask_tool::config::AppSettings;
use sdf_mask_tool::model::doc::{Doc, MAX_STEPS, MIN_STEPS, clamp_steps};

pub struct SettingsWindow {
    pub open: bool,
    /// 待确认的步数（改完点「应用」才落到当前文档）
    pub steps_input: usize,
    /// 上次落盘时的设置，用于判断"有没有改动要存"
    saved: Option<AppSettings>,
}

impl Default for SettingsWindow {
    fn default() -> Self {
        Self {
            open: false,
            steps_input: 0,
            saved: None,
        }
    }
}

/// 设置面板产生的副作用，由 App 执行（面板本身不碰文档）。
pub struct SettingsOutput {
    /// 把当前文档的离散帧数改成这个值
    pub apply_steps: Option<usize>,
    /// 设置内容变了，需要存盘
    pub dirty: bool,
}

impl SettingsWindow {
    pub fn open_window(&mut self, doc: &Doc, settings: &AppSettings) {
        self.open = true;
        self.steps_input = doc.steps;
        self.saved = Some(settings.clone());
    }
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(
        RichText::new(title)
            .strong()
            .color(Color32::from_rgb(0xb8, 0xb8, 0xb8)),
    );
    ui.add_space(2.0);
}

fn hint(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .size(10.0)
            .color(Color32::from_gray(0x8a)),
    );
}

/// 显示设置面板。返回本次产生的副作用。
pub fn show(
    ui: &mut Ui,
    win: &mut SettingsWindow,
    doc: &Doc,
    settings: &mut AppSettings,
) -> SettingsOutput {
    let mut out = SettingsOutput {
        apply_steps: None,
        dirty: false,
    };
    if !win.open {
        return out;
    }
    let mut open = win.open;
    let before = settings.clone();

    egui::Window::new("设置")
        .collapsible(false)
        .resizable(false)
        .default_width(420.0)
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            // ============ 时间轴 ============
            section(ui, "参数轴");
            ui.horizontal(|ui| {
                ui.label("当前工程刻度数");
                ui.add(
                    egui::DragValue::new(&mut win.steps_input)
                        .range(MIN_STEPS..=MAX_STEPS)
                        .speed(1.0),
                );
                ui.label(
                    RichText::new(format!("（{}–{}）", MIN_STEPS, MAX_STEPS))
                        .size(10.0)
                        .color(Color32::from_gray(0x80)),
                );
            });
            hint(
                ui,
                "关键帧吸附到这些格子上计算，角度本身仍是连续的。\n\
                 改大更细腻但重算更慢；改小更快但粗糙。",
            );

            let clamped = clamp_steps(win.steps_input);
            if clamped != doc.steps {
                ui.add_space(3.0);
                ui.colored_label(
                    Color32::from_rgb(0xd0, 0x90, 0x50),
                    format!(
                        "将改为 {clamped} 帧。所有关键帧会重新吸附：\n\
                         • 变大：旧帧保留，位置略微移动\n\
                         • 变小：撞到同一格的两个帧会合并，后者覆盖前者"
                    ),
                );
                ui.horizontal(|ui| {
                    if ui.button("应用到当前工程").clicked() {
                        out.apply_steps = Some(clamped);
                    }
                    if ui.button("恢复").clicked() {
                        win.steps_input = doc.steps;
                    }
                });
            } else {
                ui.add_space(3.0);
                ui.label(
                    RichText::new(format!("当前工程：{} 刻度", doc.steps))
                        .size(11.0)
                        .color(Color32::from_gray(0xb0)),
                );
            }

            ui.horizontal(|ui| {
                ui.label("新建工程默认");
                ui.add(
                    egui::DragValue::new(&mut settings.default_steps)
                        .range(MIN_STEPS..=MAX_STEPS)
                        .speed(1.0),
                );
                ui.label("刻度");
            });
            hint(ui, "之后 「新建工程」 用这个值，不影响已有工程。");

            ui.separator();

            // ============ 绘制 ============
            section(ui, "绘制");
            ui.horizontal(|ui| {
                ui.label("默认笔刷半径");
                ui.add(
                    egui::DragValue::new(&mut settings.default_radius)
                        .range(1.0..=512.0)
                        .speed(0.5)
                        .suffix(" px"),
                );
            });
            hint(ui, "新建工程时的笔刷大小；当前会话仍用工具栏上的值。");

            ui.checkbox(
                &mut settings.auto_keyframe_on_paint,
                "在没参数点的位置落笔时自动加点",
            )
            .on_hover_text(
                "开启时，画笔/形状/平滑等在「当前没有关键帧」的位置落笔，\n\
                 会自动建一帧并在上面画，不用先按 Ins。\n\
                 新帧的起点由下面那个开关决定（插值结果或空白）。",
            );
            hint(
                ui,
                if settings.auto_keyframe_on_paint {
                    "开：随手就能画，落笔处自动建点。"
                } else {
                    "关：没参数点的地方画不了，得先按 Ins 加参数点。"
                },
            );

            ui.checkbox(&mut settings.interp_new_keyframe, "新参数点从插值结果开始")
                .on_hover_text(
                    "开启时，加关键帧会用相邻两帧在该角度的插值遮罩做起点，\n\
                 你只需修改不同的部分；关闭时从空白开始画。",
                );
            hint(
                ui,
                if settings.interp_new_keyframe {
                    "开：新点已包含「这个位置本该有的样子」，改起来省事。"
                } else {
                    "关：新点是空白，要自己从零画。"
                },
            );

            ui.separator();

            // ============ 新建工程 ============
            section(ui, "新建工程的画布尺寸");
            ui.horizontal(|ui| {
                ui.add(
                    egui::DragValue::new(&mut settings.default_canvas_w)
                        .range(16..=8192)
                        .speed(8.0),
                );
                ui.label("×");
                ui.add(
                    egui::DragValue::new(&mut settings.default_canvas_h)
                        .range(16..=8192)
                        .speed(8.0),
                );
                ui.label("px");
            });
            hint(
                ui,
                "只影响「新建工程」，不改变当前画布。改当前画布用「文件 → 画布大小」。",
            );

            ui.separator();

            // ============ 撤销与纠错显示 ============
            section(ui, "其他");
            ui.horizontal(|ui| {
                ui.label("撤销步数上限");
                ui.add(
                    egui::DragValue::new(&mut settings.undo_limit)
                        .range(1..=100)
                        .speed(1.0),
                );
            });
            hint(
                ui,
                "撤销用整份文档快照，每步约 8MB（1K 四通道两帧）。\n\
                 调大能退更远，但吃内存。改完下次压栈时生效。",
            );

            ui.checkbox(&mut settings.show_violations, "画布上标出嵌套违规")
                .on_hover_text("红色叠加显示哪些像素破坏了嵌套；不影响检测本身");
            hint(ui, "关掉只是不画红标记，检测和纠错功能照常可用。");

            ui.add_space(10.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("关闭").clicked() {
                    win.open = false;
                }
                ui.label(
                    RichText::new("设置改动会自动保存")
                        .size(10.0)
                        .color(Color32::from_gray(0x80)),
                );
            });
        });

    // 立即生效的设置改动要落盘
    if *settings != before {
        settings.sanitize();
        out.dirty = true;
    }

    win.open = open;
    out
}
