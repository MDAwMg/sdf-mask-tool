mod bake_bg;
mod ui;

use eframe::egui::{self, Color32, Key, Modifiers, Rect, RichText};

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::{Doc, angle_to_step, step_angle, step_norm};
use sdf_mask_tool::ops::ShapeKind;
use sdf_mask_tool::sdf;
use ui::canvas_view::{CanvasState, PaintValue, RADIUS_MAX, RADIUS_MIN, Tool};
use ui::menus::Action;

const LAYER_W: f32 = 240.0;

/// 画布上最多叠加多少条 UV 线段。
///
/// 与 3D 面板的三角形上限同理：UV 线框是给美术对照用的，超过这个密度
/// 画面只剩一片糊，而每帧遍历它们会直接吃掉交互帧率。
/// 8 万面左右的角色，去重后的 UV 边通常在 1–3 万量级。
const UV_EDGE_BUDGET: usize = 40_000;
const TIMELINE_H: f32 = 132.0;
/// 菜单栏 + 工具条两行
const MENUBAR_H: f32 = 26.0;
const TOOLBAR_H: f32 = 40.0;
/// 画布与 3D 预览之间的分隔条宽度
const SPLIT_W: f32 = 6.0;

/// 窗口与任务栏图标。
///
/// egui 的 IconData 收 RGBA8 像素而非 ICO 文件，因此这里嵌入 assets/icon 下的
/// 256×256 PNG，解码后交给 ViewportBuilder。缺失时退化为无图标，不影响启动。
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/icon/sdf-mask-tool-256.png");

fn window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(WINDOW_ICON_PNG).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    })
}

fn main() -> eframe::Result<()> {
    sdf_mask_tool::logging::init();

    // 窗口尺寸可用环境变量覆盖，方便在不同缩放比例的屏幕上验证布局
    let (w, h) = std::env::var("SDF_WIN")
        .ok()
        .and_then(|s| {
            let mut it = s.split('x');
            let w = it.next()?.trim().parse::<f32>().ok()?;
            let h = it.next()?.trim().parse::<f32>().ok()?;
            Some((w, h))
        })
        .unwrap_or((1360.0, 860.0));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([w, h])
        .with_min_inner_size([900.0, 600.0])
        .with_title("SDF Mask Tool");
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "SDF Mask Tool",
        options,
        Box::new(|cc| {
            ui::theme::install(&cc.egui_ctx);
            // 窗口已创建，此处补一次 DWM 调用把原生标题栏固定为深色。
            // egui 只会经 winit 走未公开 API，在部分 Windows 版本上不生效；
            // 不补这一步时标题栏颜色随系统 DWM 状态浮动（有时黑有时白）。
            sdf_mask_tool::win_theme::apply_dark_title_bar(cc);
            Ok(Box::new(App::new()))
        }),
    )
}

struct App {
    doc: Doc,
    canvas: CanvasState,
    /// 时间轴吸附步号 0..31，决定预览重算哪一格、关键帧落在哪个整数角度
    step: usize,
    /// 参数轴的**连续**播放头位置 0..1。界面显示的就是该值，可停在任意位置，
    /// 但真正计算仍按 step 吸附。
    playhead: f32,
    show_mask: bool,
    show_log: bool,
    show_about: bool,
    show_canvas_size: bool,
    /// 画布大小对话框里待确认的尺寸
    size_input: (u32, u32),
    export_win: ui::export_window::ExportWindow,
    /// 叠加层颜色选择器状态
    color_win: ui::color_picker::ColorPickerState,
    /// 设置面板
    settings_win: ui::settings::SettingsWindow,
    /// 持久化的应用设置（离散帧默认值、撤销上限等）
    settings: sdf_mask_tool::config::AppSettings,
    status: String,
    project_path: Option<std::path::PathBuf>,
    /// 当前角度下用于显示的遮罩（由 texel 采样得到，和导出完全同源）
    display_mask: Mask,
    display_dirty: bool,
    last_calc: Option<(u64, usize, usize, usize)>,
    /// 预览反向开关（导出窗口里的"反向"也同步这一份）
    export_invert: bool,
    /// 数据不合法时的提示（来回翻转等），有值时画布不更新
    display_error: Option<String>,
    /// 统一的撤销栈：图层 / 时间轴 / 画板操作都进这里
    undo_stack: UndoStack,
    /// 当前通道的嵌套违规检测结果（缓存）。
    ///
    /// 原来这套检测只在导出窗口内可用，必须先点导出才能发现问题。
    /// 现在移到 App 层，笔刷栏的「自动纠错」按钮和画布红标记都直接引用该结果，
    /// 导出窗口也复用同一份，三处判断永远不会不一致。
    violations: Option<sdf_mask_tool::model::nesting::Violations>,
    violations_key: Option<u64>,
    /// 检测算出来的"改动更少"的修复方向
    violations_dir: sdf_mask_tool::model::nesting::NestDir,
    /// UV 线框的缓存键（模型路径 + UV 集 + 有无模型）。
    /// 变了才重算线段，否则每帧重建去重集合。
    canvas_uv_key: Option<(Option<std::path::PathBuf>, usize, bool)>,
    /// 3D 预览面板（模型、相机、渲染缓存）
    model_panel: sdf_mask_tool::preview::ModelPanel,
    /// 画布 / 模型 的显示模式
    view_mode: sdf_mask_tool::preview::ViewMode,
    /// 左右分栏比例（0..1，画布占的比例）。拖动分隔条时变。
    split_ratio: f32,
    /// 后台烘焙器：把"算 texel 图"移出 UI 线程
    baker: bake_bg::BackgroundBaker,
    /// 当前 texel 缓存对应的是哪份文档指纹。
    /// 与 `Channel::frames_fingerprint` 比对，判断要不要重烘。
    last_baked_fp: Option<u64>,
    /// texel 图的本地快照 `(指纹, texel 图, 方向)`。
    ///
    /// 拖动播放头时每跨一格都要重新采样，而 `Channel::cached_texels()`
    /// 每次会 clone 2MB。留一份快照后，同一份内容只取一次，
    /// 之后都是借用采样（零拷贝）。
    texel_snapshot: Option<(u64, Vec<u16>, sdf::Direction)>,
    /// 关键帧剪贴板：`(来源角度, 遮罩)`。
    ///
    /// 原来的"复制关键帧"是**一步到位**的（直接复制到紧邻空位并跳过去），
    /// 名字却叫"复制"，而"粘贴"无处可寻，容易被判定为功能残缺。
    /// 而且旧实现只能复制到**紧邻**的一格，想复制到远处、想粘多次都做不到。
    ///
    /// 现在拆成两套：
    /// - **复制 / 粘贴**（Ctrl+C / Ctrl+V）：PS 习惯，可粘贴到任意位置、可多次
    /// - **原位复制**（Shift+D）：保留原来的一步到位快捷方式
    ///
    /// 剪贴板不参与撤销/存盘——剪贴板是编辑期状态，和选区同性质。
    keyframe_clip: Option<(f32, Mask)>,
}

/// 撤销栈里的一条：整份文档的快照。
///
/// 用快照而不是逐操作写命令，是因为图层结构和时间轴结构改动面太广
/// （增删图层会移动索引、改尺寸会重建所有帧），
/// 为每种操作分别写 apply/undo 极易出错且难测。快照只有一处实现，天然正确。
///
/// 代价是内存：1K 画布 4 通道 2 关键帧约 8MB/条。所以限制条数，
/// 并且在内容没变的操作上跳过快照。
struct Snapshot {
    doc: Doc,
    /// 该快照的估算占用，压栈时算一次，避免每次压栈全量重扫
    bytes: usize,
    step: usize,
    label: String,
}

struct UndoStack {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// `undo` 里所有快照的累计占用
    undo_bytes: usize,
}

/// 撤销栈的条数上限（硬上限，防止极小画布下堆积过多条目）。
const UNDO_LIMIT_MAX: usize = 100;

/// 撤销栈的总内存预算。
///
/// 只按条数限制是不够的：1K 画布单帧 1MB，3 图层 × 4 通道 × 8 帧约 96MB/条，
/// 20 条就是 1.9GB，够把进程撑爆。按字节算才能真正封住上界。
///
/// 512MB 的取舍：1K 常规工程（1–3 层、5–8 帧）能留十几次撤销，
/// 足够覆盖"画错一步想退回去"的实际使用；超出则丢最旧的。
const UNDO_BUDGET_BYTES: usize = 512 * 1024 * 1024;

/// 估算一份文档在快照里的占用。
///
/// 只数大块：各帧遮罩是唯一的量级来源，其余字段相对可忽略。
/// `Channel::clone` 会丢掉距离场缓存，但保留 texel 缓存（2MB），所以要算进去。
fn doc_bytes(doc: &Doc) -> usize {
    let mut n = 0usize;
    for l in &doc.layers {
        for ch in &l.channels {
            for f in &ch.frames {
                n += f.mask.data().len();
            }
            if ch.has_texel_cache() {
                n += (doc.width as usize) * (doc.height as usize) * 2;
            }
        }
    }
    if let Some(bg) = &doc.background {
        n += bg.as_raw().len();
    }
    n
}

impl Default for UndoStack {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
            undo_bytes: 0,
        }
    }
}

impl UndoStack {
    fn push(&mut self, doc: &Doc, step: usize, label: String) {
        self.push_snapshot(doc.clone(), step, label);
    }

    /// 直接压入一份**已经拍好**的快照。
    ///
    /// 用于调用方在改动前就留下副本的情况（例如图层面板：面板自身直接改文档，
    /// 改完再取副本已经来不及）。
    fn push_snapshot(&mut self, before: Doc, step: usize, label: String) {
        let bytes = doc_bytes(&before);
        self.undo.push(Snapshot {
            doc: before,
            bytes,
            step,
            label,
        });
        self.undo_bytes += bytes;
        // 超出条数上限或内存预算时丢弃最旧的。
        // 至少留一条：否则"刚做了一步就撤销不了"。
        while self.undo.len() > 1
            && (self.undo.len() > UNDO_LIMIT_MAX || self.undo_bytes > UNDO_BUDGET_BYTES)
        {
            if let Some(dropped) = self.undo.first() {
                self.undo_bytes = self.undo_bytes.saturating_sub(dropped.bytes);
            }
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo(&mut self, doc: &mut Doc, step: usize) -> Option<(Doc, usize, String)> {
        let s = self.undo.pop()?;
        self.undo_bytes = self.undo_bytes.saturating_sub(s.bytes);
        self.redo.push(Snapshot {
            doc: doc.clone(),
            bytes: doc_bytes(doc),
            step,
            label: s.label.clone(),
        });
        Some((s.doc, s.step, s.label))
    }

    fn redo(&mut self, doc: &mut Doc, step: usize) -> Option<(Doc, usize, String)> {
        let s = self.redo.pop()?;
        self.undo.push(Snapshot {
            doc: doc.clone(),
            bytes: doc_bytes(doc),
            step,
            label: s.label.clone(),
        });
        Some((s.doc, s.step, s.label))
    }
}

impl App {
    fn new() -> Self {
        // 设置先读出来：新工程的画布尺寸、刻度数、笔刷半径都受设置影响
        let settings = sdf_mask_tool::config::load();

        let mut doc = Doc::with_steps(
            settings.default_canvas_w,
            settings.default_canvas_h,
            settings.default_steps,
        );
        doc.seed_default_frames();

        // 命令行直接打开图片或工程，便于测试与双击打开
        if let Some(arg) = std::env::args().nth(1) {
            let p = std::path::PathBuf::from(&arg);
            if p.is_file() {
                if sdf_mask_tool::io::is_project_path(&p) {
                    match sdf_mask_tool::io::open_project(&p) {
                        Ok(d) => {
                            log::info!("命令行打开工程 {}", p.display());
                            doc = d;
                        }
                        Err(e) => log::error!("打开工程失败：{e}"),
                    }
                } else {
                    match sdf_mask_tool::io::load_background(&p, doc.width, doc.height) {
                        Ok(bg) => {
                            log::info!("命令行导入底图 {}", p.display());
                            doc.background = Some(bg);
                        }
                        Err(e) => log::error!("导入底图失败：{e}"),
                    }
                }
            }
        }

        log::info!(
            "启动完成：画布 {}x{}，图层 {} 个",
            doc.width,
            doc.height,
            doc.layers.len()
        );

        // 从设置恢复 3D 预览的状态：分栏比例、显示模式、模型显示选项。
        // 这些是"调整后应当记住"的参数，和笔刷半径同属一类。
        let mut mp = sdf_mask_tool::preview::ModelPanel::default();
        mp.wireframe = settings.model_wireframe;
        mp.backface_cull = settings.model_backface_cull;
        mp.show_uv_on_canvas = settings.show_uv_on_canvas;
        let vm = match settings.view_mode {
            0 => sdf_mask_tool::preview::ViewMode::CanvasOnly,
            1 => sdf_mask_tool::preview::ViewMode::ModelOnly,
            _ => sdf_mask_tool::preview::ViewMode::Both,
        };
        let split = settings.split_ratio.clamp(0.15, 0.85);

        let mut app = Self {
            doc,
            canvas: CanvasState::default(),
            step: 0,
            playhead: 0.0,
            show_mask: true,
            show_log: false,
            show_about: false,
            show_canvas_size: false,
            size_input: (1024, 1024),
            export_win: ui::export_window::ExportWindow::default(),
            color_win: ui::color_picker::ColorPickerState::default(),
            settings_win: ui::settings::SettingsWindow::default(),
            settings,
            status: String::new(),
            project_path: None,
            display_mask: Mask::new(1024, 1024),
            display_dirty: true,
            last_calc: None,
            export_invert: false,
            display_error: None,
            undo_stack: UndoStack::default(),
            violations: None,
            violations_key: None,
            violations_dir: sdf_mask_tool::model::nesting::NestDir::Grows,
            canvas_uv_key: None,
            model_panel: mp,
            view_mode: vm,
            split_ratio: split,
            baker: bake_bg::BackgroundBaker::new(),
            last_baked_fp: None,
            texel_snapshot: None,
            keyframe_clip: None,
        };
        app.size_input = (app.doc.width, app.doc.height);
        app.canvas.fit(&app.doc, egui::vec2(880.0, 600.0));
        // 笔刷半径也吃设置（新建工程时的默认值）
        app.canvas.set_radius(app.settings.default_radius);

        // 调试用：SDF_OPEN_EXPORT=1 时启动即打开导出窗口，便于无 GUI 交互的自动验证
        if std::env::var("SDF_OPEN_EXPORT").is_ok() {
            app.export_win.open_window(&app.doc);
        }
        // 同理：SDF_OPEN_SETTINGS=1 启动即开设置面板
        if std::env::var("SDF_OPEN_SETTINGS").is_ok() {
            app.settings_win.open_window(&app.doc, &app.settings);
        }
        // 调试用：SDF_TOOL=shape|lasso|select|smooth|brush|fill 指定启动工具。
        // 工具栏在切换工具后宽度会变（选项条内容不同），自动化点按钮时坐标会漂，
        // 这个开关让验证脚本直接落到目标工具，不必跟布局较劲。
        if let Ok(t) = std::env::var("SDF_TOOL") {
            app.canvas.tool = match t.as_str() {
                "shape" => ui::canvas_view::Tool::Shape,
                "lasso" => ui::canvas_view::Tool::Lasso,
                "select" => ui::canvas_view::Tool::Select,
                "smooth" => ui::canvas_view::Tool::Smooth,
                "fill" => ui::canvas_view::Tool::Fill,
                _ => ui::canvas_view::Tool::Brush,
            };
        }
        // 调试用：SDF_BRUSH=24,3 预设「笔刷半径,模糊半径」。
        // 滑条在工具栏上，自动化点击容易被相邻控件抢走（实测把笔刷拖到了 44px），
        // 用这个开关能精确落值。
        if let Ok(v) = std::env::var("SDF_BRUSH") {
            let n: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if !n.is_empty() {
                app.canvas.set_radius(n[0]);
            }
            if n.len() > 1 {
                app.canvas.smooth_radius = n[1].clamp(1.0, 20.0);
            }
        }
        // 调试用：SDF_SELECT_RECT=x0,y0,x1,y1 启动即建一个矩形选区
        if let Ok(v) = std::env::var("SDF_SELECT_RECT") {
            let n: Vec<f32> = v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            if n.len() == 4 {
                let m = sdf_mask_tool::selection::Selection::from_rect(
                    app.doc.width,
                    app.doc.height,
                    n[0],
                    n[1],
                    n[2],
                    n[3],
                );
                app.canvas.selection.replace(m);
            }
        }
        // 调试用：SDF_MODEL=<路径> 启动即加载 3D 模型，并切到「同时」显示。
        // 理由和 SDF_TOOL 一样：菜单项/DnD 在自动化里难触发，这个能直接落到目标状态。
        if let Ok(p) = std::env::var("SDF_MODEL") {
            let path = std::path::PathBuf::from(&p);
            app.model_panel.load(&path);
            if app.model_panel.has_mesh() {
                app.view_mode = sdf_mask_tool::preview::ViewMode::Both;
            }
        }
        // 调试用：SDF_VIEW=canvas|model|both 指定显示模式
        if let Ok(v) = std::env::var("SDF_VIEW") {
            app.view_mode = match v.as_str() {
                "canvas" => sdf_mask_tool::preview::ViewMode::CanvasOnly,
                "model" => sdf_mask_tool::preview::ViewMode::ModelOnly,
                _ => sdf_mask_tool::preview::ViewMode::Both,
            };
        }
        app
    }

    /// 处理拖进窗口的文件。
    ///
    /// - 图片（png/jpg/webp/bmp）→ 当底图
    /// - 工程文件（`io::is_project_path` 认定的扩展名）→ 打开工程
    ///
    /// 多个文件时只取第一个能识别的，并在状态栏说明跳过了哪些。
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let files: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if files.is_empty() {
            return;
        }

        let mut handled = false;
        let mut skipped = Vec::new();

        for p in files {
            if handled {
                skipped.push(p);
                continue;
            }
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();

            if sdf_mask_tool::io::is_project_path(&p) {
                match sdf_mask_tool::io::open_project(&p) {
                    Ok(d) => {
                        log::info!("拖入打开工程 {}", p.display());
                        self.doc = d;
                        self.goto_step(0);
                        self.display_mask = Mask::new(self.doc.width, self.doc.height);
                        self.last_calc = None;
                        self.undo_stack = UndoStack::default();
                        self.canvas.tex_bg_reset();
                        self.canvas.fit_requested = true;
                        self.export_win.invalidate();
                        self.reset_preview_caches();
                        self.touch();
                        self.project_path = Some(p);
                        self.say("已打开工程");
                        handled = true;
                    }
                    Err(e) => {
                        log::error!("拖入打开工程失败：{e}");
                        self.status = e;
                    }
                }
            } else if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp") {
                match sdf_mask_tool::io::load_background(&p, self.doc.width, self.doc.height) {
                    Ok(bg) => {
                        log::info!("拖入底图 {}", p.display());
                        self.doc.background = Some(bg);
                        self.doc.background_visible = true;
                        self.canvas.tex_bg_reset();
                        self.canvas.mark_dirty();
                        // 底图变了，3D 预览要重算
                        self.model_panel.invalidate();
                        self.say(format!(
                            "已导入底图：{}",
                            p.file_name().unwrap_or_default().to_string_lossy()
                        ));
                        handled = true;
                    }
                    Err(e) => {
                        log::error!("拖入底图失败：{e}");
                        self.status = e;
                    }
                }
            } else if sdf_mask_tool::model3d::is_model_path(&p) {
                // 3D 模型：只做预览，不改文档
                let name = p
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                self.model_panel.load(&p);
                if self.model_panel.has_mesh() {
                    self.ensure_canvas_visible_after_import();
                    let tris = self
                        .model_panel
                        .mesh
                        .as_ref()
                        .map(|m| m.tri_count)
                        .unwrap_or(0);
                    self.say(format!("已加载模型：{name}（{tris} 三角）"));
                    handled = true;
                } else {
                    self.status = self
                        .model_panel
                        .error
                        .clone()
                        .unwrap_or_else(|| "模型加载失败".into());
                }
            } else {
                skipped.push(p);
            }
        }

        if !skipped.is_empty() {
            let names: Vec<String> = skipped
                .iter()
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            log::warn!("拖入时跳过了不支持的文件：{}", names.join(", "));
            if !handled {
                self.status = format!("不支持的文件：{}", names.join(", "));
            }
        }
    }

    /// 导入模型成功后，保证画布没有被藏起来。
    ///
    /// # 为什么不能只在 CanvasOnly 时切换
    ///
    /// 早期写法是"若当前是仅画布，则切到同时"。它漏掉了 `ModelOnly`：
    /// 一旦显示模式停在那里（误选了下拉框、或上设置文件残留），
    /// 导入后画面只剩 3D 模型，画布整块宽度归零——而绘制只能在画布上，
    /// 看起来就像"画布没了"。
    ///
    /// 这里改成无条件保证：只要不是"同时"，就切到"同时"。
    /// 用户想只看模型，导入后再手动切一次即可，不会卡在无法绘制的状态。
    fn ensure_canvas_visible_after_import(&mut self) {
        use sdf_mask_tool::preview::ViewMode;
        if self.view_mode != ViewMode::Both {
            let was = self.view_mode;
            self.view_mode = ViewMode::Both;
            if was == ViewMode::ModelOnly {
                // 明说一句，避免"我明明选的只看模型，怎么变了"
                self.say("已切到「同时」显示：绘制需要画布");
            }
        }
        // 比例过小同样会让画布看起来"没有"：极端窄条与没有区别。
        // 夹到能实际绘制的最小比例。
        if self.split_ratio < 0.25 {
            self.split_ratio = 0.25;
        }
    }

    fn angle(&self) -> f32 {
        step_angle(self.step, self.doc.steps)
    }

    fn is_on_keyframe(&self) -> bool {
        self.doc
            .active_channel_ref()
            .has_kf_at_step(self.step, self.doc.steps)
    }

    fn touch(&mut self) {
        self.display_dirty = true;
        self.canvas.mark_dirty();
    }

    /// 换文档（新建 / 打开 / 撤销到另一份）时，把所有跟"上一份文档"绑定的
    /// 预览缓存清干净。
    ///
    /// **不清会闪出错误的图**：`texel_snapshot` 存的是上一份文档的 texel 图，
    /// 而渲染只比对指纹——新旧文档指纹不同，但 snapshot 仍会被当作
    /// "可以顶上的旧图"先画出来。表现是打开工程时闪一下上一个工程的画面。
    fn reset_preview_caches(&mut self) {
        self.texel_snapshot = None;
        self.last_baked_fp = None;
        self.last_calc = None;
        self.display_dirty = true;
        self.canvas.mark_dirty();
    }

    /// 由违规检测导出"数据不合法"的提示文案。
    ///
    /// 原来这条提示只在 `texels()` 烘焙失败时出现。但现在停在关键帧上会跳过烘焙
    /// （那是 137ms 的白烧），提示就会凭空消失。而"数据不合法"是数据本身的性质，
    /// 与要不要烘焙无关——已验证 `violations.count==0` ⟺ 烘焙成功（8 个用例）。
    ///
    /// 文案刻意与烘焙错误保持同样的关键词（"来回翻转"/"冲突"），
    /// 因为 `display_error` 在状态栏那段代码里按关键词分流。
    fn violation_error(&self) -> Option<String> {
        let v = self.violations.as_ref()?;
        if v.count == 0 {
            return None;
        }
        Some(format!(
            "有像素的明暗来回翻转，两种方向都编码不了。当前违规 {} 个像素（共 {}）。\n\
             每个像素在角度轴上只能翻转一次：要么一直受光后转暗，要么一直遮着后转亮，不能来回变。",
            v.count, v.total
        ))
    }

    // ------------------------------------------------------------ 显示遮罩

    fn refresh_display(&mut self) {
        let step = self.step;
        let steps = self.doc.steps;
        let angle = self.angle();

        let fp: u64 = {
            let ch = self.doc.active_channel_ref();
            let mut acc: u64 = ch.frames.len() as u64;
            for f in ch.frames.iter() {
                acc = acc.wrapping_mul(1099511628211) ^ f.mask.fingerprint();
            }
            acc
        };
        // 缓存键用吸附步号：连续拖动播放头时，同一格内不重复算。
        // 视觉上的连续来自播放头的绘制位置，不是每帧都重算图像。
        let key = (fp, step, self.doc.active_layer, self.doc.active_channel);
        if !self.display_dirty && self.last_calc == Some(key) {
            return;
        }

        let (li, ri) = self.doc.active_channel_ref().bracket(angle);
        if li.is_none() && ri.is_none() {
            self.last_calc = Some(key);
            self.display_dirty = false;
            self.display_mask.clear();
            return;
        }

        // 停在关键帧上就直接显示该帧本身——**不需要烘焙**。
        //
        // 这是纯粹的等价替换，不是近似：任意 texel 图在其编码角度上采样，
        // 结果必然还原出原始遮罩（等价性验证见 verify_keyframe_bake，4 个用例全为 0 像素差）。
        // 而烘焙一次 1K 四帧要 137ms、8 帧要 366ms——加关键帧后播放头正好停在新帧上，
        // 付这个代价算出一张和手里一模一样的图，然后直接丢掉。
        //
        // 原来这里有个 `is_stroking()` 前置条件（落笔中才走快路径），
        // 于是"加完关键帧"这个最常见的停下场景被排除在外，白烧一次全量烘焙。
        // 现在无条件走：落笔中、加帧后、手动拖到某帧，都是同一条路径。
        //
        // 关键帧的角度是连续的，所以这里必须按**步号**找（角度精确匹配会落空）。
        if self.is_on_keyframe() {
            if let Some(f) = self.doc.active_channel_ref().frame_for_step(step, steps) {
                self.display_mask = f.mask.clone();
                if self.export_invert {
                    // 采样路径会施加反向，快路径绕过了采样，这里补齐。
                    // 目前主预览的这个开关恒为 false（没有 UI 接该开关），
                    // 但保持语义正确，免得将来接上时出现"反向了但预览不变"。
                    self.display_mask.invert();
                }
                // **不能在这里把 display_error 清空。**
                // 跳过烘焙后就失去了"烘焙失败"这个信号，但"数据不合法"这个事实
                // 与烘焙无关（violations.count==0 ⟺ bake 成功，已用 8 个用例验证）。
                // 所以错误提示改为从违规检测导出，否则停在关键帧上时提示会凭空消失，
                // "数据不合法"的提示就不会出现。
                self.display_error = self.violation_error();
                self.last_calc = Some(key);
                self.display_dirty = false;
                return;
            }
        }

        // 预览走和导出同一套算法：把每个像素的 texel 当唯一状态，
        // 任意时刻只做一次阈值比较。这样"看到的"就是"导出的"。
        let t = self.playhead.clamp(0.0, 1.0);
        let (dw, dh) = (self.doc.width, self.doc.height);

        // **后台烘焙：把"算"和"显示"分开。**
        //
        // 三级策略：
        //  1. 快照指纹 == 当前指纹 → 直接采样（最常见）
        //  2. 指纹不符但手里有旧图 → 用旧图顶上 + 派发后台重算
        //  3. 什么都没有（首次）→ 只能同步烘一次
        //
        // 第 2 步是手感的关键：加点后指纹变了，但不应为重新烘焙而等待
        // 50–90ms。旧图与真值的采样差异实测最大 0.047%（492/1M 像素），
        // 肉眼无感；真值几十毫秒后由 `poll_background_bake` 悄悄换上。
        let fp = self.doc.active_channel_ref().channel_fingerprint();

        // **先判断再准备数据**：`bake_job_frames()` 要拷全部遮罩（8 帧 8MB），
        // 而这里是每帧都跑的。不先问一句，就等于每帧白烧一次 8MB 拷贝。
        //
        // **完全没有缓存时不要派发**：那种情况下面会同步烘一次，
        // 再派发后台任务就等于同一件事算两遍（启动时实测白烧 25ms）。
        // 有缓存才派发——那才是"用旧图顶着，后台补新的"。
        let has_any =
            self.texel_snapshot.is_some() || self.doc.active_channel_ref().has_texel_cache();
        if has_any && self.last_baked_fp != Some(fp) && !self.baker.is_running(fp) {
            let frames = self.doc.active_channel_ref().bake_job_frames();
            self.baker.request(fp, frames, dw as usize, dh as usize);
        }

        // 快照只在指纹变化时刷新（避免每格拖动都 clone 2MB）。
        //
        // **必须校验指纹**：`cached_texels()` 故意不检查指纹，返回的是
        // "可能略旧"的图（供后台烘焙期间顶着用）。若把该图贴上**当前**指纹，
        // 快照的指纹就会与内容不符，于是：
        //   - 采样永远用旧图 → 改完图后播放毫无变化
        //   - `last_baked_fp` 被误导，后台烘焙不再派发 → 永远不更新
        //
        // 实测过的症状：新建工程后在 90° 画一笔，"从空帧长出来"这个
        // 方向完全不工作，画面停在画之前。原因就是旧图被贴了新指纹。
        if self.texel_snapshot.as_ref().map(|(sfp, _, _)| *sfp) != Some(fp)
            && let Some((tx, d)) = self.doc.active_channel_ref().cached_texels()
        {
            // 只有缓存**确实属于当前内容**时才采纳；否则宁可没有快照，
            // 让该分支走下面的同步烘焙（或等后台结果回来）。
            let cache_is_current = self.doc.active_channel_ref().cache_matches(fp);
            if cache_is_current {
                self.texel_snapshot = Some((fp, tx, d));
            }
        }

        // 有快照就直接采样（借用，不 clone）。
        // 走到这里说明快照指纹 == 当前指纹，内容一定是最新的。
        if let Some((_, tx, d)) = &self.texel_snapshot {
            sdf::sample_texels(tx, t, *d, &mut self.display_mask, self.export_invert);
            // 数据合法性由违规检测负责（已验证与烘焙判定等价），
            // 不因为"用的是旧图"而误报。
            self.display_error = self.violation_error();
            self.last_calc = Some(key);
            self.display_dirty = false;
            return;
        }

        // 快照过期或不存在。分两种情况：
        //
        // a) 有旧缓存 → **临时顶着用**（后台正在烘），画面会短暂落后一点点，
        //    但绝不阻塞 UI。这是手感的关键：改完图不等几十毫秒。
        // b) 连旧缓存都没有（首次）→ 只能同步烘一次。
        //
        // 注意 (a) 里**不能**把这份旧图存进 `texel_snapshot` 并贴上当前指纹
        // ——那正是上一版"画完播放毫无变化"的根因。
        if let Some((tx, d)) = self.doc.active_channel_ref().cached_texels() {
            sdf::sample_texels(&tx, t, d, &mut self.display_mask, self.export_invert);
            self.display_error = self.violation_error();
            self.last_calc = Some(key);
            self.display_dirty = false;
            return;
        }

        // 首次：没有任何可用的图，同步烘一次（每个通道一生一次）
        let t0 = std::time::Instant::now();
        match self.doc.active_channel_mut().texels(dw, dh) {
            Ok((tx, d)) => {
                log::info!("首次烘焙 {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
                sdf::sample_texels(&tx, t, d, &mut self.display_mask, self.export_invert);
                self.texel_snapshot = Some((fp, tx, d));
                self.last_baked_fp = Some(fp);
                self.display_error = self.violation_error();
            }
            Err(_) => {
                // 首次烘焙就失败：数据不合法，降级显示当前帧。
                //
                // **关键：这里绝不能直接 return 留旧画面。**
                // 违规时也必须能看清各参数点画了什么，
                // 否则想手动修都没法看——那是比错误本身更糟的状态。
                self.display_error = self.violation_error();
                if let Some(f) = self.doc.active_channel_ref().frame_for_step(step, steps) {
                    self.display_mask = f.mask.clone();
                } else {
                    // 不在关键帧上：找最近的一个关键帧顶上，
                    // 保证画面永远有东西可看，而不是空白。
                    let ch = self.doc.active_channel_ref();
                    let angle = self.angle();
                    let (l, r) = ch.bracket(angle);
                    let pick = l
                        .or(r)
                        .or_else(|| if ch.frames.is_empty() { None } else { Some(0) });
                    if let Some(i) = pick {
                        self.display_mask = ch.frames[i].mask.clone();
                    }
                }
            }
        }
        // 也要记 key，否则每帧都重新尝试烘焙
        self.last_calc = Some(key);
        self.display_dirty = false;
    }

    /// 每帧调一次：把后台算好的结果装进来。
    ///
    /// 指纹对不上（等待期间文档又被改动）时结果会被丢弃，
    /// 此时 `last_baked_fp` 不更新，下一帧会重新派发。
    fn poll_background_bake(&mut self, ctx: &egui::Context) {
        let fp = self.doc.active_channel_ref().channel_fingerprint();
        if let Some(r) = self.baker.poll(fp) {
            let (dw, dh) = (self.doc.width, self.doc.height);
            let ms = r.ms;
            if self
                .doc
                .active_channel_mut()
                .install_baked(r.fp, r.res, dw, dh)
            {
                self.last_baked_fp = Some(r.fp);
                // **立刻刷新本地快照**，否则 `refresh_display` 下一帧读到的
                // 还是旧快照（旧快照的指纹与当前不符 → 会走"旧缓存临时顶上"分支），
                // 表现就是"后台算完了但画面不变"。
                self.texel_snapshot = self
                    .doc
                    .active_channel_ref()
                    .cached_texels()
                    .map(|(tx, d)| (r.fp, tx, d));
                self.display_dirty = true;
                self.canvas.mark_dirty();
                // 有结果就重画一次，才能看到更新后的插值
                log::info!("后台烘焙完成：{ms:.1} ms");
            }
        }
        // 还有任务在跑 → 请求下一次重绘，好及时取回结果。
        // 不能只靠"结果回来自然有事件"——egui 是事件驱动的，
        // 不产生输入事件就不会有下一帧，结果会一直留在 channel 里无人读取。
        if self.baker.is_busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }

    /// 跳到某个吸附步，并同步连续播放头。
    ///
    /// `step` 和 `playhead` 必须永远一致：前者决定计算与吸附，后者决定播放头画在哪。
    /// 任何改 step 的地方都要走这里，否则两者会脱节（表现为"点了下一帧但播放头没动"）。
    fn goto_step(&mut self, s: usize) {
        let s = s.min(self.doc.steps - 1);
        self.step = s;
        self.playhead = step_norm(s, self.doc.steps);
        self.display_dirty = true;
    }

    /// 跳到某个角度对应的吸附步
    fn goto_angle(&mut self, angle: f32) {
        self.goto_step(angle_to_step(angle, self.doc.steps));
    }

    // ------------------------------------------------------------ 动作分发

    fn run(&mut self, action: Action) {
        match action {
            Action::NewProject => {
                let (w, h, steps) = (
                    self.settings.default_canvas_w,
                    self.settings.default_canvas_h,
                    self.settings.default_steps,
                );
                self.doc = Doc::with_steps(w, h, steps);
                self.doc.seed_default_frames();
                self.project_path = None;
                self.goto_step(0);
                self.display_mask = Mask::new(w, h);
                self.canvas.tex_bg_reset();
                self.canvas.fit_requested = true;
                self.canvas.set_radius(self.settings.default_radius);
                self.undo_stack = UndoStack::default();
                self.reset_preview_caches();
                self.touch();
                self.say(format!("已新建工程（{w}×{h}，{steps} 帧）"));
            }
            Action::OpenProject => self.open_project(),
            Action::SaveProject | Action::SaveProjectAs => self.save_project(action),
            Action::ImportBackground => self.import_background(),
            Action::ImportModel => self.import_model(),
            Action::FocusModel => {
                if self.model_panel.has_mesh() {
                    // 聚焦时把显示模式切到能看见模型的那一档，
                    // 否则按 F 不产生任何效果（面板根本没有显示）。
                    if self.view_mode == sdf_mask_tool::preview::ViewMode::CanvasOnly {
                        self.view_mode = sdf_mask_tool::preview::ViewMode::Both;
                    }
                    self.model_panel.focus();
                    self.say("已聚焦模型");
                }
            }
            Action::ResetModelView => {
                if self.model_panel.has_mesh() {
                    if self.view_mode == sdf_mask_tool::preview::ViewMode::CanvasOnly {
                        self.view_mode = sdf_mask_tool::preview::ViewMode::Both;
                    }
                    self.model_panel.focus_reset();
                    self.say("已复位模型视角");
                }
            }
            Action::UnloadModel => {
                if self.model_panel.has_mesh() {
                    self.model_panel.unload();
                    self.say("已卸载 3D 模型");
                }
            }
            Action::SetViewMode(m) => {
                self.view_mode = m;
            }
            Action::ExportCurrentLayer => {
                // 打开导出窗口：先展示位深/反向/预览，再执行导出
                self.export_win.invalidate();
                self.export_win.open_window(&self.doc);
            }
            Action::ExportAllLayers => {
                self.export_win.invalidate();
                self.export_win.open_window(&self.doc);
            }
            Action::Quit => {
                log::info!("退出请求");
                std::process::exit(0);
            }

            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::FillFrame => {
                let (s, steps) = (self.step, self.doc.steps);
                if self.is_on_keyframe() {
                    self.push_undo("填满本帧");
                    self.doc.fill_active_frame(s, steps, 1);
                    self.touch();
                    self.say("已填满本帧");
                } else {
                    self.say("当前不是参数点，先加一个参数点再填充");
                }
            }
            Action::EraseFrame => {
                let (s, steps) = (self.step, self.doc.steps);
                if self.is_on_keyframe() {
                    self.push_undo("擦除本帧");
                    self.doc.fill_active_frame(s, steps, 0);
                    self.touch();
                    self.say("已擦除本帧");
                } else {
                    self.say("当前不是参数点");
                }
            }
            Action::InvertFrame => {
                let (s, steps) = (self.step, self.doc.steps);
                if self.is_on_keyframe() {
                    self.push_undo("反相本帧");
                    self.doc.invert_active_frame(s, steps);
                    self.touch();
                    self.say("已反相本帧");
                } else {
                    self.say("当前不是参数点");
                }
            }
            Action::DuplicateLayer => {
                let i = self.doc.active_layer;
                self.push_undo("复制图层");
                if self.doc.duplicate_layer(i).is_some() {
                    self.touch();
                    self.say("已复制图层");
                } else {
                    self.undo_stack.undo.pop();
                    self.say("图层不存在");
                }
            }
            Action::DeleteLayer => {
                let i = self.doc.active_layer;
                if self.doc.layers.len() <= 1 {
                    self.say("至少保留一个图层");
                } else {
                    self.push_undo("删除图层");
                    self.doc.remove_layer(i);
                    self.touch();
                    self.say("已删除图层");
                }
            }
            Action::ClearLayer => {
                let i = self.doc.active_layer;
                self.push_undo("清空图层内容");
                self.doc.clear_layer(i);
                self.touch();
                self.say("已清空图层内容");
            }
            Action::InvertLayer => {
                let i = self.doc.active_layer;
                self.push_undo("反相图层");
                self.doc.invert_layer(i);
                self.touch();
                self.say("已反相整个图层（可 Ctrl+Z 撤销）");
            }
            Action::MirrorLayer => {
                let i = self.doc.active_layer;
                self.push_undo("镜像图层");
                self.doc.mirror_layer(i);
                self.touch();
                self.say("已左右镜像整个图层（可 Ctrl+Z 撤销）");
            }
            Action::OpenSettings => {
                self.settings_win.open_window(&self.doc, &self.settings);
            }

            Action::FitToWindow => {
                self.canvas.fit_requested = true;
                self.say("已适应窗口");
            }
            Action::ZoomIn => {
                self.canvas.zoom = (self.canvas.zoom * 1.25).min(32.0);
            }
            Action::ZoomOut => {
                self.canvas.zoom = (self.canvas.zoom / 1.25).max(0.02);
            }
            Action::ZoomActual => self.canvas.zoom = 1.0,
            Action::ToggleMaskOverlay => self.show_mask = !self.show_mask,
            Action::ToggleBackground => {
                self.doc.background_visible = !self.doc.background_visible;
                self.canvas.mark_dirty();
            }
            Action::ToggleUvOverlay => {
                // 没有模型或该模型没有 UV 时，说明为什么按了没反应，
                // 而不是静默什么都不做。
                if !self.model_panel.has_mesh() {
                    self.say("还没有导入模型");
                } else if self.model_panel.uv_set_count() == 0 {
                    self.say("这个模型没有 UV");
                } else {
                    self.model_panel.show_uv_on_canvas = !self.model_panel.show_uv_on_canvas;
                    self.say(if self.model_panel.show_uv_on_canvas {
                        "画布已叠加 UV 线框"
                    } else {
                        "画布已隐藏 UV 线框"
                    });
                }
            }

            Action::AddLayer => {
                let n = self.doc.layers.len() + 1;
                self.push_undo("新建图层");
                self.doc.add_layer(&format!("图层 {n}"));
                self.doc.seed_default_frames();
                self.touch();
                self.say("已新建图层");
            }
            Action::LayerUp => {
                let i = self.doc.active_layer;
                if i > 0 {
                    self.push_undo("上移图层");
                    self.doc.move_layer(i, i - 1);
                    self.touch();
                }
            }
            Action::LayerDown => {
                let i = self.doc.active_layer;
                if i + 1 < self.doc.layers.len() {
                    self.push_undo("下移图层");
                    self.doc.move_layer(i, i + 1);
                    self.touch();
                }
            }
            Action::NextLayer => {
                if self.doc.active_layer + 1 < self.doc.layers.len() {
                    self.doc.active_layer += 1;
                    self.touch();
                }
            }
            Action::PrevLayer => {
                if self.doc.active_layer > 0 {
                    self.doc.active_layer -= 1;
                    self.touch();
                }
            }
            Action::NextChannel => {
                self.doc.active_channel = (self.doc.active_channel + 1) % 4;
                self.touch();
            }
            Action::PrevChannel => {
                self.doc.active_channel = (self.doc.active_channel + 3) % 4;
                self.touch();
            }

            Action::AddKeyframe => {
                // 落在**当前播放头的连续位置**，不是吸附格。
                // 参数点精确落在当前播放头位置（仅视觉，不对齐刻度）。
                let a = (self.playhead).clamp(0.0, 1.0) * 90.0;
                self.do_add_keyframe_at(a);
            }
            Action::AddKeyframeAtNorm(t) => {
                let a = (t * 90.0).clamp(0.0, 90.0);
                self.do_add_keyframe_at(a);
            }
            Action::DeleteKeyframe => {
                let (s, steps) = (self.step, self.doc.steps);
                if self.doc.active_channel_ref().has_kf_at_step(s, steps) {
                    let n_before = self.doc.active_channel_ref().frames.len();
                    if n_before <= 2 {
                        self.say("至少保留两个参数点，否则无法导出");
                    } else {
                        self.push_undo("删除参数点");
                        self.doc.active_channel_mut().remove_frame_at_step(s, steps);
                        self.touch();
                        self.say("已删除参数点");
                    }
                } else {
                    self.say("当前不是参数点");
                }
            }
            Action::DeleteKeyframeAtAngle(a) => {
                let steps = self.doc.steps;
                self.do_delete_keyframe_at_step(angle_to_step(a, steps));
            }
            Action::DuplicateKeyframe => {
                // 兼容旧引用：等同于"原位复制到下一个空位"
                let (s, steps) = (self.step, self.doc.steps);
                self.do_duplicate_keyframe(s, steps);
            }
            Action::CopyKeyframe => self.do_copy_keyframe(),
            Action::PasteKeyframe => self.do_paste_keyframe(),
            // 旧行为：复制到紧邻空位（Shift+D）。语义与 Copy 不同，故分开。
            Action::DuplicateKeyframeAdjacent => {
                let (s, steps) = (self.step, self.doc.steps);
                self.do_duplicate_keyframe(s, steps);
            }
            Action::DuplicateKeyframeAtAngle(a) => {
                let steps = self.doc.steps;
                self.do_duplicate_keyframe(angle_to_step(a, steps), steps);
            }
            Action::ClearAllKeyframes => {
                let n = self.doc.active_channel_ref().frames.len();
                if n == 0 {
                    self.say("这个通道没有参数点");
                } else {
                    self.push_undo("清空全部参数点");
                    self.doc.active_channel_mut().frames.clear();
                    self.doc.active_channel_mut().clear_dist_cache();
                    // 清空后补回两端各一帧，保证还能继续画
                    self.doc.seed_default_frames();
                    self.goto_step(0);
                    self.touch();
                    self.say("已清空参数点，并重置为两端空白点");
                }
            }
            Action::ReverseTimeline => {
                let n = self.doc.active_channel_ref().frames.len();
                if n < 2 {
                    self.say("至少需要两个参数点才能反转");
                } else {
                    self.push_undo("反转参数轴");
                    self.doc.reverse_channel_timeline();
                    // 反转后所有角度都镜像了，播放头也跟着翻到另一头，
                    // 不然会误判为"画的内容消失了"
                    self.playhead = 1.0 - self.playhead;
                    self.step = self.doc.steps - 1 - self.step.min(self.doc.steps - 1);
                    self.touch();
                    self.export_win.invalidate();
                    self.say("已反转参数轴（可 Ctrl+Z 撤销）");
                }
            }
            Action::MoveKeyframeBlocked => {
                self.say("目标位置已有别的参数点，没有移动");
            }
            Action::MoveKeyframeNotice(a) => {
                self.say(format!("参数点移到 {:.3}", a / 90.0));
            }
            Action::PrevKeyframe => {
                let (s, steps) = (self.step, self.doc.steps);
                match self.doc.prev_keyframe(s, steps) {
                    Some(a) => {
                        self.goto_angle(a);
                        self.touch();
                    }
                    None => self.say("前面没有参数点了"),
                }
            }
            Action::NextKeyframe => {
                let (s, steps) = (self.step, self.doc.steps);
                match self.doc.next_keyframe(s, steps) {
                    Some(a) => {
                        self.goto_angle(a);
                        self.touch();
                    }
                    None => self.say("后面没有参数点了"),
                }
            }
            Action::PrevStep => {
                self.goto_step(self.step.saturating_sub(1));
                self.touch();
            }
            Action::NextStep => {
                self.goto_step(self.step + 1);
                self.touch();
            }
            Action::GotoStart => {
                self.goto_step(0);
                self.touch();
            }
            Action::GotoEnd => {
                self.goto_step(self.doc.steps - 1);
                self.touch();
            }
            Action::CanvasSize => {
                self.size_input = (self.doc.width, self.doc.height);
                self.show_canvas_size = true;
            }

            Action::ToolBrush => self.canvas.tool = Tool::Brush,
            Action::ToolFill => self.canvas.tool = Tool::Fill,
            Action::ToolSelect => self.canvas.tool = Tool::Select,
            Action::ToolSmooth => self.canvas.tool = Tool::Smooth,
            Action::ToolShape => self.canvas.tool = Tool::Shape,
            Action::ToolLasso => self.canvas.tool = Tool::Lasso,
            Action::ToggleShapeKind => {
                self.canvas.shape = self.canvas.shape.toggled();
                self.canvas.tool = Tool::Shape;
                self.say(format!("形状改为{}", self.canvas.shape.name()));
            }
            Action::ToggleShapeFill => {
                self.canvas.shape_filled = !self.canvas.shape_filled;
                self.say(if self.canvas.shape_filled {
                    "形状改为实心"
                } else {
                    "形状改为空心"
                });
            }
            Action::ToggleSelectMode => {
                self.canvas.select_mode = self.canvas.select_mode.toggled();
                self.canvas.tool = Tool::Lasso;
                self.say(format!("选区工具改为{}", self.canvas.select_mode.name()));
            }
            Action::CycleSelectOp => {
                self.canvas.select_op = match self.canvas.select_op {
                    ui::canvas_view::SelectOp::Replace => ui::canvas_view::SelectOp::Add,
                    ui::canvas_view::SelectOp::Add => ui::canvas_view::SelectOp::Subtract,
                    ui::canvas_view::SelectOp::Subtract => ui::canvas_view::SelectOp::Replace,
                };
                self.say(format!("选区模式：{}", self.canvas.select_op.name()));
            }
            Action::SelectAll => {
                let (w, h) = (self.doc.width, self.doc.height);
                self.canvas.selection.select_all(w, h);
                self.canvas.edge_cache_reset();
                self.canvas.mark_dirty();
                self.say("已全选");
            }
            Action::DeselectAll => {
                self.canvas.selection.clear();
                self.canvas.edge_cache_reset();
                self.canvas.mark_dirty();
                self.say("已取消选区");
            }
            Action::InvertSelection => {
                let (w, h) = (self.doc.width, self.doc.height);
                self.canvas.selection.invert(w, h);
                self.canvas.edge_cache_reset();
                self.canvas.mark_dirty();
                self.say("已反选");
            }
            Action::TogglePaintValue => {
                self.canvas.value = self.canvas.value.toggled();
                self.say(format!("笔刷改为{}色", self.canvas.value.name()));
            }
            Action::BrushSizeDown => {
                self.canvas.scale_radius(1.0 / 1.25);
                self.say(format!("笔刷 {:.0} px", self.canvas.radius));
            }
            Action::BrushSizeUp => {
                self.canvas.scale_radius(1.25);
                self.say(format!("笔刷 {:.0} px", self.canvas.radius));
            }
            Action::BrushSize1 => {
                self.canvas.set_radius(6.0);
                self.say("笔刷 6 px");
            }
            Action::BrushSize2 => {
                self.canvas.set_radius(24.0);
                self.say("笔刷 24 px");
            }
            Action::BrushSize3 => {
                self.canvas.set_radius(80.0);
                self.say("笔刷 80 px");
            }

            Action::About => self.show_about = true,
        }
    }

    /// 一键修复：把当前通道的所有关键帧改成层层嵌套。
    ///
    /// 修复会改动绘制内容，所以必须进撤销栈。
    fn do_repair_nesting(&mut self, dir: sdf_mask_tool::model::nesting::NestDir) {
        let (w, h) = (self.doc.width, self.doc.height);
        let li = self.doc.active_layer;
        let ci = self.doc.active_channel;

        // 先把所有帧的遮罩取出来，改完再写回（避免借用冲突）
        let mut masks: Vec<Mask> = self.doc.layers[li].channels[ci]
            .frames
            .iter()
            .map(|f| f.mask.clone())
            .collect();

        if masks.is_empty() {
            self.say("这个通道没有参数点");
            return;
        }

        self.push_undo(format!("修复嵌套（{}）", dir.label()));
        let changed =
            sdf_mask_tool::model::nesting::repair(&mut masks, dir, w as usize, h as usize);

        for (i, m) in masks.into_iter().enumerate() {
            self.doc.layers[li].channels[ci].frames[i].mask = m;
        }
        self.doc.layers[li].channels[ci].clear_dist_cache();

        self.export_win.invalidate();
        self.touch();
        self.say(format!(
            "已修复为「{}」，改动 {changed} 个像素（可 Ctrl+Z 撤销）",
            dir.label()
        ));
    }

    fn say(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        log::info!("{m}");
        self.status = m;
    }

    // ------------------------------------------------------------ 文件操作

    /// 导入 3D 模型（只预览，不进工程数据）。
    fn import_model(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("3D 模型", &["obj", "fbx"])
            .set_title("选择 3D 模型（仅用于预览）")
            .pick_file()
        else {
            return;
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        self.model_panel.load(&path);
        if self.model_panel.has_mesh() {
            self.ensure_canvas_visible_after_import();
            let tris = self
                .model_panel
                .mesh
                .as_ref()
                .map(|m| m.tri_count)
                .unwrap_or(0);
            self.say(format!("已加载模型：{name}（{tris} 三角）"));
        } else {
            self.status = self
                .model_panel
                .error
                .clone()
                .unwrap_or_else(|| "模型加载失败".into());
        }
    }

    fn import_background(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("图片", &["png", "jpg", "jpeg", "webp", "bmp"])
            .pick_file()
        else {
            return;
        };
        match sdf_mask_tool::io::load_background(&path, self.doc.width, self.doc.height) {
            Ok(bg) => {
                log::info!("导入底图：{}", path.display());
                self.doc.background = Some(bg);
                self.doc.background_visible = true;
                self.canvas.tex_bg_reset();
                self.canvas.mark_dirty();
                self.say(format!(
                    "已导入：{}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            Err(e) => {
                log::error!("导入底图失败：{e}");
                self.status = e;
            }
        }
    }

    fn save_project(&mut self, action: Action) {
        let path = if action == Action::SaveProjectAs || self.project_path.is_none() {
            let Some(p) = rfd::FileDialog::new()
                .add_filter("SDF 遮罩工程", &[sdf_mask_tool::io::PROJECT_EXT])
                .set_file_name(format!("project.{}", sdf_mask_tool::io::PROJECT_EXT))
                .save_file()
            else {
                return;
            };
            p
        } else {
            self.project_path.clone().unwrap()
        };
        match sdf_mask_tool::io::save_project(&self.doc, &path) {
            Ok(()) => {
                log::info!("保存工程 → {}", path.display());
                self.status = "已保存".into();
                self.project_path = Some(path);
            }
            Err(e) => {
                log::error!("保存失败：{e}");
                self.status = e;
            }
        }
    }

    fn open_project(&mut self) {
        // 打开时把旧扩展名一并列入筛选器：改名前存下的工程仍能在对话框里看到
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "SDF 遮罩工程",
                &[
                    sdf_mask_tool::io::PROJECT_EXT,
                    sdf_mask_tool::io::PROJECT_EXT_LEGACY,
                ],
            )
            .pick_file()
        else {
            return;
        };
        match sdf_mask_tool::io::open_project(&path) {
            Ok(doc) => {
                log::info!("打开工程 {}", path.display());
                self.doc = doc;
                self.goto_step(0);
                // display_mask 尺寸必须跟新文档一致，否则下一帧插值会越界。
                // 972 是这个字段唯一的使用者，在换文档的地方一并重建。
                self.display_mask = Mask::new(self.doc.width, self.doc.height);
                self.canvas.tex_bg_reset();
                self.canvas.fit_requested = true;
                self.reset_preview_caches();
                self.touch();
                self.status = "已打开".into();
                self.project_path = Some(path);
            }
            Err(e) => {
                log::error!("打开失败：{e}");
                self.status = e;
            }
        }
    }

    fn undo(&mut self) {
        match self.undo_stack.undo(&mut self.doc, self.step) {
            Some((doc, step, label)) => {
                self.doc = doc;
                self.goto_step(step);
                self.after_undo_redo();
                self.say(format!("已撤销：{label}"));
            }
            None => self.say("没有可撤销的操作"),
        }
    }

    fn redo(&mut self) {
        match self.undo_stack.redo(&mut self.doc, self.step) {
            Some((doc, step, label)) => {
                self.doc = doc;
                self.goto_step(step);
                self.after_undo_redo();
                self.say(format!("已重做：{label}"));
            }
            None => self.say("没有可重做的操作"),
        }
    }

    /// 撤销/重做之后要一起收拾的缓存与显示状态
    fn after_undo_redo(&mut self) {
        // 文档整个换掉了，显示遮罩的尺寸也要跟着走
        if self.display_mask.width() != self.doc.width
            || self.display_mask.height() != self.doc.height
        {
            self.display_mask = Mask::new(self.doc.width, self.doc.height);
        }
        self.last_calc = None;
        self.canvas.undo_stack.clear();
        self.canvas.redo_stack.clear();
        self.export_win.invalidate();
        // 撤销/重做是把整份文档换掉，预览缓存必须一起丢——
        // 否则会先画出"上一个状态"的 texel 图再跳变。
        self.reset_preview_caches();
        self.touch();
    }

    /// 在改动文档之前调一次，把当前状态压进撤销栈。
    ///
    /// 注意：这条快照记录的是"改动**之前**"的状态，所以要在改之前调。
    /// `label` 会显示在"已撤销：xxx"里，写清楚是什么操作。
    fn push_undo(&mut self, label: impl Into<String>) {
        let label = label.into();
        self.undo_stack.push(&self.doc, self.step, label);
    }

    /// 加关键帧的统一入口：落在**精确的连续角度**上，遮罩按设置决定是否用插值起点。
    ///
    /// 两条约束都在这里：
    /// - "准确落到当前的位置（仅视觉）"：角度不做吸附
    /// - "从空的开始画要改成创建自动插值的 mask"：起点是该角度的插值结果
    fn do_add_keyframe_at(&mut self, angle: f32) {
        let a = angle.clamp(0.0, 90.0);
        if self.doc.angle_taken(a, None) {
            self.say("这个位置已经有参数点了");
            return;
        }
        let interp = self.settings.interp_new_keyframe;
        let n_before = self.doc.active_channel_ref().frames.len();

        // 起点直接取"屏幕上现在显示的那张图"，不再重算。
        //
        // `interpolated_mask_at` 要现算两次全幅距离变换（1K 下 59ms），
        // 但那个位置此刻**正显示在画布上**——`display_mask` 已经是该位置的采样结果。
        // 加关键帧绝大多数情况就是"停在某处按下加帧"，两者角度完全重合。
        //
        // 只在角度对得上时才复用（容差取半步以内的极小值）：
        // 从菜单/时间轴右键加帧时角度可能与播放头不同，那就老实重算。
        //
        // 顺带修掉一个隐藏的不一致：`interpolated_mask_at` 走连续距离场，
        // 而预览/导出走 texel 量化路径，实测两者最大差 152 像素（0.0145%）。
        // 用 display_mask 当起点，"加帧前后的画面"才是真正逐像素连续的。
        let start_mask = if interp {
            let same_angle = (a - self.angle()).abs() < 1e-3;
            if same_angle && !self.display_mask.data().is_empty() {
                Some(self.display_mask.clone())
            } else {
                None
            }
        } else {
            None
        };

        self.push_undo(format!("在 {:.3} 加参数点", a / 90.0));
        let m = match start_mask {
            Some(m) => m,
            None => {
                // 角度对不上（或选择了空白起点）：走原来的重算路径
                if interp {
                    self.doc.interpolated_mask_at(a)
                } else {
                    self.doc.blank_mask()
                }
            }
        };
        let steps = self.doc.steps;
        self.doc.active_channel_mut().insert_frame(a, m);
        self.step = sdf_mask_tool::model::doc::angle_to_step(a, steps);
        self.playhead = a / 90.0;

        if let Some(f) = self.doc.active_channel_ref().frame_at(a) {
            log::info!(
                "加参数点 {:.3}：点数 {n_before}→{}，插值={interp}，新点遮 {} 像素",
                a / 90.0,
                self.doc.active_channel_ref().frames.len(),
                f.mask.count()
            );
        }
        // 播放头**精确**停在新点位置，这样画布就是这一个点
        self.display_dirty = true;
        self.touch();

        // 加完帧立刻做一次违规体检。
        // 跳过烘焙后，"数据不合法"不再由烘焙失败来发现，必须主动查一次，
        // 否则要等到切到非参数点才看到提示。1K 四点实测 1.6ms，可以同步跑。
        self.refresh_violations();

        // 烘焙交给后台线程（`refresh_display` 会派发，`poll_background_bake` 收回）。
        // 加帧本身不等烘焙——UI 立刻回到可用状态，这是"UI 与算法分离"的意义。
        //
        // 这里只标记"缓存已过期"，让下一帧的 refresh_display 去派发任务。
        // 不做同步预热：那会让按 Ins 时停顿 50–90ms，正是要避免的。
        self.last_baked_fp = None;

        self.say(if interp {
            format!("已在 {:.3} 加参数点（从插值结果开始）", a / 90.0)
        } else {
            format!("已在 {:.3} 加参数点（空白起点）", a / 90.0)
        });
    }

    /// 复制关键帧的统一入口（菜单、右键、快捷键都走这里）
    fn do_duplicate_keyframe(&mut self, step: usize, steps: usize) {
        if !self.doc.active_channel_ref().has_kf_at_step(step, steps) {
            self.say("当前不是参数点，没法复制");
            return;
        }
        self.push_undo("复制参数点");
        match self.doc.duplicate_keyframe(step, steps) {
            Some(na) => {
                self.goto_angle(na);
                self.touch();
                self.say(format!("已复制到 {:.3}", na / 90.0));
            }
            None => {
                // 没有空位，把刚压的快照弹掉，避免留下一个无效撤销点
                self.undo_stack.undo.pop();
                self.say("没有空位可放（相邻格都已被占用）");
            }
        }
    }

    /// 把当前关键帧放进剪贴板。
    ///
    /// 和 `do_duplicate_keyframe` 的区别：那个是"立刻复制到隔壁"，
    /// 这个是"记住内容，之后想粘哪就粘哪"。两者都保留。
    fn do_copy_keyframe(&mut self) {
        let (step, steps) = (self.step, self.doc.steps);
        let Some(f) = self.doc.active_channel_ref().frame_for_step(step, steps) else {
            self.say("当前不在参数点上，没有可复制的");
            return;
        };
        let (angle, mask) = (f.angle, f.mask.clone());
        self.keyframe_clip = Some((angle, mask));
        self.say(format!(
            "已复制 {:.3} 处的参数点（按 Ctrl+V 粘贴）",
            angle / 90.0
        ));
    }

    /// 把剪贴板里的遮罩粘贴到**当前播放头位置**。
    ///
    /// 三种情形：
    /// - 当前格没有关键帧 → 新建一帧
    /// - 当前格已有参数点 → 覆盖该参数点（状态栏会说明，避免误以为没生效）
    /// - 剪贴板为空 → 提示怎么复制
    fn do_paste_keyframe(&mut self) {
        let Some((src_angle, mask)) = self.keyframe_clip.clone() else {
            self.say("剪贴板是空的——先选中一个参数点按 Ctrl+C");
            return;
        };
        let (step, steps) = (self.step, self.doc.steps);
        let angle = step_angle(step, steps);
        let existing = self.doc.active_channel_ref().has_kf_at_step(step, steps);

        self.push_undo(if existing {
            "粘贴参数点（覆盖）"
        } else {
            "粘贴参数点"
        });
        // 用 Insert 语义：同角度会覆盖，不同角度会插入并保持有序
        self.doc.active_channel_mut().insert_frame(angle, mask);
        self.touch();
        self.refresh_violations();

        if existing {
            self.say(format!(
                "已用 {:.3} 处的内容覆盖 {:.3}",
                src_angle / 90.0,
                angle / 90.0
            ));
        } else {
            self.say(format!(
                "已粘贴到 {:.3}（来自 {:.3}）",
                angle / 90.0,
                src_angle / 90.0
            ));
        }
    }

    /// 按步号删关键帧的统一入口
    fn do_delete_keyframe_at_step(&mut self, step: usize) {
        let steps = self.doc.steps;
        if !self.doc.active_channel_ref().has_kf_at_step(step, steps) {
            self.say("这一格没有参数点");
            return;
        }
        if self.doc.active_channel_ref().frames.len() <= 2 {
            self.say("至少保留两个参数点，否则无法导出");
            return;
        }
        self.push_undo("删除参数点");
        self.doc
            .active_channel_mut()
            .remove_frame_at_step(step, steps);
        self.touch();
        self.say("已删除该参数点");
    }

    // ------------------------------------------------------------ 快捷键

    /// 处理快捷键。
    ///
    /// # 表驱动：一处定义，菜单文案与判定共用
    ///
    /// 旧实现把判定写成一条长 `if/else` 链，菜单里的提示文字是另外手写的。
    /// 两者一旦不同步就会出现"菜单写着 Shift+D，实际要吃 Ctrl+Shift+D"这类
    /// 静默失效——按了没反应，也没有任何提示。
    ///
    /// 现在只有 `SHORTCUTS` 一张表：判定遍历它，菜单也从它取名。
    ///
    /// # 顺序约束：修饰键多的条目必须排在前面
    ///
    /// `consume_key` 用 `matches_logically` 判定修饰键（egui
    /// `data/input/modifiers.rs`）：只要求模式里的修饰键处于按下状态，
    /// **不会拒绝多余的 Shift**。所以 `(SHIFT, F)` 也能匹配到 `(NONE, F)`。
    /// 表按"修饰键数量从多到少"排序，保证更具体的条目先被看到。
    ///
    /// # 输入焦点
    ///
    /// 只在**文本编辑**控件获得焦点时屏蔽快捷键，否则输入框里打 b 会切到笔刷。
    /// 不能用 `egui_wants_keyboard_input()`——该函数的实体是"任意控件持有焦点"，
    /// 按钮与可选中标签都算，会导致点过一次按钮后单键快捷键集体失效。
    fn handle_shortcuts(&mut self, ctx: &egui::Context) -> Option<Action> {
        if text_edit_has_focus(ctx) {
            return None;
        }
        let mut act = None;
        ctx.input_mut(|i| {
            for sc in SHORTCUTS {
                if i.consume_key(sc.mods, sc.key) {
                    act = Some(sc.action);
                    break;
                }
            }
        });
        act
    }
}

/// 一条快捷键定义。
pub struct Shortcut {
    pub mods: Modifiers,
    pub key: Key,
    pub action: Action,
    /// 显示的组合键名。菜单与关于对话框都从这里取，与实际判定同源。
    pub label: &'static str,
    /// 该组合键做什么。
    pub description: &'static str,
}

/// 构造一条快捷键。宏而非结构体字面量：一条一行，
/// 加减条目时不必逐字段改，`rustfmt` 也不会把它展开成四行。
macro_rules! sc {
    ($mods:expr, $key:expr, $act:expr, $label:expr, $desc:expr) => {
        Shortcut {
            mods: $mods,
            key: $key,
            action: $act,
            label: $label,
            description: $desc,
        }
    };
}

/// 全部快捷键。**顺序即优先级**：修饰键多的必须排在前面。
///
/// 原因见 `handle_shortcuts` 的说明——`consume_key` 不会拒绝多余的 Shift，
/// 所以 `(SHIFT, F)` 也能匹配到 `(NONE, F)`。更具体的组合要先被看到。
///
/// 这张表是**唯一真相**：键盘判定遍历它，菜单文案、关于对话框也从它取名。
/// 早先判定写成一条长 `if/else`、文案另手写，两者不同步就出现
/// "菜单写着 Shift+D、实际要吃 Ctrl+Shift+D"这种按了没反应的静默失效。
pub const SHORTCUTS: &[Shortcut] = &[
    // ---- Ctrl/Cmd + Shift（最具体，排最前）----
    sc!(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z, Action::Redo, "Ctrl+Shift+Z", "重做"),
    sc!(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S, Action::SaveProjectAs, "Ctrl+Shift+S", "工程另存为"),
    sc!(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::I, Action::InvertSelection, "Ctrl+Shift+I", "反选"),
    // ---- Ctrl/Cmd ----
    sc!(Modifiers::COMMAND, Key::Z, Action::Undo, "Ctrl+Z", "撤销"),
    sc!(Modifiers::COMMAND, Key::Y, Action::Redo, "Ctrl+Y", "重做"),
    sc!(Modifiers::COMMAND, Key::S, Action::SaveProject, "Ctrl+S", "保存工程"),
    sc!(Modifiers::COMMAND, Key::O, Action::OpenProject, "Ctrl+O", "打开工程"),
    sc!(Modifiers::COMMAND, Key::N, Action::NewProject, "Ctrl+N", "新建工程"),
    sc!(Modifiers::COMMAND, Key::C, Action::CopyKeyframe, "Ctrl+C", "复制参数点到剪贴板"),
    sc!(Modifiers::COMMAND, Key::V, Action::PasteKeyframe, "Ctrl+V", "粘贴参数点"),
    sc!(Modifiers::COMMAND, Key::A, Action::SelectAll, "Ctrl+A", "全选"),
    sc!(Modifiers::COMMAND, Key::D, Action::DeselectAll, "Ctrl+D", "取消选区"),
    sc!(Modifiers::COMMAND, Key::U, Action::ToggleUvOverlay, "Ctrl+U", "显示 / 隐藏 UV 线框"),
    sc!(Modifiers::COMMAND, Key::Num0, Action::FitToWindow, "Ctrl+0", "适应窗口"),
    sc!(Modifiers::COMMAND, Key::Plus, Action::ZoomIn, "Ctrl++", "放大"),
    sc!(Modifiers::COMMAND, Key::Equals, Action::ZoomIn, "Ctrl+=", "放大"),
    sc!(Modifiers::COMMAND, Key::Minus, Action::ZoomOut, "Ctrl+-", "缩小"),
    sc!(Modifiers::COMMAND, Key::ArrowLeft, Action::PrevKeyframe, "Ctrl+←", "上一个参数点"),
    sc!(Modifiers::COMMAND, Key::ArrowRight, Action::NextKeyframe, "Ctrl+→", "下一个参数点"),
    // ---- Alt ----
    sc!(Modifiers::ALT, Key::ArrowUp, Action::PrevLayer, "Alt+↑", "上一个图层"),
    sc!(Modifiers::ALT, Key::ArrowDown, Action::NextLayer, "Alt+↓", "下一个图层"),
    // ---- 单独 Shift（必须早于同键的无修饰版）----
    //
    // Shift+D 是 Blender 的「原位复制」。此前这里写成 Ctrl+Shift+D，
    // 而菜单与提示都写 Shift+D——按键与文案不符，功能形同不存在。
    sc!(Modifiers::SHIFT, Key::D, Action::DuplicateKeyframeAdjacent, "Shift+D", "原位复制参数点到下一个空位"),
    sc!(Modifiers::SHIFT, Key::R, Action::PrevChannel, "Shift+R", "上一个通道"),
    sc!(Modifiers::SHIFT, Key::Tab, Action::ToggleBackground, "Shift+Tab", "显示 / 隐藏底图"),
    sc!(Modifiers::SHIFT, Key::F, Action::ResetModelView, "Shift+F", "复位模型视角"),
    // ---- 无修饰单键 ----
    sc!(Modifiers::NONE, Key::Tab, Action::ToggleMaskOverlay, "Tab", "显示 / 隐藏遮罩"),
    sc!(Modifiers::NONE, Key::F, Action::FocusModel, "F", "聚焦模型"),
    sc!(Modifiers::NONE, Key::B, Action::ToolBrush, "B", "画笔"),
    sc!(Modifiers::NONE, Key::G, Action::ToolFill, "G", "填充"),
    sc!(Modifiers::NONE, Key::V, Action::ToolSelect, "V", "选择"),
    sc!(Modifiers::NONE, Key::S, Action::ToolSmooth, "S", "平滑"),
    sc!(Modifiers::NONE, Key::U, Action::ToolShape, "U", "形状"),
    sc!(Modifiers::NONE, Key::M, Action::ToolLasso, "M", "套索"),
    sc!(Modifiers::NONE, Key::X, Action::TogglePaintValue, "X", "切换黑 / 白"),
    sc!(Modifiers::NONE, Key::OpenBracket, Action::BrushSizeDown, "[", "笔刷变小"),
    sc!(Modifiers::NONE, Key::CloseBracket, Action::BrushSizeUp, "]", "笔刷变大"),
    sc!(Modifiers::NONE, Key::Num1, Action::BrushSize1, "1", "小笔刷"),
    sc!(Modifiers::NONE, Key::Num2, Action::BrushSize2, "2", "中笔刷"),
    sc!(Modifiers::NONE, Key::Num3, Action::BrushSize3, "3", "大笔刷"),
    sc!(Modifiers::NONE, Key::Num4, Action::ZoomActual, "4", "实际大小（100%）"),
    sc!(Modifiers::NONE, Key::ArrowLeft, Action::PrevStep, "←", "上一步"),
    sc!(Modifiers::NONE, Key::ArrowRight, Action::NextStep, "→", "下一步"),
    sc!(Modifiers::NONE, Key::Home, Action::GotoStart, "Home", "跳到开头"),
    sc!(Modifiers::NONE, Key::End, Action::GotoEnd, "End", "跳到结尾"),
    sc!(Modifiers::NONE, Key::Delete, Action::DeleteKeyframe, "Del", "删除参数点"),
    sc!(Modifiers::NONE, Key::Insert, Action::AddKeyframe, "Ins", "加参数点"),
    sc!(Modifiers::NONE, Key::R, Action::NextChannel, "R", "下一个通道"),
];

/// 按动作查显示名。菜单与提示从这里取名，保证与实际注册的判定一致。
pub fn shortcut_label(action: Action) -> Option<&'static str> {
    SHORTCUTS.iter().find(|s| s.action == action).map(|s| s.label)
}

/// 文本编辑控件是否持有键盘焦点。
///
/// 只有此时才应屏蔽单键快捷键——否则输入框里打 "b" 会切到笔刷。
///
/// 不能用 `egui_wants_keyboard_input()`：该函数的实体是 `memory.focused().is_some()`，
/// 而按钮、可选中标签同样能获得焦点，用那个判断会变成
/// "点过一次按钮后所有单键快捷键集体失灵"。
/// `text_edit_focused()` 检查焦点控件是否持有一份 `TextEditState`，
/// 正好只覆盖真正的文本输入。
fn text_edit_has_focus(ctx: &egui::Context) -> bool {
    ctx.text_edit_focused()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // ---- 拖入文件：图片当底图，工程扩展名当工程 ----
        self.handle_dropped_files(&ctx);

        // ---- 快捷键 ----
        if let Some(a) = self.handle_shortcuts(&ctx) {
            self.run(a);
        }

        let full = ui.available_rect_before_wrap();

        // ---- 菜单栏 ----
        let menu_rect = Rect::from_min_size(full.min, egui::vec2(full.width(), MENUBAR_H));
        ui.painter()
            .rect_filled(menu_rect, 0.0, Color32::from_rgb(0x1e, 0x1e, 0x1e));
        {
            let mut mui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(menu_rect.shrink2(egui::vec2(6.0, 2.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            if let Some(a) = ui::menus::menu_bar(&mut mui) {
                self.run(a);
            }
        }

        // ---- 工具条 ----
        let toolbar_rect = Rect::from_min_size(
            egui::pos2(full.min.x, full.min.y + MENUBAR_H),
            egui::vec2(full.width(), TOOLBAR_H),
        );
        self.draw_toolbar(ui, toolbar_rect);

        // ---- 主体分区 ----
        let body_top = full.min.y + MENUBAR_H + TOOLBAR_H;
        let body = Rect::from_min_max(egui::pos2(full.min.x, body_top), full.max);
        let canvas_right = body.max.x - LAYER_W;
        let body_bottom = body.max.y - TIMELINE_H;

        let canvas_rect = Rect::from_min_max(body.min, egui::pos2(canvas_right, body_bottom));
        let layer_rect = Rect::from_min_max(
            egui::pos2(canvas_right, body.min.y),
            egui::pos2(canvas_right + LAYER_W, body_bottom),
        );
        let timeline_rect = Rect::from_min_max(egui::pos2(body.min.x, body_bottom), body.max);

        ui.painter()
            .rect_filled(layer_rect, 0.0, Color32::from_rgb(0x24, 0x24, 0x24));
        ui.painter()
            .rect_filled(timeline_rect, 0.0, Color32::from_rgb(0x22, 0x22, 0x22));

        // 日志面板（可选，从底部往上叠一层）
        let mut canvas_bottom = body_bottom;
        if self.show_log {
            let log_h = 150.0_f32.min(body.height() * 0.35);
            let log_rect = Rect::from_min_max(
                egui::pos2(body.min.x, body_bottom - log_h),
                egui::pos2(body.max.x, body_bottom),
            );
            canvas_bottom = log_rect.min.y;
            ui.painter()
                .rect_filled(log_rect, 0.0, Color32::from_rgb(0x1c, 0x1c, 0x1c));
            let mut lui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(log_rect.shrink2(egui::vec2(10.0, 4.0)))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            sdf_mask_tool::logging::show_panel(&mut lui);
        }

        // 后台烘焙结果：每帧尝试取一次。取到就装上并置脏，让本帧画出新图。
        self.poll_background_bake(ui.ctx());

        // **顺序要紧**：违规检测必须在 refresh_display 之前刷新。
        // 停在参数点上时 refresh_display 跳过烘焙，该分支的"数据不合法"提示
        // 改为从 violations 读；若这里先跑 display，读到的就是上一帧的旧结果
        // （表现为"画错了但提示晚一帧才出现"）。
        self.refresh_violations();
        self.refresh_display();
        self.refresh_violations();

        // ---- 画布 / 3D 分栏 ----
        //
        // 分隔条宽度固定，用比例 split_ratio 分配。三档模式决定谁显示：
        // 只显示一个时另一个宽度直接归零，不留空白。
        // **注意必须在 canvas_bottom 确定之后算**——日志面板会把底部往上顶。
        //
        // 兜底：没有任何模型时，不管设置里存的什么模式，都不显示 3D 面板。
        // 否则旧配置（view_mode=2）会在启动时留一块"把模型拖进来"的空面板，
        // 与"默认不显示 3D 模型"的要求相悖。
        let effective_mode = if self.model_panel.has_mesh() {
            self.view_mode
        } else {
            sdf_mask_tool::preview::ViewMode::CanvasOnly
        };
        let show_c = effective_mode.show_canvas();
        let show_m = effective_mode.show_model();
        // 只在状态变化时打一行，避免每帧刷日志
        {
            static LAST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);
            let cur = (effective_mode as u32) << 8 | (show_c as u32) << 1 | (show_m as u32);
            if LAST.swap(cur, std::sync::atomic::Ordering::Relaxed) != cur {
                log::info!(
                    "分栏决策：view_mode={:?} 有模型={} → show_canvas={} show_model={} ratio={:.2}",
                    effective_mode,
                    self.model_panel.has_mesh(),
                    show_c,
                    show_m,
                    self.split_ratio
                );
            }
        }
        let split_w = if show_c && show_m { SPLIT_W } else { 0.0 };
        let usable = (canvas_rect.width() - split_w).max(0.0);
        let (canvas_w, model_w) = match (show_c, show_m) {
            (true, true) => {
                let cw = usable * self.split_ratio.clamp(0.15, 0.85);
                (cw, usable - cw)
            }
            (true, false) => (usable, 0.0),
            (false, true) => (0.0, usable),
            // 两个都不显示时兜底给画布，避免出现全空区域
            (false, false) => (usable, 0.0),
        };

        let view_rect = Rect::from_min_max(
            canvas_rect.min,
            egui::pos2(canvas_rect.max.x, canvas_bottom),
        );
        let canvas_only_rect = Rect::from_min_max(
            view_rect.min,
            egui::pos2(view_rect.min.x + canvas_w, view_rect.max.y),
        );
        let split_rect = Rect::from_min_max(
            egui::pos2(canvas_only_rect.max.x, view_rect.min.y),
            egui::pos2(canvas_only_rect.max.x + split_w, view_rect.max.y),
        );
        let model_rect = Rect::from_min_max(
            egui::pos2(split_rect.max.x, view_rect.min.y),
            egui::pos2(split_rect.max.x + model_w, view_rect.max.y),
        );

        // ---- 画布 ----
        if show_c {
            let cr = canvas_only_rect;
            let mut cui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(cr)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            let on_kf = self.is_on_keyframe();
            let ang = self.angle();
            let (cur_step, cur_steps) = (self.step, self.doc.steps);

            // 设置同步到画布：自动插帧开关。每帧同步（只写两个 bool，零成本），
            // 这样在设置面板里一改就立刻生效，不需要额外的"应用"按钮。
            self.canvas.auto_keyframe = self.settings.auto_keyframe_on_paint;
            self.canvas.interp_new_frame = self.settings.interp_new_keyframe;

            // UV 线框同步。只在开关打开时算，且按 UV 集数量做上限：
            // 遍历几十万条线段会直接吃掉交互帧率，而线画到那个密度也只是糊成一片。
            self.canvas.show_uv = self.model_panel.show_uv_on_canvas;
            if self.canvas.show_uv {
                // 只在模型或 UV 集变化时重算，避免每帧重建去重集合
                let uv_key = (
                    self.model_panel.path.clone(),
                    self.model_panel.uv_set,
                    self.model_panel.has_mesh(),
                );
                if self.canvas_uv_key.as_ref() != Some(&uv_key) {
                    self.canvas.uv_edges = self.model_panel.uv_edges(UV_EDGE_BUDGET);
                    self.canvas_uv_key = Some(uv_key);
                    log::info!("UV 线框：{} 条线段", self.canvas.uv_edges.len());
                }
            } else if !self.canvas.uv_edges.is_empty() {
                // 关掉时清空，避免"关了还残留"以及白占内存
                self.canvas.uv_edges.clear();
                self.canvas_uv_key = None;
            }

            // 撤销快照的时机
            //
            // 一次完整笔画只占一个撤销点（否则按十次 Ctrl+Z 才退一笔）。
            // 快照必须在**第一次改动像素之前**拍下：早先是 show() 返回后
            // 才比对 stroke 的沿去压栈，那时第一笔已经写进文档，
            // 快照里含着那一笔——撤销后笔尖还在，反复撤销也清不掉。
            //
            // 现在由画布在改动前把文档副本放进 `undo_snapshot`，这里取走。
            self.canvas.undo_snapshot = None;
            // 违规红标记用 App 层的检测结果（和画笔栏的按钮同源），
            // 不再依赖导出窗口是否打开。
            //
            // 这里**借用**而不克隆：1K 画布下这是 1M 个 bool，每帧拷一次
            // 就是每帧白烧 1MB 内存带宽。把字段分别拆开借用即可避开
            // 同时可变借用整个 self 的冲突。
            // 违规红标记只有在「检测」开关打开时才画（开关只管显示，不改数据）
            let bad: &[bool] = if self.settings.show_violations {
                self.violations
                    .as_ref()
                    .map(|v| v.bad.as_slice())
                    .unwrap_or(&[])
            } else {
                &[]
            };
            let painted = ui::canvas_view::show(
                &mut cui,
                &mut self.doc,
                &mut self.canvas,
                cur_step,
                cur_steps,
                &self.display_mask,
                self.show_mask,
                // **能不能画**，而不是"这里有没有帧"。
                //
                // 旧版这里传的是 `is_on_keyframe()`，于是没有关键帧时整条绘制
                // 分支被跳过——落笔完全没反应，必须先按 Ins 加点。
                // 在没点的位置落笔需要自动加点再绘画，所以这里必须
                // 放开：只要允许自动建帧，就永远可画。
                //
                // 关掉自动插帧开关时退回旧行为（必须先加帧）。
                on_kf || self.settings.auto_keyframe_on_paint,
                bad,
            );
            let _ = ang;
            // 画布在第一次改动像素前留下了文档副本，取走压栈。
            // 副本是"改动前"的，所以撤销一笔能退干净。
            if let Some(before) = self.canvas.undo_snapshot.take() {
                self.undo_stack
                    .push_snapshot(before, cur_step, "绘制".to_string());
            }
            if painted.painted {
                self.display_dirty = true;
                self.canvas.mark_dirty();
                self.refresh_display();
                self.canvas.mark_dirty();
                cui.ctx().request_repaint();
            }
            if let Some(a) = painted.action {
                self.run(a);
            }
        }

        // ---- 分隔条（可拖动改变画布/模型比例）----
        if show_c && show_m && split_w > 0.0 {
            let resp = ui.interact(
                split_rect,
                egui::Id::unique("split_drag"),
                egui::Sense::click_and_drag(),
            );
            // 悬停/拖动时高亮，让"这里能拖"看得出来
            let hot = resp.hovered() || resp.dragged();
            ui.painter().rect_filled(
                split_rect,
                0.0,
                if hot {
                    Color32::from_rgb(0xe8, 0x87, 0x3a)
                } else {
                    Color32::from_rgb(0x2e, 0x2e, 0x2e)
                },
            );
            if hot {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
            if resp.dragged() {
                let dx = resp.drag_delta().x;
                if dx != 0.0 && usable > 1.0 {
                    self.split_ratio = (self.split_ratio + dx / usable).clamp(0.15, 0.85);
                }
            }
        }

        // ---- 3D 预览 ----
        if show_m {
            let mut mui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(model_rect.shrink2(egui::vec2(6.0, 4.0)))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            // 借用拆开：panel 要可变，mask/底图只读，view_mode 在面板里可改
            let mask = &self.display_mask;
            let bg = self.doc.background.as_ref();
            let out = sdf_mask_tool::preview::show(
                &mut mui,
                &mut self.model_panel,
                mask,
                bg,
                &mut self.view_mode,
            );
            // 相机被操作过才请求重绘；静止时不请求，egui 就不出帧
            if out.camera_changed {
                mui.ctx().request_repaint();
            }
        }

        // ---- 3D 相关设置回写 ----
        //
        // **放在 if show_m 外面**：切到「仅画布」时面板不画，
        // 但可能刚拖完分隔条，那一帧的改动仍然要存下来。
        //
        // 只在真的变了时写盘——每帧无脑 save 会把磁盘打爆。
        {
            let vm_u8 = match self.view_mode {
                sdf_mask_tool::preview::ViewMode::CanvasOnly => 0u8,
                sdf_mask_tool::preview::ViewMode::ModelOnly => 1,
                sdf_mask_tool::preview::ViewMode::Both => 2,
            };
            let changed = (self.settings.split_ratio - self.split_ratio).abs() > 1e-4
                || self.settings.view_mode != vm_u8
                || self.settings.model_wireframe != self.model_panel.wireframe
                || self.settings.model_backface_cull != self.model_panel.backface_cull
                || self.settings.show_uv_on_canvas != self.model_panel.show_uv_on_canvas;
            if changed {
                self.settings.split_ratio = self.split_ratio;
                self.settings.view_mode = vm_u8;
                self.settings.model_wireframe = self.model_panel.wireframe;
                self.settings.model_backface_cull = self.model_panel.backface_cull;
                self.settings.show_uv_on_canvas = self.model_panel.show_uv_on_canvas;
                sdf_mask_tool::config::save(&self.settings);
            }
        }

        // ---- 图层面板 ----
        {
            let mut lui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(layer_rect.shrink2(egui::vec2(8.0, 6.0)))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            let ang = self.angle();
            let out = ui::layer_panel::show(&mut lui, &mut self.doc, ang, self.step);
            if let Some(a) = out.action {
                self.run(a);
            }
            // 面板已经在改动前留好了快照，这里直接压栈
            if let Some((before, label)) = out.undo_snapshot {
                self.undo_stack.push_snapshot(before, self.step, label);
            }
            if out.changed {
                self.touch();
            }
        }

        // ---- 时间轴 ----
        {
            let mut tui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(timeline_rect.shrink2(egui::vec2(10.0, 6.0)))
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            let out =
                ui::timeline::show(&mut tui, &mut self.doc, &mut self.step, &mut self.playhead);
            if out.changed {
                self.touch();
            }
            if let Some(na) = out.action {
                self.run(na);
            }
        }

        // ---- 关于对话框 ----
        if self.show_about {
            egui::Window::new("关于")
                .collapsible(false)
                .resizable(false)
                .open(&mut self.show_about)
                .show(&ctx, |ui| {
                    ui.label(RichText::new("SDF Mask Tool").strong().size(16.0));
                    ui.label("卡通渲染用的 SDF 遮罩贴图工具");
                    ui.separator();
                    ui.label("在 0–1 的参数轴上逐点画二值遮罩，烘焙成一张灰度贴图。");
                    ui.add_space(6.0);
                    ui.label(RichText::new("快捷键").strong());
                    // 从 SHORTCUTS 表生成，不手写——手写会与实际判定脱节。
                    egui::Grid::new("about_keys")
                        .num_columns(2)
                        .spacing([14.0, 3.0])
                        .show(ui, |ui| {
                            for sc in SHORTCUTS {
                                ui.label(RichText::new(sc.label).monospace());
                                ui.label(sc.description);
                                ui.end_row();
                            }
                        });
                });
        }

        // ---- 导出窗口 ----
        if self.export_win.open {
            ui::export_window::show(ui, &mut self.export_win, &self.doc);
            // 修复请求由 App 执行，因为要进撤销栈
            if let Some(dir) = self.export_win.repair_requested.take() {
                self.do_repair_nesting(dir);
            }
        }

        // ---- 叠加层颜色选择器 ----
        if self.color_win.open {
            let mut open = self.color_win.open;
            egui::Window::new("叠加层颜色")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(&ctx, |ui| {
                    let before = self.canvas.mask_color;
                    let mut alpha = self.canvas.mask_alpha;
                    ui::color_picker::show(
                        ui,
                        &mut self.color_win,
                        &mut self.canvas.mask_color,
                        &mut alpha,
                    );
                    if alpha != self.canvas.mask_alpha {
                        self.canvas.mask_alpha = alpha;
                        self.canvas.mark_dirty();
                    }
                    if self.canvas.mask_color != before {
                        self.canvas.mark_dirty();
                    }
                });
            self.color_win.open = open;
        }

        // ---- 设置面板 ----
        if self.settings_win.open {
            let out = ui::settings::show(ui, &mut self.settings_win, &self.doc, &mut self.settings);
            if let Some(n) = out.apply_steps {
                self.apply_steps(n);
            }
            if out.dirty {
                // 撤销上限、默认笔刷这类立即生效项：存盘 + 让当前会话跟上
                sdf_mask_tool::config::save(&self.settings);
                self.canvas.mark_dirty();
            }
        }

        // ---- 画布大小对话框 ----
        if self.show_canvas_size {
            let mut open = self.show_canvas_size;
            let mut apply = false;
            egui::Window::new("画布大小")
                .collapsible(false)
                .resizable(false)
                .open(&mut open)
                .show(&ctx, |ui| {
                    ui.label("宽 / 高（16–8192）");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut self.size_input.0)
                                .range(16..=8192)
                                .speed(8.0),
                        );
                        ui.label("×");
                        ui.add(
                            egui::DragValue::new(&mut self.size_input.1)
                                .range(16..=8192)
                                .speed(8.0),
                        );
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("改尺寸会把所有参数点按最近邻缩放，二值不会被抹成灰。")
                            .size(11.0)
                            .color(Color32::from_gray(0x99)),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("应用").clicked() {
                            apply = true;
                        }
                        if ui.button("取消").clicked() {
                            self.show_canvas_size = false;
                        }
                    });
                });
            if apply {
                let (w, h) = self.size_input;
                if (w, h) != (self.doc.width, self.doc.height) {
                    // 改尺寸是最近邻重采样：放大后再缩回**不可逆**，
                    // 必须能撤销，否则一次误设就永久毁掉原图。
                    self.push_undo(format!(
                        "画布 {}×{} → {}×{}",
                        self.doc.width, self.doc.height, w, h
                    ));
                }
                self.doc.resize(w, h);
                self.canvas.tex_bg_reset();
                self.canvas.fit_requested = true;
                self.display_mask = Mask::new(self.doc.width, self.doc.height);
                // 尺寸变了，所有按旧尺寸算的缓存都无效：
                // texel 快照、上次烘焙指纹、违规检测结果、选区。
                // 漏掉这些会让预览停在旧尺寸的图上（`sample_texels` 只填 min(len) 个像素）。
                self.reset_preview_caches();
                self.canvas.clear_selection();
                self.refresh_violations();
                self.touch();
                self.show_canvas_size = false;
                self.say(format!("画布已改为 {}×{}", self.doc.width, self.doc.height));
            }
        }

        ui.allocate_rect(full, egui::Sense::hover());
    }
}

impl App {
    /// 画笔栏上的「检测」与「自动修正」两个按钮。
    ///
    /// 检测和修正是**分开**的两个操作：
    /// - 「检测」是开关，只管画布上显不显示红色叠加（看错误用）
    /// - 「自动修正」才真的改数据
    ///
    /// 合并成一个按钮的问题是：要"先看看哪儿错了"就必须冒着被改数据的风险。
    fn draw_repair_buttons(&mut self, ui: &mut egui::Ui) {
        let n = self.doc.active_channel_ref().frames.len();
        let vcount = self.violation_count();
        let has_v = vcount > 0;

        // ---- 检测开关 ----
        // 状态颜色直接反映"有没有错"：无违规灰、有违规橙
        let detect_text = if n < 2 {
            RichText::new("检测").color(Color32::from_gray(0x77))
        } else if self.settings.show_violations && has_v {
            RichText::new(format!("检测 {vcount}")).color(Color32::from_rgb(0xf0, 0xc0, 0x60))
        } else {
            RichText::new("检测").color(Color32::from_gray(0xb0))
        };
        let on = self.settings.show_violations;
        let detect_btn = egui::Button::new(detect_text).selected(on);
        let detect = ui.add_enabled(n >= 2, detect_btn).on_hover_text(if n < 2 {
            "至少需要两个参数点".to_string()
        } else if on && has_v {
            format!(
                "正在标出 {vcount} 个违规像素（画布红色区域）。\n\
                     点一下关掉标记；检测本身不会改动任何绘制内容。"
            )
        } else if on {
            "这个通道没有违规像素。\n点一下关掉标记。".to_string()
        } else if has_v {
            format!(
                "检测到 {vcount} 个违规像素，但标记已隐藏。\n\
                     点一下在画布上标红。"
            )
        } else {
            "在画布上标出破坏嵌套的像素（只显示，不改数据）".to_string()
        });
        if detect.clicked() {
            self.settings.show_violations = !self.settings.show_violations;
            sdf_mask_tool::config::save(&self.settings);
            self.canvas.mark_dirty();
        }

        // ---- 自动修正 ----
        // 只改数据，不管显示。没有违规时禁用，避免"点了没反应"的疑惑。
        let fix_text = RichText::new("自动修正");
        let fix_btn = if has_v {
            egui::Button::new(fix_text.color(Color32::from_rgb(0xf0, 0xc0, 0x60)))
                .fill(Color32::from_rgb(0x4a, 0x38, 0x22))
        } else {
            egui::Button::new(fix_text.color(Color32::from_gray(0x88)))
        };
        let fix = ui.add_enabled(has_v, fix_btn).on_hover_text(if has_v {
            format!(
                "把各帧改成层层嵌套，修掉那 {vcount} 个违规像素：\n\
                     • 阴影扩张：后一帧补上前一帧的遮罩\n\
                     • 阴影收缩：后一帧挖掉前一帧没有的部分\n\
                     会改动绘制内容，可 Ctrl+Z 撤销"
            )
        } else {
            "当前通道已经是层层嵌套的，无需修正".to_string()
        });
        if fix.clicked() && has_v {
            let dir = self.violations_dir;
            self.do_repair_nesting(dir);
        }

        // 有违规时给个"换方向修"的小入口
        if has_v
            && ui
                .small_button("换向")
                .on_hover_text("改用另一个方向修正（改动量通常更大，但可能更符合原意）")
                .clicked()
        {
            let other = match self.violations_dir {
                sdf_mask_tool::model::nesting::NestDir::Grows => {
                    sdf_mask_tool::model::nesting::NestDir::Shrinks
                }
                sdf_mask_tool::model::nesting::NestDir::Shrinks => {
                    sdf_mask_tool::model::nesting::NestDir::Grows
                }
            };
            self.do_repair_nesting(other);
        }
    }
}

impl App {
    /// 工具栏上"随当前工具变化"的那一段。
    ///
    /// 只有当前工具真正用得上的控件才显示——否则六个工具各自的参数
    /// 会把工具栏撑爆，而且大部分控件在任何时刻都是无关的。
    fn draw_tool_options(&mut self, ui: &mut egui::Ui) {
        match self.canvas.tool {
            Tool::Brush => {
                ui.label("笔刷");
                let mut r = self.canvas.radius;
                if ui
                    .add_sized(
                        [90.0, 20.0],
                        egui::Slider::new(&mut r, RADIUS_MIN..=RADIUS_MAX)
                            .logarithmic(true)
                            .show_value(false)
                            .trailing_fill(true),
                    )
                    .on_hover_text("[ 变小，] 变大；1/2/3 预设")
                    .changed()
                {
                    self.canvas.set_radius(r);
                }
                let mut rn = self.canvas.radius;
                if ui
                    .add_sized(
                        [46.0, 20.0],
                        egui::DragValue::new(&mut rn)
                            .range(RADIUS_MIN..=RADIUS_MAX)
                            .speed(0.5)
                            .suffix(" px"),
                    )
                    .on_hover_text("直接输入像素半径")
                    .changed()
                {
                    self.canvas.set_radius(rn);
                }
            }
            Tool::Fill => {
                ui.label(
                    RichText::new("点一下翻转连通区")
                        .size(11.0)
                        .color(Color32::from_gray(0x99)),
                );
            }
            Tool::Select => {
                ui.label(
                    RichText::new("拖动移动遮罩内容（有选区则只移动选区内）")
                        .size(11.0)
                        .color(Color32::from_gray(0x99)),
                );
            }
            Tool::Smooth => {
                // 平滑现在是笔刷式：先给笔刷大小（作用范围），再给模糊半径（磨的粒度）。
                // 两个都暴露出来，以便同时控制"磨多大一块"和"磨多细"。
                ui.label("笔刷");
                let mut r = self.canvas.radius;
                if ui
                    .add_sized(
                        [90.0, 20.0],
                        egui::Slider::new(&mut r, RADIUS_MIN..=RADIUS_MAX)
                            .logarithmic(true)
                            .show_value(false)
                            .trailing_fill(true),
                    )
                    .on_hover_text("平滑的作用范围（和画笔同一套笔刷大小）")
                    .changed()
                {
                    self.canvas.set_radius(r);
                }
                let mut rn = self.canvas.radius;
                if ui
                    .add_sized(
                        [46.0, 20.0],
                        egui::DragValue::new(&mut rn)
                            .range(RADIUS_MIN..=RADIUS_MAX)
                            .speed(0.5)
                            .suffix(" px"),
                    )
                    .changed()
                {
                    self.canvas.set_radius(rn);
                }

                ui.separator();
                ui.label("模糊");
                ui.add_sized(
                    [80.0, 20.0],
                    egui::Slider::new(&mut self.canvas.smooth_radius, 1.0..=20.0)
                        .show_value(true)
                        .suffix(" px"),
                )
                .on_hover_text(
                    "模糊半径决定磨的粒度：越大越圆润，但细于它的孤立线会被抹掉。\n\
                     混合在二值化之前完成，所以结果仍是 0/1。",
                );

                ui.label(
                    RichText::new("按住拖动连续磨 · 边缘平滑收束不留接缝")
                        .size(11.0)
                        .color(Color32::from_gray(0x99)),
                );
            }
            Tool::Shape => {
                let mut kind = self.canvas.shape;
                for k in [ShapeKind::Rect, ShapeKind::Ellipse] {
                    if ui.selectable_label(kind == k, k.name()).clicked() {
                        kind = k;
                        self.canvas.shape = k;
                    }
                }
                ui.separator();
                ui.checkbox(&mut self.canvas.shape_filled, "实心")
                    .on_hover_text("关掉则只画边框，线宽用下面的值");
                if !self.canvas.shape_filled {
                    ui.label("线宽");
                    ui.add(
                        egui::DragValue::new(&mut self.canvas.line_w)
                            .range(1.0..=32.0)
                            .speed(0.5)
                            .suffix(" px"),
                    );
                }
                ui.label(
                    RichText::new("Shift 等比")
                        .size(11.0)
                        .color(Color32::from_gray(0x99)),
                );
            }
            Tool::Lasso => {
                let mut mode = self.canvas.select_mode;
                for m in [
                    ui::canvas_view::SelectMode::Rect,
                    ui::canvas_view::SelectMode::Lasso,
                ] {
                    if ui.selectable_label(mode == m, m.name()).clicked() {
                        mode = m;
                        self.canvas.select_mode = m;
                    }
                    let _ = mode;
                }
                ui.separator();
                // 选区操作三选一
                for op in [
                    ui::canvas_view::SelectOp::Replace,
                    ui::canvas_view::SelectOp::Add,
                    ui::canvas_view::SelectOp::Subtract,
                ] {
                    if ui
                        .selectable_label(self.canvas.select_op == op, op.name())
                        .on_hover_text(match op {
                            ui::canvas_view::SelectOp::Replace => "替换现有选区",
                            ui::canvas_view::SelectOp::Add => "并入现有选区（也可按住 Shift 拖）",
                            ui::canvas_view::SelectOp::Subtract => {
                                "从现有选区减去（也可按住 Alt 拖）"
                            }
                        })
                        .clicked()
                    {
                        self.canvas.select_op = op;
                    }
                }
                ui.separator();
                if ui.button("全选").on_hover_text("Ctrl+A").clicked() {
                    self.run(Action::SelectAll);
                }
                if ui.button("取消").on_hover_text("Ctrl+D").clicked() {
                    self.run(Action::DeselectAll);
                }
                if ui.button("反选").on_hover_text("Ctrl+Shift+I").clicked() {
                    self.run(Action::InvertSelection);
                }
                if self.canvas.selection.is_active() {
                    ui.label(
                        RichText::new(format!("已选 {} 像素", self.canvas.selection.count()))
                            .size(11.0)
                            .color(Color32::from_rgb(0x7a, 0xc0, 0x7a)),
                    );
                }
            }
        }
    }
}

impl App {
    fn draw_toolbar(&mut self, ui: &mut egui::Ui, rect: Rect) {
        ui.painter()
            .rect_filled(rect, 0.0, Color32::from_rgb(0x26, 0x26, 0x26));

        // ------------------------------------------------------------------
        // 工具栏分成**左右两块**，各自独立的矩形区域。
        //
        // 为什么不用一个横向布局 + right_to_left 尾插：那样右侧信息会和左侧
        // 控件争同一行的剩余宽度，状态栏一长就挤成一团、互相叠字（实测）。
        // 划成两块后，左边拿固定预算，右边拿固定宽度，谁也不会侵占谁。
        // ------------------------------------------------------------------
        const RIGHT_W: f32 = 420.0;
        let inner = rect.shrink2(egui::vec2(10.0, 5.0));
        let right_w = RIGHT_W.min(inner.width() * 0.45);
        let left_rect =
            Rect::from_min_max(inner.min, egui::pos2(inner.max.x - right_w, inner.max.y));
        let right_rect =
            Rect::from_min_max(egui::pos2(inner.max.x - right_w, inner.min.y), inner.max);

        let mut bar = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(left_rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        // 左侧可能装不下（工具多、选项宽），超出部分裁掉而不是撑破布局
        bar.set_clip_rect(left_rect);
        bar.horizontal(|ui| {
            for t in [
                Tool::Brush,
                Tool::Fill,
                Tool::Select,
                Tool::Smooth,
                Tool::Shape,
                Tool::Lasso,
            ] {
                if ui
                    .selectable_label(self.canvas.tool == t, t.name())
                    .on_hover_text(t.hint())
                    .clicked()
                {
                    self.canvas.tool = t;
                }
            }

            ui.separator();

            // 随工具变化的选项条：只显示当前工具用得上的东西，
            // 避免工具栏被十几个互不相关的控件塞满
            self.draw_tool_options(ui);

            ui.separator();

            // 黑白笔：一个方块，点一下换色，X 也能换
            let (v, col) = (self.canvas.value, self.canvas.value.swatch());
            let label = format!("{}色", v.name());
            let btn = egui::Button::new(RichText::new(&label).color(if v == PaintValue::Black {
                Color32::from_rgb(0xdd, 0xdd, 0xdd)
            } else {
                Color32::from_rgb(0x22, 0x22, 0x22)
            }))
            .fill(col);
            if ui
                .add(btn)
                .on_hover_text("切换黑/白 (X)：黑=盖上遮罩，白=擦掉遮罩")
                .clicked()
            {
                self.canvas.value = v.toggled();
            }

            ui.separator();

            // 检测 / 自动修正：两个独立按钮（职责分开）
            self.draw_repair_buttons(ui);

            ui.separator();
            ui.toggle_value(&mut self.show_mask, "遮罩")
                .on_hover_text("显示 / 隐藏遮罩叠加 (Tab)");

            // 叠加层颜色：点色块打开独立的选择器窗口（HEX / RGB / HSL + 透明度）
            let mut picker_open = self.color_win.open;
            ui::color_picker::color_button(ui, self.canvas.mask_color, &mut picker_open);
            self.color_win.open = picker_open;

            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("{:.0}%", self.canvas.mask_alpha * 100.0))
                    .size(11.0)
                    .color(Color32::from_gray(0x99)),
            )
            .on_hover_text("叠加层不透明度，点色块可调");

            ui.separator();
            // 画布缩放。**注意这里不放"适应窗口"按钮**——
            // 放在工具条末尾时，窗口一窄就被 set_clip_rect 裁掉，
            // 屏幕上只剩一个「适」字（实测）。
            // 那个按钮挪到右侧信息区，见 draw_toolbar 的右半部分。
            ui.label(
                RichText::new(format!("{:.0}%", self.canvas.zoom * 100.0))
                    .size(11.0)
                    .color(Color32::from_gray(0x88)),
            )
            .on_hover_text("画布缩放。Ctrl+0 适应窗口");

            ui.separator();

            // 数据违规时的提示：明确指出"现在看到的是单帧，不是插值结果"，
            // 否则会以为预览坏了。同时给出两条出路（手动修 / 自动修）。
            // 这条提示放在**右侧信息区**（空间更宽），不占左侧控件的位置。
            if let Some(err) = &self.display_error {
                let short = if err.contains("来回翻转") || err.contains("冲突") {
                    "数据不合法：显示当前帧本身，无插值".to_string()
                } else {
                    format!("无法预览：{err}")
                };
                ui.label(
                    RichText::new(short)
                        .size(11.0)
                        .color(Color32::from_rgb(0xe0, 0x90, 0x60)),
                )
                .on_hover_text(format!(
                    "{err}\n\n\
                     想手动修的话：切到各个关键帧一帧帧改，画布显示的是该帧的原始内容。\n\
                     想自动修的话：点「自动修正」。"
                ));
                ui.separator();
            }
        });

        // ---------------- 右侧信息区 ----------------
        let mut rbar = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(right_rect)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        rbar.set_clip_rect(right_rect);
        rbar.horizontal(|ui| {
            // right_to_left 布局：先加的在最右边。
            // 所以把「最不能丢」的控件放最前面——这些控件在裁切时最后才被挤掉。
            //
            // 「适应窗口」原来在左侧工具条末尾，窗口一窄就只剩半个「适」字
            // （实测）。放到右边固定宽度区就不会被裁。
            if ui
                .button("适应窗口")
                .on_hover_text("把画布缩放回完整可见 (Ctrl+0)")
                .clicked()
            {
                self.canvas.fit_requested = true;
            }

            // ---- 视图模式：唯一入口的下拉框 ----
            //
            // 之前是三档 selectable_label，而且**绑在 3D 面板内部**：
            // 在面板里点了「画布」之后面板不再绘制，那个按钮也一起消失，
            // 于是永远切不回来（实测）。
            //
            // 现在放在右侧固定宽度区——这里是 right_to_left 布局中
            // 最靠右的优先级位置，裁切时最后才被挤掉，因此永远点得到。
            egui::ComboBox::from_id_salt("view_mode_combo")
                .selected_text(self.view_mode.label())
                .width(72.0)
                .show_ui(ui, |ui| {
                    for m in [
                        sdf_mask_tool::preview::ViewMode::CanvasOnly,
                        sdf_mask_tool::preview::ViewMode::ModelOnly,
                        sdf_mask_tool::preview::ViewMode::Both,
                    ] {
                        ui.selectable_value(&mut self.view_mode, m, m.label());
                    }
                })
                .response
                .on_hover_text("画布 / 3D 模型 / 同时显示");

            ui.toggle_value(&mut self.show_log, "日志");

            // 状态栏：单行省略，完整内容放 hover。
            // 这是"挤成一团"的直接修法——长状态不再撑破整行。
            if !self.status.is_empty() {
                let max_chars = 30;
                let short: String = if self.status.chars().count() > max_chars {
                    let cut: String = self.status.chars().take(max_chars - 1).collect();
                    format!("{cut}…")
                } else {
                    self.status.clone()
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(short).color(Color32::from_rgb(0x7a, 0xc0, 0x7a)),
                    )
                    .truncate(),
                )
                .on_hover_text(&self.status);
            }

            // 图层/通道：允许截断
            let info = format!(
                "{} / {}通道",
                self.doc.active().name,
                self.doc.active_channel_ref().name
            );
            ui.add(
                egui::Label::new(RichText::new(info).color(Color32::from_gray(0x99))).truncate(),
            );
        });
    }

    /// 刷新嵌套违规检测结果（带缓存，内容没变就不重算）。
    ///
    /// 每帧都调是安全的：指纹相等时直接返回。
    fn refresh_violations(&mut self) {
        let w = self.doc.width as usize;
        let h = self.doc.height as usize;
        let ch = self.doc.active_channel_ref();
        // 指纹现在是记忆化的（内容不变时 O(1)），所以这里每帧调用是廉价的。
        // 但仍要对每一个点求一次指纹 —— 点数多时也是线性成本，
        // 所以下面命中缓存仍然提前返回。
        let key = {
            let mut acc: u64 = ch.frames.len() as u64;
            for f in ch.frames.iter() {
                acc = acc.wrapping_mul(1099511628211) ^ f.mask.fingerprint();
            }
            acc
        };
        if self.violations_key == Some(key) && self.violations.is_some() {
            return;
        }
        let refs: Vec<&Mask> = ch.frames.iter().map(|f| &f.mask).collect();
        if refs.len() < 2 {
            self.violations = None;
            self.violations_key = Some(key);
            return;
        }
        let plan = sdf_mask_tool::model::nesting::plan(&refs, w, h);
        self.violations_dir = plan.dir;
        self.violations = Some(plan.violations);
        self.violations_key = Some(key);
    }

    /// 当前通道有多少嵌套违规像素（0 表示干净）
    fn violation_count(&self) -> usize {
        self.violations.as_ref().map(|v| v.count).unwrap_or(0)
    }

    /// 改离散帧数：进撤销栈，改完重新对齐关键帧与播放头。
    fn apply_steps(&mut self, steps: usize) {
        let steps = sdf_mask_tool::model::doc::clamp_steps(steps);
        if steps == self.doc.steps {
            return;
        }
        let old = self.doc.steps;
        self.push_undo(format!("刻度数 {old} → {steps}"));
        self.doc.set_steps(steps);
        // 播放头可能越界，夹回来
        self.step = self.step.min(steps - 1);
        self.playhead = self.playhead.clamp(0.0, 1.0);
        self.settings_win.steps_input = steps;
        self.last_calc = None;
        self.export_win.invalidate();
        self.touch();
        self.say(format!("刻度数已改为 {steps}（可 Ctrl+Z 撤销）"));
    }
}

#[cfg(test)]
mod shortcut_tests {
    use super::*;

    /// 表里不能有重复的按键组合。
    ///
    /// 重复时后一条永远不可达（前一条先消费掉按键），
    /// 而菜单仍显示两条——用户按下去只有一条生效，且无从判断是哪条。
    #[test]
    fn 快捷键不重复() {
        for (i, a) in SHORTCUTS.iter().enumerate() {
            for b in SHORTCUTS.iter().skip(i + 1) {
                assert!(
                    !(a.key == b.key && a.mods == b.mods),
                    "快捷键 {} ({:?}) 与 {} ({:?}) 冲突，后者永远不可达",
                    a.label,
                    a.action,
                    b.label,
                    b.action
                );
            }
        }
    }

    /// 同键条目里，**更具体的**必须排在前面。
    ///
    /// 不去推理 `matches_logically` 的规则（推错过两次），而是**直接枚举**
    /// 所有修饰键状态，用复刻的判定函数找出"同时匹配"的组合。
    ///
    /// 两条同时匹配时，排在前面的先消费按键、后面的永不生效。
    /// 这可能是有意的（`Ctrl+Shift+Z` 在前、`Ctrl+Z` 在后，按 `Ctrl+Z`
    /// 也能兜到重做——但那时 Shift 没按，实际只有 `Ctrl+Z` 匹配），
    /// 也可能是有害的（`F` 在 `Shift+F` 前面，按 `Shift+F` 被 `F` 吃掉）。
    ///
    /// 判据：**后一条不能比前一条"更宽松"**。
    /// 若后一条要求的修饰键包含前一条的要求（是前一条的超集），
    /// 那么按后一条的组合键时前一条也会匹配 → 后一条不可达。
    ///
    /// 复刻 `cmd_ctrl_matches` 的 Windows 分支：
    /// - 模式要求 shift/alt 时必须按下
    /// - 模式不要求 ctrl/command 时，实际也不能按下
    #[test]
    fn 同键条目不互相吞噬() {
        fn matches(actual: (bool, bool, bool), pattern: Modifiers) -> bool {
            let (shift, alt, ctrl) = actual;
            if pattern.alt && !alt {
                return false;
            }
            if pattern.shift && !shift {
                return false;
            }
            if !pattern.ctrl && !pattern.command {
                return !ctrl;
            }
            if pattern.ctrl && !ctrl {
                return false;
            }
            if pattern.command && !ctrl {
                return false;
            }
            true
        }

        let mut states = Vec::new();
        for shift in [false, true] {
            for alt in [false, true] {
                for ctrl in [false, true] {
                    if shift || alt || ctrl {
                        states.push((shift, alt, ctrl));
                    }
                }
            }
        }

        // 只在"后一条是前一条的超集"时报错——那才是真正不可达的形态。
        fn is_superset(back: Modifiers, front: Modifiers) -> bool {
            let back_ctrl = back.ctrl || back.command;
            let front_ctrl = front.ctrl || front.command;
            // 后一条要求的，前一条也都要求，且至少多要一个
            let covers = (!back.shift || front.shift)
                && (!back.alt || front.alt)
                && (!back_ctrl || front_ctrl);
            let strictly_more = (back.shift && !front.shift)
                || (back.alt && !front.alt)
                || (back_ctrl && !front_ctrl);
            covers && strictly_more
        }

        for (i, a) in SHORTCUTS.iter().enumerate() {
            for b in SHORTCUTS.iter().skip(i + 1) {
                if a.key != b.key {
                    continue;
                }
                if !is_superset(b.mods, a.mods) {
                    continue;
                }
                // 确认这确实是"同时匹配"的场景，再报错
                let both = states
                    .iter()
                    .any(|st| matches(*st, a.mods) && matches(*st, b.mods));
                assert!(
                    !both,
                    "{} 排在 {} 之前，而后者要求更多修饰键——\
                     按 {} 时两者同时匹配，后一条永不生效。请把 {} 移到 {} 前面。",
                    a.label, b.label, b.label, b.label, a.label
                );
            }
        }
    }

    /// 每个动作最多对应一条快捷键，且 label/description 都不为空。
    #[test]
    fn 每条都有可读的说明() {
        for sc in SHORTCUTS {
            assert!(!sc.label.is_empty(), "{:?} 缺显示名", sc.action);
            assert!(!sc.description.is_empty(), "{} 缺说明", sc.label);
        }
    }

    /// 按住 Shift+D（不按 Ctrl/Alt）时，必须且只能命中"原位复制参数点"。
    ///
    /// 这条是回归用例：早期这里绑的是 Ctrl+Shift+D，而界面全写 Shift+D，
    /// 表现为"对着参数点按 Shift+D 毫无反应"。
    ///
    /// 所以判据不能只看 `shift` 字段（`COMMAND.plus(SHIFT)` 的 shift 也是 true，
    /// 那种写法曾经骗过一次），而是**模拟真实按键**逐条问"会匹配吗"。
    #[test]
    fn shift_d_绑定到原位复制() {
        /// 复刻 `cmd_ctrl_matches`：要求 ctrl/command 就必须按下，
        /// 不要求时也必须确实没按。
        fn matches(shift: bool, alt: bool, ctrl: bool, m: Modifiers) -> bool {
            if m.alt && !alt {
                return false;
            }
            if m.shift && !shift {
                return false;
            }
            if !m.ctrl && !m.command {
                return !ctrl;
            }
            if m.ctrl && !ctrl {
                return false;
            }
            if m.command && !ctrl {
                return false;
            }
            true
        }

        // 按住 Shift+D，不按 Ctrl、不按 Alt
        let hits: Vec<(&str, bool)> = SHORTCUTS
            .iter()
            .filter(|s| s.key == Key::D)
            .map(|s| (s.label, matches(true, false, false, s.mods)))
            .collect();

        let wins: Vec<&Shortcut> = SHORTCUTS
            .iter()
            .filter(|s| s.key == Key::D && matches(true, false, false, s.mods))
            .collect();

        let names: Vec<&str> = wins.iter().map(|s| s.label).collect();
        assert_eq!(
            wins.len(),
            1,
            "按 Shift+D 时应当只有一条生效，实际命中 {names:?}；\
             表里所有 D 键条目及其匹配结果：{hits:?}"
        );
        assert_eq!(
            wins[0].action,
            Action::DuplicateKeyframeAdjacent,
            "Shift+D 应触发「原位复制参数点」，实际是 {:?}",
            wins[0].action
        );
        assert_eq!(wins[0].label, "Shift+D", "显示名与实际按键必须一致");
    }

    /// 菜单里标注 Alt+↑/↓ 与 Shift+R，就必须真的注册了它们。
    ///
    /// 此前菜单写着这两组组合键，代码里根本没有对应分支——
    /// 按下去没有任何反应，也没有提示。
    #[test]
    fn 菜单标注的组合键都已注册() {
        let has = |key: Key, mods: Modifiers| {
            SHORTCUTS.iter().any(|s| s.key == key && s.mods == mods)
        };
        assert!(has(Key::ArrowUp, Modifiers::ALT), "Alt+↑ 未注册");
        assert!(has(Key::ArrowDown, Modifiers::ALT), "Alt+↓ 未注册");
        assert!(has(Key::R, Modifiers::SHIFT), "Shift+R 未注册");
        assert_eq!(
            shortcut_label(Action::NextLayer).map(|s| s.to_string()),
            Some("Alt+↓".to_string())
        );
    }
}

/// 图标资源的完整性检查。
///
/// ICO 在构建期被 build.rs 嵌入资源段，PNG 在运行期被 include_bytes! 读成窗口
/// 图标，两者都按文件名约定取值。文件缺失或尺寸退化时不会报错，只是界面上的
/// 图标变成空白，因此这里把约定固化成断言。
#[cfg(test)]
mod icon_tests {
    /// 嵌入的窗口图标必须是可解码的 256×256 RGBA。
    ///
    /// egui 的 IconData 只接受 RGBA8 原始像素，尺寸写错会让图标被拉伸或裁掉。
    #[test]
    fn 窗口图标可解码且尺寸正确() {
        let img = image::load_from_memory(super::WINDOW_ICON_PNG)
            .expect("窗口图标解码失败：assets/icon/sdf-mask-tool-256.png");
        let rgba = img.to_rgba8();
        assert_eq!(
            rgba.dimensions(),
            (256, 256),
            "窗口图标必须是 256×256，实际 {:?}",
            rgba.dimensions()
        );
    }

    /// 图标资源文件必须存在，且 ICO 目录声明的尺寸与实际载荷自洽。
    ///
    /// build.rs 只靠这个文件传递约定；目录项指向的偏移越界时，
    /// 资源管理器会显示空白图标且不报错。
    #[test]
    fn ico_文件结构完整() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/icon/sdf-mask-tool.ico");
        let bytes = std::fs::read(path).expect("图标文件不存在");

        assert!(bytes.len() > 6, "图标文件过短");
        assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 0, "reserved 必须为 0");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 1, "类型必须是 1（图标）");

        let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        assert_eq!(count, 7, "图标应含 7 个尺寸，实际 {count}");
        assert!(
            bytes.len() >= 6 + 16 * count,
            "目录区被截断：{} 字节容纳不下 {count} 项",
            bytes.len()
        );

        let mut sizes = Vec::new();
        for i in 0..count {
            let e = 6 + 16 * i;
            let width = bytes[e];
            let height = bytes[e + 1];
            let size = u32::from_le_bytes([bytes[e + 8], bytes[e + 9], bytes[e + 10], bytes[e + 11]]) as usize;
            let offset = u32::from_le_bytes([bytes[e + 12], bytes[e + 13], bytes[e + 14], bytes[e + 15]]) as usize;

            assert_eq!(width, height, "第 {i} 项宽高不一致");
            assert!(
                offset.checked_add(size).is_some_and(|end| end <= bytes.len()),
                "第 {i} 项载荷越界：offset={offset} size={size} file={}",
                bytes.len()
            );

            let declared = if width == 0 { 256 } else { width as usize };
            sizes.push(declared);
        }

        assert_eq!(
            sizes,
            vec![16, 24, 32, 48, 64, 128, 256],
            "尺寸必须从小到大且不重复"
        );
    }
}
