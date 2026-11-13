//! 图层面板：图层列表 + R/G/B/A 子条目。
//!
//! 子条目可以拖到别的图层上，把内容搬过去（Blender / PS 的通道搬运）。
//! 这里用「交换」而不是「移动」：交换天然可逆，目标位置原有内容不会被静默丢掉，
//! 拖错了再拖回来就行。

use eframe::egui::{self, Color32, RichText, Ui};

use sdf_mask_tool::model::{ChannelMode, Doc};

pub struct LayerPanelOutput {
    pub changed: bool,
    pub action: Option<crate::ui::menus::Action>,
    /// 本帧做了需要进撤销栈的结构改动时，这里是**改动之前**的文档副本与说明。
    ///
    /// 面板直接改 `Doc`，而压栈需要改动前的内容，所以由面板在应用前先留一份
    /// 快照，调用方负责写入撤销栈。没有结构改动时为 `None`
    /// （只改可见性这类不进撤销栈的操作不产生快照，避免白拷整份文档）。
    pub undo_snapshot: Option<(Doc, String)>,
}

/// 延后应用的结构改动。
///
/// 收集到循环之后再统一应用：这样只需在应用前拍一次快照，
/// 也避免边遍历 `doc.layers` 边改结构。
enum StructuralOp {
    AddLayer(String),
    RemoveLayer(usize),
    SwapChannels((usize, usize), (usize, usize)),
    SetChannelMode(usize, ChannelMode),
}

impl StructuralOp {
    fn label(&self) -> &'static str {
        match self {
            StructuralOp::AddLayer(_) => "新建图层",
            StructuralOp::RemoveLayer(_) => "删除图层",
            StructuralOp::SwapChannels(..) => "交换通道内容",
            StructuralOp::SetChannelMode(..) => "切换通道模式",
        }
    }

    fn apply(self, doc: &mut Doc) {
        match self {
            StructuralOp::AddLayer(name) => doc.add_layer(&name),
            StructuralOp::RemoveLayer(i) => doc.remove_layer(i),
            StructuralOp::SwapChannels(a, b) => {
                if doc.swap_channel(a, b) {
                    doc.active_layer = b.0;
                    doc.active_channel = b.1;
                }
            }
            StructuralOp::SetChannelMode(li, m) => {
                if let Some(l) = doc.layers.get_mut(li) {
                    l.channel_mode = m;
                }
            }
        }
        // 每种改动都可能让活动下标失效或让绘制目标与导出目标脱节
        doc.normalize_active();
    }
}

/// 拖拽时携带的数据：(图层, 通道)
#[derive(Clone, Copy, Debug)]
struct ChannelDrag {
    layer: usize,
    channel: usize,
}

pub fn show(ui: &mut Ui, doc: &mut Doc, _angle: f32, step: usize) -> LayerPanelOutput {
    let mut changed = false;
    let mut action: Option<crate::ui::menus::Action> = None;
    // 结构改动延后应用，应用前拍一次快照供撤销使用
    let mut ops: Vec<StructuralOp> = Vec::new();

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("图层").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 减号在右、加号在左（右到左布局里先加的先在右边）。
            // 两者视觉形式一致，和加号对称。
            let can_del = doc.layers.len() > 1;
            let del = ui.add_enabled(can_del, egui::Button::new("−").small());
            if del
                .on_hover_text(if can_del {
                    "删除当前图层"
                } else {
                    "至少保留一个图层"
                })
                .clicked()
            {
                ops.push(StructuralOp::RemoveLayer(doc.active_layer));
            }
            if ui.small_button("＋").on_hover_text("新建图层").clicked() {
                let n = doc.layers.len() + 1;
                ops.push(StructuralOp::AddLayer(format!("图层 {n}")));
                changed = true;
            }
        });
    });
    ui.separator();

    let n_layers = doc.layers.len();
    // 本帧收集到的拖拽结果，循环结束后统一应用，避免边遍历边改结构
    let mut swap: Option<((usize, usize), (usize, usize))> = None;

    egui::ScrollArea::vertical()
        .id_salt("layer_scroll")
        .max_height((ui.available_height() - 90.0).max(60.0))
        .show(ui, |ui| {
            for li in 0..n_layers {
                let selected = doc.active_layer == li;
                let name = doc.layers[li].name.clone();
                let visible = doc.layers[li].visible;
                let open_id = egui::Id::unique(("layer_open", li));
                let mut open = ui.memory(|m| m.data.get_temp::<bool>(open_id).unwrap_or(true));

                ui.horizontal(|ui| {
                    if expand_arrow(ui, open).clicked() {
                        open = !open;
                        ui.memory_mut(|m| m.data.insert_temp(open_id, open));
                    }
                    let eye = if visible { "◉" } else { "○" };
                    if ui
                        .add(egui::Button::new(eye).frame(false))
                        .on_hover_text("显示/隐藏图层")
                        .clicked()
                    {
                        doc.layers[li].visible = !visible;
                        changed = true;
                    }
                    let label = RichText::new(&name).color(if selected {
                        Color32::from_rgb(0xe8, 0x87, 0x3a)
                    } else {
                        Color32::from_rgb(0xd8, 0xd8, 0xd8)
                    });
                    let resp = ui.selectable_label(selected, label.clone());
                    if resp.clicked() {
                        doc.active_layer = li;
                        changed = true;
                    }
                    // 右键：复制 / 清空 / 上下移 / 删除 / 反相 / 镜像
                    // **每帧都调用**（context_menu 自己管开关，加条件会打不开）
                    resp.context_menu(|ui| {
                        if let Some(a) = crate::ui::menus::layer_context_menu(ui, selected) {
                            action = Some(a);
                        }
                    });
                });

                if open {
                    ui.indent(("ch", li), |ui| {
                        // 灰度模式下只用 R 通道，G/B/A 置灰且不可选。
                        // **不删除数据**：切回 RGBA 时原来的内容还在。
                        let gray = doc.layers[li].channel_mode == ChannelMode::Gray;
                        for ci in 0..4 {
                            let disabled = gray && ci != 0;
                            let is_active = doc.active_layer == li && doc.active_channel == ci;
                            ui.horizontal(|ui| {
                                let cv = doc.layers[li].channels[ci].visible;
                                let eye = if cv { "◉" } else { "○" };
                                let eye_resp = ui
                                    .add_enabled(
                                        !disabled,
                                        egui::Button::new(eye).frame(false).small(),
                                    )
                                    .on_hover_text(if disabled {
                                        "灰度模式只用 R 通道；切到 RGBA 可编辑"
                                    } else {
                                        "显示/隐藏通道"
                                    });
                                if eye_resp.clicked() {
                                    doc.layers[li].channels[ci].visible = !cv;
                                    changed = true;
                                }

                                let ch = &doc.layers[li].channels[ci];
                                let n = ch.frames.len();
                                // 关键帧角度是连续的，按步号判断这一帧有没有内容
                                let has_kf = ch.has_kf_at_step(step, doc.steps);
                                // 角标：这一帧有没有内容
                                let mark = if n == 0 {
                                    String::new()
                                } else if has_kf {
                                    format!("  ● {n}点")
                                } else {
                                    format!("  ○ {n}点")
                                };
                                let txt = format!("{}{}", ch.name, mark);
                                let color = if disabled {
                                    // 置灰但保留帧数角标，让人知道数据还在
                                    Color32::from_gray(0x5a)
                                } else if is_active {
                                    Color32::from_rgb(0xe8, 0x87, 0x3a)
                                } else {
                                    Color32::from_gray(0xb0)
                                };
                                let label = RichText::new(txt).size(12.0).color(color);
                                let resp = ui
                                    .add_enabled(
                                        !disabled,
                                        egui::Button::new(label)
                                            .selected(is_active)
                                            .frame(false)
                                            // **必须显式开启拖拽感应**。
                                            //
                                            // egui 的 Button 默认是 `Sense::click()`（只感应点击），
                                            // 而 egui 只有对声明了 `senses_drag()` 的控件才会置
                                            // "dragged" 状态。少了这一行，下面的 dnd_set_drag_payload
                                            // 永远不会执行，拖拽功能形同不存在——而且不报错。
                                            .sense(egui::Sense::click_and_drag()),
                                    )
                                    .on_hover_text(if disabled {
                                        "灰度模式只用 R 通道；切到 RGBA 可编辑"
                                    } else {
                                        "可拖到别的图层或通道上交换内容"
                                    });
                                if resp.clicked() && !disabled {
                                    doc.active_layer = li;
                                    doc.active_channel = ci;
                                    changed = true;
                                }

                                // ---- 拖拽源 / 目标（灰度模式下不参与）----
                                if disabled {
                                    return;
                                }
                                let payload = ChannelDrag {
                                    layer: li,
                                    channel: ci,
                                };
                                if resp.dragged() {
                                    resp.dnd_set_drag_payload(payload);
                                }
                                if resp.dnd_hover_payload::<ChannelDrag>().is_some() {
                                    // 悬停时描一圈，表示可以放
                                    ui.painter().rect_stroke(
                                        resp.rect,
                                        6.0,
                                        egui::Stroke::new(1.5, Color32::from_rgb(0x5a, 0xa0, 0xe0)),
                                        egui::StrokeKind::Outside,
                                    );
                                }
                                if let Some(p) = resp.dnd_release_payload::<ChannelDrag>()
                                    && (p.layer != li || p.channel != ci)
                                {
                                    swap = Some(((p.layer, p.channel), (li, ci)));
                                }
                            });
                        }
                    });
                }

                ui.add_space(3.0);
            }
        });

    if let Some((a, b)) = swap {
        ops.push(StructuralOp::SwapChannels(a, b));
    }

    ui.separator();
    ui.horizontal(|ui| {
        ui.label("通道");
        let li = doc.active_layer;
        let mut mode = doc.layers[li].channel_mode;
        if ui
            .selectable_label(mode == ChannelMode::Gray, "灰度")
            .clicked()
        {
            mode = ChannelMode::Gray;
        }
        if ui
            .selectable_label(mode == ChannelMode::Rgba, "RGBA")
            .clicked()
        {
            mode = ChannelMode::Rgba;
        }
        if mode != doc.layers[li].channel_mode {
            // 通道模式也进撤销栈：灰度/RGBA 决定导出单通道还是四通道，
            // 切错会让"画了但导出没有"，必须能撤回。
            ops.push(StructuralOp::SetChannelMode(li, mode));
        }
    });

    // 应用结构改动：先留快照再改，保证撤销能回到改动前。
    // 同一帧的多个结构改动合并成一个撤销点——表现为一个结果。
    let mut undo_snapshot = None;
    if !ops.is_empty() {
        let label = ops[0].label().to_string();
        undo_snapshot = Some((doc.clone(), label));
        for op in ops {
            op.apply(doc);
        }
        changed = true;
    }

    LayerPanelOutput {
        changed,
        action,
        undo_snapshot,
    }
}

/// 自绘展开/折叠三角。
///
/// **不能用 `▼` / `▶` 字符**：这两个码位会被 Windows 的 Segoe UI Emoji 接管，
/// 渲染成蓝色 emoji 图标，主题色完全管不着——在深灰面板上会跳出一块亮蓝。
///
/// 这里用纯 `Shape` 画三角形，颜色取自主题前景色，
/// 与旁边的眼睛图标、文字保持同一套灰阶。
fn expand_arrow(ui: &mut Ui, open: bool) -> egui::Response {
    // 与 `small_button` 的行高接近，避免这一行比别的行高出一截
    let size = egui::Vec2::new(14.0, 14.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let rect = egui::Rect::from_center_size(rect.center(), size);

    let c = rect.center();
    let d = 3.6;

    // 展开时朝下（▼ 的等价形状），折叠时朝右（▶ 的等价形状）
    let pts = if open {
        vec![
            egui::pos2(c.x - d, c.y - d * 0.6),
            egui::pos2(c.x + d, c.y - d * 0.6),
            egui::pos2(c.x, c.y + d * 0.9),
        ]
    } else {
        vec![
            egui::pos2(c.x - d * 0.6, c.y - d),
            egui::pos2(c.x - d * 0.6, c.y + d),
            egui::pos2(c.x + d * 0.9, c.y),
        ]
    };

    // 灰阶：与图层/通道文字同色，悬停时提亮
    let color = if resp.hovered() {
        Color32::from_gray(0xd8)
    } else {
        Color32::from_gray(0x9a)
    };

    ui.painter()
        .add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));

    resp.on_hover_text(if open { "折叠" } else { "展开" })
}
