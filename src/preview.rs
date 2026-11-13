//! 3D 预览面板。
//!
//! # 性能契约（这是本模块存在的第一理由）
//!
//! 性能是第一约束：静止时不得重复渲染，否则交互明显卡顿。
//! 所以这里的设计原则是反过来的——**默认什么都不做**：
//!
//! - 渲染只在 `RenderKey` 变化时执行一次。`RenderKey` 覆盖了所有会影响画面的输入。
//! - 静止状态下**不调用 `request_repaint`**，egui 就不会继续出帧。
//! - 渲染分辨率按固定档位量化（360 / 540 / 720），拖动窗口不会每像素重算。
//! - 拖相机过程中用低档，松手才补一张高档。
//!
//! 验证方式：`renders_done` 计数器。静止多帧后该计数器必须不变。

use eframe::egui::{
    self, Color32, ColorImage, Rect, RichText, Sense, TextureHandle, TextureOptions, Ui, Vec2,
};

use crate::mask::Mask;
use crate::model3d::raster::{self, Camera, RenderOpts, Tex};
use crate::model3d::{self, Mesh};

/// 渲染分辨率档位（长边像素数）。
///
/// 按档位量化而不是跟随面板尺寸，是为了让"拖动窗口边缘"不触发每像素一次重算。
/// **注意这是长边**，另一边由面板宽高比算出——不再强制正方形。
///
/// 档位选择依据（实测 73,710 三角的模型）：
/// - 360² 拖动档：约 4.5ms，够流畅
/// - 1080² 静止档：约 14ms（tile 并行后约 7ms），线条细、锯齿少
///
/// 原来静止档是 720²，在 150% DPI 下（面板约 1000px 宽）会被放大约 1.5 倍，
/// 线条看起来偏粗、锯齿明显。提到 1080² 后接近 1:1，视觉上解决这个问题。
pub const SIZE_DRAG: usize = 360;
pub const SIZE_MID: usize = 720;
pub const SIZE_FULL: usize = 1080;

const LABEL_GRAY: Color32 = Color32::from_gray(0x96);

/// 左右分栏的显示模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ViewMode {
    /// 只看画布（3D 面板隐藏，画布占满）
    ///
    /// **这是默认值**：默认不显示 3D 模型，
    /// 只有拖进模型或主动切换时才出现 3D 面板。
    #[default]
    CanvasOnly,
    /// 只看模型（画布隐藏）
    ModelOnly,
    /// 左右同时
    Both,
}

impl ViewMode {
    pub fn label(self) -> &'static str {
        match self {
            ViewMode::CanvasOnly => "画布",
            ViewMode::ModelOnly => "模型",
            ViewMode::Both => "同时",
        }
    }

    pub fn show_canvas(self) -> bool {
        matches!(self, ViewMode::CanvasOnly | ViewMode::Both)
    }

    pub fn show_model(self) -> bool {
        matches!(self, ViewMode::ModelOnly | ViewMode::Both)
    }
}

/// 决定画面是否需要重算的全部输入。
///
/// **任何一个字段变化都必须体现在这里**，否则会出现"改了但没变"。
#[derive(Clone, PartialEq)]
struct RenderKey {
    /// 相机参数的量化指纹
    cam: [i32; 6],
    size: usize,
    uv_set: usize,
    wireframe: bool,
    backface_cull: bool,
    show_base: bool,
    show_mask: bool,
    flip_v: bool,
    /// 模型内容指纹（换了模型才变）
    mesh_fp: u64,
    /// mask 内容指纹
    mask_fp: u64,
    /// 底图内容指纹
    base_fp: u64,
    /// 宽高比 ×100（量化为整数，避免浮点抖动导致每帧重算）
    aspect_q: i32,
}

/// 按长边档位和宽高比算出实际渲染像素尺寸。
///
/// 长边固定到档位上（保证重算粒度稳定），短边按比例得出。
/// 这样宽面板得到横向大图、窄面板得到竖向大图，都铺满整个窗口，
/// 而不是永远只在中间显示一块正方形。
pub fn render_size(long_side: usize, aspect: f32) -> (usize, usize) {
    let aspect = if aspect.is_finite() && aspect > 0.05 {
        aspect.clamp(0.05, 20.0)
    } else {
        1.0
    };
    let long = long_side.max(16);
    let (w, h) = if aspect >= 1.0 {
        // 宽 > 高：宽是长边
        (long, ((long as f32 / aspect).round() as usize).max(16))
    } else {
        // 高 > 宽：高是长边
        (((long as f32 * aspect).round() as usize).max(16), long)
    };
    (w, h)
}

/// 3D 预览面板的全部状态。
pub struct ModelPanel {
    pub mesh: Option<Mesh>,
    /// 模型文件路径（显示用）
    pub path: Option<std::path::PathBuf>,
    pub camera: Camera,
    /// 当前用第几套 UV
    pub uv_set: usize,
    pub wireframe: bool,
    pub backface_cull: bool,
    pub show_base: bool,
    pub show_mask: bool,
    pub flip_v: bool,
    /// 把当前 UV 集叠到画布上（与 3D 面板的 UV 选择共用同一套）
    pub show_uv_on_canvas: bool,

    /// 上一帧的渲染键。相等就跳过整个渲染流程。
    last_key: Option<RenderKey>,
    tex: Option<TextureHandle>,
    /// 每轮渲染产出的统计，显示在面板底部
    last_stats: Option<Stats>,
    /// 渲染次数。测试用来断言"闲置不重算"。
    pub renders_done: u64,
    /// 上一次渲染耗时（毫秒）
    pub last_ms: f32,

    /// 正在拖动相机：用于决定用低分辨率还是全分辨率
    dragging: bool,
    /// 松手后需要补一张全分辨率
    needs_refine: bool,
    /// 当前视口宽高比（宽/高），由面板实际尺寸决定。
    ///
    /// 渲染分辨率要按面板宽高比算，不能写死正方形——否则宽面板里只有中间一块方图，
    /// 窄面板里模型会被切掉两侧。
    aspect: f32,

    /// 滚轮缩放的最后活动时刻（egui 时间，秒）。
    ///
    /// 滚轮是连续事件流，单独一帧无法判断"是否仍在滚动"。用一个时间窗口
    /// 表示"正在缩放"，窗口内保持低分辨率，超时后补全分辨率。
    /// 这和拖动用 `dragging` 标记是同一套思路。
    wheel_active_until: f64,
    /// 上一帧渲染用时（秒），用于自适应判断该用哪一档。
    last_render_secs: f32,

    /// mask 纹理缓存
    mask_tex_data: Vec<u8>,
    mask_tex_fp: u64,
    mask_tex_w: usize,
    mask_tex_h: usize,
    /// 底图纹理缓存
    base_tex_data: Vec<u8>,
    base_tex_fp: u64,
    base_tex_w: usize,
    base_tex_h: usize,

    /// 强制失效计数器。`invalidate()` 之后即使相机等参数完全没变，
    /// 也要重渲一次——用该字段把"内容级失效"编码进 `RenderKey`。
    ///
    /// 为什么需要：`invalidate()` 只清空 `last_key`，但如果紧接着的一帧
    /// 算出的 key 恰好和上一帧相同（比如换了一模一样的模型），
    /// 光靠 `last_key == None` 也能触发——可一旦有别处提前写了 `last_key`，
    /// 就漏渲了。epoch 把"被要求重画"这件事本身变成 key 的一部分，杜绝这类竞态。
    uv_set_epoch: u32,

    pub error: Option<String>,
}

#[derive(Clone, Copy)]
struct Stats {
    ms: f32,
    tris_drawn: usize,
    tris_total: usize,
    /// 渲染时的实际像素尺寸（宽, 高）。跟随面板宽高比，不是正方形。
    size: (usize, usize),
}

impl Default for ModelPanel {
    fn default() -> Self {
        Self {
            mesh: None,
            path: None,
            camera: Camera::default(),
            uv_set: 0,
            wireframe: false,
            backface_cull: true,
            show_base: true,
            show_mask: true,
            flip_v: true,
            show_uv_on_canvas: false,
            last_key: None,
            tex: None,
            last_stats: None,
            renders_done: 0,
            last_ms: 0.0,
            dragging: false,
            needs_refine: false,
            aspect: 1.0,
            wheel_active_until: 0.0,
            last_render_secs: 0.0,
            mask_tex_data: Vec::new(),
            mask_tex_fp: 0,
            mask_tex_w: 0,
            mask_tex_h: 0,
            base_tex_data: Vec::new(),
            base_tex_fp: 0,
            base_tex_w: 0,
            base_tex_h: 0,
            uv_set_epoch: 0,
            error: None,
        }
    }
}

impl ModelPanel {
    /// 供测试读取：已完成的渲染次数。
    pub fn renders(&self) -> u64 {
        self.renders_done
    }

    /// 供测试读取：(三角数, 顶点数)。
    pub fn mesh_info(&self) -> (usize, usize) {
        self.mesh
            .as_ref()
            .map(|m| (m.tri_count, m.vert_count))
            .unwrap_or((0, 0))
    }

    /// 强制下一帧重渲（测试与外部状态变更用）。
    pub fn force_redraw(&mut self) {
        self.invalidate();
        self.uv_set_epoch = self.uv_set_epoch.wrapping_add(1);
    }

    /// 模拟相机旋转（测试用，不经过 UI）。
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.camera.orbit(dx, dy);
    }

    /// 模拟拖动状态切换（测试用）。
    pub fn set_dragging(&mut self, v: bool) {
        self.dragging = v;
    }

    /// 结束拖动并请求补一张全分辨率。
    ///
    /// 正常松手时由 UI 调用；中键在窗口外松开、egui 收不到 Released 时，
    /// 也由每帧的兜底检查调用。两条路径都要能可靠复位。
    pub fn release_drag(&mut self) {
        if self.dragging {
            self.dragging = false;
            self.invalidate();
        }
    }

    /// 标记"滚轮缩放进行中"，把当前时刻之后的一小段时间划为活跃期。
    ///
    /// 滚轮是一个连续事件流（一次滚动会产生好几帧 delta），
    /// 单帧无法判断"还在滚"还是"滚完了"。用 180ms 的静默窗口区分：
    /// 窗口内保持低分辨率，窗口外自动补全分辨率。
    ///
    /// 180ms 的依据：鼠标滚轮的单次"咔哒"间隔典型值 50–120ms，
    /// 取 180ms 能覆盖连滚但不至于让停手后的等待明显。
    pub fn begin_wheel_zoom(&mut self, now: f64) {
        self.wheel_active_until = now + 0.18;
        self.dragging = true; // 复用同一套"低分辨率"判定
    }

    /// 每帧调用：滚轮静默期结束后，自动补一张全分辨率。
    ///
    /// 返回 true 表示刚刚结束缩放，调用方可以据此请求重绘。
    ///
    /// 只在"由滚轮驱动"的情况下动手（`wheel_active_until > 0`），
    /// 中键拖动仍由松手事件负责，两者互不干扰。
    pub fn tick_idle_refine(&mut self, now: f64) -> bool {
        if self.wheel_active_until <= 0.0 {
            return false;
        }
        if now <= self.wheel_active_until {
            return false;
        }
        self.wheel_active_until = 0.0;
        self.dragging = false;
        self.invalidate();
        true
    }
}

impl ModelPanel {
    pub fn has_mesh(&self) -> bool {
        self.mesh.is_some()
    }

    /// 当前 `uv_set` 对应的 UV 线段（画布归一化坐标）。
    ///
    /// 没有模型、该模型没有 UV、或指定集不存在时返回空。
    ///
    /// `max_edges` 是硬上限：UV 线框是给美术对照用的，超过几千条之后
    /// 画面只剩一片糊，而每帧遍历这些线段会直接吃掉交互帧率。
    pub fn uv_edges(&self, max_edges: usize) -> Vec<crate::uv_overlay::UvEdge> {
        let Some(m) = &self.mesh else {
            return Vec::new();
        };
        match m.uv_set(self.uv_set) {
            Some(uv) => crate::uv_overlay::edges(uv, max_edges),
            None => Vec::new(),
        }
    }

    /// 当前模型可选的 UV 集数量（0 表示没有 UV）
    pub fn uv_set_count(&self) -> usize {
        self.mesh.as_ref().map(|m| m.stats.uv_sets).unwrap_or(0)
    }

    /// 加载模型。成功后相机自动取景。
    pub fn load(&mut self, path: &std::path::Path) {
        self.error = None;
        match model3d::load_mesh(path) {
            Ok(m) => {
                log::info!(
                    "加载模型 {}：{} 三角 / {} 顶点 / {} 套UV，{:.1}ms",
                    path.display(),
                    m.tri_count,
                    m.vert_count,
                    m.stats.uv_sets,
                    m.stats.parse_ms
                );
                self.camera
                    .fit_bounds(m.center, m.half_extents, m.up(), self.aspect);
                self.uv_set = 0;
                self.mesh = Some(m);
                self.path = Some(path.to_path_buf());
                self.invalidate();
            }
            Err(e) => {
                log::warn!("加载模型失败 {}：{e}", path.display());
                self.error = Some(e.to_string());
            }
        }
    }

    pub fn unload(&mut self) {
        self.mesh = None;
        self.path = None;
        self.tex = None;
        self.last_key = None;
        self.last_stats = None;
        self.error = None;
    }

    /// 强制下一帧重算。
    pub fn invalidate(&mut self) {
        self.last_key = None;
    }

    /// 把相机重新对准模型（F 键 / 右键菜单「复位视角」）。
    ///
    /// 保留当前 yaw，只重算距离和俯仰——这样转到侧面按 F，
    /// 不会突然被拽回正面，但能立刻把模型重新框满画面。
    /// 想连朝向一起重置时用 `focus_reset`。
    pub fn focus(&mut self) {
        let Some(m) = &self.mesh else {
            return;
        };
        let (center, half, up) = (m.center, m.half_extents, m.up());
        self.camera
            .fit_bounds(center, half, up, self.aspect_effective());
        self.invalidate();
    }

    /// 复位视角：朝向也归零，回到标准正面视图。
    pub fn focus_reset(&mut self) {
        let Some(m) = &self.mesh else {
            return;
        };
        let (center, half, up) = (m.center, m.half_extents, m.up());
        self.camera.yaw = 0.0;
        self.camera
            .fit_bounds(center, half, up, self.aspect_effective());
        self.invalidate();
    }

    /// 供取舍景比例用。拿不到真实面板尺寸时退回 1.0。
    fn aspect_effective(&self) -> f32 {
        if self.aspect.is_finite() && self.aspect > 0.05 {
            self.aspect
        } else {
            1.0
        }
    }

    /// 把 mask 交给面板，并按需重建纹理缓存。
    ///
    /// 纹理重建有指纹保护：内容没变就直接返回，不会每帧重新打包 1M 像素。
    pub fn set_mask(&mut self, mask: &Mask) {
        self.rebuild_mask_tex(mask);
    }

    /// 当前 mask 纹理指纹（测试断言用）。
    pub fn mask_fp(&self) -> u64 {
        self.mask_tex_fp
    }

    fn mesh_fingerprint(mesh: Option<&Mesh>) -> u64 {
        match mesh {
            None => 0,
            Some(m) => {
                // 只用规模 + 半径做指纹：内容不会中途变（加载一次就固定）
                let mut h = 0xcbf29ce484222325u64;
                for v in [
                    m.tri_count as u64,
                    m.vert_count as u64,
                    m.radius.to_bits() as u64,
                ] {
                    h = (h ^ v).wrapping_mul(0x100000001b3);
                }
                h
            }
        }
    }

    /// 把 mask 打包成 RGBA 纹理数据（白=遮住，透明=没遮）。
    ///
    /// 用 alpha 而不是直接染色，是为了让遮罩在底图上"半透明叠加"——
    /// 和 2D 画布的显示语义保持一致。
    fn rebuild_mask_tex(&mut self, mask: &Mask) {
        let fp = mask.fingerprint();
        if fp == self.mask_tex_fp && !self.mask_tex_data.is_empty() {
            return;
        }
        let (w, h) = (mask.width() as usize, mask.height() as usize);
        self.mask_tex_data.clear();
        self.mask_tex_data.reserve(w * h * 4);
        for &v in mask.data() {
            if v != 0 {
                // 偏亮的暖白，压暗后仍能看清
                self.mask_tex_data
                    .extend_from_slice(&[0xff, 0xf0, 0xd8, 0xd0]);
            } else {
                self.mask_tex_data.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
        self.mask_tex_w = w;
        self.mask_tex_h = h;
        self.mask_tex_fp = fp;
    }

    /// 把底图打包成 RGBA。
    fn rebuild_base_tex(&mut self, bg: Option<&image::RgbaImage>) {
        let Some(img) = bg else {
            if !self.base_tex_data.is_empty() {
                self.base_tex_data.clear();
                self.base_tex_fp = 0;
            }
            return;
        };
        // 简单指纹：尺寸 + 抽样若干点。整图哈希对 1K 图每帧太贵。
        let mut fp = (img.width() as u64) << 32 | img.height() as u64;
        let step = ((img.width() * img.height()) / 4096).max(1);
        let mut i = 0u64;
        while i < (img.width() * img.height()) as u64 {
            let px = img.get_pixel(
                (i % img.width() as u64) as u32,
                (i / img.width() as u64) as u32,
            );
            fp = (fp ^ (px[0] as u64) << 16 ^ (px[1] as u64) << 8 ^ px[2] as u64)
                .wrapping_mul(0x100000001b3);
            i += step as u64;
        }
        if fp == self.base_tex_fp && !self.base_tex_data.is_empty() {
            return;
        }
        self.base_tex_data.clear();
        self.base_tex_data.extend_from_slice(img.as_raw());
        self.base_tex_w = img.width() as usize;
        self.base_tex_h = img.height() as usize;
        self.base_tex_fp = fp;
    }
}

/// 面板的返回结果。
pub struct PanelOut {
    /// 相机被操作过（需要重绘）
    pub camera_changed: bool,
}

/// 渲染决策：给定当前面板状态，算出这一帧该不该重渲。
///
/// **抽成独立函数是为了可测**。把"要不要重算"藏在 `show()` 的 UI 代码里，
/// 就只能靠截图肉眼看，而肉眼根本看不出"少渲了一帧"或"多渲了一万帧"。
/// 单独拿出来之后，可以直接对该函数写断言。
pub fn decide_render(panel: &ModelPanel, mask_fp: u64, base_fp: u64) -> Option<(RenderKey, usize)> {
    if panel.mesh.is_none() {
        return None;
    }
    let long = if panel.dragging { SIZE_DRAG } else { SIZE_FULL };
    // 宽高比量化到 1% 档：面板被拖宽 1 像素不会触发重算，
    // 但换档（比如从竖长变横长）会。
    let aspect_q = (panel.aspect * 100.0).round() as i32;
    let key = RenderKey {
        cam: [
            (panel.camera.yaw * 10000.0) as i32,
            (panel.camera.pitch * 10000.0) as i32,
            (panel.camera.dist * 1000.0) as i32,
            (panel.camera.target[0] * 1000.0) as i32,
            (panel.camera.target[1] * 1000.0) as i32,
            (panel.camera.target[2] * 1000.0) as i32,
        ],
        size: long,
        // uv_set_epoch 混进来是为了让"被要求重画"本身成为 key 的一部分
        uv_set: panel.uv_set.wrapping_add(panel.uv_set_epoch as usize),
        wireframe: panel.wireframe,
        backface_cull: panel.backface_cull,
        show_base: panel.show_base,
        show_mask: panel.show_mask,
        flip_v: panel.flip_v,
        mesh_fp: ModelPanel::mesh_fingerprint(panel.mesh.as_ref()),
        mask_fp,
        base_fp,
        aspect_q,
    };
    if panel.last_key.as_ref() == Some(&key) {
        return None;
    }
    Some((key, long))
}

/// 画 3D 面板。
///
/// `view_mode` 保留在签名里但不再使用：显示模式切换已经统一到顶部工具条。
/// 保留参数是为了不打乱调用方，也让将来"面板内再有需要时"有位置可用。
pub fn show(
    ui: &mut Ui,
    panel: &mut ModelPanel,
    mask: &Mask,
    bg: Option<&image::RgbaImage>,
    _view_mode: &mut ViewMode,
) -> PanelOut {
    let mut camera_changed = false;

    // ---- 顶部控件行 ----
    //
    // 这里**不再放显示模式切换**：那个入口已经统一到顶部工具条的下拉框。
    // 原因：放在面板内部时，切到「仅画布」后面板本身不绘制，
    // 按钮随之消失，再也切不回来（实测反馈）。
    ui.horizontal(|ui| {
        ui.label(RichText::new("3D 预览").strong().size(12.0));

        // 右侧：卸载
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if panel.has_mesh() && ui.small_button("卸载").on_hover_text("移除模型").clicked()
            {
                panel.unload();
            }
        });
    });

    // ---- 第二行：模型设置（可换行，窄面板下不会丢控件）----
    if panel.has_mesh() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.checkbox(&mut panel.wireframe, "线框")
                .on_hover_text("在模型表面画出三角形边线");
            ui.checkbox(&mut panel.backface_cull, "背面剔除")
                .on_hover_text("不画背对镜头的面，速度更快");
            ui.checkbox(&mut panel.show_base, "底图")
                .on_hover_text("在模型上贴底图");
            ui.checkbox(&mut panel.show_mask, "遮罩")
                .on_hover_text("在模型上叠加当前角度的遮罩");

            // UV 集选择 + 是否把该套 UV 叠到画布上
            //
            // 哪怕只有一套 UV 也显示选择与开关：单套 UV 的模型同样需要
            // "看看贴图落在模型的哪一块"，这与"能不能换套"是两件事。
            //
            // 下拉框而不是一排数字按钮：多 UV 的模型可能有七八套
            // （身体/头发/衣服/配件各自一套），横排会把工具栏撑爆。
            let n_uv = panel.uv_set_count();
            if n_uv > 0 {
                ui.separator();
                ui.label(RichText::new("UV").size(11.0).color(LABEL_GRAY));
                let cur = panel.uv_set.min(n_uv.saturating_sub(1));
                egui::ComboBox::from_id_salt("uv_set_combo")
                    .selected_text(format!("第 {} 套", cur + 1))
                    .width(86.0)
                    .show_ui(ui, |ui| {
                        for s in 0..n_uv {
                            ui.selectable_value(
                                &mut panel.uv_set,
                                s,
                                format!("第 {} 套", s + 1),
                            )
                            .on_hover_text(format!("使用第 {} 套 UV", s + 1));
                        }
                    });
                ui.checkbox(&mut panel.show_uv_on_canvas, "画布显示")
                    .on_hover_text(
                        "把当前这套 UV 的线框叠到画布上，用于对照贴图区域与模型部位 (Ctrl+U)",
                    );
            }
        });
    }

    ui.add_space(2.0);

    // ---- 没有模型：空状态 ----
    if !panel.has_mesh() {
        let avail = ui.available_size();
        let (rect, _) = ui.allocate_exact_size(avail, Sense::hover());
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 6.0, Color32::from_rgb(0x14, 0x14, 0x16));
        let msg = panel
            .error
            .clone()
            .unwrap_or_else(|| "把 .obj / .fbx 拖进来，或用「文件 → 导入 3D 模型」".to_string());
        let color = if panel.error.is_some() {
            Color32::from_rgb(0xe0, 0x80, 0x60)
        } else {
            Color32::from_gray(0x70)
        };
        p.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            msg,
            egui::FontId::proportional(13.0),
            color,
        );
        p.rect_stroke(
            rect,
            6.0,
            egui::Stroke::new(1.0, Color32::from_rgb(0x30, 0x30, 0x30)),
            egui::StrokeKind::Inside,
        );
        return PanelOut { camera_changed };
    }

    // ---- 交互区 ----
    let avail = ui.available_size();
    let footer_h = 18.0;
    let view_h = (avail.y - footer_h).max(40.0);
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(avail.x, view_h),
        // 必须同时感应 click 和 drag：中键拖动要靠 drag
        Sense::click_and_drag(),
    );

    // 把面板实际宽高比交给渲染：宽面板出横图、窄面板出竖图，铺满整个窗口。
    // 这是"只能在正方形区域里看模型"的修复点。
    if rect.width() > 1.0 && rect.height() > 1.0 {
        panel.aspect = rect.width() / rect.height();
    }

    // **保险：中键没按下就必须认为没在拖动。**
    //
    // 为什么需要：如果在窗口外松开中键（切了窗口、被别的程序抢走焦点），
    // egui 收不到 Released 事件，`dragging` 就会永远停在 true——
    // 表现是分辨率永久卡在 360 拖动档、画面一直糊。这是实测踩到过的。
    // 每帧用真实的按键状态兜一次底，杜绝"卡在拖动档"。
    //
    // **例外**：滚轮缩放期间 `dragging` 也是 true，但那时中键本来就没按下，
    // 不能让兜底逻辑把状态清掉——所以只在"没有滚轮活跃期"时才做这个兜底。
    let now = ui.ctx().input(|i| i.time);
    if panel.dragging
        && panel.wheel_active_until <= 0.0
        && !ui.ctx().input(|i| i.pointer.middle_down())
    {
        panel.release_drag();
    }

    // 滚轮静默期结束 → 补一张全分辨率
    if panel.tick_idle_refine(now) {
        ui.ctx().request_repaint();
    }
    // 还在静默期内：安排一次稍后的重绘，保证"停手后能自动变清晰"。
    // 不请求的话 egui 在没输入时不出帧，那张全分辨率图永远补不上。
    if panel.wheel_active_until > now {
        let wait = ((panel.wheel_active_until - now) * 1000.0) as u64 + 16;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(wait));
    }

    // 中键 / Shift+中键 / Ctrl+中键 / 滚轮
    if resp.dragged_by(egui::PointerButton::Middle) {
        let d = resp.drag_delta();
        if d.x != 0.0 || d.y != 0.0 {
            let mods = ui.ctx().input(|i| i.modifiers);
            if mods.shift {
                panel.camera.orbit(d.x, d.y);
            } else if mods.ctrl || mods.command {
                // 竖直拖动缩放：往上拖拉近
                panel.camera.zoom(1.0 + d.y * 0.004);
            } else {
                panel.camera.pan(d.x, d.y, view_h);
            }
            panel.dragging = true;
            panel.needs_refine = true;
            camera_changed = true;
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    } else if resp.hovered() {
        // 松手：从拖动档升到全分辨率，只补一次
        panel.release_drag();
        let wheel = ui.ctx().input(|i| i.smooth_scroll_delta.y);
        if wheel.abs() > 0.01 {
            // 滚轮往上 = 拉近
            panel.camera.zoom((1.0f32 - wheel * 0.0015).clamp(0.5, 2.0));
            camera_changed = true;
            // **滚轮也进拖动档**。
            //
            // 原来滚轮直接给全分辨率，而 Ctrl+中键拖动走低档——同一个"缩放"
            // 操作两种表现：滚轮清晰但卡，Ctrl+中键糊但流畅（实测）。
            // 现在统一：滚轮期间也用低档，停下来后由 `tick_idle_refine`
            // 自动补一张全分辨率。
            panel.begin_wheel_zoom(ui.ctx().input(|i| i.time));
            panel.invalidate();
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::Default);
    } else {
        panel.release_drag();
    }

    // 右键菜单：复位视角
    resp.context_menu(|ui| {
        if ui.button("复位视角").clicked() {
            if let Some(m) = &panel.mesh {
                panel
                    .camera
                    .fit_bounds(m.center, m.half_extents, m.up(), panel.aspect);
            }
            panel.invalidate();
            ui.close();
        }
    });

    // ---- 决定这一帧要不要重算 ----
    panel.rebuild_mask_tex(mask);
    panel.rebuild_base_tex(bg);

    // **关键判断**：键没变就一个字节都不算。
    // 决策本身在 `decide_render` 里，那个函数是纯的、可单测的。
    let mask_fp = panel.mask_tex_fp;
    let base_fp = panel.base_tex_fp;
    if let Some((key, size)) = decide_render(panel, mask_fp, base_fp) {
        render_now(panel, ui.ctx(), &key, size);
        panel.last_key = Some(key);
    }

    // ---- 绘制纹理 ----
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, Color32::from_rgb(0x14, 0x14, 0x16));
    if let Some(t) = &panel.tex {
        // **直接铺满 rect**。
        //
        // 不用等比缩放是因为：渲染尺寸已经按 rect 的宽高比算出来了，
        // 两者比例一致，铺满不会变形。而之前的 .min(1.0) + 居中
        // 会让图缩到中间一小块，看起来像"正方形显示区域"。
        p.image(
            t.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    p.rect_stroke(
        rect,
        6.0,
        egui::Stroke::new(1.0, Color32::from_rgb(0x33, 0x33, 0x33)),
        egui::StrokeKind::Inside,
    );

    // ---- 底部状态行 ----
    if let Some(s) = &panel.last_stats {
        let txt = format!(
            "{}×{}  {:.1} ms  三角 {}/{}",
            s.size.0, s.size.1, s.ms, s.tris_drawn, s.tris_total
        );
        ui.label(RichText::new(txt).size(10.0).color(LABEL_GRAY));
    } else {
        ui.label(
            RichText::new("拖动中键平移，Shift+中键旋转，Ctrl+中键/滚轮缩放")
                .size(10.0)
                .color(LABEL_GRAY),
        );
    }

    PanelOut { camera_changed }
}

/// 执行一次渲染并上传纹理。
fn render_now(panel: &mut ModelPanel, ctx: &egui::Context, key: &RenderKey, long_side: usize) {
    let Some(mesh) = &panel.mesh else {
        return;
    };
    let t0 = std::time::Instant::now();

    // **按面板宽高比出图**，不再写死正方形。
    // 原来固定 size×size，导致宽面板里只显示中间一块方图、
    // 窄面板里模型左右被切掉——看到的"正方形显示区域"就是这个。
    let (rw, rh) = render_size(long_side, panel.aspect);

    let opts = RenderOpts {
        width: rw,
        height: rh,
        uv_set: key.uv_set.min(mesh.stats.uv_sets.saturating_sub(1)),
        wireframe: key.wireframe,
        backface_cull: key.backface_cull,
        show_base: key.show_base,
        show_mask: key.show_mask,
        flip_v: key.flip_v,
        // 多线程（tile binning）：三角数够多时自动启用
        parallel: true,
        // 不再做"微小三角剔除"：实测该优化只省 16~25% 时间，
        // 却在模型上挖出成片孔洞（拖动档丢 11% 内容）。
        // 详见 `raster::render` 里的注释。
        #[allow(deprecated)]
        min_tri_px2: 0.0,
    };

    let base = Tex {
        data: &panel.base_tex_data,
        w: panel.base_tex_w,
        h: panel.base_tex_h,
    };
    let mask = Tex {
        data: &panel.mask_tex_data,
        w: panel.mask_tex_w,
        h: panel.mask_tex_h,
    };

    let out = raster::render(
        mesh,
        &panel.camera,
        &opts,
        if panel.base_tex_w > 0 {
            Some(base)
        } else {
            None
        },
        if panel.mask_tex_w > 0 {
            Some(mask)
        } else {
            None
        },
    );

    let ms = t0.elapsed().as_secs_f32() * 1000.0;

    // 一个三角都没画出来是异常状态，把诊断打出来（正常时不打，避免刷屏）。
    if out.tris_drawn == 0 && out.tris_total > 0 {
        log::warn!(
            "3D 渲染结果为空：{}\n  选项={:?}",
            raster::diagnose(mesh, &panel.camera, &opts),
            opts
        );
    }

    // 每次重渲都记一行。这个模块的正确性靠"渲染次数"验证，
    // 没有这行日志就无法回答"到底重算了没有、为什么重算"。
    log::debug!(
        "3D 重渲 #{}：{}×{} 拖动={} 相机 yaw={:.3} pitch={:.3} dist={:.3} target=[{:.2},{:.2},{:.2}] 画出 {}/{}",
        panel.renders_done + 1,
        rw,
        rh,
        panel.dragging,
        panel.camera.yaw,
        panel.camera.pitch,
        panel.camera.dist,
        panel.camera.target[0],
        panel.camera.target[1],
        panel.camera.target[2],
        out.tris_drawn,
        out.tris_total
    );
    let img = ColorImage::from_rgba_unmultiplied([out.width, out.height], &out.rgba);
    match &mut panel.tex {
        Some(t) => t.set(img, TextureOptions::LINEAR),
        None => {
            panel.tex = Some(ctx.load_texture("model_preview", img, TextureOptions::LINEAR));
        }
    }

    panel.renders_done += 1;
    panel.last_ms = ms;
    panel.last_stats = Some(Stats {
        ms,
        tris_drawn: out.tris_drawn,
        tris_total: out.tris_total,
        size: (rw, rh),
    });
}

// ============================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个最小的合法网格（两个三角的正方形）。
    fn tiny_mesh() -> Mesh {
        let verts = vec![
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let tris = vec![[0u32, 1, 2], [0, 2, 3]];
        let uv = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
        ];
        Mesh::from_indexed(verts, tris, vec![uv])
    }

    fn panel_with_mesh() -> ModelPanel {
        let mut p = ModelPanel::default();
        let m = tiny_mesh();
        p.camera
            .fit_bounds(m.center, m.half_extents, m.up(), p.aspect);
        p.mesh = Some(m);
        p
    }

    // ------------------------------------------------ 渲染决策（核心契约）

    #[test]
    fn 首次决策要求渲染() {
        let p = panel_with_mesh();
        assert!(
            decide_render(&p, 0, 0).is_some(),
            "没有渲染过时必须渲染一次"
        );
    }

    #[test]
    fn 没有模型时不渲染() {
        let p = ModelPanel::default();
        assert!(decide_render(&p, 0, 0).is_none(), "没有模型不该渲染");
    }

    /// **这是整个模块最重要的断言**：
    /// 算出一次 key 之后，只要输入不变，再问一次必须返回 None。
    #[test]
    fn 输入不变时第二次决策返回不渲染() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        assert!(
            decide_render(&p, 0, 0).is_none(),
            "输入完全没变却又要求渲染，这就是每帧重渲的开始"
        );
    }

    #[test]
    fn 静止一百次决策只渲染一次() {
        let mut p = panel_with_mesh();
        let mut renders = 0;
        for _ in 0..100 {
            if let Some((key, _)) = decide_render(&p, 0, 0) {
                p.last_key = Some(key);
                renders += 1;
            }
        }
        assert_eq!(renders, 1, "静止 100 帧应只渲染 1 次，实际 {renders}");
    }

    #[test]
    fn 旋转相机触发一次重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.orbit(20.0, 0.0);
        assert!(decide_render(&p, 0, 0).is_some(), "旋转后必须重渲");

        // 记下新的 key，再问就不该再渲了
        let (k2, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(k2);
        assert!(decide_render(&p, 0, 0).is_none(), "旋转结束后应恢复静止");
    }

    #[test]
    fn 滚动缩放触发重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.camera.zoom(1.2);
        assert!(decide_render(&p, 0, 0).is_some(), "缩放后必须重渲");
    }

    #[test]
    fn 平移触发重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.camera.pan(50.0, 30.0, 700.0);
        assert!(decide_render(&p, 0, 0).is_some(), "平移后必须重渲");
    }

    #[test]
    fn 拖动状态改变分辨率档位() {
        let mut p = panel_with_mesh();
        let (k, size) = decide_render(&p, 0, 0).unwrap();
        assert_eq!(size, SIZE_FULL, "静止时用全分辨率");
        p.last_key = Some(k);

        p.set_dragging(true);
        let (k2, size2) = decide_render(&p, 0, 0).expect("进入拖动档必须重渲一次");
        assert_eq!(size2, SIZE_DRAG, "拖动时降档");
        p.last_key = Some(k2);

        p.set_dragging(false);
        let (_, size3) = decide_render(&p, 0, 0).expect("松手必须补一张全分辨率");
        assert_eq!(size3, SIZE_FULL, "松手回到全分辨率");
    }

    #[test]
    fn 切换uv集触发重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.uv_set = 1;
        assert!(decide_render(&p, 0, 0).is_some(), "换 UV 集必须重渲");
    }

    #[test]
    fn 线框与剔除开关触发重渲() {
        for setter in [0usize, 1, 2, 3] {
            let mut p = panel_with_mesh();
            let (key, _) = decide_render(&p, 0, 0).unwrap();
            p.last_key = Some(key);
            match setter {
                0 => p.wireframe = !p.wireframe,
                1 => p.backface_cull = !p.backface_cull,
                2 => p.show_base = !p.show_base,
                _ => p.show_mask = !p.show_mask,
            }
            assert!(
                decide_render(&p, 0, 0).is_some(),
                "开关 {setter} 变了却没触发重渲"
            );
        }
    }

    #[test]
    fn mask指纹变化触发重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 100, 0).unwrap();
        p.last_key = Some(key);

        assert!(decide_render(&p, 100, 0).is_none(), "mask 指纹没变不该重渲");
        assert!(decide_render(&p, 200, 0).is_some(), "mask 指纹变了必须重渲");
    }

    #[test]
    fn 底图指纹变化触发重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 55).unwrap();
        p.last_key = Some(key);

        assert!(decide_render(&p, 0, 55).is_none(), "底图没变不该重渲");
        assert!(decide_render(&p, 0, 66).is_some(), "底图变了必须重渲");
    }

    #[test]
    fn 换模型触发重渲() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        // 尺寸不同的模型 → 指纹不同
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        p.mesh = Some(Mesh::from_indexed(verts, vec![[0u32, 1, 2]], Vec::new()));
        assert!(decide_render(&p, 0, 0).is_some(), "换模型必须重渲");
    }

    #[test]
    fn invalidate后必须重渲一次() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.invalidate();
        assert!(decide_render(&p, 0, 0).is_some(), "invalidate 之后必须重渲");
    }

    // ------------------------------------------------ 纹理缓存

    #[test]
    fn 同样的mask不会重复打包() {
        let mut p = ModelPanel::default();
        let m = Mask::new(64, 64);
        p.set_mask(&m);
        let fp1 = p.mask_fp();
        let len1 = p.mask_tex_data.len();

        p.set_mask(&m);
        assert_eq!(p.mask_fp(), fp1, "同样内容指纹应稳定");
        assert_eq!(p.mask_tex_data.len(), len1, "不该重复打包");
    }

    #[test]
    fn mask内容变化后指纹更新() {
        let mut p = ModelPanel::default();
        let mut m = Mask::new(64, 64);
        p.set_mask(&m);
        let fp1 = p.mask_fp();

        m.set(5, 5, 1);
        p.set_mask(&m);
        assert_ne!(p.mask_fp(), fp1, "改了一个像素指纹必须变");
    }

    #[test]
    fn mask纹理是rgba四字节且尺寸正确() {
        let mut p = ModelPanel::default();
        let m = Mask::new(32, 16);
        p.set_mask(&m);
        assert_eq!(p.mask_tex_w, 32);
        assert_eq!(p.mask_tex_h, 16);
        assert_eq!(p.mask_tex_data.len(), 32 * 16 * 4, "必须是 RGBA8");
    }

    #[test]
    fn 遮住的像素不透明_没遮的像素全透明() {
        let mut p = ModelPanel::default();
        let mut m = Mask::new(4, 4);
        m.set(0, 0, 1);
        p.set_mask(&m);
        // 第一个像素：不透明
        assert_eq!(p.mask_tex_data[3], 0xd0, "遮住的像素应不透明");
        // 第二个像素：全 0
        assert_eq!(&p.mask_tex_data[4..8], &[0, 0, 0, 0], "没遮的像素应全透明");
    }

    // ------------------------------------------------ 显示模式

    #[test]
    fn 显示模式决定谁可见() {
        assert!(ViewMode::CanvasOnly.show_canvas());
        assert!(!ViewMode::CanvasOnly.show_model());

        assert!(!ViewMode::ModelOnly.show_canvas());
        assert!(ViewMode::ModelOnly.show_model());

        assert!(ViewMode::Both.show_canvas());
        assert!(ViewMode::Both.show_model());
    }

    #[test]
    fn 默认仅显示画布() {
        // 默认不显示 3D 模型，
        // 只有拖进模型或主动切换时才出现 3D 面板。
        assert_eq!(ViewMode::default(), ViewMode::CanvasOnly);
    }

    // ------------------------------------------------ 相机行为的集成验证

    #[test]
    fn 相机旋转不会无限累积yaw() {
        let mut p = panel_with_mesh();
        // 疯狂转 1000 次
        for _ in 0..1000 {
            p.orbit(37.0, 11.0);
        }
        assert!(
            p.camera.yaw >= 0.0 && p.camera.yaw < std::f32::consts::TAU,
            "yaw 应收敛在 [0, 2π)，实际 {}",
            p.camera.yaw
        );
        assert!(p.camera.pitch.abs() < std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn 中键松开后拖动状态必须复位() {
        let mut p = panel_with_mesh();
        let (key, size) = decide_render(&p, 0, 0).unwrap();
        assert_eq!(size, SIZE_FULL, "静止时是全分辨率");
        p.last_key = Some(key);

        // 模拟拖动中
        p.set_dragging(true);
        let (k2, size2) = decide_render(&p, 0, 0).unwrap();
        assert_eq!(size2, SIZE_DRAG, "拖动时降到 360");
        p.last_key = Some(k2);

        // 模拟"在窗口外松开中键"导致 dragging 卡住，然后用兜底逻辑复位
        p.release_drag();
        assert!(!p.dragging, "release_drag 必须清掉拖动状态");
        assert!(
            decide_render(&p, 0, 0).is_some(),
            "复位后必须补渲一张全分辨率"
        );
    }

    // ------------------------------------------------ 渲染尺寸跟随宽高比

    #[test]
    fn 正方形视口出正方形图() {
        let (w, h) = render_size(720, 1.0);
        assert_eq!((w, h), (720, 720));
    }

    #[test]
    fn 宽视口出横图() {
        // 面板 2:1 宽，应该出 720×360，而不是 720×720 的方图
        let (w, h) = render_size(720, 2.0);
        assert_eq!(w, 720, "宽是长边");
        assert_eq!(h, 360, "高按比例缩到一半");
    }

    #[test]
    fn 窄视口出竖图() {
        // 面板 1:2 高，应该出 360×720
        let (w, h) = render_size(720, 0.5);
        assert_eq!(w, 360, "宽按比例缩");
        assert_eq!(h, 720, "高是长边");
    }

    #[test]
    fn 渲染尺寸始终是长边等于档位() {
        for aspect in [0.2f32, 0.5, 1.0, 1.5, 3.0] {
            let (w, h) = render_size(720, aspect);
            assert_eq!(
                w.max(h),
                720,
                "长边必须正好是 720（aspect={aspect}，实际 {w}×{h}）"
            );
        }
    }

    #[test]
    fn 极端宽高比不会产生零尺寸() {
        for aspect in [0.0f32, -5.0, 1e9, f32::NAN] {
            let (w, h) = render_size(720, aspect);
            assert!(w >= 16, "宽不能塌成 0（aspect={aspect}→{w}）");
            assert!(h >= 16, "高不能塌成 0（aspect={aspect}→{h}）");
        }
    }

    #[test]
    fn 宽高比变化触发重渲() {
        let mut p = panel_with_mesh();
        p.aspect = 1.0;
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.aspect = 2.0;
        assert!(
            decide_render(&p, 0, 0).is_some(),
            "面板宽高比变了必须重渲，否则图会被拉伸变形"
        );
    }

    #[test]
    fn 极小的宽高比抖动不触发重渲() {
        let mut p = panel_with_mesh();
        p.aspect = 1.500;
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        // 0.1% 的变化（拖窗口 1 像素级别）不该重算
        p.aspect = 1.5015;
        assert!(
            decide_render(&p, 0, 0).is_none(),
            "1% 以内的宽高比抖动不应触发重算，否则拖窗口会卡"
        );
    }

    // ------------------------------------------------ 相机取景考虑宽高比

    #[test]
    fn 窄视口的相机比方形拉得更远() {
        let mut square = Camera::default();
        square.fit_aspect([0.0; 3], 1.0, [0.0, 1.0, 0.0], 1.0);

        let mut narrow = Camera::default();
        narrow.fit_aspect([0.0; 3], 1.0, [0.0, 1.0, 0.0], 0.4);

        assert!(
            narrow.dist > square.dist,
            "窄视口水平视野小，相机必须拉远才装得下（方形 {} vs 窄 {}）",
            square.dist,
            narrow.dist
        );
    }

    #[test]
    fn 宽视口的相机不比方形更近() {
        let mut square = Camera::default();
        square.fit_aspect([0.0; 3], 1.0, [0.0, 1.0, 0.0], 1.0);
        let mut wide = Camera::default();
        wide.fit_aspect([0.0; 3], 1.0, [0.0, 1.0, 0.0], 3.0);
        // 宽视口垂直视野不变，所以距离相同；但绝不能更近
        assert!(wide.dist >= square.dist - 1e-4);
    }

    /// 用同一份网格造面板（`panel_with_mesh` 的便捷别名）。
    fn mk_panel() -> ModelPanel {
        panel_with_mesh()
    }

    #[test]
    fn 聚焦会重设相机距离() {
        let mut p = mk_panel();
        // 先把相机推得很远
        p.camera.dist = 999.0;
        let before = p.camera.dist;

        p.focus();

        assert!(
            (p.camera.dist - before).abs() > 1.0,
            "聚焦后距离应回到取景值，实际仍是 {before} → {}",
            p.camera.dist
        );
        assert!(p.camera.dist > 0.0, "距离必须为正");
    }

    #[test]
    fn 聚焦保留已有旋转角度() {
        let mut p = mk_panel();
        p.camera.yaw = 1.234;
        p.camera.pitch = 0.321;

        p.focus();

        assert!(
            (p.camera.yaw - 1.234).abs() < 1e-5,
            "聚焦不该改变朝向——转到侧面再按 F，不应被拽回正面"
        );
        // pitch 会被 fit 重置为 0（取景要正对），这是预期行为
        assert_eq!(p.camera.pitch, 0.0);
    }

    #[test]
    fn 复位视角会同时归零朝向() {
        let mut p = mk_panel();
        p.camera.yaw = 2.5;
        p.camera.pitch = 0.5;

        p.focus_reset();

        assert_eq!(p.camera.yaw, 0.0, "复位应回到标准正面");
        assert_eq!(p.camera.pitch, 0.0);
    }

    #[test]
    fn 聚焦会请求一次重渲() {
        let mut p = mk_panel();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);
        assert!(decide_render(&p, 0, 0).is_none(), "静止时不该重渲");

        p.focus();
        assert!(
            decide_render(&p, 0, 0).is_some(),
            "聚焦后必须重渲，否则画面不更新"
        );
    }

    #[test]
    fn 没有模型时聚焦是安全的空操作() {
        let mut p = ModelPanel::default();
        p.focus(); // 不该 panic
        p.focus_reset();
        assert!(!p.has_mesh());
    }

    // ------------------------------------------------ 自动平滑

    #[test]
    fn 加载后自动算好平滑法线() {
        // 造一个共享边的两个面，加载后 smooth_normals 必须已填好
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.1],
        ];
        let tris = vec![[0u32, 1, 2], [1, 3, 2]];
        let m = Mesh::from_indexed(verts, tris, Vec::new());
        assert_eq!(
            m.smooth_normals.len(),
            m.tri_count * 3,
            "构造时就该算好逐角法线，无需外部再设置"
        );
    }

    #[test]
    fn 卸载模型后不再渲染() {
        let mut p = panel_with_mesh();
        let (key, _) = decide_render(&p, 0, 0).unwrap();
        p.last_key = Some(key);

        p.unload();
        assert!(!p.has_mesh());
        assert!(decide_render(&p, 0, 0).is_none(), "卸载后不该渲染");
    }
}
