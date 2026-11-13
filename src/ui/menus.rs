//! 菜单栏与画布右键菜单。菜单项只产出「动作」，由 App 统一执行，
//! 这样菜单本身不碰数据，容易扩展也容易测。

use eframe::egui::{self, Ui};

/// 菜单项的显示文字：`标题    快捷键`。
///
/// 快捷键从 `SHORTCUTS` 表里查，**不手写**。手写时代码改了判定而文案没跟上，
/// 就会出现"菜单写着 Shift+D、实际要吃 Ctrl+Shift+D"这种静默失效。
pub(crate) fn item_label(title: &str, action: Action) -> String {
    match crate::shortcut_label(action) {
        Some(k) => format!("{title}    {k}"),
        None => title.to_string(),
    }
}

/// 带自动快捷键标注的菜单按钮。没有绑定快捷键时只显示标题。
pub(crate) fn shortcut_button(ui: &mut Ui, title: &str, action: Action) -> egui::Response {
    ui.button(item_label(title, action))
}

/// 菜单能触发的所有动作
///
/// 带 `f32` 的变体（如 `DeleteKeyframeAt`）让这个枚举失去 `Eq`，
/// 但只有 `PartialEq` 就够了——只需和具体动作比较，不进哈希表。
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    // 文件
    NewProject,
    OpenProject,
    SaveProject,
    SaveProjectAs,
    ImportBackground,
    /// 导入 3D 模型（仅预览，不进工程数据）
    ImportModel,
    /// 卸载 3D 模型
    UnloadModel,
    /// 聚焦模型：把相机重新框住模型（保留当前朝向）
    FocusModel,
    /// 复位视角：朝向归零并重新框住模型
    ResetModelView,
    ExportCurrentLayer,
    ExportAllLayers,
    CanvasSize,
    Quit,

    // 编辑
    Undo,
    Redo,
    FillFrame,
    EraseFrame,
    InvertFrame,
    DuplicateLayer,
    DeleteLayer,
    ClearLayer,
    /// 反相整个图层（所有通道所有帧取补集）
    InvertLayer,
    /// 水平镜像整个图层
    MirrorLayer,

    // 设置
    /// 打开「设置」窗口（离散帧数等）
    OpenSettings,

    // 视图
    FitToWindow,
    ZoomIn,
    ZoomOut,
    ZoomActual,
    ToggleMaskOverlay,
    ToggleBackground,
    /// 在画布上叠加 / 隐藏模型 UV 线框
    ToggleUvOverlay,
    /// 设置画布 / 模型的显示模式
    SetViewMode(sdf_mask_tool::preview::ViewMode),

    // 图层
    AddLayer,
    LayerUp,
    LayerDown,
    NextLayer,
    PrevLayer,
    NextChannel,
    PrevChannel,

    // 时间轴
    AddKeyframe,
    DeleteKeyframe,
    /// 在指定**连续位置**加关键帧（0..1，时间轴右键菜单用）
    AddKeyframeAtNorm(f32),
    /// 删除指定**角度**的关键帧（连续定位，时间轴右键/双击用）
    DeleteKeyframeAtAngle(f32),
    /// 复制指定角度（落在某步）的关键帧
    DuplicateKeyframeAtAngle(f32),
    DuplicateKeyframe,
    /// 把当前关键帧放进剪贴板（真正的"复制"，之后可粘贴到任意位置）
    CopyKeyframe,
    /// 把剪贴板里的遮罩粘贴到当前播放头位置
    PasteKeyframe,
    /// 把当前关键帧复制到剪贴板，并**原样重复**到下一个空位（旧行为，快捷）
    DuplicateKeyframeAdjacent,
    /// 清空当前通道的所有关键帧
    ClearAllKeyframes,
    /// 反转当前通道的时间轴（角度镜像 + 重排）
    ReverseTimeline,
    /// 拖关键帧到已被占用的位置（提示用，不做改动）
    MoveKeyframeBlocked,
    /// 关键帧移动完成（带上新角度，用于状态栏提示）
    MoveKeyframeNotice(f32),
    PrevKeyframe,
    NextKeyframe,
    PrevStep,
    NextStep,
    GotoStart,
    GotoEnd,

    // 工具
    ToolBrush,
    ToolFill,
    ToolSelect,
    ToolSmooth,
    ToolShape,
    ToolLasso,
    /// 切换形状工具的矩形/圆形
    ToggleShapeKind,
    /// 切换形状实心/空心
    ToggleShapeFill,
    /// 切换选区工具的框选/套索
    ToggleSelectMode,
    /// 循环切换选区操作（新建/加选/减选）
    CycleSelectOp,
    /// 选区相关
    SelectAll,
    DeselectAll,
    InvertSelection,
    TogglePaintValue,
    BrushSizeDown,
    BrushSizeUp,
    BrushSize1,
    BrushSize2,
    BrushSize3,

    // 帮助
    About,
}

/// 顶栏菜单。返回这一帧要执行的动作。
pub fn menu_bar(ui: &mut Ui) -> Option<Action> {
    let mut act = None;
    egui::MenuBar::new().ui(ui, |ui| {
        ui.menu_button("文件", |ui| {
            if ui.button("新建工程").clicked() {
                act = Some(Action::NewProject);
                ui.close();
            }
            if ui.button("打开工程…").clicked() {
                act = Some(Action::OpenProject);
                ui.close();
            }
            if shortcut_button(ui, "保存工程", Action::SaveProject).clicked() {
                act = Some(Action::SaveProject);
                ui.close();
            }
            if ui.button("工程另存为…").clicked() {
                act = Some(Action::SaveProjectAs);
                ui.close();
            }
            ui.separator();
            if ui.button("导入底图…").clicked() {
                act = Some(Action::ImportBackground);
                ui.close();
            }
            if ui
                .button("导入 3D 模型…")
                .on_hover_text("支持 .obj / .fbx（二进制）。模型只用于预览，不参与导出。")
                .clicked()
            {
                act = Some(Action::ImportModel);
                ui.close();
            }
            if ui.button("卸载 3D 模型").clicked() {
                act = Some(Action::UnloadModel);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "聚焦模型", Action::FocusModel).clicked() {
                act = Some(Action::FocusModel);
                ui.close();
            }
            if ui.button("复位模型视角").clicked() {
                act = Some(Action::ResetModelView);
                ui.close();
            }
            if ui.button("画布大小…").clicked() {
                act = Some(Action::CanvasSize);
                ui.close();
            }
            if ui.button("设置…").clicked() {
                act = Some(Action::OpenSettings);
                ui.close();
            }
            ui.separator();
            if ui.button("导出当前图层…").clicked() {
                act = Some(Action::ExportCurrentLayer);
                ui.close();
            }
            if ui.button("导出全部图层…").clicked() {
                act = Some(Action::ExportAllLayers);
                ui.close();
            }
            ui.separator();
            if ui.button("退出").clicked() {
                act = Some(Action::Quit);
                ui.close();
            }
        });

        ui.menu_button("编辑", |ui| {
            if shortcut_button(ui, "撤销", Action::Undo).clicked() {
                act = Some(Action::Undo);
                ui.close();
            }
            if shortcut_button(ui, "重做", Action::Redo).clicked() {
                act = Some(Action::Redo);
                ui.close();
            }
            ui.separator();
            if ui.button("用遮罩填满当前点").clicked() {
                act = Some(Action::FillFrame);
                ui.close();
            }
            if ui.button("擦除当前点").clicked() {
                act = Some(Action::EraseFrame);
                ui.close();
            }
            if ui.button("反相当前点").clicked() {
                act = Some(Action::InvertFrame);
                ui.close();
            }
            ui.separator();
            if ui.button("复制图层").clicked() {
                act = Some(Action::DuplicateLayer);
                ui.close();
            }
            if ui.button("清空图层内容").clicked() {
                act = Some(Action::ClearLayer);
                ui.close();
            }
            if ui.button("删除图层").clicked() {
                act = Some(Action::DeleteLayer);
                ui.close();
            }
            ui.separator();
            if ui.button("反相图层").clicked() {
                act = Some(Action::InvertLayer);
                ui.close();
            }
            if ui.button("镜像图层（左右翻转）").clicked() {
                act = Some(Action::MirrorLayer);
                ui.close();
            }
        });

        ui.menu_button("视图", |ui| {
            if shortcut_button(ui, "适应窗口", Action::FitToWindow).clicked() {
                act = Some(Action::FitToWindow);
                ui.close();
            }
            if shortcut_button(ui, "放大", Action::ZoomIn).clicked() {
                act = Some(Action::ZoomIn);
                ui.close();
            }
            if shortcut_button(ui, "缩小", Action::ZoomOut).clicked() {
                act = Some(Action::ZoomOut);
                ui.close();
            }
            if shortcut_button(ui, "实际大小", Action::ZoomActual).clicked() {
                act = Some(Action::ZoomActual);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "显示 / 隐藏遮罩", Action::ToggleMaskOverlay).clicked() {
                act = Some(Action::ToggleMaskOverlay);
                ui.close();
            }
            if shortcut_button(ui, "显示 / 隐藏底图", Action::ToggleBackground).clicked() {
                act = Some(Action::ToggleBackground);
                ui.close();
            }
            if shortcut_button(ui, "显示 / 隐藏 UV 线框", Action::ToggleUvOverlay).clicked() {
                act = Some(Action::ToggleUvOverlay);
                ui.close();
            }
            ui.separator();
            // 画布 / 模型 的显示方式
            if ui.button("仅显示画布").clicked() {
                act = Some(Action::SetViewMode(
                    sdf_mask_tool::preview::ViewMode::CanvasOnly,
                ));
                ui.close();
            }
            if ui.button("仅显示模型").clicked() {
                act = Some(Action::SetViewMode(
                    sdf_mask_tool::preview::ViewMode::ModelOnly,
                ));
                ui.close();
            }
            if ui.button("同时显示").clicked() {
                act = Some(Action::SetViewMode(sdf_mask_tool::preview::ViewMode::Both));
                ui.close();
            }
        });

        ui.menu_button("图层", |ui| {
            if ui.button("新建图层").clicked() {
                act = Some(Action::AddLayer);
                ui.close();
            }
            if ui.button("复制当前图层").clicked() {
                act = Some(Action::DuplicateLayer);
                ui.close();
            }
            if ui.button("清空图层内容").clicked() {
                act = Some(Action::ClearLayer);
                ui.close();
            }
            if ui.button("删除图层").clicked() {
                act = Some(Action::DeleteLayer);
                ui.close();
            }
            ui.separator();
            if ui.button("反相图层").clicked() {
                act = Some(Action::InvertLayer);
                ui.close();
            }
            if ui.button("镜像图层（左右翻转）").clicked() {
                act = Some(Action::MirrorLayer);
                ui.close();
            }
            ui.separator();
            if ui.button("上移一层").clicked() {
                act = Some(Action::LayerUp);
                ui.close();
            }
            if ui.button("下移一层").clicked() {
                act = Some(Action::LayerDown);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "下一个图层", Action::NextLayer).clicked() {
                act = Some(Action::NextLayer);
                ui.close();
            }
            if shortcut_button(ui, "上一个图层", Action::PrevLayer).clicked() {
                act = Some(Action::PrevLayer);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "下一个通道", Action::NextChannel).clicked() {
                act = Some(Action::NextChannel);
                ui.close();
            }
            if ui.button("上一个通道").clicked() {
                act = Some(Action::PrevChannel);
                ui.close();
            }
        });

        ui.menu_button("参数轴", |ui| {
            if shortcut_button(ui, "加参数点", Action::AddKeyframe).clicked() {
                act = Some(Action::AddKeyframe);
                ui.close();
            }
            if shortcut_button(ui, "删除参数点", Action::DeleteKeyframe).clicked() {
                act = Some(Action::DeleteKeyframe);
                ui.close();
            }
            if shortcut_button(ui, "复制参数点到剪贴板", Action::CopyKeyframe).clicked() {
                act = Some(Action::CopyKeyframe);
                ui.close();
            }
            if shortcut_button(ui, "粘贴参数点", Action::PasteKeyframe).clicked() {
                act = Some(Action::PasteKeyframe);
                ui.close();
            }
            if shortcut_button(
                ui,
                "原位复制到下一个空位",
                Action::DuplicateKeyframeAdjacent,
            )
            .clicked()
            {
                act = Some(Action::DuplicateKeyframeAdjacent);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "上一个参数点", Action::PrevKeyframe).clicked() {
                act = Some(Action::PrevKeyframe);
                ui.close();
            }
            if shortcut_button(ui, "下一个参数点", Action::NextKeyframe).clicked() {
                act = Some(Action::NextKeyframe);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "上一步", Action::PrevStep).clicked() {
                act = Some(Action::PrevStep);
                ui.close();
            }
            if shortcut_button(ui, "下一步", Action::NextStep).clicked() {
                act = Some(Action::NextStep);
                ui.close();
            }
            if shortcut_button(ui, "跳到 0", Action::GotoStart).clicked() {
                act = Some(Action::GotoStart);
                ui.close();
            }
            if shortcut_button(ui, "跳到 1", Action::GotoEnd).clicked() {
                act = Some(Action::GotoEnd);
                ui.close();
            }
            ui.separator();
            if ui.button("反转参数轴").clicked() {
                act = Some(Action::ReverseTimeline);
                ui.close();
            }
        });

        ui.menu_button("工具", |ui| {
            if shortcut_button(ui, "画笔", Action::ToolBrush).clicked() {
                act = Some(Action::ToolBrush);
                ui.close();
            }
            if shortcut_button(ui, "填充", Action::ToolFill).clicked() {
                act = Some(Action::ToolFill);
                ui.close();
            }
            if shortcut_button(ui, "选择（移动遮罩）", Action::ToolSelect).clicked() {
                act = Some(Action::ToolSelect);
                ui.close();
            }
            if shortcut_button(ui, "平滑", Action::ToolSmooth).clicked() {
                act = Some(Action::ToolSmooth);
                ui.close();
            }
            if shortcut_button(ui, "形状", Action::ToolShape).clicked() {
                act = Some(Action::ToolShape);
                ui.close();
            }
            if shortcut_button(ui, "选区", Action::ToolLasso).clicked() {
                act = Some(Action::ToolLasso);
                ui.close();
            }
            ui.separator();
            if ui.button("矩形 / 圆形").clicked() {
                act = Some(Action::ToggleShapeKind);
                ui.close();
            }
            if ui.button("实心 / 空心").clicked() {
                act = Some(Action::ToggleShapeFill);
                ui.close();
            }
            if ui.button("框选 / 套索").clicked() {
                act = Some(Action::ToggleSelectMode);
                ui.close();
            }
            if ui.button("循环选区模式（新建/加选/减选）").clicked() {
                act = Some(Action::CycleSelectOp);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "全选", Action::SelectAll).clicked() {
                act = Some(Action::SelectAll);
                ui.close();
            }
            if shortcut_button(ui, "取消选区", Action::DeselectAll).clicked() {
                act = Some(Action::DeselectAll);
                ui.close();
            }
            if shortcut_button(ui, "反选", Action::InvertSelection).clicked() {
                act = Some(Action::InvertSelection);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "切换黑/白", Action::TogglePaintValue).clicked() {
                act = Some(Action::TogglePaintValue);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "笔刷变小", Action::BrushSizeDown).clicked() {
                act = Some(Action::BrushSizeDown);
                ui.close();
            }
            if shortcut_button(ui, "笔刷变大", Action::BrushSizeUp).clicked() {
                act = Some(Action::BrushSizeUp);
                ui.close();
            }
            ui.separator();
            if shortcut_button(ui, "小笔刷", Action::BrushSize1).clicked() {
                act = Some(Action::BrushSize1);
                ui.close();
            }
            if shortcut_button(ui, "中笔刷", Action::BrushSize2).clicked() {
                act = Some(Action::BrushSize2);
                ui.close();
            }
            if shortcut_button(ui, "大笔刷", Action::BrushSize3).clicked() {
                act = Some(Action::BrushSize3);
                ui.close();
            }
        });

        ui.menu_button("帮助", |ui| {
            if ui.button("关于").clicked() {
                act = Some(Action::About);
                ui.close();
            }
        });
    });
    act
}

/// 画布右键菜单
pub fn canvas_context_menu(ui: &mut Ui) -> Option<Action> {
    let mut act = None;
    ui.menu_button("填充", |ui| {
        if ui.button("用遮罩填满当前点").clicked() {
            act = Some(Action::FillFrame);
            ui.close();
        }
        if ui.button("擦除当前点").clicked() {
            act = Some(Action::EraseFrame);
            ui.close();
        }
        if ui.button("反相当前点").clicked() {
            act = Some(Action::InvertFrame);
            ui.close();
        }
    });
    ui.separator();
    if ui.button("适应窗口").clicked() {
        act = Some(Action::FitToWindow);
        ui.close();
    }
    if ui.button("实际大小").clicked() {
        act = Some(Action::ZoomActual);
        ui.close();
    }
    ui.separator();
    ui.menu_button("参数点", |ui| {
        if ui.button("在当前参数位置加参数点").clicked() {
            act = Some(Action::AddKeyframe);
            ui.close();
        }
        if shortcut_button(ui, "复制参数点到剪贴板", Action::CopyKeyframe).clicked() {
            act = Some(Action::CopyKeyframe);
            ui.close();
        }
        if shortcut_button(ui, "粘贴参数点", Action::PasteKeyframe).clicked() {
            act = Some(Action::PasteKeyframe);
            ui.close();
        }
        if shortcut_button(
            ui,
            "原位复制到下一个空位",
            Action::DuplicateKeyframeAdjacent,
        )
        .clicked()
        {
            act = Some(Action::DuplicateKeyframeAdjacent);
            ui.close();
        }
        if ui.button("删除当前参数点").clicked() {
            act = Some(Action::DeleteKeyframe);
            ui.close();
        }
    });
    act
}

/// 图层条目右键菜单
pub fn layer_context_menu(ui: &mut Ui, is_active: bool) -> Option<Action> {
    let mut act = None;
    if ui.button("复制图层").clicked() {
        act = Some(Action::DuplicateLayer);
        ui.close();
    }
    if ui.button("清空图层内容").clicked() {
        act = Some(Action::ClearLayer);
        ui.close();
    }
    ui.separator();
    if ui.button("反相图层").clicked() {
        act = Some(Action::InvertLayer);
        ui.close();
    }
    if ui.button("镜像图层（左右翻转）").clicked() {
        act = Some(Action::MirrorLayer);
        ui.close();
    }
    if is_active {
        ui.separator();
        if ui.button("上移一层").clicked() {
            act = Some(Action::LayerUp);
            ui.close();
        }
        if ui.button("下移一层").clicked() {
            act = Some(Action::LayerDown);
            ui.close();
        }
        if ui.button("删除图层").clicked() {
            act = Some(Action::DeleteLayer);
            ui.close();
        }
    }
    act
}
