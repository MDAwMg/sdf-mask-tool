//! 画布视图：底图上叠半透明遮罩，二值笔刷绘制。
//!
//! 没有橡皮擦这个工具——遮罩只有「黑」和「白」两种笔。
//! 黑笔写入 1（这块被遮），白笔写入 0（这块不遮）。X 键在两者之间切换，
//! 相当于把所有绘图软件里的「画笔/橡皮」压成了一个状态。

use eframe::egui::{
    self, Color32, ColorImage, Rect, Sense, TextureHandle, TextureOptions, Ui, Vec2,
};

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::Doc;
use sdf_mask_tool::ops::{self, ShapeKind};
use sdf_mask_tool::paint;
use sdf_mask_tool::selection::{self, Selection};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// 笔刷：按当前颜色涂抹
    Brush,
    /// 填充：翻转与落点连通的同色区
    Fill,
    /// 选择：框选/拖动遮罩内容
    Select,
    /// 平滑：把边界抹平，仍是二值
    Smooth,
    /// 形状：矩形 / 圆形
    Shape,
    /// 选区：矩形框选 / 套索（只画蚂蚁线，不改遮罩）
    Lasso,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Tool::Brush => "画笔",
            Tool::Fill => "填充",
            Tool::Select => "选择",
            Tool::Smooth => "平滑",
            Tool::Shape => "形状",
            Tool::Lasso => "选区",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Tool::Brush => "画笔 (B)",
            Tool::Fill => "填充 (G)",
            Tool::Select => "选择 (V)：拖动移动遮罩内容",
            Tool::Smooth => "平滑 (S)：抹平边界，仍是二值",
            Tool::Shape => "形状 (U)：Shift 等比",
            Tool::Lasso => "选区 (M)：框选/套索，Shift 加选，Alt 减选",
        }
    }

    /// 这个工具是否需要在关键帧上落笔
    #[allow(dead_code)]
    pub fn is_paint(self) -> bool {
        matches!(self, Tool::Brush | Tool::Fill | Tool::Shape)
    }
}

/// 选区工具的两种模式
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectMode {
    /// 矩形框选
    Rect,
    /// 自由套索
    Lasso,
}

impl SelectMode {
    pub fn name(self) -> &'static str {
        match self {
            SelectMode::Rect => "框选",
            SelectMode::Lasso => "套索",
        }
    }
    pub fn toggled(self) -> Self {
        match self {
            SelectMode::Rect => SelectMode::Lasso,
            SelectMode::Lasso => SelectMode::Rect,
        }
    }
}

/// 选区操作方式（PS 的四种子模式里最常用的三种）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectOp {
    Replace,
    Add,
    Subtract,
}

impl SelectOp {
    pub fn name(self) -> &'static str {
        match self {
            SelectOp::Replace => "新建",
            SelectOp::Add => "加选",
            SelectOp::Subtract => "减选",
        }
    }
}

/// 笔刷颜色：写入 1 还是 0。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaintValue {
    /// 黑：盖上遮罩（写入 1）
    Black,
    /// 白：擦掉遮罩（写入 0）
    White,
}

impl PaintValue {
    #[inline]
    pub fn byte(self) -> u8 {
        match self {
            PaintValue::Black => 1,
            PaintValue::White => 0,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            PaintValue::Black => PaintValue::White,
            PaintValue::White => PaintValue::Black,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PaintValue::Black => "黑",
            PaintValue::White => "白",
        }
    }

    /// 界面上的小方块颜色
    pub fn swatch(self) -> Color32 {
        match self {
            PaintValue::Black => Color32::from_rgb(0x1a, 0x1a, 0x1a),
            PaintValue::White => Color32::from_rgb(0xf0, 0xf0, 0xf0),
        }
    }
}

pub struct CanvasState {
    pub tool: Tool,
    /// 笔刷颜色，X 键切换
    pub value: PaintValue,
    /// 笔刷半径，**单位是画布像素**。绘制和光标都用该字段，只有这一个来源。
    pub radius: f32,
    pub zoom: f32,
    pub center: Vec2,
    /// 遮罩叠加颜色
    pub mask_color: [f32; 3],
    /// 遮罩不透明度 0..1
    pub mask_alpha: f32,

    tex_bg: Option<TextureHandle>,
    tex_mask: Option<TextureHandle>,
    /// 违规标记纹理
    tex_viol: Option<TextureHandle>,
    viol_key: u64,
    /// 底图纹理对应的版本号，换底图时重建
    bg_version: u64,
    /// 上次生成遮罩纹理时的颜色/透明度，变了要重做
    mask_style: (i32, i32, i32, i32),
    /// 遮罩纹理是否需要重建
    mask_dirty: bool,
    last_pos: Option<(f32, f32)>,
    stroke_active: bool,
    fitted: bool,
    /// 外部请求重新适应窗口（菜单/按钮触发）
    pub fit_requested: bool,
    /// 撤销栈：每次落笔前的遮罩快照 (层, 通道, 帧下标, 快照)
    ///
    /// **已废弃**：真正的撤销栈在 App 层。保留字段只为不破坏外部引用。
    pub undo_stack: Vec<(usize, usize, usize, Mask)>,
    /// 重做栈：撤销时把"撤销前的状态"压这里
    pub redo_stack: Vec<(usize, usize, usize, Mask)>,
    /// 落笔**之前**的文档副本，供 App 层压撤销栈。
    ///
    /// 画布在第一次改动像素前填入文档副本，App 在 `show()` 返回后取走。
    /// 因为副本是在改动前拍的，撤销一笔能退干净。
    ///
    /// 早先 App 在 `show()` 返回后才比对 stroke 的沿去压栈，
    /// 那时第一笔已经写进文档，快照里含着那一笔，撤销后笔尖还在。
    pub undo_snapshot: Option<Doc>,

    // ---------------------------------------------------------- 本轮新增
    /// 图形工具的当前形状
    pub shape: ShapeKind,
    /// 形状是否实心
    pub shape_filled: bool,
    /// 空心形状的线宽（像素）
    pub line_w: f32,
    /// 平滑工具半径（像素）
    pub smooth_radius: f32,
    /// 选区工具的框选 / 套索模式
    pub select_mode: SelectMode,
    /// 选区操作：新建 / 加选 / 减选
    pub select_op: SelectOp,
    /// 文档级选区。**不进撤销栈**，是编辑期状态。
    pub selection: Selection,
    /// 在没有关键帧的位置落笔时，是否自动建一帧（由设置同步过来）
    pub auto_keyframe: bool,
    /// 自动建帧时，新帧起点用插值结果还是空白（由设置同步过来）
    pub interp_new_frame: bool,
    /// 是否在画布上叠加模型 UV 线框。
    ///
    /// 用于对照"贴图某块区域对应模型的哪一块"。只在加载了模型时有效。
    pub show_uv: bool,
    /// 叠加的 UV 线段（UV 坐标系）。模型或 UV 集变化时才重算。
    pub uv_edges: Vec<sdf_mask_tool::uv_overlay::UvEdge>,
    /// 已变换到屏幕坐标的线段，绘制直接用它。
    ///
    /// 缓存理由：绘制每帧发生，而 UV 与画布布局不变时变换结果完全相同。
    /// 每帧重变换 4 万条线段是纯粹的浪费。
    uv_screen: Vec<sdf_mask_tool::uv_overlay::ScreenEdge>,
    /// `uv_screen` 对应的画布矩形与源数据版本，变了才重算
    uv_screen_key: Option<(u64, [i32; 4])>,
    /// UV 线框颜色
    pub uv_color: [f32; 3],

    /// 形状 / 框选 / 套索拖动中的起止点（画布像素坐标）
    drag_shape: Option<(f32, f32, f32, f32)>,
    /// 套索拖动中已采样的点
    lasso_pts: Vec<(f32, f32)>,
    /// 选择工具拖动中的起点指针（画布坐标）
    select_origin: Option<(f32, f32)>,
    /// 选择工具拖动起点的原始遮罩，每帧都从该快照重算，避免逐帧平移累积毛刺
    select_base: Option<Mask>,
    /// 选择工具拖动起点的原始选区
    select_base_sel: Option<Selection>,
    /// 选择工具这次拖动是否已压过撤销快照
    select_snapshot: bool,

    /// 蚂蚁线的相位（0..8，随时间滚动）
    pub march_phase: u8,
    /// 蚂蚁线缓存：选区边缘线段，避免每帧重算
    edge_cache: Option<(usize, Vec<selection::Edge>)>,
}

/// 撤销栈上限。一张 1K 遮罩 1MB，20 步约 20MB，可接受。
pub const UNDO_LIMIT: usize = 20;

/// 笔刷半径范围。上限给到画布一半，方便一笔盖住半张脸。
pub const RADIUS_MIN: f32 = 1.0;
pub const RADIUS_MAX: f32 = 512.0;

/// 蚂蚁线的虚线格宽度（屏幕像素）
const DASH_PX: f32 = 6.0;

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            tool: Tool::Brush,
            value: PaintValue::Black,
            radius: 24.0,
            zoom: 1.0,
            center: Vec2::ZERO,
            mask_color: [1.0, 1.0, 1.0],
            mask_alpha: 0.5,
            tex_bg: None,
            tex_mask: None,
            tex_viol: None,
            viol_key: 0,
            bg_version: 0,
            mask_style: (0, 0, 0, 0),
            mask_dirty: true,
            last_pos: None,
            stroke_active: false,
            fitted: false,
            fit_requested: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            undo_snapshot: None,
            shape: ShapeKind::Rect,
            shape_filled: true,
            line_w: 2.0,
            smooth_radius: 3.0,
            select_mode: SelectMode::Rect,
            select_op: SelectOp::Replace,
            selection: Selection::none(),
            auto_keyframe: true,
            interp_new_frame: true,
            show_uv: false,
            uv_edges: Vec::new(),
            uv_screen: Vec::new(),
            uv_screen_key: None,
            // 青色：与遮罩（橙）、违规（红）都不同，叠在灰度遮罩上也看得清
            uv_color: [0.35, 0.85, 0.90],
            drag_shape: None,
            lasso_pts: Vec::new(),
            select_origin: None,
            select_base: None,
            select_base_sel: None,
            select_snapshot: false,
            march_phase: 0,
            edge_cache: None,
        }
    }
}

impl CanvasState {
    pub fn mark_dirty(&mut self) {
        self.mask_dirty = true;
    }

    /// 换底图后要重建纹理
    pub fn tex_bg_reset(&mut self) {
        self.tex_bg = None;
        self.bg_version = 0;
    }

    /// 选区变了，蚂蚁线缓存作废
    pub fn edge_cache_reset(&mut self) {
        self.edge_cache = None;
    }

    /// 丢弃选区。
    ///
    /// 换文档或改画布尺寸后必须调用：选区按旧画布坐标保存，
    /// 尺寸变小后 `sel.get` 越界返回 0，表现是"选区外怎么画都没反应"，
    /// 而画布上又不显示蚂蚁线，故障原因无从判断。
    pub fn clear_selection(&mut self) {
        self.selection = Selection::none();
        self.edge_cache = None;
    }

    /// 是否正在落笔。绘制过程中不做插值，避免每笔都重算距离场。
    pub fn is_stroking(&self) -> bool {
        self.stroke_active
    }

    /// 按倍数改笔刷大小。[ 和 ] 走这里。
    pub fn scale_radius(&mut self, factor: f32) {
        self.radius = (self.radius * factor).clamp(RADIUS_MIN, RADIUS_MAX);
    }

    pub fn set_radius(&mut self, r: f32) {
        self.radius = r.clamp(RADIUS_MIN, RADIUS_MAX);
    }

    pub fn fit(&mut self, doc: &Doc, avail: Vec2) {
        let sx = avail.x / doc.width as f32;
        let sy = avail.y / doc.height as f32;
        self.zoom = (sx.min(sy) * 0.92).max(0.02);
        self.center = Vec2::new(doc.width as f32 * 0.5, doc.height as f32 * 0.5);
    }
}

fn to_image(pos: egui::Pos2, rect: Rect, zoom: f32, center: Vec2) -> (f32, f32) {
    let mid = rect.center();
    (
        (pos.x - mid.x) / zoom + center.x,
        (pos.y - mid.y) / zoom + center.y,
    )
}

fn to_screen(x: f32, y: f32, rect: Rect, zoom: f32, center: Vec2) -> egui::Pos2 {
    let mid = rect.center();
    egui::pos2(mid.x + (x - center.x) * zoom, mid.y + (y - center.y) * zoom)
}

/// 把遮罩做成一张 RGBA：被遮的地方上色，其余全透明。
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

pub struct CanvasOutput {
    pub painted: bool,
    pub action: Option<crate::ui::menus::Action>,
}

/// 一次落笔/填充会改动的目标：某层某通道的某一帧。
#[derive(Clone, Copy)]
struct FrameRef {
    li: usize,
    ci: usize,
    fi: usize,
}

/// 找当前步号对应的关键帧。
///
/// 关键帧角度是**连续**的（拖动不吸附），所以不能用角度精确匹配，
/// 必须按步号分桶查找——否则拖过一次之后所有帧都"找不到了"，画布就不给画笔。
fn frame_ref(doc: &Doc, step: usize, steps: usize) -> Option<FrameRef> {
    let fi = doc.active_channel_ref().index_at_step(step, steps)?;
    Some(FrameRef {
        li: doc.active_layer,
        ci: doc.active_channel,
        fi,
    })
}

/// 找帧；找不到且允许自动建帧时，**当场建一帧**再返回。
///
/// # 为什么需要这个
///
/// 旧行为：`frame_ref` 返回 `None` → 整个绘制分支跳过 → 落笔没反应，
/// 必须先按 Ins 加参数点才能画。现已改为在无点处落笔自动加点再绘画。
///
/// # 新帧的起点
///
/// 取 `display`——也就是**此刻画布上显示的那张图**。
/// 这不是权宜之计，而是唯一正确的选择：
/// - 屏幕上显示什么，就应画在什么上面（否则一落笔画面就跳）
/// - 已验证：用 texel 采样结果当起点，插帧前后逐像素差异为 **0**
/// - 成本 ~1ms（含撤销快照），不会让画笔顿
///
/// `interp = false` 时退回"空白起点"，遵守设置里那个开关的语义。
///
/// 通道完全为空（一张帧都没有）时没有可插的值，建空白帧。
fn frame_ref_or_create(
    doc: &mut Doc,
    step: usize,
    steps: usize,
    display: &Mask,
    auto: bool,
    interp: bool,
) -> Option<(FrameRef, bool)> {
    if let Some(fr) = frame_ref(doc, step, steps) {
        return Some((fr, false));
    }
    // 开关关掉 → 回到旧行为（画不了，得先按 Ins 加帧）
    if !auto {
        return None;
    }
    let angle = sdf_mask_tool::model::doc::step_angle(step, steps);

    // **先看有没有近到该复用已有帧的关键帧。**
    //
    // 点在接近末点（如 0.995）时本意几乎肯定是末点。如果不吸附、直接另建一点，
    // 就会变成「0°空 → 89.5°有内容 → 90°空」——「遮了又亮」，两种方向都
    // 编码不了（实测 4200 像素违规）。只是想在最末点画画，
    // 点偏一点就掉进这个坑，而且完全看不出为什么。
    //
    // 阈值取 **2 个吸附格**，不是半格。理由是实测：
    // 默认 64 帧时半格只有 0.71°，而时间轴一格的屏幕宽度约 14px，
    // 手抖几像素就会偏 1–2 格（实测点末点落在 0.968，差 2.86°）。
    // 按半格判吸附等于没做——这正是第一版没生效的原因。
    //
    // 2 格已经足够小（90/63*2 ≈ 2.9°），不会把"真的想插在中间"吞掉：
    // 那种情况落点会离两端都很远。
    let snap_deg = 90.0 / (steps.saturating_sub(1).max(1)) as f32 * 2.0;
    {
        let ch = doc.active_channel_ref();
        let mut best: Option<(usize, f32)> = None;
        for (i, f) in ch.frames.iter().enumerate() {
            let d = (f.angle - angle).abs();
            if d <= snap_deg && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((i, d));
            }
        }
        if let Some((i, d)) = best {
            // 用 debug 级别：拖动时每帧都会调到这里，Info 会刷屏。
            // 需要排查吸附问题时把日志级别调到 Debug 即可。
            log::debug!(
                "落笔吸附：{:.3} → {:.3}（差 {:.3} ≤ {:.3}）",
                angle / 90.0,
                doc.active_channel_ref().frames[i].angle / 90.0,
                d / 90.0,
                snap_deg / 90.0
            );
            return Some((
                FrameRef {
                    li: doc.active_layer,
                    ci: doc.active_channel,
                    fi: i,
                },
                false,
            ));
        }
    }

    let empty = doc.active_channel_ref().frames.is_empty();
    let m = if interp && !empty && !display.data().is_empty() {
        display.clone()
    } else {
        doc.blank_mask()
    };
    let before = doc.active_channel_ref().frames.len();
    doc.active_channel_mut().insert_frame(angle, m);
    // 插入后按步号重找：角度是连续的，不能用精确匹配
    let fr = frame_ref(doc, step, steps)?;
    log::info!(
        "自动加点：{:.3} 点数 {before}→{}（起点={}）",
        angle / 90.0,
        doc.active_channel_ref().frames.len(),
        if interp && !empty {
            "屏幕内容"
        } else {
            "空白"
        }
    );
    Some((fr, true))
}

/// 在改动像素**之前**留下文档副本，供 App 层压撤销栈。
///
/// 一次完整操作只留一份（`undo_snapshot` 已有内容就不覆盖），
/// 所以一次笔画只占一个撤销点，而不是每小段一条。
fn push_snapshot(state: &mut CanvasState, doc: &Doc, _fr: FrameRef) {
    if state.undo_snapshot.is_none() {
        state.undo_snapshot = Some(doc.clone());
    }
}

/// 把选区外的像素还原成操作前的样子。
///
/// 画笔/填充/形状都是"整帧一起改"的粗暴实现，改完再把选区外的部分盖回去，
/// 比在每条绘制路径里都塞选区判断要好维护得多。
/// 代价是一次额外拷贝，但只在有选区时才付。
fn restore_outside(mask: &mut Mask, before: &Mask, sel: &Mask) {
    let (w, h) = (mask.width(), mask.height());
    for y in 0..h {
        for x in 0..w {
            if sel.get(x, y) == 0 {
                mask.set(x, y, before.get(x, y));
            }
        }
    }
}

/// Shift 等比：把终点拉成以起点为角的正方形。
///
/// 取两个方向位移里较大的那个做边长，这样手感是"往哪边拖得多就跟哪边"，
/// 和 PS 的 Shift 画方一致。
fn square_corner(sx: f32, sy: f32, ex: f32, ey: f32) -> (f32, f32) {
    let dx = ex - sx;
    let dy = ey - sy;
    let side = dx.abs().max(dy.abs());
    (sx + side * dx.signum(), sy + side * dy.signum())
}

/// 应用一次选区操作（新建 / 加选 / 减选）
fn apply_selection(state: &mut CanvasState, m: Mask, op: SelectOp) {
    match op {
        SelectOp::Replace => state.selection.replace(m),
        SelectOp::Add => {
            // 并集：有旧选区就在旧的上叠新的
            match state.selection.mask() {
                Some(old) => {
                    let mut merged = old.clone();
                    union_into(&mut merged, &m);
                    state.selection.replace(merged);
                }
                None => state.selection.replace(m),
            }
        }
        SelectOp::Subtract => {
            let mut sel = state
                .selection
                .mask()
                .cloned()
                .unwrap_or_else(|| Mask::new(m.width(), m.height()));
            subtract_from(&mut sel, &m);
            state.selection.replace(sel);
        }
    }
}

fn union_into(a: &mut Mask, b: &Mask) {
    let (w, h) = (a.width(), a.height());
    for y in 0..h {
        for x in 0..w {
            if b.get(x, y) != 0 {
                a.set(x, y, 1);
            }
        }
    }
}

fn subtract_from(a: &mut Mask, b: &Mask) {
    let (w, h) = (a.width(), a.height());
    for y in 0..h {
        for x in 0..w {
            if b.get(x, y) != 0 {
                a.set(x, y, 0);
            }
        }
    }
}

/// 移动遮罩内容。
///
/// 有两种语义，取决于有没有选区：
/// - **有选区**：只移动选区内的像素，选区外原样保留（PS 里"选区内容可拖动"）
/// - **无选区**：移动整张图的全部内容
///
/// 实现都是"取出来再写到新位置"，所以必须先清掉源区域，
/// 否则平移会变成"复制并叠加"，拖几次整片就糊满了。
fn move_mask_content(mask: &mut Mask, dx: i32, dy: i32, sel: Option<&Mask>) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let src = mask.clone();

    // 先把要移动的部分清空
    for y in 0..h {
        for x in 0..w {
            let inside = sel.is_none_or(|s| s.get(x as u32, y as u32) != 0);
            if inside {
                mask.set(x as u32, y as u32, 0);
            }
        }
    }
    // 再把位移后的内容写回去
    for y in 0..h {
        for x in 0..w {
            if src.get(x as u32, y as u32) == 0 {
                continue;
            }
            let inside = sel.is_none_or(|s| s.get(x as u32, y as u32) != 0);
            if !inside {
                continue;
            }
            let nx = x + dx;
            let ny = y + dy;
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                mask.set(nx as u32, ny as u32, 1);
            }
        }
    }
}

pub fn show(
    ui: &mut Ui,
    doc: &mut Doc,
    state: &mut CanvasState,
    step: usize,
    steps: usize,
    display: &Mask,
    show_mask: bool,
    editable: bool,
    // 违规像素标记（红色叠加）。空切片表示没有违规。
    violations: &[bool],
) -> CanvasOutput {
    let mut painted = false;
    let mut action: Option<crate::ui::menus::Action> = None;
    let avail = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());

    if !state.fitted || state.fit_requested {
        state.fit(doc, avail);
        state.fitted = true;
        state.fit_requested = false;
    }

    // ---- 滚轮缩放 ----
    if response.hovered() {
        let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.1 {
            let anchor = response
                .hover_pos()
                .map(|p| to_image(p, rect, state.zoom, state.center));
            let factor = (scroll * 0.0025).exp();
            state.zoom = (state.zoom * factor).clamp(0.02, 32.0);
            if let (Some(p), Some(before)) = (response.hover_pos(), anchor) {
                let after = to_image(p, rect, state.zoom, state.center);
                state.center.x += before.0 - after.0;
                state.center.y += before.1 - after.1;
            }
        }
    }

    // ---- 中键平移 ----
    //
    // 这里必须用**逐帧增量**累加，不能用"锚点 - 累计位移"。
    // `Response::drag_delta()` 返回的是本次事件以来的位移增量，
    // 若把 `drag_delta()` 当成累计位移去减锚点，画面只会在第一帧动一下，之后纹丝不动
    // （看起来就是"中键拖不动画布"）。
    if response.dragged_by(egui::PointerButton::Middle) {
        let delta = response.drag_delta();
        state.center -= delta / state.zoom;
    }

    // 中键拖动时给一只"抓手"光标，让人知道正在平移
    if response.dragged_by(egui::PointerButton::Middle) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    } else if response.hovered() && ui.ctx().input(|i| i.pointer.middle_down()) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }

    // ---------------------------------------------------------- 交互（按工具分派）
    //
    // 左键 = 主操作；右键拖动 = 反色/减选（保留 PS 习惯）。
    // 每种工具都自己处理 drag_started / dragged / drag_stopped 三段，
    // 因为各工具的"预览"和"落地"时机不同：
    // - 画笔：拖动时直接改数据（逐笔）
    // - 形状/框选/套索：拖动时只画辅助线，松手才落地
    // - 选择（移动遮罩）：拖动时实时改数据，因为要看到位移效果

    let shift = ui.ctx().input(|i| i.modifiers.shift);
    let alt = ui.ctx().input(|i| i.modifiers.alt);

    // 选区操作模式：Shift 加选、Alt 减选（PS 的行为）
    let op = if shift && state.tool == Tool::Lasso {
        SelectOp::Add
    } else if alt && state.tool == Tool::Lasso {
        SelectOp::Subtract
    } else {
        state.select_op
    };

    match state.tool {
        Tool::Brush | Tool::Fill => {
            let paint: Option<u8> = if response.dragged_by(egui::PointerButton::Primary)
                || response.clicked_by(egui::PointerButton::Primary)
            {
                Some(state.value.byte())
            } else if response.dragged_by(egui::PointerButton::Secondary) {
                Some(state.value.toggled().byte())
            } else {
                None
            };

            if let (Some(value), true) = (paint, editable) {
                if let Some(pos) = response.interact_pointer_pos() {
                    let (ix, iy) = to_image(pos, rect, state.zoom, state.center);
                    if let Some(fr) = frame_ref_or_create(
                        doc,
                        step,
                        steps,
                        display,
                        state.auto_keyframe,
                        state.interp_new_frame,
                    )
                    .map(|(fr, _)| fr)
                    {
                        if (response.drag_started() || response.clicked()) && !state.stroke_active {
                            state.stroke_active = true;
                            push_snapshot(state, doc, fr);
                        }
                        let sel = state.selection.mask().cloned();
                        let f = &mut doc.layers[fr.li].channels[fr.ci].frames[fr.fi];
                        match state.tool {
                            Tool::Fill => {
                                if response.drag_started() || response.clicked() {
                                    let mw = f.mask.width();
                                    let mh = f.mask.height();
                                    let px = (ix.max(0.0) as u32).min(mw - 1);
                                    let py = (iy.max(0.0) as u32).min(mh - 1);
                                    // 填充后要把选区外的改动撤回来，否则填充会无视选区
                                    let before = if sel.is_some() {
                                        Some(f.mask.clone())
                                    } else {
                                        None
                                    };
                                    paint::flood_fill(&mut f.mask, px, py, value);
                                    if let (Some(b), Some(s)) = (before, sel.as_ref()) {
                                        restore_outside(&mut f.mask, &b, s);
                                    }
                                    painted = true;
                                }
                            }
                            Tool::Brush => {
                                let erase = value == 0;
                                let before = if sel.is_some() {
                                    Some(f.mask.clone())
                                } else {
                                    None
                                };
                                match state.last_pos {
                                    Some(prev) => paint::stroke(
                                        &mut f.mask,
                                        prev,
                                        (ix, iy),
                                        state.radius,
                                        erase,
                                    ),
                                    None => paint::stamp(&mut f.mask, ix, iy, state.radius, erase),
                                }
                                if let (Some(b), Some(s)) = (before, sel.as_ref()) {
                                    restore_outside(&mut f.mask, &b, s);
                                }
                                painted = true;
                            }
                            _ => {}
                        }
                    }
                    state.last_pos = Some((ix, iy));
                }
            } else {
                state.last_pos = None;
                state.stroke_active = false;
            }
        }

        Tool::Shape => {
            if editable && response.drag_started() {
                if let Some(pos) = response.interact_pointer_pos() {
                    let (ix, iy) = to_image(pos, rect, state.zoom, state.center);
                    state.drag_shape = Some((ix, iy, ix, iy));
                }
            }
            if response.dragged()
                && let (Some(p), Some(d)) = (response.interact_pointer_pos(), &mut state.drag_shape)
            {
                let (ix, iy) = to_image(p, rect, state.zoom, state.center);
                let (sx, sy, _, _) = *d;
                // Shift = 等比：把另一个角拉到正方形/正圆
                let (ex, ey) = if shift {
                    square_corner(sx, sy, ix, iy)
                } else {
                    (ix, iy)
                };
                state.drag_shape = Some((sx, sy, ex, ey));
                painted = false;
            }
            if response.drag_stopped()
                && let Some((sx, sy, ex, ey)) = state.drag_shape.take()
            {
                // 形状是松手才落地，所以在这里自动建帧是安全的：
                // 拖动过程中只画辅助线，不会每帧都建一次。
                if let Some(fr) = frame_ref_or_create(
                    doc,
                    step,
                    steps,
                    display,
                    state.auto_keyframe,
                    state.interp_new_frame,
                )
                .map(|(fr, _)| fr)
                {
                    push_snapshot(state, doc, fr);
                    let sel = state.selection.mask().cloned();
                    let v = state.value.byte();
                    let kind = state.shape;
                    let filled = state.shape_filled;
                    let lw = state.line_w;
                    let f = &mut doc.layers[fr.li].channels[fr.ci].frames[fr.fi];
                    let before = if sel.is_some() {
                        Some(f.mask.clone())
                    } else {
                        None
                    };
                    match kind {
                        ShapeKind::Rect => {
                            ops::draw_rect(&mut f.mask, sx, sy, ex, ey, v, filled, lw)
                        }
                        ShapeKind::Ellipse => {
                            ops::draw_ellipse(&mut f.mask, sx, sy, ex, ey, v, filled, lw)
                        }
                    }
                    if let (Some(b), Some(s)) = (before, sel.as_ref()) {
                        restore_outside(&mut f.mask, &b, s);
                    }
                    painted = true;
                }
            }
        }

        Tool::Smooth => {
            // 笔刷式局部平滑：按住拖动持续磨（像橡皮那样），松手结束。
            //
            // 和上一版"点一下平滑整片"的区别：
            // - 上一版一按就动整张图，无法只修某块
            // - 这一版只动笔刷圆内，且圆边按衰减收束，不留接缝
            //
            // 用 drag_started 压一次快照、拖动中每帧磨、松手停。
            // 每帧都从**当前内容**磨是安全的：smooth_brush 是局部操作，
            // 反复施加只会更平滑，不会像整片平滑那样越磨越糊。
            if editable
                && (response.dragged_by(egui::PointerButton::Primary)
                    || response.dragged_by(egui::PointerButton::Secondary))
                && let Some(pos) = response.interact_pointer_pos()
                && let Some(fr) = frame_ref_or_create(
                    doc,
                    step,
                    steps,
                    display,
                    state.auto_keyframe,
                    state.interp_new_frame,
                )
                .map(|(fr, _)| fr)
            {
                let (ix, iy) = to_image(pos, rect, state.zoom, state.center);
                if response.drag_started() {
                    push_snapshot(state, doc, fr);
                }
                // 标记"正在落笔"：App 层靠该标志跳过整片重烘，只显示当前参数点本身。
                // 没有这一步的话，磨的每一帧都会触发 135ms 的 texel 重烘焙，直接卡死。
                state.stroke_active = true;
                let sel = state.selection.mask().cloned();
                let (br, sr) = (state.radius, state.smooth_radius);
                let f = &mut doc.layers[fr.li].channels[fr.ci].frames[fr.fi];
                let n = ops::smooth_brush(&mut f.mask, ix, iy, br, sr, sel.as_ref());
                if n > 0 {
                    painted = true;
                }
            }
            // 松手：结束落笔态，App 层恢复走正常采样预览
            if response.drag_stopped() {
                state.stroke_active = false;
            }
            // 单击也磨一次（点按比拖更精确）
            if editable
                && response.clicked_by(egui::PointerButton::Primary)
                && let Some(pos) = response.interact_pointer_pos()
                && let Some(fr) = frame_ref_or_create(
                    doc,
                    step,
                    steps,
                    display,
                    state.auto_keyframe,
                    state.interp_new_frame,
                )
                .map(|(fr, _)| fr)
            {
                let (ix, iy) = to_image(pos, rect, state.zoom, state.center);
                push_snapshot(state, doc, fr);
                let sel = state.selection.mask().cloned();
                let (br, sr) = (state.radius, state.smooth_radius);
                let f = &mut doc.layers[fr.li].channels[fr.ci].frames[fr.fi];
                ops::smooth_brush(&mut f.mask, ix, iy, br, sr, sel.as_ref());
                painted = true;
            }
        }

        Tool::Select => {
            // 移动整块遮罩内容。有选区就只移动选区内的像素。
            //
            // **关键：始终从拖动起点的原始遮罩重算，不要一帧帧往上叠平移。**
            // 逐帧平移会让边界反复重采样，每采样一次就和原版错开一点，
            // 累积起来边界会变成明显的阶梯状毛刺（实测就能看到）。
            // 正确做法是记住起点快照 + 累计位移，每帧都从快照重算一次。
            if editable
                && response.drag_started()
                && let Some(pos) = response.interact_pointer_pos()
            {
                let (ix, iy) = to_image(pos, rect, state.zoom, state.center);
                state.select_origin = Some((ix, iy));
                state.select_base = None;
                state.select_base_sel = None;
                state.select_snapshot = false;
            }
            if response.dragged()
                && let (Some(p), Some(origin)) =
                    (response.interact_pointer_pos(), state.select_origin)
                && let Some(fr) = frame_ref(doc, step, steps)
            {
                let (ix, iy) = to_image(p, rect, state.zoom, state.center);
                // 总位移 = 当前指针 - 起点，而不是相邻帧的增量
                let total_x = ix - origin.0;
                let total_y = iy - origin.1;
                let sx = total_x.round() as i32;
                let sy = total_y.round() as i32;

                if !state.select_snapshot {
                    push_snapshot(state, doc, fr);
                    state.select_snapshot = true;
                    // 记下起点的原始内容与原始选区
                    state.select_base =
                        Some(doc.layers[fr.li].channels[fr.ci].frames[fr.fi].mask.clone());
                    state.select_base_sel = Some(state.selection.clone());
                }

                if let (Some(base), Some(base_sel)) =
                    (state.select_base.clone(), state.select_base_sel.clone())
                {
                    let f = &mut doc.layers[fr.li].channels[fr.ci].frames[fr.fi];
                    f.mask = base.clone();
                    // 没动就别白算一遍
                    if sx != 0 || sy != 0 {
                        move_mask_content(&mut f.mask, sx, sy, base_sel.mask());
                        painted = true;
                    }
                    // 选区显示位置 = 原始选区 + 总位移（同样从原始值算）
                    state.selection = base_sel;
                    state.selection.translate(sx, sy);
                }
            }
            if response.drag_stopped() {
                state.select_origin = None;
                state.select_base = None;
                state.select_base_sel = None;
            }
        }

        Tool::Lasso => {
            if response.drag_started()
                && let Some(pos) = response.interact_pointer_pos()
            {
                let (ix, iy) = to_image(pos, rect, state.zoom, state.center);
                match state.select_mode {
                    SelectMode::Rect => state.drag_shape = Some((ix, iy, ix, iy)),
                    SelectMode::Lasso => {
                        state.lasso_pts.clear();
                        state.lasso_pts.push((ix, iy));
                    }
                }
            }
            if response.dragged() {
                if let Some(p) = response.interact_pointer_pos() {
                    let (ix, iy) = to_image(p, rect, state.zoom, state.center);
                    match state.select_mode {
                        SelectMode::Rect => {
                            if let Some(d) = &mut state.drag_shape {
                                let (sx, sy, _, _) = *d;
                                let (ex, ey) = if shift {
                                    square_corner(sx, sy, ix, iy)
                                } else {
                                    (ix, iy)
                                };
                                state.drag_shape = Some((sx, sy, ex, ey));
                            }
                        }
                        SelectMode::Lasso => {
                            // 抽稀：距离太近的点不采，否则一条线几百个点，多边形填充会慢
                            let keep = state.lasso_pts.last().is_none_or(|(px, py)| {
                                ((ix - px).powi(2) + (iy - py).powi(2)) > 4.0
                            });
                            if keep {
                                state.lasso_pts.push((ix, iy));
                            }
                        }
                    }
                }
            }
            if response.drag_stopped() {
                let (w, h) = (doc.width, doc.height);
                let new_mask = match state.select_mode {
                    SelectMode::Rect => state
                        .drag_shape
                        .take()
                        .map(|(sx, sy, ex, ey)| Selection::from_rect(w, h, sx, sy, ex, ey))
                        .unwrap_or_else(|| Mask::new(w, h)),
                    SelectMode::Lasso => {
                        let pts = std::mem::take(&mut state.lasso_pts);
                        if pts.len() >= 3 {
                            Selection::from_polygon(w, h, &pts)
                        } else {
                            Mask::new(w, h)
                        }
                    }
                };
                apply_selection(state, new_mask, op);
                state.edge_cache = None;
            }
        }
    }

    // ---- 右键菜单 ----
    // **必须每帧都调用**：`context_menu` 是声明式的，由 `context_menu` 自己维护弹窗开关。
    // 包在 `if secondary_clicked()` 里的话，只有右键那一次会调用，
    // 之后弹窗虽然被标记为打开却没人渲染 —— 表现就是"右键没反应"。
    //
    // 右键拖动画反色的冲突由 egui 自己处理：egui 只在"右键单击（未拖动）"时开菜单。
    response.context_menu(|ui| {
        if let Some(a) = crate::ui::menus::canvas_context_menu(ui) {
            action = Some(a);
        }
    });

    // ---- 绘制到屏幕 ----
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_rgb(0x16, 0x16, 0x16));

    let tl = to_screen(0.0, 0.0, rect, state.zoom, state.center);
    let br = to_screen(
        doc.width as f32,
        doc.height as f32,
        rect,
        state.zoom,
        state.center,
    );
    let img_rect = Rect::from_min_max(tl, br);
    let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));

    // 棋盘格（表示"没有底图"的区域）
    if doc.background.is_none() || !doc.background_visible {
        draw_checker(&painter, img_rect, state.zoom);
    }

    // 底图
    if doc.background_visible {
        if let Some(bg) = &doc.background {
            // 版本号必须能区分"换了一张图"：只看长度不够（同尺寸图长度一样），
            // 取数据指针 + 长度 + 首个/中间像素做指纹。
            let raw = bg.as_raw();
            let version = (raw.as_ptr() as u64)
                ^ (raw.len() as u64).rotate_left(17)
                ^ (if raw.is_empty() {
                    0
                } else {
                    raw[raw.len() / 2] as u64
                } << 8);
            if state.tex_bg.is_none() || state.bg_version != version {
                let img = ColorImage::from_rgba_unmultiplied(
                    [bg.width() as usize, bg.height() as usize],
                    raw,
                );
                match &mut state.tex_bg {
                    Some(t) => t.set(img, TextureOptions::LINEAR),
                    None => {
                        state.tex_bg =
                            Some(ui.ctx().load_texture("bg", img, TextureOptions::LINEAR))
                    }
                }
                state.bg_version = version;
            }
            if let Some(t) = &state.tex_bg {
                painter.image(t.id(), img_rect, uv, Color32::WHITE);
            }
        }
    }

    // 遮罩（半透明叠在图上）
    // 内容脏、颜色变、透明度变，任一都要重做纹理
    let style_key = (
        (state.mask_color[0] * 1000.0) as i32,
        (state.mask_color[1] * 1000.0) as i32,
        (state.mask_color[2] * 1000.0) as i32,
        (state.mask_alpha * 1000.0) as i32,
    );
    if show_mask && (state.mask_dirty || state.tex_mask.is_none() || state.mask_style != style_key)
    {
        let rgba = mask_to_rgba(display, state.mask_color, state.mask_alpha);
        let img = ColorImage::from_rgba_unmultiplied(
            [display.width() as usize, display.height() as usize],
            &rgba,
        );
        match &mut state.tex_mask {
            Some(t) => t.set(img, TextureOptions::NEAREST),
            None => {
                state.tex_mask = Some(ui.ctx().load_texture("mask", img, TextureOptions::NEAREST))
            }
        }
        state.mask_dirty = false;
        state.mask_style = style_key;
    }
    if show_mask && let Some(t) = &state.tex_mask {
        painter.image(t.id(), img_rect, uv, Color32::WHITE);
    }

    // UV 线框：叠在遮罩之上，用于对照"贴图区域 ↔ 模型部位"。
    //
    // 画在遮罩之后是刻意的：遮罩是半透明的，先画 UV 会被盖住颜色，
    // 深色遮罩区里基本看不见线。放在最后才能保证任何底色下都可读。
    //
    // 坐标变换的结果按"画布矩形 + 线段数"缓存：UV 与布局不变时
    // 每帧重算 4 万次乘法是纯浪费。
    if state.show_uv && !state.uv_edges.is_empty() {
        let key = (
            state.uv_edges.len() as u64,
            [
                img_rect.min.x as i32,
                img_rect.min.y as i32,
                img_rect.width() as i32,
                img_rect.height() as i32,
            ],
        );
        if state.uv_screen_key != Some(key) {
            state.uv_screen = sdf_mask_tool::uv_overlay::to_screen(
                &state.uv_edges,
                [img_rect.min.x, img_rect.min.y],
                [img_rect.width(), img_rect.height()],
            );
            state.uv_screen_key = Some(key);
        }

        let c = Color32::from_rgb(
            (state.uv_color[0].clamp(0.0, 1.0) * 255.0) as u8,
            (state.uv_color[1].clamp(0.0, 1.0) * 255.0) as u8,
            (state.uv_color[2].clamp(0.0, 1.0) * 255.0) as u8,
        );
        let stroke = egui::Stroke::new(1.0, c.gamma_multiply(0.85));

        // 一次性塞进 painter，而不是逐个 line_segment：
        // 每次调用都有独立开销，4 万次会明显拖慢构建阶段。
        let segs: Vec<egui::Shape> = state
            .uv_screen
            .iter()
            .map(|e| {
                egui::Shape::line_segment(
                    [egui::pos2(e.a[0], e.a[1]), egui::pos2(e.b[0], e.b[1])],
                    stroke,
                )
            })
            .collect();
        painter.extend(segs);
    }

    // 违规像素：红色叠加，一眼看出"哪里破坏了嵌套"
    if !violations.is_empty() {
        let vkey = violations.len() as u64
            ^ (violations.iter().filter(|b| **b).count() as u64)
            ^ (state.mask_dirty as u64);
        if state.tex_viol.is_none() || state.viol_key != vkey {
            let rgba = violations_to_rgba(violations, display.width(), display.height());
            let img = ColorImage::from_rgba_unmultiplied(
                [display.width() as usize, display.height() as usize],
                &rgba,
            );
            match &mut state.tex_viol {
                Some(t) => t.set(img, TextureOptions::NEAREST),
                None => {
                    state.tex_viol = Some(ui.ctx().load_texture(
                        "mx_viol",
                        img,
                        TextureOptions::NEAREST,
                    ))
                }
            }
            state.viol_key = vkey;
        }
        if let Some(t) = &state.tex_viol {
            painter.image(t.id(), img_rect, uv, Color32::WHITE);
        }
    }

    // 画布边框
    painter.rect_stroke(
        img_rect,
        0.0,
        egui::Stroke::new(1.0, Color32::from_rgb(0x50, 0x50, 0x50)),
        egui::StrokeKind::Outside,
    );

    // ---------------------------------------------------------- 选区蚂蚁线
    //
    // PS 的流动虚线：黑白相间的线段，相位随时间滚动。
    // 这里不生成纹理，直接把每条边缘线段按世界坐标切成小段交替上色——
    // 线段数量只和边界长度有关（周长级），远比整幅像素少。
    if state.selection.is_active() {
        // 相位每帧加 1，循环 8 段；请求重绘以维持动画
        state.march_phase = state.march_phase.wrapping_add(1) % 8;
        ui.ctx().request_repaint();

        // 边缘线段缓存：选区内容指纹不变就复用。
        // 用"数量 + 包围盒"做指纹：两者都变时才会重算，够用且便宜。
        let key = {
            let b = state.selection.bounds();
            state.selection.count()
                ^ b.map(|(x0, y0, x1, y1)| {
                    (x0 as usize) << 48 ^ (y0 as usize) << 32 ^ (x1 as usize) << 16 ^ (y1 as usize)
                })
                .unwrap_or(0)
        };
        if state.edge_cache.as_ref().map(|(k, _)| *k) != Some(key) {
            state.edge_cache = Some((key, state.selection.edges()));
        }
        let edges = &state.edge_cache.as_ref().unwrap().1;

        // 虚线的节奏。
        //
        // 选区边界是由**大量 1 像素长的短线段**拼起来的（不是几条长线），
        // 所以不能"逐条线段判断够不够长再切"——低缩放下每条只有 0.59px，
        // 全被跳过，画面上就什么也看不到。
        //
        // 正确做法：把每条短线段整体当作一个"色块单位"，
        // 用边线的**世界坐标位置**决定黑白，相邻像素自然交替。
        let zoom = state.zoom;
        // 一个虚线格对应的世界像素数。缩得越小格子越多，视觉上仍是标准虚线。
        let dash_world = (DASH_PX / zoom.max(0.01)).max(1.0);
        let phase = state.march_phase as f32 * dash_world / 8.0;

        for e in edges {
            let a = to_screen(e.x0, e.y0, rect, zoom, state.center);
            let b = to_screen(e.x1, e.y1, rect, zoom, state.center);
            // 用线段中点的世界坐标定色，同一像素内保持一致
            let mid_x = (e.x0 + e.x1) * 0.5;
            let mid_y = (e.y0 + e.y1) * 0.5;
            // 横向线段按 x 排色，纵向线段按 y 排色，保证沿线连续
            let along = if (e.y1 - e.y0).abs() < 0.01 {
                mid_x
            } else {
                mid_y
            };
            let on = (((along / dash_world) + phase) as i32).rem_euclid(2) == 0;
            let c = if on {
                Color32::WHITE
            } else {
                Color32::from_rgb(0x10, 0x10, 0x10)
            };
            painter.line_segment([a, b], egui::Stroke::new(1.0, c));
        }
    }

    // ---------------------------------------------------------- 拖动中的形状预览
    if let Some((sx, sy, ex, ey)) = state.drag_shape {
        let a = to_screen(sx, sy, rect, state.zoom, state.center);
        let b = to_screen(ex, ey, rect, state.zoom, state.center);
        let r = Rect::from_two_pos(a, b);
        let c = match state.tool {
            Tool::Lasso => Color32::from_rgb(0xff, 0xd0, 0x60),
            _ => Color32::from_rgb(0x7a, 0xc0, 0xff),
        };
        match (state.tool, state.shape) {
            (Tool::Shape, ShapeKind::Ellipse) => {
                // 椭圆预览：用多边形近似，64 段足够圆
                let n = 64;
                let mut pts = Vec::with_capacity(n);
                for i in 0..n {
                    let t = i as f32 / n as f32 * std::f32::consts::TAU;
                    pts.push(egui::pos2(
                        r.center().x + r.width() * 0.5 * t.cos(),
                        r.center().y + r.height() * 0.5 * t.sin(),
                    ));
                }
                painter.add(egui::Shape::closed_line(pts, egui::Stroke::new(1.0, c)));
            }
            _ => {
                painter.rect_stroke(r, 0.0, egui::Stroke::new(1.0, c), egui::StrokeKind::Outside);
            }
        }
    }

    // 套索拖动中的自由曲线
    if !state.lasso_pts.is_empty() {
        let pts: Vec<egui::Pos2> = state
            .lasso_pts
            .iter()
            .map(|(x, y)| to_screen(*x, *y, rect, state.zoom, state.center))
            .collect();
        if pts.len() >= 2 {
            painter.add(egui::Shape::line(
                pts.clone(),
                egui::Stroke::new(1.0, Color32::from_rgb(0xff, 0xd0, 0x60)),
            ));
        }
        // 首尾虚线，提示"松手会闭合"
        if let (Some(&f), Some(&l)) = (pts.first(), pts.last()) {
            painter.line_segment(
                [l, f],
                egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xff, 0xd0, 0x60, 120)),
            );
        }
    }

    // 笔刷光标。黑笔用深色圈 + 浅色外描边，白笔用浅色圈，深底浅底都看得见。
    if response.hovered() {
        if let Some(pos) = response.hover_pos() {
            let r = (state.radius * state.zoom).max(1.0);
            match state.tool {
                Tool::Brush => {
                    let (inner, outer) = match state.value {
                        PaintValue::Black => (
                            Color32::from_rgb(0x10, 0x10, 0x10),
                            Color32::from_white_alpha(190),
                        ),
                        PaintValue::White => (
                            Color32::from_rgb(0xf4, 0xf4, 0xf4),
                            Color32::from_black_alpha(190),
                        ),
                    };
                    painter.circle_stroke(pos, r, egui::Stroke::new(3.0, outer));
                    painter.circle_stroke(pos, r, egui::Stroke::new(1.4, inner));
                }
                Tool::Fill => {
                    let c = if state.value == PaintValue::Black {
                        Color32::from_white_alpha(200)
                    } else {
                        Color32::from_black_alpha(200)
                    };
                    painter.circle_stroke(pos, 5.0, egui::Stroke::new(1.5, c));
                    painter.line_segment(
                        [pos - egui::vec2(8.0, 0.0), pos + egui::vec2(8.0, 0.0)],
                        egui::Stroke::new(1.0, c),
                    );
                    painter.line_segment(
                        [pos - egui::vec2(0.0, 8.0), pos + egui::vec2(0.0, 8.0)],
                        egui::Stroke::new(1.0, c),
                    );
                }
                Tool::Smooth => {
                    // 平滑是笔刷式工具：光标和画笔一样是笔刷圈（表示作用范围），
                    // 里面再加一个淡圈表示"模糊半径"——
                    // 两者配合才能让人预判：作用多大范围、磨得多细。
                    let outer = (state.radius * state.zoom).max(1.0);
                    let inner = (state.smooth_radius * state.zoom).max(1.0);
                    // 外圈：作用范围（和画笔一致的双描边，深底浅底都看得见）
                    painter.circle_stroke(
                        pos,
                        outer,
                        egui::Stroke::new(3.0, Color32::from_black_alpha(150)),
                    );
                    painter.circle_stroke(
                        pos,
                        outer,
                        egui::Stroke::new(1.4, Color32::from_rgb(0x9a, 0xd0, 0xff)),
                    );
                    // 内圈：模糊半径（半透明，表示"磨的粒度"）
                    painter.circle_stroke(
                        pos,
                        inner.min(outer),
                        egui::Stroke::new(
                            1.0,
                            Color32::from_rgba_unmultiplied(0x9a, 0xd0, 0xff, 110),
                        ),
                    );
                }
                Tool::Select => {
                    // 移动：四向箭头，用一个十字 + 四个小三角示意
                    let c = Color32::from_white_alpha(210);
                    let d = 7.0;
                    painter.line_segment(
                        [pos - egui::vec2(d, 0.0), pos + egui::vec2(d, 0.0)],
                        egui::Stroke::new(1.2, c),
                    );
                    painter.line_segment(
                        [pos - egui::vec2(0.0, d), pos + egui::vec2(0.0, d)],
                        egui::Stroke::new(1.2, c),
                    );
                    for (ox, oy) in [(d, 0.0), (-d, 0.0), (0.0, d), (0.0, -d)] {
                        let tip = pos + egui::vec2(ox * 1.3, oy * 1.3);
                        let perp = egui::vec2(oy * 0.45, ox * 0.45);
                        painter.add(egui::Shape::convex_polygon(
                            vec![
                                tip,
                                pos + egui::vec2(ox, oy) + perp,
                                pos + egui::vec2(ox, oy) - perp,
                            ],
                            c,
                            egui::Stroke::NONE,
                        ));
                    }
                }
                Tool::Shape | Tool::Lasso => {
                    // 这两个工具靠拖动预览线表达自己，光标只需一个十字准星
                    let c = Color32::from_white_alpha(190);
                    painter.line_segment(
                        [pos - egui::vec2(6.0, 0.0), pos + egui::vec2(6.0, 0.0)],
                        egui::Stroke::new(1.0, c),
                    );
                    painter.line_segment(
                        [pos - egui::vec2(0.0, 6.0), pos + egui::vec2(0.0, 6.0)],
                        egui::Stroke::new(1.0, c),
                    );
                }
            }
        }
    }

    CanvasOutput { painted, action }
}

/// 把违规标记做成 RGBA：违规像素红色半透明，其余全透明。
fn violations_to_rgba(bad: &[bool], w: u32, h: u32) -> Vec<u8> {
    let n = (w * h) as usize;
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        if bad.get(i).copied().unwrap_or(false) {
            // 醒目的红色 + 较高不透明度，确保在深色背景上看得清
            out.extend_from_slice(&[230, 60, 60, 120]);
        } else {
            out.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    out
}

fn draw_checker(painter: &egui::Painter, rect: Rect, zoom: f32) {
    let cell = (16.0 * zoom).clamp(8.0, 64.0);
    let c1 = Color32::from_rgb(0x1e, 0x1e, 0x1e);
    let c2 = Color32::from_rgb(0x26, 0x26, 0x26);
    let cols = (rect.width() / cell).ceil() as i32;
    let rows = (rect.height() / cell).ceil() as i32;
    for y in 0..rows {
        for x in 0..cols {
            let c = if (x + y) % 2 == 0 { c1 } else { c2 };
            let r = Rect::from_min_size(
                egui::pos2(rect.min.x + x as f32 * cell, rect.min.y + y as f32 * cell),
                Vec2::splat(cell),
            );
            painter.rect_filled(r.intersect(rect), 0.0, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdf_mask_tool::model::doc::Doc;

    /// 自动插帧：没有关键帧的位置落笔时，应当当场建一帧再返回。
    ///
    /// 约定行为：在没有参数点的位置落笔时自动加点，再在该点上绘画。
    /// 回归风险很高——`editable` 参数一旦重新绑到 `is_on_keyframe()`，
    /// 整条绘制分支就会被跳过，表现为"点了没反应"。
    #[test]
    fn 没帧的位置落笔会自动建帧() {
        let mut doc = Doc::with_steps(64, 64, 32);
        doc.active_channel_mut()
            .insert_frame(0.0, Mask::new(64, 64));
        doc.active_channel_mut()
            .insert_frame(90.0, Mask::new(64, 64));

        // 45° 那格没有关键帧
        let step = sdf_mask_tool::model::doc::angle_to_step(45.0, 32);
        assert!(frame_ref(&doc, step, 32).is_none(), "前提：那里本来没帧");
        let before = doc.active_channel_ref().frames.len();

        // 屏幕内容：造一张有内容的图，便于验证"起点来自屏幕"
        let mut display = Mask::new(64, 64);
        for y in 20..44 {
            for x in 20..44 {
                display.set(x, y, 1);
            }
        }

        let got = frame_ref_or_create(&mut doc, step, 32, &display, true, true);
        assert!(got.is_some(), "开启自动插帧时应建帧");
        let (_, created) = got.unwrap();
        assert!(created, "应报告「新建了」");
        assert_eq!(
            doc.active_channel_ref().frames.len(),
            before + 1,
            "帧数应 +1"
        );

        // 起点必须是屏幕内容（逐像素）
        let fr = frame_ref(&doc, step, 32).expect("建完应能取到");
        let mask = &doc.layers[fr.li].channels[fr.ci].frames[fr.fi].mask;
        let diff = (0..(64 * 64) as usize)
            .filter(|&i| mask.data()[i] != display.data()[i])
            .count();
        assert_eq!(diff, 0, "新帧内容应与屏幕内容逐像素一致");
    }

    /// 关掉开关时退回旧行为：不建帧，返回 None（等于画不了）。
    #[test]
    fn 关闭自动插帧时不建帧() {
        let mut doc = Doc::with_steps(64, 64, 32);
        doc.active_channel_mut()
            .insert_frame(0.0, Mask::new(64, 64));
        doc.active_channel_mut()
            .insert_frame(90.0, Mask::new(64, 64));
        let step = sdf_mask_tool::model::doc::angle_to_step(45.0, 32);
        let before = doc.active_channel_ref().frames.len();

        let got = frame_ref_or_create(&mut doc, step, 32, &Mask::new(64, 64), false, true);
        assert!(got.is_none(), "关掉开关时不该建帧");
        assert_eq!(doc.active_channel_ref().frames.len(), before, "帧数不该变");
    }

    /// 已经在关键帧上时，不应重复建帧。
    #[test]
    fn 有关键帧时不重复建帧() {
        let mut doc = Doc::with_steps(64, 64, 32);
        doc.active_channel_mut()
            .insert_frame(0.0, Mask::new(64, 64));
        doc.active_channel_mut()
            .insert_frame(90.0, Mask::new(64, 64));
        let step = 0;
        let before = doc.active_channel_ref().frames.len();

        let got = frame_ref_or_create(&mut doc, step, 32, &Mask::new(64, 64), true, true);
        assert!(got.is_some());
        assert!(!got.unwrap().1, "不该报告「新建了」");
        assert_eq!(doc.active_channel_ref().frames.len(), before, "帧数不该变");
    }

    /// 落笔点离已有帧很近时，应当**复用那一帧**而不是另建一帧。
    ///
    /// 为什么这很重要：点在接近末点（如 0.995）时本意几乎肯定是末点。
    /// 若另建一帧，就变成「0°空 → 89.5°有内容 → 90°空」＝「遮了又亮」，
    /// 两种方向都编码不了（实测 4200 像素违规），且原因不易察觉。
    #[test]
    fn 落笔点靠近已有帧时复用而不新建() {
        let steps = 32usize;
        let mut doc = Doc::with_steps(64, 64, steps);
        doc.active_channel_mut()
            .insert_frame(0.0, Mask::new(64, 64));
        doc.active_channel_mut()
            .insert_frame(90.0, Mask::new(64, 64));

        // 半格 = 45/(32-1) ≈ 1.45°。取 90° 附近 1.0° 的点
        let angle = 89.0_f32;
        let step = sdf_mask_tool::model::doc::angle_to_step(angle, steps);
        // 该格可能正好没有关键帧
        if frame_ref(&doc, step, steps).is_none() {
            let before = doc.active_channel_ref().frames.len();
            let got = frame_ref_or_create(&mut doc, step, steps, &Mask::new(64, 64), true, true);
            assert!(got.is_some(), "应返回一个帧引用");
            let (_, created) = got.unwrap();
            assert!(!created, "靠得太近，应当吸附而不是新建");
            assert_eq!(doc.active_channel_ref().frames.len(), before, "帧数不该变");
        }
    }

    /// 远离所有已有帧时，仍应正常新建。
    #[test]
    fn 远离已有帧时正常新建() {
        let steps = 32usize;
        let mut doc = Doc::with_steps(64, 64, steps);
        doc.active_channel_mut()
            .insert_frame(0.0, Mask::new(64, 64));
        doc.active_channel_mut()
            .insert_frame(90.0, Mask::new(64, 64));
        let before = doc.active_channel_ref().frames.len();

        // 45° 离两端各 45°，远大于半格 1.45°
        let step = sdf_mask_tool::model::doc::angle_to_step(45.0, steps);
        assert!(frame_ref(&doc, step, steps).is_none(), "前提：45° 本来没帧");
        let got = frame_ref_or_create(&mut doc, step, steps, &Mask::new(64, 64), true, true);
        assert!(got.is_some());
        assert!(got.unwrap().1, "应当是新建的");
        assert_eq!(
            doc.active_channel_ref().frames.len(),
            before + 1,
            "帧数应 +1"
        );
    }

    /// 空通道（一张帧都没有）：没有可插的值，应建空白帧。
    #[test]
    fn 空通道落笔建空白帧() {
        let mut doc = Doc::with_steps(64, 64, 32);
        assert!(doc.active_channel_ref().frames.is_empty(), "前提：空通道");
        let step = sdf_mask_tool::model::doc::angle_to_step(45.0, 32);

        // 即使传了有内容的 display，也不该用 display（无相邻点可插）
        let mut display = Mask::new(64, 64);
        display.fill(1);
        let got = frame_ref_or_create(&mut doc, step, 32, &display, true, true);
        assert!(got.is_some(), "应建出第一帧");
        let fr = frame_ref(&doc, step, 32).unwrap();
        let mask = &doc.layers[fr.li].channels[fr.ci].frames[fr.fi].mask;
        assert!(
            mask.data().iter().all(|&v| v == 0),
            "空通道应建空白帧，不能拿屏幕内容（那种情况屏幕也是空的）"
        );
    }
}
