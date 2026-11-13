//! 时间轴 = 一个 0..1 的轴。**播放头视觉连续，参数点对齐刻度。**
//!
//! ## 为什么参数点必须离散，播放头却可以连续
//!
//! 重算成本按刻度收：每越过一个刻度才重新采样一次整张图。若让**参数点**
//! 落在任意位置，等价于把刻度数无限拉高——同一刻度里能塞进多个参数点，
//! 它们既分不出"谁是当前点"，读盘时还会被合并掉内容。
//!
//! 播放头不存数据，只表示"现在看哪儿"，所以跟随指针连续移动是安全的，
//! 拖动也顺滑。它只把 `step`（最近刻度）同步给计算层。
//!
//! 因此本模块的落点规则分两类：
//! - **参数点**（播放头拖动松手、单击落在点上）：经 `snap_norm` 对齐刻度
//! - **播放头**（拖动、单击空白、滚轮）：位置连续，仅 `step` 取最近刻度
//!
//! 刻度本身是性能约束，不是审美选择，所以界面必须把它画出来。
//!
//! ## 图标不用 emoji 箭头
//!
//! `◀` `▶`（U+25C0/25B6）在 Windows 会被 Segoe UI Emoji 接管，渲染成蓝色
//! emoji 图标，主题色管不着。所以这里全部**自绘箭头形状**，绕开字体回退。
//!
//! ## 右键菜单必须每帧都调用
//!
//! egui 的 `context_menu()` 是**声明式**的：该函数每帧检查"这个 widget 刚被右键了吗"，
//! 然后自己维护弹窗的开关状态。如果把 `context_menu()` 包在 `if response.secondary_clicked()` 里，
//! 就只有在**右键那一次**会调用该函数，之后虽然弹窗已经被标记为打开，
//! 却再没有机会被渲染出来——表现就是"右键完全没反应"。
//!
//! 所以下面的所有 `context_menu` 都不带条件。
//!
//! ## 交互（参照 Blender Dope Sheet）
//! - 菱形 = 参数点，可拖到任意刻度（**松手吸附到最近刻度**），双击/右键删除
//! - 播放头是一条带倒三角的竖线，**连续**移动
//! - 右键轨道 → 时间轴操作菜单；右键工具栏空白 → 同一套菜单

use eframe::egui::{self, Color32, RichText, Sense, Ui, Vec2};

use sdf_mask_tool::model::ChannelMode;
use sdf_mask_tool::model::doc::{Doc, norm_to_step, step_norm};

use crate::ui::menus::Action;

pub struct TimelineOutput {
    pub changed: bool,
    pub action: Option<Action>,
}

const PLAYHEAD_TRI_H: f32 = 9.0;
const KF_R: f32 = 6.0;
/// 关键帧**点击**判定半径（比视觉半径大，好点中）
const KF_HIT_X: f32 = 13.0;
const KF_HIT_Y: f32 = 15.0;
const PLAYHEAD_HIT_X: f32 = 9.0;

/// 配色：跟主题一致，不用任何蓝色
const TRACK_BG: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
const TICK: Color32 = Color32::from_gray(0x38);
const AXIS: Color32 = Color32::from_gray(0x44);
const KF_FILL: Color32 = Color32::from_rgb(0xcc, 0xcc, 0xcc);
const KF_CUR: Color32 = Color32::from_rgb(0xe8, 0x87, 0x3a);
const KF_BLOCKED: Color32 = Color32::from_rgb(0x88, 0x50, 0x30);
const PLAYHEAD: Color32 = Color32::from_rgb(0xc8, 0xc8, 0xc8);
const PLAYHEAD_EDGE: Color32 = Color32::from_gray(0x66);

#[derive(Clone, Copy, PartialEq)]
enum Drag {
    /// 正在拖某个参数点（用该点的下标标识，因为角度会连续变化）
    Key(usize),
    /// 正在拖播放头
    Playhead,
}

/// 拖动中的实时状态：连续候选位置 + 是否被占用。
#[derive(Clone, Copy)]
struct DragState {
    what: Drag,
    /// 连续候选位置 0..1
    t: f32,
}

/// 把任意 0..1 位置对齐到最近的刻度。
///
/// 界面落点一律经此函数：刻度是重算的缓存粒度，落点必须与之一致，
/// 否则两个参数点会吸进同一刻度并互相覆盖。
fn snap_norm(t: f32, steps: usize) -> f32 {
    step_norm(norm_to_step(t, steps), steps)
}

pub fn show(ui: &mut Ui, doc: &mut Doc, step: &mut usize, playhead: &mut f32) -> TimelineOutput {
    let mut changed = false;
    let mut action: Option<Action> = None;
    let steps = doc.steps;
    *step = (*step).min(steps - 1);
    // **不要在这里把播放头对齐到刻度**。
    //
    // 播放头的视觉是连续的：拖动时它停在两格之间，下次进本函数若强行
    // 拉回刻度，画面会每帧抖一下（拖到 0.53 被拽回 0.54）。
    // 只夹到合法区间，位置本身保持原样。
    *playhead = playhead.clamp(0.0, 1.0);

    let drag_id = egui::Id::unique("timeline_drag");
    let mut drag = ui.memory(|m| {
        m.data
            .get_temp::<Option<DragState>>(drag_id)
            .unwrap_or(None)
    });

    // ---------------------------------------------------------- 工具栏
    let bar_resp = ui.horizontal(|ui| {
        ui.label(RichText::new("参数").strong());
        ui.add_space(4.0);
        // 位置一律用 0..1 表述：整条轴只有这一套坐标，
        // 不再混用「0.571」和「37.1°」两种写法。
        ui.label(
            RichText::new(format!("{:.3}", *playhead))
                .strong()
                .color(Color32::from_rgb(0xd8, 0xd8, 0xd8)),
        );
        ui.label(
            RichText::new(format!("{} 刻度", steps))
                .size(11.0)
                .color(Color32::from_gray(0x88)),
        )
        .on_hover_text("参数轴被等分成多少格（只影响拖动时的吸附粒度），可在「设置」里修改");

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        let on_kf = doc.active_channel_ref().has_kf_at_step(*step, steps);

        // 自绘箭头按钮（不用 emoji，避免被系统渲染成蓝色图标）
        if arrow_button(ui, false, "上一个参数点").clicked() {
            action = Some(Action::PrevKeyframe);
        }
        if arrow_button(ui, true, "下一个参数点").clicked() {
            action = Some(Action::NextKeyframe);
        }

        ui.add_space(4.0);
        if on_kf {
            ui.label(RichText::new("◆").color(KF_CUR))
                .on_hover_text("当前是参数点");
            if ui
                .small_button("复制")
                .on_hover_text("把当前参数点的内容放进剪贴板 (Ctrl+C)，之后可在任意位置粘贴")
                .clicked()
            {
                action = Some(Action::CopyKeyframe);
            }
            if ui
                .small_button("粘贴")
                .on_hover_text("把剪贴板内容放到当前播放头位置 (Ctrl+V)")
                .clicked()
            {
                action = Some(Action::PasteKeyframe);
            }
            if ui
                .small_button("原位复制")
                .on_hover_text("复制到紧邻的空位并跳过去 (Shift+D)")
                .clicked()
            {
                action = Some(Action::DuplicateKeyframeAdjacent);
            }
            if ui
                .small_button("删除")
                .on_hover_text("删除当前参数点 (Del)")
                .clicked()
            {
                action = Some(Action::DeleteKeyframe);
            }
        } else {
            ui.label(RichText::new("◇").color(Color32::from_gray(0x90)))
                .on_hover_text("当前不在参数点上");
            if ui
                .small_button("加点")
                .on_hover_text("在当前参数位置插入参数点 (Ins)")
                .clicked()
            {
                action = Some(Action::AddKeyframe);
            }
            // 没有关键帧时也允许粘贴——这正是"复制到远处"的用法：
            // 在 A 处 Ctrl+C，把播放头移到别处，Ctrl+V。
            if ui
                .small_button("粘贴")
                .on_hover_text("把剪贴板内容放到当前位置 (Ctrl+V)")
                .clicked()
            {
                action = Some(Action::PasteKeyframe);
            }
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let n = doc.active_channel_ref().frames.len();
            let (hint, color) = if n < 2 {
                (
                    "至少两个参数点才能导出".to_string(),
                    Color32::from_rgb(0xd0, 0x90, 0x50),
                )
            } else {
                (format!("{n} 个参数点"), Color32::from_gray(0x99))
            };
            ui.label(RichText::new(hint).size(11.0).color(color));

            if doc.active().channel_mode == ChannelMode::Rgba {
                ui.label(
                    RichText::new("RGBA")
                        .size(11.0)
                        .color(Color32::from_gray(0xb0)),
                )
                .on_hover_text("本图层的四个通道各有一条独立参数轴");
            }
        });
    });

    // 工具栏右键 → 同一套时间轴菜单。**不加条件**，见文件头说明。
    bar_resp.response.context_menu(|ui| {
        if let Some(a) = timeline_context_menu(ui, doc, *step, None, *playhead) {
            action = Some(a);
        }
    });

    // ---------------------------------------------------------- 轨道
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 78.0),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, TRACK_BG);

    let pad = 16.0;
    let x0 = rect.min.x + pad;
    let x1 = rect.max.x - pad;
    let y_line = rect.min.y + 44.0;
    let span = x1 - x0;

    let t_to_x = |t: f32| x0 + t.clamp(0.0, 1.0) * span;
    let x_to_t = |x: f32| ((x - x0) / span).clamp(0.0, 1.0);
    let to_norm = |angle: f32| (angle / 90.0).clamp(0.0, 1.0);

    // ---- 刻度：按 steps 画满整条轴，避免与真实刻度数脱节 ----
    //
    // 原来写死 64 格，改成设置里的刻度数后，画面刻度与实际落点就对不上：
    // 显示 64 格、吸附却按 steps 分桶，落点会落在没有刻度的位置上。
    //
    // 刻度过密时抽稀：保证相邻刻度的屏幕间距不小于 3px，
    // 否则 512 刻度会糊成一条实心色带。
    let min_px = 3.0;
    let stride = if span > 0.0 {
        ((steps as f32 * min_px / span).ceil() as usize).max(1)
    } else {
        1
    };
    let major_every = (steps - 1).max(1).div_ceil(4);
    for i in (0..steps).step_by(stride) {
        let x = t_to_x(step_norm(i, steps));
        let (h, c) = if i == 0 || i == steps - 1 {
            (7.0, Color32::from_gray(0x50))
        } else if i % major_every == 0 {
            (5.0, Color32::from_gray(0x44))
        } else {
            (3.0, TICK)
        };
        painter.line_segment(
            [egui::pos2(x, y_line - h), egui::pos2(x, y_line + 3.0)],
            egui::Stroke::new(1.0, c),
        );
    }

    painter.line_segment(
        [egui::pos2(x0, y_line), egui::pos2(x1, y_line)],
        egui::Stroke::new(2.0, AXIS),
    );

    for (t, label) in [(0.0f32, "0"), (0.5, "0.5"), (1.0, "1")] {
        painter.text(
            egui::pos2(t_to_x(t), y_line + 16.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(10.0),
            Color32::from_gray(0x80),
        );
    }

    // ---------------------------------------------------------- 关键帧
    // 按**帧下标**迭代：角度是连续的，不能按步号去重（会漏掉同桶的帧）
    let frame_count = doc.active_channel_ref().frames.len();
    let frame_angles: Vec<f32> = doc
        .active_channel_ref()
        .frames
        .iter()
        .map(|f| f.angle)
        .collect();

    // 命中测试：取离指针最近的关键帧（返回帧下标）
    let mut hit: Option<usize> = None;
    if let Some(p) = response.hover_pos().or(response.interact_pointer_pos()) {
        let mut best = f32::MAX;
        for (i, &a) in frame_angles.iter().enumerate() {
            let kx = match drag {
                Some(DragState {
                    what: Drag::Key(k),
                    t,
                }) if k == i => t_to_x(t),
                _ => t_to_x(to_norm(a)),
            };
            let dx = (p.x - kx).abs();
            let dy = (p.y - y_line).abs();
            if dx <= KF_HIT_X && dy <= KF_HIT_Y && dx < best {
                best = dx;
                hit = Some(i);
            }
        }
    }

    let mut blocked_now = false;
    for (i, &a) in frame_angles.iter().enumerate() {
        let being_dragged = matches!(drag, Some(DragState { what: Drag::Key(k), .. }) if k == i);
        // 拖动中的关键帧画在连续的候选位置上（视觉跟手，不吸附）
        let kx = match drag {
            Some(DragState {
                what: Drag::Key(k),
                t,
            }) if k == i => t_to_x(t),
            _ => t_to_x(to_norm(a)),
        };
        let r = if being_dragged { KF_R + 1.5 } else { KF_R };

        // "当前帧"高亮：角度落在播放头所在的那一桶
        let in_current_bucket = norm_to_step(to_norm(a), steps) == *step;
        let blocked = being_dragged && drag.is_some_and(|d| doc.angle_taken(d.t * 90.0, Some(i)));
        if being_dragged && blocked {
            blocked_now = true;
        }

        let fill = if blocked {
            KF_BLOCKED
        } else if in_current_bucket {
            KF_CUR
        } else {
            KF_FILL
        };
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(kx, y_line - r),
                egui::pos2(kx + r, y_line),
                egui::pos2(kx, y_line + r),
                egui::pos2(kx - r, y_line),
            ],
            fill,
            egui::Stroke::new(1.0, Color32::from_gray(0x1a)),
        ));
    }

    // ---------------------------------------------------------- 播放头
    //
    // **播放头在视觉上是连续的**：它只表示"当前看的是哪个位置"，不存任何数据，
    // 所以跟随指针连续移动是安全的，也让拖动的手感平滑。
    //
    // 计算仍按吸附步号（`step`）分桶——采样一次整张图是毫秒级的开销，
    // 若按连续位置重算，每移动一个像素都要付一次，交互会卡死。
    // 两者分工：`playhead` 决定画在哪、采样取哪个 t；`step` 决定缓存与吸附。
    let cx = t_to_x(*playhead);

    // 三角形的三个顶点。竖线顶端必须与三角形的**上边**对齐，
    // 否则线会从三角形顶部冒出去一截，看起来像没接上。
    let tri_tip = egui::pos2(cx, y_line - 12.0);
    let tri_top = tri_tip.y - PLAYHEAD_TRI_H;

    painter.line_segment(
        [egui::pos2(cx, tri_top), egui::pos2(cx, y_line + 10.0)],
        egui::Stroke::new(1.5, PLAYHEAD),
    );
    {
        painter.add(egui::Shape::convex_polygon(
            vec![
                tri_tip,
                egui::pos2(cx + 6.0, tri_top),
                egui::pos2(cx - 6.0, tri_top),
            ],
            PLAYHEAD,
            egui::Stroke::new(1.0, PLAYHEAD_EDGE),
        ));
    }

    // 播放头所在吸附帧位置画一根短标记，让"计算用的是哪一格"可见
    let snap_x = t_to_x(step_norm(*step, steps));
    if (snap_x - cx).abs() > 2.0 {
        painter.line_segment(
            [
                egui::pos2(snap_x, y_line + 6.0),
                egui::pos2(snap_x, y_line + 12.0),
            ],
            egui::Stroke::new(1.0, KF_CUR.gamma_multiply(0.75)),
        );
    }

    // 拖动到被占用的位置：画一条警示线
    if blocked_now {
        painter.line_segment(
            [egui::pos2(cx, rect.min.y), egui::pos2(cx, y_line + 10.0)],
            egui::Stroke::new(1.0, KF_BLOCKED),
        );
    }

    // ---------------------------------------------------------- 交互
    let playhead_hit = |p: &egui::Pos2| (p.x - cx).abs() <= PLAYHEAD_HIT_X;

    if response.drag_started() {
        let what = match (hit, response.interact_pointer_pos()) {
            (Some(i), _) => Drag::Key(i),
            (None, Some(p)) if playhead_hit(&p) => Drag::Playhead,
            (None, _) => Drag::Playhead,
        };
        drag = Some(DragState { what, t: *playhead });
        ui.memory_mut(|m| m.data.insert_temp(drag_id, drag));
    }

    if response.dragged()
        && let Some(p) = response.interact_pointer_pos()
    {
        let raw = x_to_t(p.x);
        match drag {
            Some(DragState {
                what: Drag::Key(_), ..
            }) => {
                // 参数点要落进刻度，所以候选位置先对齐；
                // 菱形始终画在真实落点上，松手不会"跳一下"。
                let t = snap_norm(raw, steps);
                if let Some(d) = &mut drag {
                    d.t = t;
                }
                ui.memory_mut(|m| m.data.insert_temp(drag_id, drag));
            }
            _ => {
                // 播放头**视觉连续**：位置跟随指针，不做对齐。
                // 计算用的步号仍取最近刻度——重算是毫秒级开销，不能按像素重来。
                let ns = norm_to_step(raw, steps);
                let t = raw.clamp(0.0, 1.0);
                if ns != *step || (t - *playhead).abs() > 1e-6 {
                    *step = ns;
                    *playhead = t;
                    changed = true;
                }
                if let Some(d) = &mut drag {
                    d.t = t;
                }
                ui.memory_mut(|m| m.data.insert_temp(drag_id, drag));
            }
        }
    }

    // 松手：把参数点落到**对齐后的刻度**上
    if response.drag_stopped() {
        if let Some(DragState {
            what: Drag::Key(idx),
            t,
        }) = drag
        {
            let new_angle = snap_norm(t, steps) * 90.0;
            if doc.angle_taken(new_angle, Some(idx)) {
                // 目标刻度被占：不挪，回到原位。用状态栏说明而不是静默失败。
                action = Some(Action::MoveKeyframeBlocked);
            } else {
                let old = doc.active_channel_ref().frames.get(idx).map(|f| f.angle);
                if doc.move_frame_to_angle(idx, new_angle) {
                    // 播放头跟到参数点的新位置
                    *step = norm_to_step(new_angle / 90.0, steps);
                    *playhead = step_norm(*step, steps);
                    changed = true;
                    if let Some(o) = old
                        && (o - new_angle).abs() > 0.01
                    {
                        action = Some(Action::MoveKeyframeNotice(new_angle));
                    }
                }
            }
        }
        drag = None;
        ui.memory_mut(|m| m.data.insert_temp(drag_id, drag));
    }

    // 单击（**只认左键**）：定位播放头到最近刻度。
    // 用 `clicked()` 会把右键也算进来——右键一按播放头就跳走，还会抢掉右键菜单。
    if response.clicked_by(egui::PointerButton::Primary)
        && let Some(p) = response.interact_pointer_pos()
        && p.x >= x0 - 12.0
        && p.x <= x1 + 12.0
    {
        // 单击空白处：播放头连续落到指针位置。
        // 落在参数点上则精确对齐到该点（那一点本来就在刻度上）。
        let t = match hit {
            Some(i) => to_norm(frame_angles[i]),
            None => x_to_t(p.x).clamp(0.0, 1.0),
        };
        let ns = norm_to_step(t, steps);
        if ns != *step || (t - *playhead).abs() > 1e-6 {
            *step = ns;
            *playhead = t;
            changed = true;
        }
    }

    // 双击关键帧 = 删除
    if response.double_clicked()
        && let Some(i) = hit
        && let Some(a) = frame_angles.get(i)
    {
        action = Some(Action::DeleteKeyframeAtAngle(*a));
    }

    // ---- 右键菜单：轨道。**每帧都调用**，见文件头说明。----
    let clicked_angle = hit.and_then(|i| frame_angles.get(i).copied());
    let clicked_norm = response
        .interact_pointer_pos()
        .map(|p| x_to_t(p.x))
        .unwrap_or(*playhead);
    response.context_menu(|ui| {
        if let Some(a) = timeline_context_menu(ui, doc, *step, clicked_angle, clicked_norm) {
            action = Some(a);
        }
    });

    // 滚轮：按刻度步进播放头。
    //
    // 播放头视觉连续，但滚轮本来就是"一格一格"的操作——
    // 按整格走才能让 `step` 与显示位置在同一档上，避免出现
    // "播放头停在 0.53、计算却按 0.54"的错位感。
    if response.hovered() && drag.is_none() {
        let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 1.0 {
            let dir: isize = if scroll > 0.0 { -1 } else { 1 };
            let ns = (*step as isize + dir).clamp(0, steps as isize - 1) as usize;
            if ns != *step {
                *step = ns;
                *playhead = step_norm(ns, steps);
                changed = true;
            }
        }
    }

    // 提示文案：只说"松手会怎样"。
    let hint = if blocked_now {
        "此刻度已有别的参数点".to_string()
    } else if drag.is_some() {
        "移动到最近刻度".to_string()
    } else {
        "拖菱形移动 · 双击删点 · 滚轮切刻度 · 右键更多".to_string()
    };
    painter.text(
        egui::pos2(rect.max.x - 10.0, rect.max.y - 10.0),
        egui::Align2::RIGHT_BOTTOM,
        hint,
        egui::FontId::proportional(10.0),
        if blocked_now {
            KF_BLOCKED
        } else {
            Color32::from_gray(0x66)
        },
    );

    let _ = frame_count;
    TimelineOutput { changed, action }
}

/// 自绘箭头按钮。
///
/// 不用 `◀` `▶` 字符：这两个字符在 Windows 会被 Segoe UI Emoji 接管，
/// 渲染成蓝色 emoji 图标，主题色完全管不着——这正是那两块蓝底的来源。
fn arrow_button(ui: &mut Ui, right: bool, tip: &str) -> egui::Response {
    let size = Vec2::new(26.0, 18.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());

    let hovered = resp.hovered();
    let bg = if resp.is_pointer_button_down_on() {
        Color32::from_gray(0x4a)
    } else if hovered {
        Color32::from_rgb(0x44, 0x44, 0x44)
    } else {
        Color32::from_rgb(0x33, 0x33, 0x33)
    };
    let fg = if hovered {
        Color32::from_rgb(0xf0, 0xf0, 0xf0)
    } else {
        Color32::from_rgb(0xb8, 0xb8, 0xb8)
    };

    let p = ui.painter();
    p.rect_filled(rect, 5.0, bg);
    p.rect_stroke(
        rect,
        5.0,
        egui::Stroke::new(1.0, Color32::from_rgb(0x3d, 0x3d, 0x3d)),
        egui::StrokeKind::Inside,
    );

    // 三角箭头：纯 shapes，绝无字体依赖
    let c = rect.center();
    let d = 4.0;
    let tri = if right {
        vec![
            egui::pos2(c.x + d * 0.7, c.y),
            egui::pos2(c.x - d * 0.6, c.y - d),
            egui::pos2(c.x - d * 0.6, c.y + d),
        ]
    } else {
        vec![
            egui::pos2(c.x - d * 0.7, c.y),
            egui::pos2(c.x + d * 0.6, c.y - d),
            egui::pos2(c.x + d * 0.6, c.y + d),
        ]
    };
    p.add(egui::Shape::convex_polygon(tri, fg, egui::Stroke::NONE));

    resp.on_hover_text(tip)
}

/// 时间轴右键菜单。
///
/// `clicked_angle` 是右键落点命中的关键帧角度（没有则为 None）；
/// `clicked_norm` 是右键落点的连续位置，用来"在这个位置加帧"。
fn timeline_context_menu(
    ui: &mut Ui,
    doc: &Doc,
    current: usize,
    clicked_angle: Option<f32>,
    clicked_norm: f32,
) -> Option<Action> {
    let mut act = None;
    let steps = doc.steps;

    if let Some(a) = clicked_angle {
        if ui.button("删除这个参数点").clicked() {
            act = Some(Action::DeleteKeyframeAtAngle(a));
            ui.close();
        }
        // 右键点的是**另一个**关键帧，不是当前播放头那一帧。
        // 所以这里只给"复制到紧邻空位"——剪贴板版需要先切过去选中该点。
        if ui.button("复制这个参数点到下一个空位").clicked() {
            act = Some(Action::DuplicateKeyframeAtAngle(a));
            ui.close();
        }
        ui.separator();
    } else if ui.button("在这个位置加参数点").clicked() {
        act = Some(Action::AddKeyframeAtNorm(clicked_norm));
        ui.close();
    }

    if doc.active_channel_ref().has_kf_at_step(current, steps) {
        if ui.button("删除当前参数点    Del").clicked() {
            act = Some(Action::DeleteKeyframe);
            ui.close();
        }
        if ui.button("复制当前参数点到剪贴板    Ctrl+C").clicked() {
            act = Some(Action::CopyKeyframe);
            ui.close();
        }
        if ui.button("原位复制到下一个空位    Shift+D").clicked() {
            act = Some(Action::DuplicateKeyframeAdjacent);
            ui.close();
        }
    } else if ui.button("在当前参数位置加参数点    Ins").clicked() {
        act = Some(Action::AddKeyframe);
        ui.close();
    }

    ui.separator();
    if ui.button("上一个参数点    Ctrl+←").clicked() {
        act = Some(Action::PrevKeyframe);
        ui.close();
    }
    if ui.button("下一个参数点    Ctrl+→").clicked() {
        act = Some(Action::NextKeyframe);
        ui.close();
    }
    ui.separator();
    if ui.button("跳到开头    Home").clicked() {
        act = Some(Action::GotoStart);
        ui.close();
    }
    if ui.button("跳到结尾    End").clicked() {
        act = Some(Action::GotoEnd);
        ui.close();
    }
    ui.separator();
    if ui.button("反转参数轴").clicked() {
        act = Some(Action::ReverseTimeline);
        ui.close();
    }
    if ui.button("清空这个通道的所有参数点").clicked() {
        act = Some(Action::ClearAllKeyframes);
        ui.close();
    }

    act
}
