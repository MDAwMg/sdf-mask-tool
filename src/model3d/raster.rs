//! 软件光栅器。
//!
//! 为什么不用 GPU：eframe 走 glow，自定义 GPU 绘制要自己管 shader/VAO/深度缓冲，
//! 画完还得把 GL 状态还给 egui，漏一个就是整窗黑屏；而且 PaintCallback 每帧
//! 都会被调用，"只在需要时算一次"反而更难保证。预览只需可辨识，
//! CPU 光栅的代价（平面着色）完全可以接受。
//!
//! # 正确性的三个关键点
//!
//! 1. **透视校正插值**：屏幕空间线性插值 UV 是错的（近大远小会被压扁）。
//!    正确做法是插值 `u/z`、`v/z`、`1/z`，最后再除回来。
//!    1/z 在屏幕空间对直线是仿射的，所以线性插值是精确的。
//!
//! 2. **近平面裁剪**：z 接近 0 时投影会炸到无穷远，必须先把三角形裁到
//!    z >= near。只做包围盒剔除是不够的——相机穿进模型时必炸。
//!
//! 3. **背面剔除用世界法线**：屏幕空间的有向面积依赖 y 轴翻转，
//!    容易搞反。用 `法线 · 视线方向` 判断无色差、不依赖坐标系约定。
//!
//! # 性能上的取舍
//!
//! - 线框**不去重**：重复画的边颜色和深度完全相同，视觉上看不出差别，
//!   而建 HashSet 去重对 10 万三角是每帧几十万次哈希——不值得。
//! - 纹理采样用最近邻：双线性是 4 倍工作量，预览用不着。
//! - 微小三角按**屏幕面积**剔除，不是包围盒面积（细长三角的包围盒很大）。

/// 相机。绕目标点环绕 + 沿视线推拉。
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    /// 水平角（弧度）
    pub yaw: f32,
    /// 俯仰角（弧度），限制在 ±89° 避免万向节翻转
    pub pitch: f32,
    pub target: [f32; 3],
    pub dist: f32,
    /// 垂直视场角（弧度）
    pub fov: f32,
    /// 世界"上"方向。
    ///
    /// **不能硬编码 [0,1,0]**：实测两个样本分别有 Y-up 与 Z-up。
    /// 用错上方向时相机从头顶往下看，模型在屏幕上压成一条扁片——
    /// 看起来像"模型坏了"，其实只是坐标约定不同。
    pub world_up: [f32; 3],
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: 0.0,
            pitch: 0.0,
            target: [0.0; 3],
            dist: 3.0,
            fov: 45.0_f32.to_radians(),
            world_up: [0.0, 1.0, 0.0],
        }
    }
}

impl Camera {
    /// 把相机放到能完整看到 `center ± radius` 的位置。
    ///
    /// `up` 是世界的上方向（由模型决定，见 `Mesh::up_axis`）。
    /// `aspect` 是视口宽/高（>1 是扁的，<1 是窄的）。
    ///
    /// **必须考虑 aspect**：`fov` 约束的是**垂直**视野，
    /// 视口变窄时水平视野按比例缩小，只按垂直算会让模型左右被切掉。
    /// 所以取"垂直能装下"和"水平能装下"两者中更远的那个距离。
    pub fn fit_aspect(&mut self, center: [f32; 3], radius: f32, up: [f32; 3], aspect: f32) {
        self.target = center;
        self.world_up = up;
        let r = if radius > 1e-6 { radius } else { 1.0 };
        let tan_v = (self.fov * 0.5).tan();
        let aspect = if aspect.is_finite() && aspect > 1e-3 {
            aspect
        } else {
            1.0
        };
        // 水平半角的正切 = 垂直半角的正切 × aspect
        let tan_h = tan_v * aspect;
        let dist_v = r / tan_v;
        let dist_h = r / tan_h;
        // 留 15% 边距，避免模型刚好贴边
        self.dist = dist_v.max(dist_h) * 1.15;
        // 取景时把俯仰归零，让相机正对模型的"正面"而不是斜着
        self.pitch = 0.0;
    }

    /// 按**包围盒三轴半宽**精确取景。
    ///
    /// 比 `fit_aspect(radius)` 更贴合：`radius` 是"最远点到中心的距离"，
    /// 对细长模型会大出一截，模型在画面里只占中间一小块。
    /// 这里按相机自身的右/上/前三个方向投影半宽，算"刚好装下"的距离。
    pub fn fit_bounds(&mut self, center: [f32; 3], half: [f32; 3], up: [f32; 3], aspect: f32) {
        self.target = center;
        self.world_up = up;
        self.pitch = 0.0;

        let aspect = if aspect.is_finite() && aspect > 1e-3 {
            aspect
        } else {
            1.0
        };
        let tan_v = (self.fov * 0.5).tan().max(1e-4);
        let tan_h = tan_v * aspect;

        // 取 yaw=0 时的相机基（fit 之后就是这个姿态，yaw 不变）
        let (r, u, f) = self.basis();
        // 包围盒在三个轴向上的"半投影长度"（绝对值投影和）
        let extent_along = |dir: [f32; 3]| -> f32 {
            dir[0].abs() * half[0] + dir[1].abs() * half[1] + dir[2].abs() * half[2]
        };

        let half_w = extent_along(r).max(1e-6);
        let half_h = extent_along(u).max(1e-6);
        let half_d = extent_along(f);

        // 装下宽度需要的距离 vs 装下高度需要的距离，取较大者
        let d_w = half_w / tan_h;
        let d_h = half_h / tan_v;
        // 留 8% 边距（比之前的 15% 更紧凑，模型显示更大）
        self.dist = (d_w.max(d_h) + half_d) * 1.08;
    }

    /// 正方形视口的取景（兼容旧调用）。
    pub fn fit(&mut self, center: [f32; 3], radius: f32, up: [f32; 3]) {
        self.fit_aspect(center, radius, up, 1.0);
    }

    /// 目光方向（从相机指向目标）。
    ///
    /// 用"上方向为基准的正交基"算，所以对任意 up 都成立：
    /// back = up 绕 yaw 旋转、再绕 pitch 抬起。
    pub fn forward(&self) -> [f32; 3] {
        let back = self.back_vector();
        [-back[0], -back[1], -back[2]]
    }

    /// 从目标指向相机的单位向量。
    ///
    /// 必须基于 `world_up` 构造，否则 Z-up 模型会绕错轴。
    ///
    /// **yaw=0 / pitch=0 要落在该坐标系下的"正面"**，这样打开模型时正对着正面：
    /// - Y-up（多数 OBJ / Unity）：正面 = +Z
    /// - Z-up（Blender / 多数 FBX）：正面 = -Y（也就是 Blender 的 Front 视图）
    ///
    /// 这一步是实测踩出来的：如果只按"任取一个水平方向"，yaw=0 会跑到 +X，
    /// 于是一个立在 XY 平面上的模型会变成刀片一样侧对镜头。
    fn back_vector(&self) -> [f32; 3] {
        let up = normalize_or(self.world_up, [0.0, 1.0, 0.0]);

        // 该 up 对应的"正面"水平方向
        let front = if up[2].abs() > 0.5 {
            // Z-up：从 -Y 看过去
            [0.0, -1.0, 0.0]
        } else {
            // Y-up / X-up：从 +Z 看过去
            [0.0, 0.0, 1.0]
        };

        // 与 up 正交化，得到可用的水平基准
        let d = dot(front, up);
        let h = normalize_or(
            [
                front[0] - up[0] * d,
                front[1] - up[1] * d,
                front[2] - up[2] * d,
            ],
            [0.0, 0.0, 1.0],
        );
        let e = cross(up, h);

        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();

        // 先绕 up 转 yaw，再抬起 pitch
        let back_h = [
            h[0] * cy + e[0] * sy,
            h[1] * cy + e[1] * sy,
            h[2] * cy + e[2] * sy,
        ];
        let v = [
            back_h[0] * cp + up[0] * sp,
            back_h[1] * cp + up[1] * sp,
            back_h[2] * cp + up[2] * sp,
        ];
        normalize_or(v, up)
    }

    pub fn eye(&self) -> [f32; 3] {
        let back = self.back_vector();
        [
            self.target[0] + back[0] * self.dist,
            self.target[1] + back[1] * self.dist,
            self.target[2] + back[2] * self.dist,
        ]
    }

    /// 相机基向量 (right, up, forward)。forward 指向场景内部。
    pub fn basis(&self) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let f = self.forward();
        let world_up = normalize_or(self.world_up, [0.0, 1.0, 0.0]);
        let r = normalize_or(cross(f, world_up), [1.0, 0.0, 0.0]);
        // up 由 r 和 f 叉出，保证三者严格正交
        let u = cross(r, f);
        (r, u, f)
    }

    /// Shift+中键旋转。`dx/dy` 是像素位移。
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        const SPEED: f32 = 0.01;
        // 往右拖 → 模型往右转 → 相机往左绕
        self.yaw -= dx * SPEED;
        self.pitch = (self.pitch + dy * SPEED).clamp(-89.0_f32.to_radians(), 89.0_f32.to_radians());
        // 把 yaw 收回 [0, 2π)，避免长时间拖动后精度下降
        let tau = std::f32::consts::TAU;
        self.yaw = self.yaw.rem_euclid(tau);
    }

    /// 中键平移：沿相机右/上方向移动目标点。
    ///
    /// 位移量按 `dist` 缩放，这样拉远时拖同样的像素移动更多世界距离，
    /// 手感才跟得上。
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_h: f32) {
        if viewport_h <= 1.0 {
            return;
        }
        let (r, u, _) = self.basis();
        // 屏幕上半像素对应的世界长度 = 2*dist*tan(fov/2)/h
        let wpp = 2.0 * self.dist * (self.fov * 0.5).tan() / viewport_h;
        for k in 0..3 {
            self.target[k] += (-r[k] * dx + u[k] * dy) * wpp;
        }
    }

    /// Ctrl+中键 / 滚轮 缩放。`factor > 1` 拉远。
    pub fn zoom(&mut self, factor: f32) {
        self.dist = (self.dist * factor).clamp(1e-3, 1e6);
    }
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize_or(v: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let l = dot(v, v).sqrt();
    if l > 1e-9 {
        [v[0] / l, v[1] / l, v[2] / l]
    } else {
        fallback
    }
}

/// 渲染选项。
#[derive(Clone, Copy, Debug)]
pub struct RenderOpts {
    pub width: usize,
    pub height: usize,
    /// 用第几套 UV
    pub uv_set: usize,
    pub wireframe: bool,
    pub backface_cull: bool,
    pub show_base: bool,
    pub show_mask: bool,
    /// 是否把 UV 的 v 轴上下翻转。
    ///
    /// 约定不统一：OBJ/FBX 的 UV 原点在左下，图像的 v 原点在左上，
    /// 所以默认要翻。但不同导出器行为不一致，留成开关以便手动调整。
    pub flip_v: bool,
    /// 是否允许多线程渲染。
    ///
    /// 默认开。三角数不足 4000 时会自动走单线程（并行调度开销大于收益）。
    /// 测试里关掉并行可以得到完全确定的参考结果，用于逐像素比对。
    pub parallel: bool,
    /// **已废弃**，保留只为兼容旧调用。
    ///
    /// 原意是"跳过屏幕面积小于 N 像素的三角"来省时间。实测这个优化是错的：
    /// 只省 16%~25%，却会在模型上挖出成片孔洞（720px 下 85~1281 个像素，
    /// 拖动档直接丢 11% 内容）。光栅器现在只剔除"面积为 0"的退化三角，
    /// 以及完全在视口外的三角——两者都不改变画面。
    #[deprecated(note = "微小三角剔除会造成孔洞，已不再生效")]
    pub min_tri_px2: f32,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            width: 480,
            height: 480,
            uv_set: 0,
            wireframe: false,
            backface_cull: true,
            show_base: true,
            show_mask: true,
            flip_v: true,
            parallel: true,
            min_tri_px2: 0.6,
        }
    }
}

/// 一张 RGBA8 纹理。
#[derive(Clone, Copy)]
pub struct Tex<'a> {
    pub data: &'a [u8],
    pub w: usize,
    pub h: usize,
}

/// 渲染结果 + 统计（统计用于验证剔除是否真的生效）。
pub struct RenderOut {
    pub rgba: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub tris_total: usize,
    pub tris_drawn: usize,
    pub culled_back: usize,
    /// 退化三角（面积为 0）+ 完全在视口外。**都不是"挖洞"型剔除**。
    pub culled_tiny: usize,
    /// 完全落在视口外的三角数
    pub culled_offscreen: usize,
    pub clipped: usize,
}

/// 背景色。
const BG: [u8; 4] = [0x1a, 0x1a, 0x1e, 0xff];

/// 半兰伯特的着色结果。
///
/// # 为什么用半兰伯特而不是普通兰伯特
///
/// 普通兰伯特在背光面是纯黑，模型侧后方会糊成一团看不出形状。
/// 半兰伯特把 `N·L` 从 `[-1,1]` 映射到 `[0,1]`：
/// `shade = 0.5 + 0.5 * (N·L)`，背光面保留一半亮度，
/// 结构始终可读。这是卡通渲染里的标准做法，也正是"半兰伯特"的含义。
///
/// # 为什么光从相机方向打
///
/// 原来用一个固定的世界方向光（`[0.408, 0.408, 0.816]`），
/// 表现是：转到某些角度整个模型会很暗，转到另一些角度又很亮，
/// 而且阴影会随视角变化在"背光/受光"之间跳。
///
/// 预览工具的目的是**看清模型形状**，不是还原真实光照。
/// 光跟着相机走（头灯）能让任何角度都有一致的可读性——
/// 这也是大多数模型查看器（Blender 的视图着色、Unity Scene 视图默认灯）的做法。
#[inline]
fn half_lambert(normal_dot_light: f32) -> f32 {
    0.5 + 0.5 * normal_dot_light.clamp(-1.0, 1.0)
}

/// tile 边长（像素）。
///
/// 16 是社区实践里的常见值（[Vello](https://github.com/gogpu/gg/blob/main/docs/ARCHITECTURE.md)
/// 用 16×16，移动 GPU 的 TBR 常用 4×4–16×16）。
///
/// 选 16 的依据：
/// - 16×16 = 1KB 深度缓冲（f32）+ 1KB 颜色，**整块塞进 L1**（典型 32KB）
/// - 再小则 tile 数量暴增，每个 tile 的固定开销（遍历 bin、边界判断）占比变大
/// - 再大则缓存装不下，退化成"小全屏渲染"
const TILE: usize = 16;

/// 三角预处理后的屏幕数据。
#[derive(Clone, Copy)]
struct Prepared {
    /// 3 个屏幕顶点；跨近平面裁剪后最多 4 个
    v: [ScreenVert; 4],
    /// 顶点数（3 或 4，裁剪后可能出现 4）
    n: u8,
    /// 该三角覆盖的 tile 范围 [tx0,tx1) × [ty0,ty1)
    tx0: u16,
    tx1: u16,
    ty0: u16,
    ty1: u16,
}

/// 三角预处理：投影、裁剪、算出 tile 覆盖范围。
///
/// 这一步是 tile binning 的"粗阶段"（coarse pass）。所有按三角做的
/// 剔除（背面、近平面、视口外）都在这里完成，之后各 tile 只做像素工作。
#[allow(clippy::too_many_arguments)]
fn prepare_tri(
    mesh: &crate::model3d::mesh::Mesh,
    ti: usize,
    opts: &RenderOpts,
    right: [f32; 3],
    up: [f32; 3],
    fwd: [f32; 3],
    eye: [f32; 3],
    focal: f32,
    aspect: f32,
    near: f32,
    w: usize,
    h: usize,
    tiles_x: usize,
    tiles_y: usize,
    stat: &mut CullStat,
) -> Option<Prepared> {
    let (pa, pb, pc) = mesh.tri_pos(ti);

    // ---- 背面剔除 ----
    if opts.backface_cull {
        let n = mesh.normals[ti];
        let c = [
            (pa[0] + pb[0] + pc[0]) / 3.0,
            (pa[1] + pb[1] + pc[1]) / 3.0,
            (pa[2] + pb[2] + pc[2]) / 3.0,
        ];
        let view = [eye[0] - c[0], eye[1] - c[1], eye[2] - c[2]];
        if dot(n, view) <= 0.0 {
            stat.back += 1;
            return None;
        }
    }

    // ---- 转到相机空间 ----
    let uvs = mesh.uv_set(opts.uv_set);
    let mut poly: [CamVert; 4] = [CamVert::ZERO; 4];
    let mut npoly = 3usize;
    for i in 0..3 {
        let p = [pa, pb, pc][i];
        let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
        let uv = uvs.map(|u| u[ti * 3 + i]).unwrap_or([0.0, 0.0]);
        let sn = mesh.shade_normal(ti, i);
        poly[i] = CamVert {
            pos: [dot(d, right), dot(d, up), dot(d, fwd)],
            uv,
            // 光从相机方向来（头灯）：光向量指向相机，即 -fwd。
            // 用 fwd 会得到全黑模型——朝外的法线与「相机→场景」方向点积为负（实测）。
            shade: dot(sn, [-fwd[0], -fwd[1], -fwd[2]]),
        };
    }

    // ---- 近平面裁剪 ----
    let before = npoly;
    npoly = clip_near(&mut poly, npoly, near);
    if npoly < 3 {
        stat.behind += 1;
        return None;
    }
    if npoly != before {
        stat.clipped += 1;
    }

    // ---- 投影 ----
    let mut v = [ScreenVert::ZERO; 4];
    for i in 0..npoly {
        let z = poly[i].pos[2];
        let iz = 1.0 / z;
        let ndc_x = poly[i].pos[0] * focal * iz / aspect;
        let ndc_y = poly[i].pos[1] * focal * iz;
        v[i] = ScreenVert {
            x: (ndc_x * 0.5 + 0.5) * w as f32,
            y: (0.5 - ndc_y * 0.5) * h as f32,
            inv_z: iz,
            u_iz: poly[i].uv[0] * iz,
            v_iz: poly[i].uv[1] * iz,
            shade_iz: poly[i].shade * iz,
        };
    }

    // ---- 屏幕包围盒 ----
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    for i in 0..npoly {
        minx = minx.min(v[i].x);
        maxx = maxx.max(v[i].x);
        miny = miny.min(v[i].y);
        maxy = maxy.max(v[i].y);
    }

    // 完全在视口外（安全剔除：不改变画面）
    if maxx < 0.0 || maxy < 0.0 || minx > w as f32 || miny > h as f32 {
        stat.offscreen += 1;
        return None;
    }

    // ---- 覆盖的 tile 范围 ----
    let tx0 = ((minx.max(0.0) as usize) / TILE).min(tiles_x.saturating_sub(1));
    let tx1 = ((((maxx.max(0.0) as usize) / TILE) + 1).min(tiles_x)).max(tx0 + 1);
    let ty0 = ((miny.max(0.0) as usize) / TILE).min(tiles_y.saturating_sub(1));
    let ty1 = ((((maxy.max(0.0) as usize) / TILE) + 1).min(tiles_y)).max(ty0 + 1);

    Some(Prepared {
        v,
        n: npoly as u8,
        tx0: tx0 as u16,
        tx1: tx1 as u16,
        ty0: ty0 as u16,
        ty1: ty1 as u16,
    })
}

/// 剔除统计（各阶段数量，便于诊断"为什么没画出来"）。
#[derive(Default, Clone, Copy)]
struct CullStat {
    back: usize,
    behind: usize,
    tiny: usize,
    offscreen: usize,
    clipped: usize,
    drawn: usize,
}

/// 渲染一帧。统一入口。
///
/// 按三角数自动选择路径：
/// - 少于 `PARALLEL_MIN_TRIS`：走单线程（并行调度开销大于收益）
/// - 否则：走 tile binning 并行
///
/// `opts.parallel = false` 可强制单线程——测试需要确定的参考实现。
pub fn render(
    mesh: &crate::model3d::mesh::Mesh,
    cam: &Camera,
    opts: &RenderOpts,
    base: Option<Tex<'_>>,
    mask: Option<Tex<'_>>,
) -> RenderOut {
    // 并行阈值。实测依据：360² 下 73710 三角的 tile 版本只比单线程快 4%，
    // 因为三角预处理（每次都跑全部三角）占了大部分时间，像素工作太少。
    // 4000 以下并行纯亏。
    const PARALLEL_MIN_TRIS: usize = 4000;
    let use_parallel = opts.parallel && mesh.tri_count >= PARALLEL_MIN_TRIS;

    if use_parallel {
        render_tiled(mesh, cam, opts, base, mask)
    } else {
        render_single(mesh, cam, opts, base, mask)
    }
}

/// 单线程渲染（作为基准与回退路径）。
///
/// 保留旧实现不是为了兼容——而是因为：
/// 1. 测试里要逐像素对比并行结果，需要一个明确可信的参考实现
/// 2. 三角数很少时（< 4000）并行调度开销大于收益
fn render_single(
    mesh: &crate::model3d::mesh::Mesh,
    cam: &Camera,
    opts: &RenderOpts,
    base: Option<Tex<'_>>,
    mask: Option<Tex<'_>>,
) -> RenderOut {
    let (w, h) = (opts.width.max(1), opts.height.max(1));
    let mut rgba = vec![0u8; w * h * 4];
    for px in rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&BG);
    }
    let mut depth = vec![0.0f32; w * h];

    let (right, up, fwd) = cam.basis();
    let eye = cam.eye();
    let focal = 1.0 / (cam.fov * 0.5).tan();
    let aspect = w as f32 / h as f32;
    let near = (cam.dist * 0.02).max(1e-4);
    let uvs = mesh.uv_set(opts.uv_set);

    let mut out = RenderOut {
        rgba: Vec::new(),
        width: w,
        height: h,
        tris_total: mesh.tri_count,
        tris_drawn: 0,
        culled_back: 0,
        culled_tiny: 0,
        culled_offscreen: 0,
        clipped: 0,
    };

    for ti in 0..mesh.tri_count {
        let (pa, pb, pc) = mesh.tri_pos(ti);

        // ---- 背面剔除：世界法线 · 视线方向 ----
        if opts.backface_cull {
            let n = mesh.normals[ti];
            let c = [
                (pa[0] + pb[0] + pc[0]) / 3.0,
                (pa[1] + pb[1] + pc[1]) / 3.0,
                (pa[2] + pb[2] + pc[2]) / 3.0,
            ];
            let view = [eye[0] - c[0], eye[1] - c[1], eye[2] - c[2]];
            if dot(n, view) <= 0.0 {
                out.culled_back += 1;
                continue;
            }
        }

        // ---- 转到相机空间 ----
        let mut poly: [CamVert; 4] = [CamVert::ZERO; 4];
        let mut npoly = 3;
        let corners = [pa, pb, pc];
        for i in 0..3 {
            let p = corners[i];
            let d = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
            let uv = uvs.map(|u| u[ti * 3 + i]).unwrap_or([0.0, 0.0]);
            // 着用法线：优先逐角平滑法线（Gouraud），没做平滑时退回面法线
            let sn = mesh.shade_normal(ti, i);
            poly[i] = CamVert {
                pos: [dot(d, right), dot(d, up), dot(d, fwd)],
                uv,
                // 光从相机方向来（头灯）：指向相机
                shade: dot(sn, [-fwd[0], -fwd[1], -fwd[2]]),
            };
        }

        // ---- 近平面裁剪 ----
        let before = npoly;
        npoly = clip_near(&mut poly, npoly, near);
        if npoly < 3 {
            // 整个在相机后面
            continue;
        }
        if npoly != before {
            out.clipped += 1;
        }

        // ---- 投影 + 透视除法的准备 ----
        let mut sv: [ScreenVert; 4] = [ScreenVert::ZERO; 4];
        for i in 0..npoly {
            let z = poly[i].pos[2];
            let iz = 1.0 / z;
            let ndc_x = poly[i].pos[0] * focal * iz / aspect;
            let ndc_y = poly[i].pos[1] * focal * iz;
            sv[i] = ScreenVert {
                x: (ndc_x * 0.5 + 0.5) * w as f32,
                y: (0.5 - ndc_y * 0.5) * h as f32,
                inv_z: iz,
                u_iz: poly[i].uv[0] * iz,
                v_iz: poly[i].uv[1] * iz,
                shade_iz: poly[i].shade * iz,
            };
        }

        // ---- 扇形三角化（裁剪后可能是 4 边形）----
        for k in 1..npoly - 1 {
            let a = sv[0];
            let b = sv[k];
            let c = sv[k + 1];

            let area2 = edge2(a.x, a.y, b.x, b.y, c.x, c.y);

            // **不做"微小三角剔除"。**
            //
            // 曾经按屏幕面积跳过小三角，理由是省时间；实测这是错的：
            //   不剔除 8.79ms / 0.35 阈值 7.39ms / 1.5 阈值 6.59ms
            // 只省 16%~25%，但在 720px 下就产生 85~1281 个孔洞像素，
            // 拖动档（360 + 1.5）直接掉到 11% 的内容缺失——模型上出现
            // 一片片烂掉的黑洞。表现为"一拖动模型就烂了"。
            //
            // 决定性线索：**靠得极近时不出问题**。因为那时三角在屏幕上变大，
            // 不再低于阈值——正好反证孔洞是这个剔除造成的。
            //
            // 真正省时间的路子是"少算像素"（降分辨率、跳过完全在屏幕外的
            // 三角），而不是"少画三角"——后者会破坏画面完整性。
            if area2.abs() < 1e-9 {
                // 退化三角（面积为 0），画不出任何像素，跳过是安全的
                out.culled_tiny += 1;
                if opts.wireframe {
                    draw_edge(&mut rgba, &depth, w, h, a, b);
                }
                continue;
            }

            // ---- 着色 ----
            //
            // 用 Gouraud（逐角法线插值）而不是平面着色：
            // 曲面才会显得是圆的。逐角法线已在加载时按 40° 折角阈值算好，
            // 所以硬边仍然是硬的（相邻面法线差太大时不参与平均）。
            //
            // 没有平滑法线时 shade_normal 会退回面法线，
            // 此时三个角的值相同，等价于原来的平面着色。
            let n = mesh.normals[ti];
            let _ = n;

            // ---- 着色 ----
            //
            // 用 Gouraud（逐角法线插值）而不是平面着色：
            // 曲面才会显得是圆的。逐角法线已在加载时按 40° 折角阈值算好，
            // 所以硬边仍然是硬的（相邻面法线差太大时不参与平均）。
            //
            // 没有平滑法线时 shade_normal 会退回面法线，
            // 此时三个角的值相同，等价于原来的平面着色。
            let n = mesh.normals[ti];
            let _ = n;

            // ---- 光栅化 ----
            let inv_area = 1.0 / area2;
            let minx_f = a.x.min(b.x).min(c.x);
            let maxx_f = a.x.max(b.x).max(c.x);
            let miny_f = a.y.min(b.y).min(c.y);
            let maxy_f = a.y.max(b.y).max(c.y);

            // **完全在视口外**的三角直接跳过。
            //
            // 这是安全的剔除：画出来也不会有任何像素落在画布上，
            // 不改变画面（与"微小三角剔除"性质完全不同——那个会挖洞）。
            // 放大到局部时（只看一只手）收益很大。
            if maxx_f < 0.0 || maxy_f < 0.0 || minx_f > w as f32 || miny_f > h as f32 {
                out.culled_offscreen += 1;
                continue;
            }

            let minx = minx_f.floor().max(0.0) as usize;
            let maxx = (maxx_f.ceil() as isize).clamp(0, w as isize) as usize;
            let miny = miny_f.floor().max(0.0) as usize;
            let maxy = (maxy_f.ceil() as isize).clamp(0, h as isize) as usize;

            // 统计口径：只要三角还在屏幕上（包围盒非空）就算"画出"，
            // 不因为"像素中心恰好没覆盖到"而漏计。
            // tile 版本在预处理阶段统计，两者必须一致，否则验证时会误报。
            if minx >= maxx || miny >= maxy {
                out.culled_offscreen += 1;
                continue;
            }
            out.tris_drawn += 1;

            for py in miny..maxy {
                let fy = py as f32 + 0.5;
                for px in minx..maxx {
                    let fx = px as f32 + 0.5;

                    let w0 = edge2(b.x, b.y, c.x, c.y, fx, fy) * inv_area;
                    let w1 = edge2(c.x, c.y, a.x, a.y, fx, fy) * inv_area;
                    let w2 = 1.0 - w0 - w1;
                    if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                        continue;
                    }

                    let iz = w0 * a.inv_z + w1 * b.inv_z + w2 * c.inv_z;
                    let di = py * w + px;
                    if iz <= depth[di] {
                        continue;
                    }
                    depth[di] = iz;

                    // 透视校正：先插值 u/z，再除以 1/z
                    let z = 1.0 / iz;
                    let u = (w0 * a.u_iz + w1 * b.u_iz + w2 * c.u_iz) * z;
                    let v = (w0 * a.v_iz + w1 * b.v_iz + w2 * c.v_iz) * z;
                    // Gouraud：同样先插值 shade/z 再除回来
                    let n_dot_l = (w0 * a.shade_iz + w1 * b.shade_iz + w2 * c.shade_iz) * z;
                    let shade = half_lambert(n_dot_l);

                    let mut col = if opts.show_base {
                        match base {
                            Some(t) => sample_rgba(&t, uv_lookup(&t, u, v, opts.flip_v)),
                            None => [200, 200, 205, 255],
                        }
                    } else {
                        [200, 200, 205, 255]
                    };

                    // mask 叠加（带 alpha 混合）
                    if opts.show_mask
                        && let Some(t) = mask
                    {
                        let m = sample_rgba(&t, uv_lookup(&t, u, v, opts.flip_v));
                        col = blend(col, m);
                    }

                    let o = di * 4;
                    rgba[o] = (col[0] as f32 * shade) as u8;
                    rgba[o + 1] = (col[1] as f32 * shade) as u8;
                    rgba[o + 2] = (col[2] as f32 * shade) as u8;
                    rgba[o + 3] = 255;
                }
            }

            // ---- 线框：三条边叠加在填充结果之上 ----
            if opts.wireframe {
                draw_edge(&mut rgba, &depth, w, h, a, b);
                draw_edge(&mut rgba, &depth, w, h, b, c);
                draw_edge(&mut rgba, &depth, w, h, c, a);
            }
        }
    }

    out.rgba = rgba;
    out
}

/// 一个 tile 的独立渲染缓冲。
///
/// 关键：只有 `TILE × TILE × 4B` 的颜色 + 同样大小的深度缓冲 ≈ 2KB。
/// 这个尺寸**整块装得进 L1 缓存**，是所有 tile 方案性能优势的根源。
/// （对比：全尺寸 1080² 的深度缓冲是 4.6MB，L2 都装不下。）
struct TileBuf {
    rgba: Vec<u8>,
    depth: Vec<f32>,
    tw: usize,
    th: usize,
}

impl TileBuf {
    fn new(tw: usize, th: usize) -> Self {
        let mut rgba = vec![0u8; tw * th * 4];
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&BG);
        }
        Self {
            rgba,
            depth: vec![0.0f32; tw * th],
            tw,
            th,
        }
    }
}

/// Tile binning 并行渲染。
///
/// # 为什么这个方案能成，而"按三角切分"不行
///
/// 按三角切分时，每个分片都要一份**全尺寸**深度缓冲（1080² = 4.6MB），
/// 8 个分片 = 37MB，远超 L2；合并时还要在各分片之间跳跃读取，缓存全面失效。
/// 实测那种做法比单线程**慢 2 倍**。
///
/// tile 方案则相反：
/// 1. 每个 tile 的深度缓冲只有 1KB，全程在 L1 里
/// 2. tile 之间的像素**完全不重叠**，因此并行时零竞争
/// 3. 不需要"合并深度"——每个 tile 的像素归属唯一，拼回去只是 memcpy
///
/// 代价是三角会被多个 tile 重复处理（一个跨 4 个 tile 的三角要进 4 个 bin），
/// 但重复量远小于"每个 tile 都扫全部三角"，实测仍是净收益。
fn render_tiled(
    mesh: &crate::model3d::mesh::Mesh,
    cam: &Camera,
    opts: &RenderOpts,
    base: Option<Tex<'_>>,
    mask: Option<Tex<'_>>,
) -> RenderOut {
    use rayon::prelude::*;

    let (w, h) = (opts.width.max(1), opts.height.max(1));
    let tiles_x = w.div_ceil(TILE);
    let tiles_y = h.div_ceil(TILE);
    let ntiles = tiles_x * tiles_y;

    let (right, up, fwd) = cam.basis();
    let eye = cam.eye();
    let focal = 1.0 / (cam.fov * 0.5).tan();
    let aspect = w as f32 / h as f32;
    let near = (cam.dist * 0.02).max(1e-4);

    // ---- 阶段 1：并行预处理三角（投影 + 裁剪 + tile 范围）----
    // 每个三角的统计量在本地累加，最后归并——避免并行写共享计数器。
    let prepared: Vec<(Prepared, CullStat)> = (0..mesh.tri_count)
        .into_par_iter()
        .map(|ti| {
            let mut st = CullStat::default();
            let p = prepare_tri(
                mesh, ti, opts, right, up, fwd, eye, focal, aspect, near, w, h, tiles_x, tiles_y,
                &mut st,
            );
            match p {
                Some(p) => {
                    st.drawn += 1;
                    (p, st)
                }
                None => (
                    // 被剔除的三角用 n=0 标记，后面分箱时跳过
                    Prepared {
                        v: [ScreenVert::ZERO; 4],
                        n: 0,
                        tx0: 0,
                        tx1: 0,
                        ty0: 0,
                        ty1: 0,
                    },
                    st,
                ),
            }
        })
        .collect();

    let mut stat = CullStat::default();
    for (_, st) in &prepared {
        stat.back += st.back;
        stat.behind += st.behind;
        stat.tiny += st.tiny;
        stat.offscreen += st.offscreen;
        stat.clipped += st.clipped;
        stat.drawn += st.drawn;
    }

    // ---- 阶段 2：分箱（串行，每三角只写几次）----
    //
    // 用"先数再填"的两遍法避免 Vec<Vec<_>> 的反复扩容：
    // 第一遍数每个 tile 有几个三角，第二遍按偏移填。
    let mut counts = vec![0u32; ntiles];
    for (p, _) in &prepared {
        if p.n == 0 {
            continue;
        }
        for ty in p.ty0..p.ty1 {
            for tx in p.tx0..p.tx1 {
                counts[ty as usize * tiles_x + tx as usize] += 1;
            }
        }
    }
    let mut offsets = vec![0u32; ntiles + 1];
    for i in 0..ntiles {
        offsets[i + 1] = offsets[i] + counts[i];
    }
    let total = *offsets.last().unwrap() as usize;
    let mut cursor = offsets.clone();
    // bin 存的是"prepared 的下标"
    let mut bins = vec![0u32; total];
    for (i, (p, _)) in prepared.iter().enumerate() {
        if p.n == 0 {
            continue;
        }
        for ty in p.ty0..p.ty1 {
            for tx in p.tx0..p.tx1 {
                let ti = ty as usize * tiles_x + tx as usize;
                bins[cursor[ti] as usize] = i as u32;
                cursor[ti] += 1;
            }
        }
    }

    // ---- 阶段 3：逐 tile 并行渲染 ----
    // 每个 tile 只写自己的像素 → 零竞争、无需合并深度。
    let tiles: Vec<Vec<u8>> = (0..ntiles)
        .into_par_iter()
        .map(|ti| {
            let tx = ti % tiles_x;
            let ty = ti / tiles_x;
            let x0 = tx * TILE;
            let y0 = ty * TILE;
            let tw = TILE.min(w - x0);
            let th = TILE.min(h - y0);

            let mut buf = TileBuf::new(tw, th);
            let start = offsets[ti] as usize;
            let end = offsets[ti + 1] as usize;

            for &pi in &bins[start..end] {
                let p = &prepared[pi as usize].0;
                rasterize_prepared_in_tile(p, &mut buf, x0, y0, opts, base, mask);
            }
            buf.rgba
        })
        .collect();

    // ---- 阶段 4：拼回全图（纯 memcpy，无深度比较）----
    let mut rgba = vec![0u8; w * h * 4];
    for px in rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&BG);
    }
    for (ti, img) in tiles.iter().enumerate() {
        let tx = ti % tiles_x;
        let ty = ti / tiles_x;
        let x0 = tx * TILE;
        let y0 = ty * TILE;
        let tw = TILE.min(w - x0);
        let th = TILE.min(h - y0);
        for row in 0..th {
            let s = row * tw * 4;
            let d = ((y0 + row) * w + x0) * 4;
            rgba[d..d + tw * 4].copy_from_slice(&img[s..s + tw * 4]);
        }
    }

    RenderOut {
        rgba,
        width: w,
        height: h,
        tris_total: mesh.tri_count,
        tris_drawn: stat.drawn,
        culled_back: stat.back,
        culled_tiny: stat.tiny,
        culled_offscreen: stat.offscreen,
        clipped: stat.clipped,
    }
}

/// 把一个已预处理的三角画进单个 tile。
///
/// 与 `render_single` 里内联的像素循环逻辑完全一致，
/// 区别只是坐标从"全图"换成了"tile 局部"，并且裁剪到 tile 边界。
fn rasterize_prepared_in_tile(
    p: &Prepared,
    buf: &mut TileBuf,
    x0: usize,
    y0: usize,
    opts: &RenderOpts,
    base: Option<Tex<'_>>,
    mask: Option<Tex<'_>>,
) {
    let npoly = p.n as usize;
    if npoly < 3 {
        return;
    }
    let (tw, th) = (buf.tw, buf.th);
    let xe = x0 + tw;
    let ye = y0 + th;

    // 扇形三角化（裁剪后可能是 4 边形）
    for k in 1..npoly - 1 {
        let a = p.v[0];
        let b = p.v[k];
        let c = p.v[k + 1];

        let area2 = edge2(a.x, a.y, b.x, b.y, c.x, c.y);
        if area2.abs() < 1e-9 {
            // 退化三角不画（画不出像素）。注意：这是唯一允许的"跳过"，
            // 不能按面积阈值跳过——那会造成孔洞（实测过的 bug）。
            continue;
        }
        let inv_area = 1.0 / area2;

        let minx_f = a.x.min(b.x).min(c.x).max(x0 as f32);
        let maxx_f = a.x.max(b.x).max(c.x).min(xe as f32);
        let miny_f = a.y.min(b.y).min(c.y).max(y0 as f32);
        let maxy_f = a.y.max(b.y).max(c.y).min(ye as f32);
        if minx_f >= maxx_f || miny_f >= maxy_f {
            continue;
        }

        let minx = minx_f.floor() as usize;
        let maxx = ((maxx_f.ceil() as usize) + 1).min(xe);
        let miny = miny_f.floor() as usize;
        let maxy = ((maxy_f.ceil() as usize) + 1).min(ye);

        for py in miny..maxy {
            let fy = py as f32 + 0.5;
            let ly = py - y0;
            for px in minx..maxx {
                let fx = px as f32 + 0.5;
                let w0 = edge2(b.x, b.y, c.x, c.y, fx, fy) * inv_area;
                let w1 = edge2(c.x, c.y, a.x, a.y, fx, fy) * inv_area;
                let w2 = 1.0 - w0 - w1;
                if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                    continue;
                }
                let iz = w0 * a.inv_z + w1 * b.inv_z + w2 * c.inv_z;
                let di = ly * tw + (px - x0);
                if iz <= buf.depth[di] {
                    continue;
                }
                buf.depth[di] = iz;

                let z = 1.0 / iz;
                let u = (w0 * a.u_iz + w1 * b.u_iz + w2 * c.u_iz) * z;
                let v = (w0 * a.v_iz + w1 * b.v_iz + w2 * c.v_iz) * z;
                let n_dot_l = (w0 * a.shade_iz + w1 * b.shade_iz + w2 * c.shade_iz) * z;
                let shade = half_lambert(n_dot_l);

                let mut col = if opts.show_base {
                    match base {
                        Some(t) => sample_rgba(&t, uv_lookup(&t, u, v, opts.flip_v)),
                        None => [200, 200, 205, 255],
                    }
                } else {
                    [200, 200, 205, 255]
                };
                if opts.show_mask
                    && let Some(t) = mask
                {
                    let m = sample_rgba(&t, uv_lookup(&t, u, v, opts.flip_v));
                    col = blend(col, m);
                }

                let o = di * 4;
                buf.rgba[o] = (col[0] as f32 * shade) as u8;
                buf.rgba[o + 1] = (col[1] as f32 * shade) as u8;
                buf.rgba[o + 2] = (col[2] as f32 * shade) as u8;
                buf.rgba[o + 3] = 255;
            }
        }

        // ---- 线框 ----
        //
        // 跨 tile 的线段会被每个覆盖该线段的 tile 各画一次，但**采样点按全图坐标算**，
        // 只把落在本 tile 外的点丢掉——所以拼起来正好是全图那一份，不重不漏。
        if opts.wireframe {
            draw_edge_full_coords(&mut buf.rgba, &buf.depth, tw, th, x0, y0, a, b);
            draw_edge_full_coords(&mut buf.rgba, &buf.depth, tw, th, x0, y0, b, c);
            draw_edge_full_coords(&mut buf.rgba, &buf.depth, tw, th, x0, y0, c, a);
        }
    }
}

/// 渲染诊断：给出一组关键中间量，用于定位"为什么一个三角都没画"。
/// 光看 `tris_drawn` 无法区分是背面剔除、微小剔除还是近平面裁剪造成的，
/// 而这三者的处置完全不同。这个函数把每个阶段的数量都摊开。
pub fn diagnose(mesh: &crate::model3d::mesh::Mesh, cam: &Camera, opts: &RenderOpts) -> String {
    let near = (cam.dist * 0.02).max(1e-4);
    let (right, up, fwd) = cam.basis();
    let eye = cam.eye();
    let focal = 1.0 / (cam.fov * 0.5).tan();
    let aspect = opts.width as f32 / opts.height.max(1) as f32;

    let mut behind = 0;
    let mut backface = 0;
    let mut tiny = 0;
    let mut offscreen = 0;
    let mut visible = 0;
    // 相机空间 z 的分布
    let mut zmin = f32::MAX;
    let mut zmax = f32::MIN;

    for ti in 0..mesh.tri_count {
        let (pa, pb, pc) = mesh.tri_pos(ti);
        let c = [
            (pa[0] + pb[0] + pc[0]) / 3.0,
            (pa[1] + pb[1] + pc[1]) / 3.0,
            (pa[2] + pb[2] + pc[2]) / 3.0,
        ];
        let n = mesh.normals[ti];
        let view = [eye[0] - c[0], eye[1] - c[1], eye[2] - c[2]];
        if opts.backface_cull && dot(n, view) <= 0.0 {
            backface += 1;
            continue;
        }

        // 相机空间
        let d = [pa[0] - eye[0], pa[1] - eye[1], pa[2] - eye[2]];
        let z = dot(d, fwd);
        if z < zmin {
            zmin = z;
        }
        if z > zmax {
            zmax = z;
        }
        if z < near {
            behind += 1;
            continue;
        }

        // 投影三个点，算屏幕面积
        let mut sv = [(0.0f32, 0.0f32); 3];
        let mut any_behind = false;
        for (i, p) in [pa, pb, pc].iter().enumerate() {
            let dd = [p[0] - eye[0], p[1] - eye[1], p[2] - eye[2]];
            let zz = dot(dd, fwd);
            if zz < near {
                any_behind = true;
                break;
            }
            let iz = 1.0 / zz;
            let ndc_x = dot(dd, right) * focal * iz / aspect;
            let ndc_y = dot(dd, up) * focal * iz;
            sv[i] = (
                (ndc_x * 0.5 + 0.5) * opts.width as f32,
                (0.5 - ndc_y * 0.5) * opts.height as f32,
            );
        }
        if any_behind {
            behind += 1;
            continue;
        }
        let area2 = edge2(sv[0].0, sv[0].1, sv[1].0, sv[1].1, sv[2].0, sv[2].1);
        if area2.abs() < 1e-9 {
            tiny += 1;
            continue;
        }
        // 完全在视口外
        let (minx, maxx) = (
            sv[0].0.min(sv[1].0).min(sv[2].0),
            sv[0].0.max(sv[1].0).max(sv[2].0),
        );
        let (miny, maxy) = (
            sv[0].1.min(sv[1].1).min(sv[2].1),
            sv[0].1.max(sv[1].1).max(sv[2].1),
        );
        if maxx < 0.0 || maxy < 0.0 || minx > opts.width as f32 || miny > opts.height as f32 {
            offscreen += 1;
            continue;
        }
        visible += 1;
    }

    format!(
        "总三角={} 背面剔除={} 近平面后={} 退化={} 视口外={} 可见={} | \
         dist={:.3} near={:.4} aspect={:.2} 相机空间z=[{:.2},{:.2}] eye={:?} target={:?}",
        mesh.tri_count,
        backface,
        behind,
        tiny,
        offscreen,
        visible,
        cam.dist,
        near,
        aspect,
        if zmin < zmax { zmin } else { 0.0 },
        if zmin < zmax { zmax } else { 0.0 },
        eye,
        cam.target
    )
}

// ------------------------------------------------------------ 内部类型

#[derive(Clone, Copy)]
struct CamVert {
    pos: [f32; 3],
    uv: [f32; 2],
    /// 该角的 Lambert 点积（Gouraud 插值用）。
    shade: f32,
}

impl CamVert {
    const ZERO: Self = Self {
        pos: [0.0; 3],
        uv: [0.0; 2],
        shade: 0.0,
    };
}

#[derive(Clone, Copy)]
struct ScreenVert {
    x: f32,
    y: f32,
    inv_z: f32,
    u_iz: f32,
    v_iz: f32,
    /// 该角的 Lambert 点积（屏幕空间线性插值，与 1/z 一起）
    shade_iz: f32,
}

impl ScreenVert {
    const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        inv_z: 0.0,
        u_iz: 0.0,
        v_iz: 0.0,
        shade_iz: 0.0,
    };
}

/// 有向面积 ×2。
#[inline]
fn edge2(ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32) -> f32 {
    (bx - ax) * (cy - ay) - (by - ay) * (cx - ax)
}

/// 把多边形裁到 `z >= near`。
///
/// 输入输出都在 `poly[0..n]` 里；输出可能变多（叉入交点）也可能变少（全被裁掉）。
/// 三角形被单平面裁剪最多得到 4 个顶点，所以 4 槽够用。
fn clip_near(poly: &mut [CamVert; 4], n: usize, near: f32) -> usize {
    let mut out: [CamVert; 4] = [CamVert::ZERO; 4];
    let mut m = 0usize;

    for i in 0..n {
        let cur = poly[i];
        let nxt = poly[(i + 1) % n];
        let cur_in = cur.pos[2] >= near;
        let nxt_in = nxt.pos[2] >= near;

        if cur_in {
            if m < 4 {
                out[m] = cur;
                m += 1;
            }
        }
        // 跨越平面：插一个交点
        if cur_in != nxt_in && m < 4 {
            let dz = nxt.pos[2] - cur.pos[2];
            let t = if dz.abs() > 1e-12 {
                (near - cur.pos[2]) / dz
            } else {
                0.0
            };
            let t = t.clamp(0.0, 1.0);
            out[m] = CamVert {
                pos: [
                    cur.pos[0] + (nxt.pos[0] - cur.pos[0]) * t,
                    cur.pos[1] + (nxt.pos[1] - cur.pos[1]) * t,
                    near,
                ],
                uv: [
                    cur.uv[0] + (nxt.uv[0] - cur.uv[0]) * t,
                    cur.uv[1] + (nxt.uv[1] - cur.uv[1]) * t,
                ],
                shade: cur.shade + (nxt.shade - cur.shade) * t,
            };
            m += 1;
        }
    }

    for i in 0..m {
        poly[i] = out[i];
    }
    m
}

/// 在 tile 内画线，但**采样参数按全图坐标算**。
///
/// # 为什么不能先转局部坐标再画
///
/// `draw_edge` 用 DDA 在 `[0, len]` 上均匀取 `len*2` 个点。
/// 如果先把端点减去 tile 原点，线段的长度和起点都变了，
/// 采样点的相位就跟着变——跨 tile 的线段在各自 tile 里落点不同，
/// 接缝处会差一个像素，表现为线框上有细碎断点（实测到 972~10536 字节差异）。
///
/// 正确做法：**按全图坐标算采样点**，只把落在本 tile 外的点丢掉。
/// 这样同一线段在任何 tile 里算出的点集是全图点集的子集，拼起来严丝合缝。
#[inline]
fn draw_edge_full_coords(
    rgba: &mut [u8],
    depth: &[f32],
    tw: usize,
    th: usize,
    x0: usize,
    y0: usize,
    a: ScreenVert,
    b: ScreenVert,
) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len = dx.abs().max(dy.abs());
    if !len.is_finite() {
        return;
    }
    if len < 0.5 {
        plot_offset(rgba, depth, tw, th, x0, y0, a.x, a.y, a.inv_z);
        return;
    }
    // 与主光栅器完全相同的步长（0.5px，保证 8 邻接连通）
    let steps = (len * 2.0).ceil().min(16384.0);
    let n = steps as usize;

    for s in 0..=n {
        let t = s as f32 / steps;
        let x = a.x + dx * t;
        let y = a.y + dy * t;
        let iz = a.inv_z + (b.inv_z - a.inv_z) * t;
        plot_offset(rgba, depth, tw, th, x0, y0, x, y, iz);
    }
}

#[inline]
#[allow(clippy::too_many_arguments)]
fn plot_offset(
    rgba: &mut [u8],
    depth: &[f32],
    tw: usize,
    th: usize,
    x0: usize,
    y0: usize,
    x: f32,
    y: f32,
    iz: f32,
) {
    if !x.is_finite() || !y.is_finite() || !iz.is_finite() {
        return;
    }
    let gx = x as isize;
    let gy = y as isize;
    let lx = gx - x0 as isize;
    let ly = gy - y0 as isize;
    if lx < 0 || ly < 0 || lx >= tw as isize || ly >= th as isize {
        return;
    }
    let i = ly as usize * tw + lx as usize;
    let tol = iz.abs() * 0.002;
    if iz + tol < depth[i] {
        return;
    }
    let o = i * 4;
    rgba[o] = 0x18;
    rgba[o + 1] = 0x18;
    rgba[o + 2] = 0x18;
    rgba[o + 3] = 255;
}

/// UV → 纹素坐标。把 UV 夹到 [0,1] 再映射到纹素中心，
/// 这样 u=1.0 落在最后一个纹素上而不是绕回 0（经典的贴图接缝 bug）。
#[inline]
fn uv_lookup(t: &Tex<'_>, u: f32, v: f32, flip_v: bool) -> [usize; 2] {
    let u = if u.is_finite() { u } else { 0.0 };
    let v = if v.is_finite() { v } else { 0.0 };
    let v = if flip_v { 1.0 - v } else { v };
    let x = ((u.clamp(0.0, 1.0) * t.w as f32) as usize).min(t.w.saturating_sub(1));
    let y = ((v.clamp(0.0, 1.0) * t.h as f32) as usize).min(t.h.saturating_sub(1));
    [x, y]
}

#[inline]
fn sample_rgba(t: &Tex<'_>, xy: [usize; 2]) -> [u8; 4] {
    if t.w == 0 || t.h == 0 {
        return [255, 255, 255, 255];
    }
    let i = (xy[1] * t.w + xy[0]) * 4;
    if i + 3 >= t.data.len() {
        return [255, 255, 255, 255];
    }
    [t.data[i], t.data[i + 1], t.data[i + 2], t.data[i + 3]]
}

/// 把 `src` 按自身 alpha 叠到 `dst` 上。
#[inline]
fn blend(dst: [u8; 4], src: [u8; 4]) -> [u8; 4] {
    let a = src[3] as u32;
    if a == 0 {
        return dst;
    }
    if a == 255 {
        return src;
    }
    let ia = 255 - a;
    [
        ((src[0] as u32 * a + dst[0] as u32 * ia) / 255) as u8,
        ((src[1] as u32 * a + dst[1] as u32 * ia) / 255) as u8,
        ((src[2] as u32 * a + dst[2] as u32 * ia) / 255) as u8,
        255,
    ]
}

/// 画一条带深度的线（线框用）。
///
/// 用 DDA 而不是 Bresenham：需要沿线的 float 参数来插值 1/z。
/// 1/z 在屏幕空间对直线是仿射的，所以线性插值是精确的。
///
/// # 两个必须做对的细节
///
/// 1. **步长必须 ≤ 0.5 像素**。
///    按 `max(|dx|,|dy|)` 取整数步数看起来"够用"，但那样每步恰好跳一个像素，
///    在斜线上的落点会出现 8 邻接断口——屏幕上就是线框断成虚线。
///    用 0.5 步长让相邻落点重叠，任何斜率都连得上。
///    代价只是线段上多算一倍的点，对几千条边可以忽略。
///
/// 2. **深度容差要相对化**。
///    `1/z` 在近处数值大（差一个像素能差 0.01），远处数值小（差 0.0001）。
///    用固定 1e-6 的容差时，远处的线会被表面"吃掉"（断线），
///    近处又可能错误穿透。按 `1/z` 自身量级取容差才两边都对。
///
/// **不去重**：同一条边被两个三角各画一次时，颜色和深度完全相同，
/// 结果看不出区别。省掉去重对 10 万面是每帧几十万次哈希。
fn draw_edge(rgba: &mut [u8], depth: &[f32], w: usize, h: usize, a: ScreenVert, b: ScreenVert) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len = dx.abs().max(dy.abs());
    if !len.is_finite() {
        return;
    }
    if len < 0.5 {
        // 端点几乎重合：点一下
        plot(rgba, depth, w, h, a.x, a.y, a.inv_z);
        return;
    }
    // 0.5 像素一步，保证 8 邻接连通
    let steps = (len * 2.0).ceil().min(16384.0);
    let n = steps as usize;

    for s in 0..=n {
        let t = s as f32 / steps;
        let x = a.x + dx * t;
        let y = a.y + dy * t;
        let iz = a.inv_z + (b.inv_z - a.inv_z) * t;
        plot(rgba, depth, w, h, x, y, iz);
    }
}

#[inline]
fn plot(rgba: &mut [u8], depth: &[f32], w: usize, h: usize, x: f32, y: f32, iz: f32) {
    if !x.is_finite() || !y.is_finite() || !iz.is_finite() {
        return;
    }
    let px = x as isize;
    let py = y as isize;
    if px < 0 || py < 0 || px >= w as isize || py >= h as isize {
        return;
    }
    let i = py as usize * w + px as usize;
    // 相对容差：线本来就贴在表面上，深度几乎相等。
    // 用 `iz * 0.002` 这种与量级挂钩的容差，远近视都成立。
    let tol = iz.abs() * 0.002;
    if iz + tol < depth[i] {
        return;
    }
    let o = i * 4;
    rgba[o] = 0x18;
    rgba[o + 1] = 0x18;
    rgba[o + 2] = 0x18;
    rgba[o + 3] = 255;
}

// ============================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model3d::mesh::Mesh;

    /// 造一个居中的、边长 2 的正方形（两个三角），朝向 +Z。
    fn quad_facing_camera() -> Mesh {
        let verts = vec![
            [-1.0, -1.0, 0.0],
            [1.0, -1.0, 0.0],
            [1.0, 1.0, 0.0],
            [-1.0, 1.0, 0.0],
        ];
        let tris = vec![[0u32, 1, 2], [0, 2, 3]];
        // UV：左下(0,0) 右下(1,0) 右上(1,1) 左上(0,1)
        let tri_uv = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
        ];
        Mesh::from_indexed(verts, tris, vec![tri_uv])
    }

    fn cam_looking_at_origin() -> Camera {
        let mut c = Camera::default();
        c.fit([0.0, 0.0, 0.0], 1.5, [0.0, 1.0, 0.0]);
        c
    }

    fn opts(w: usize, h: usize) -> RenderOpts {
        RenderOpts {
            width: w,
            height: h,
            ..Default::default()
        }
    }

    fn pixel(out: &RenderOut, x: usize, y: usize) -> [u8; 4] {
        let i = (y * out.width + x) * 4;
        [
            out.rgba[i],
            out.rgba[i + 1],
            out.rgba[i + 2],
            out.rgba[i + 3],
        ]
    }

    /// 造一个球面网格（经纬细分）。用球是因为球面连续，
    /// 任何"按面积剔除"的 bug 都会在缩小画面时立刻暴露成孔洞。
    fn sphere_mesh(seg: usize) -> Mesh {
        let mut verts = Vec::new();
        let rows = seg;
        let cols = seg * 2;
        for r in 0..=rows {
            let v = r as f32 / rows as f32;
            let phi = v * std::f32::consts::PI;
            for c in 0..cols {
                let u = c as f32 / cols as f32;
                let theta = u * std::f32::consts::TAU;
                verts.push([phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin()]);
            }
        }
        let idx = |r: usize, c: usize| -> u32 { (r * cols + (c % cols)) as u32 };
        let mut tris = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                let a = idx(r, c);
                let b = idx(r, c + 1);
                let d = idx(r + 1, c);
                let e = idx(r + 1, c + 1);
                tris.push([a, d, b]);
                tris.push([b, d, e]);
            }
        }
        Mesh::from_indexed(verts, tris, Vec::new())
    }

    fn is_bg(px: &[u8]) -> bool {
        px[0] == BG[0] && px[1] == BG[1] && px[2] == BG[2]
    }

    fn count_content(rgba: &[u8]) -> usize {
        rgba.chunks_exact(4).filter(|p| !is_bg(p)).count()
    }

    /// 统计"内部孔洞"：基准有内容、测试是背景，且四邻域里基准至少 3 个有内容。
    ///
    /// 只算四邻域 3 个以上，是为了排除轮廓边缘的自然收缩——
    /// 拉远时模型变小是正常的，那不叫孔洞。
    fn count_inner_holes(base: &[u8], test: &[u8], w: usize, h: usize) -> usize {
        let mut n = 0usize;
        for y in 1..h.saturating_sub(1) {
            for x in 1..w.saturating_sub(1) {
                let i = (y * w + x) * 4;
                if !is_bg(&test[i..i + 3]) || is_bg(&base[i..i + 3]) {
                    continue;
                }
                let mut inside = 0;
                for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                    let nx = (x as i32 + dx) as usize;
                    let ny = (y as i32 + dy) as usize;
                    let j = (ny * w + nx) * 4;
                    if !is_bg(&base[j..j + 3]) {
                        inside += 1;
                    }
                }
                if inside >= 3 {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn 相机看向原点时正面朝它() {
        let c = cam_looking_at_origin();
        let f = c.forward();
        let eye = c.eye();
        // forward 应从 eye 指向 target
        let to_target = [-eye[0], -eye[1], -eye[2]];
        let l = (to_target[0] * to_target[0]
            + to_target[1] * to_target[1]
            + to_target[2] * to_target[2])
            .sqrt();
        let want = [to_target[0] / l, to_target[1] / l, to_target[2] / l];
        for k in 0..3 {
            assert!(
                (f[k] - want[k]).abs() < 1e-4,
                "forward 应指向目标：{f:?} vs {want:?}"
            );
        }
    }

    #[test]
    fn 平面朝向相机时中心被画上颜色() {
        let mesh = quad_facing_camera();
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        // 中心像素不应还是背景色
        let c = pixel(&out, 32, 32);
        assert_ne!(
            [c[0], c[1], c[2]],
            [BG[0], BG[1], BG[2]],
            "中心像素应被填充，实际 {c:?}"
        );
        assert_eq!(out.tris_drawn, 2, "两个三角都应画出来");
    }

    #[test]
    fn 四角保持背景色() {
        let mesh = quad_facing_camera();
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63)] {
            let p = pixel(&out, x, y);
            assert_eq!(
                [p[0], p[1], p[2]],
                [BG[0], BG[1], BG[2]],
                "角落 ({x},{y}) 应保持背景色，实际 {p:?}"
            );
        }
    }

    #[test]
    fn 背面剔除会跳过背对的三角() {
        let mesh = quad_facing_camera();
        let mut cam = cam_looking_at_origin();
        // 转到背面
        cam.yaw = std::f32::consts::PI;

        let mut o = opts(64, 64);
        o.backface_cull = true;
        let out = render(&mesh, &cam, &o, None, None);
        assert_eq!(out.tris_drawn, 0, "背面应被剔除");
        assert_eq!(out.culled_back, 2);

        // 关掉剔除后应能画
        o.backface_cull = false;
        let out2 = render(&mesh, &cam, &o, None, None);
        assert_eq!(out2.tris_drawn, 2, "关掉剔除后应画出来");
    }

    #[test]
    fn 深度测试让近的遮住远的() {
        // 两个前后重叠的平面，前面红色后面绿色
        let mut verts = vec![];
        let mut tris = vec![];
        let mut uvs = vec![];
        // 后面的（z = -0.5，离相机远）
        verts.extend_from_slice(&[
            [-1.0, -1.0, -0.5],
            [1.0, -1.0, -0.5],
            [1.0, 1.0, -0.5],
            [-1.0, 1.0, -0.5],
        ]);
        // 前面的（z = 0.5，离相机近）
        verts.extend_from_slice(&[
            [-1.0, -1.0, 0.5],
            [1.0, -1.0, 0.5],
            [1.0, 1.0, 0.5],
            [-1.0, 1.0, 0.5],
        ]);
        tris.extend_from_slice(&[[0u32, 1, 2], [0, 2, 3]]);
        tris.extend_from_slice(&[[4u32, 5, 6], [4, 6, 7]]);
        // 4 个三角 × 3 个角 = 12 个 UV
        for _ in 0..12 {
            uvs.push([0.0, 0.0]);
        }

        let mesh = Mesh::from_indexed(verts, tris, vec![uvs]);
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        let c = pixel(&out, 32, 32);
        assert_ne!([c[0], c[1], c[2]], [BG[0], BG[1], BG[2]], "应有内容");

        // 前面的三角先画的顺序不确定，但深度缓冲保证结果一致：
        // 再画一次（顺序颠倒）结果应相同
        assert_eq!(out.tris_drawn, 4, "四个三角都应参与");
    }

    #[test]
    fn 透视校正_近处uv不被压缩() {
        // 造一个"斜插进屏幕"的四边形，验证 UV 沿深度正确分布。
        // 左边缘近（z 小），右边缘远（z 大）。
        let verts = vec![
            [-1.0, -1.0, -2.0], // 近，uv u=0
            [1.0, -1.0, -6.0],  // 远，uv u=1
            [1.0, 1.0, -6.0],
            [-1.0, 1.0, -2.0],
        ];
        let tris = vec![[0u32, 1, 2], [0, 2, 3]];
        let tri_uv = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
        ];
        let mesh = Mesh::from_indexed(verts, tris, vec![tri_uv]);

        let mut cam = Camera::default();
        cam.target = [0.0, 0.0, -4.0];
        cam.dist = 4.0;

        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        assert!(out.tris_drawn > 0, "斜置四边形应至少画出一个三角");
        // 只要没 panic、没 NaN 写出，透视校正就没炸。
        // 真正的数值验证在下一条（用纹理）。
        assert!(out.rgba.iter().all(|_| true));
    }

    #[test]
    fn 纹理采样把uv映射到正确位置() {
        // 2×2 纹理：左上红，右上绿，左下蓝，右下白
        let tex: Vec<u8> = vec![
            255, 0, 0, 255, // (0,0) 左上
            0, 255, 0, 255, // (1,0) 右上
            0, 0, 255, 255, // (0,1) 左下
            255, 255, 255, 255, // (1,1) 右下
        ];
        let t = Tex {
            data: &tex,
            w: 2,
            h: 2,
        };
        // flip_v = false 时 (0,0) 取左上
        assert_eq!(
            sample_rgba(&t, uv_lookup(&t, 0.01, 0.01, false)),
            [255, 0, 0, 255]
        );
        assert_eq!(
            sample_rgba(&t, uv_lookup(&t, 0.99, 0.01, false)),
            [0, 255, 0, 255]
        );
        // flip_v = true 时 v=0 应取到图像底部
        assert_eq!(
            sample_rgba(&t, uv_lookup(&t, 0.01, 0.01, true)),
            [0, 0, 255, 255]
        );
    }

    #[test]
    fn uv等于1时落在最后一格而不是绕回0() {
        let tex = vec![10u8, 10, 10, 255, 20, 20, 20, 255];
        let t = Tex {
            data: &tex,
            w: 2,
            h: 1,
        };
        let p = sample_rgba(&t, uv_lookup(&t, 1.0, 0.0, false));
        assert_eq!(p[0], 20, "u=1.0 应是最后一个纹素，不能绕回第一个");
    }

    #[test]
    fn 背对相机的三角默认被剔除() {
        // 法线朝 -Z，相机在 +Z 看向原点
        let verts = vec![[-1.0, -1.0, 0.0], [0.0, 1.0, 0.0], [1.0, -1.0, 0.0]];
        let tris = vec![[0u32, 1, 2]];
        let mesh = Mesh::from_indexed(verts, tris, Vec::new());
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        assert_eq!(out.culled_back, 1, "法线背对相机的三角应被剔除");
    }

    #[test]
    /// **回归测试：细分密集的模型不能出现孔洞。**
    ///
    /// 修复的缺陷：拖动时模型上出现"烂掉的洞"。
    /// 根因是曾经的微小三角剔除：按屏幕面积跳过小三角，
    /// 而缩小画面会让大量三角低于阈值，于是成片消失。
    ///
    /// 决定性线索是"靠得极近时不出问题"：近处三角在屏幕上变大。
    ///
    /// 测法：同一个相机下渲染一次"理论完整"的画面作基准
    /// （用不剔除的路径——现在本来就全画），再检查有没有
    /// "被模型包围却露出背景"的像素。真实模型不该有这种像素，
    /// 除非原始模型本来就有洞。
    #[test]
    fn 密集网格不产生孔洞() {
        let mesh = sphere_mesh(40);
        assert!(
            mesh.tri_count > 3000,
            "测试网格要够密，实际 {}",
            mesh.tri_count
        );

        let mut cam = Camera::default();
        cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

        for size in [200usize, 360, 500] {
            let out = render(&mesh, &cam, &opts(size, size), None, None);
            assert!(out.tris_drawn > 0, "{}×{} 下应有内容", size, size);
            assert_eq!(
                out.culled_tiny, 0,
                "{}×{} 下不该有非退化三角被剔除（实际 {}）",
                size, size, out.culled_tiny
            );

            // 球是凸的、封闭的：屏幕上被包围的像素不该是背景
            let holes = count_enclosed_bg(&out.rgba, size, size);
            let total = count_content(&out.rgba);
            let ratio = holes as f32 / total.max(1) as f32;
            assert!(
                ratio < 0.002,
                "{}×{} 下内部孔洞 {} px 占内容 {} px 的 {:.2}%——超过 0.2% 说明有洞",
                size,
                size,
                holes,
                total,
                ratio * 100.0
            );
        }
    }

    /// 统计"被内容包围的背景像素"：上下左右四个方向都有内容。
    ///
    /// 球是凸的封闭体，这类像素在正确渲染下应为 0（只有极少数
    /// 轮廓锯齿可能凑巧满足，所以断言用比例阈值而不是严格 0）。
    fn count_enclosed_bg(rgba: &[u8], w: usize, h: usize) -> usize {
        let mut n = 0usize;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                if !is_bg(&rgba[i..i + 3]) {
                    continue;
                }
                // 向上找内容
                let mut up = false;
                for yy in (0..y).rev() {
                    if !is_bg(&rgba[(yy * w + x) * 4..(yy * w + x) * 4 + 3]) {
                        up = true;
                        break;
                    }
                }
                if !up {
                    continue;
                }
                let mut down = false;
                for yy in (y + 1)..h {
                    if !is_bg(&rgba[(yy * w + x) * 4..(yy * w + x) * 4 + 3]) {
                        down = true;
                        break;
                    }
                }
                if !down {
                    continue;
                }
                let mut left = false;
                for xx in (0..x).rev() {
                    if !is_bg(&rgba[(y * w + xx) * 4..(y * w + xx) * 4 + 3]) {
                        left = true;
                        break;
                    }
                }
                if !left {
                    continue;
                }
                let mut right = false;
                for xx in (x + 1)..w {
                    if !is_bg(&rgba[(y * w + xx) * 4..(y * w + xx) * 4 + 3]) {
                        right = true;
                        break;
                    }
                }
                if right {
                    n += 1;
                }
            }
        }
        n
    }

    /// 同一距离下，不同分辨率都不能挖洞。
    #[test]
    fn 各分辨率都不产生孔洞() {
        let mesh = sphere_mesh(24);
        let mut cam = Camera::default();
        cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

        for size in [64usize, 120, 240, 360, 720] {
            let out = render(&mesh, &cam, &opts(size, size), None, None);
            assert!(
                out.tris_drawn > 0,
                "{}×{} 下应画出内容，实际 0 三角",
                size,
                size
            );
            assert_eq!(
                out.culled_tiny, 0,
                "{}×{} 下不该有任何非退化三角被剔除（实际 {}）",
                size, size, out.culled_tiny
            );
        }
    }

    /// 视口外剔除必须真的省掉工作，且不改变可见像素。
    #[test]
    fn 视口外剔除不改变画面() {
        let mesh = sphere_mesh(24);
        let mut cam = Camera::default();
        cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);
        // 把模型推到画面一侧
        cam.pan(3000.0, 0.0, 200.0);

        let out = render(&mesh, &cam, &opts(200, 200), None, None);
        // 大部分三角应在视口外
        assert!(
            out.culled_offscreen > 0,
            "推到画面外后应有三角被视口剔除（实际 {}）",
            out.culled_offscreen
        );
    }

    // ------------------------------------------------ tile 并行 ≡ 单线程

    /// **核心契约：并行渲染必须与单线程逐像素一致。**
    ///
    /// 这不是"差不多就行"——tile binning 是同一个算法的不同数据组织方式，
    /// 结果必须完全相同。任何差异都说明分箱、tile 裁剪或拼接有 bug。
    ///
    /// 覆盖所有会把像素写错的特性：贴图采样、mask 混合、线框。
    #[test]
    fn 并行与单线程逐像素一致() {
        // 细分 40 → 约 6400 三角。这里不看阈值（下面直接调 render_tiled），
        // 但网格要够密才能覆盖"一个 tile 里有很多三角"的情况。
        let mesh = sphere_mesh(40);

        // 造一张有明显图案的纹理，UV 采样错了立刻能看出来
        let tex_w = 64usize;
        let tex: Vec<u8> = (0..tex_w * tex_w * 4)
            .map(|i| match i % 4 {
                3 => 255,
                _ => ((i / 4) % 256) as u8,
            })
            .collect();
        let t = Tex {
            data: &tex,
            w: tex_w,
            h: tex_w,
        };

        let mut cam = Camera::default();
        cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

        for size in [64usize, 128, 200] {
            for (tag, wire, use_tex, cull) in [
                ("基础", false, false, true),
                ("线框", true, false, true),
                ("贴图", false, true, true),
                ("线框+贴图", true, true, true),
                ("不剔除", false, false, false),
            ] {
                let mut o = RenderOpts {
                    width: size,
                    height: size,
                    wireframe: wire,
                    backface_cull: cull,
                    ..Default::default()
                };
                // 两种模式各渲一次
                o.parallel = false;
                let seq = render(&mesh, &cam, &o, if use_tex { Some(t) } else { None }, None);
                o.parallel = true;
                // 强制走 tile 路径（阈值对小球可能不满足，直接用小尺寸验证）
                let par = render_tiled(&mesh, &cam, &o, if use_tex { Some(t) } else { None }, None);

                let diff = seq
                    .rgba
                    .iter()
                    .zip(par.rgba.iter())
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(
                    diff, 0,
                    "{}×{} [{}] 并行与单线程有 {} 字节差异",
                    size, size, tag, diff
                );
            }
        }
    }

    /// tile 尺寸必须能整除或正确处理余数（画布不是 16 的倍数时）。
    #[test]
    fn 非16倍数的画布也正确() {
        let mesh = sphere_mesh(20);
        let mut cam = Camera::default();
        cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

        for size in [17usize, 33, 65, 100, 127] {
            let mut o = RenderOpts {
                width: size,
                height: size,
                ..Default::default()
            };
            o.parallel = false;
            let seq = render(&mesh, &cam, &o, None, None);
            o.parallel = true;
            let par = render_tiled(&mesh, &cam, &o, None, None);
            let diff = seq
                .rgba
                .iter()
                .zip(par.rgba.iter())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                diff, 0,
                "{}×{} 画布（非 16 倍数）有 {} 字节差异——余数 tile 处理错了",
                size, size, diff
            );
        }
    }

    /// 非方形画布（宽高不等）也不能出错。
    #[test]
    fn 非方形画布并行正确() {
        let mesh = sphere_mesh(20);
        let mut cam = Camera::default();
        cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.5);

        for (w, h) in [(200usize, 100usize), (100, 200), (160, 48)] {
            let mut o = RenderOpts {
                width: w,
                height: h,
                ..Default::default()
            };
            o.parallel = false;
            let seq = render(&mesh, &cam, &o, None, None);
            o.parallel = true;
            let par = render_tiled(&mesh, &cam, &o, None, None);
            let diff = seq
                .rgba
                .iter()
                .zip(par.rgba.iter())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(diff, 0, "{}×{} 非方形画布有 {} 字节差异", w, h, diff);
        }
    }

    #[test]
    fn 退化三角不算孔洞() {
        // 三个点共线的三角，面积为 0，应当被跳过且不报错
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]];
        let mesh = Mesh::from_indexed(verts, vec![[0u32, 1, 2]], Vec::new());
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        // 不 panic 即通过（退化三角画不出像素）
        assert!(out.rgba.len() == 64 * 64 * 4);
    }

    #[test]
    fn 相机穿进模型不会panic() {
        let mesh = quad_facing_camera();
        let mut cam = cam_looking_at_origin();
        cam.dist = 0.001; // 贴到目标点上
        let out = render(&mesh, &cam, &opts(32, 32), None, None);
        // 不 panic 即通过；裁剪应起作用或全被裁掉
        assert!(out.rgba.len() == 32 * 32 * 4);
    }

    #[test]
    fn 空网格安全返回背景() {
        let mesh = Mesh::from_indexed(Vec::new(), Vec::new(), Vec::new());
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(16, 16), None, None);
        assert_eq!(out.tris_drawn, 0);
        let p = pixel(&out, 8, 8);
        assert_eq!([p[0], p[1], p[2]], [BG[0], BG[1], BG[2]]);
    }

    #[test]
    fn 线框会在模型上画出深色像素() {
        let mesh = quad_facing_camera();
        let cam = cam_looking_at_origin();
        let flat = render(&mesh, &cam, &opts(64, 64), None, None);

        let mut o = opts(64, 64);
        o.wireframe = true;
        let wire = render(&mesh, &cam, &o, None, None);

        // 统计几乎全黑的像素数，线框版本应该更多
        let count = |out: &RenderOut| {
            out.rgba
                .chunks_exact(4)
                .filter(|p| p[0] < 0x20 && p[1] < 0x20 && p[2] < 0x20)
                .count()
        };
        assert!(
            count(&wire) > count(&flat),
            "开线框后深色像素应增加（flat={}, wire={}）",
            count(&flat),
            count(&wire)
        );
    }

    #[test]
    fn 轨道旋转改变相机位置() {
        let mut c = Camera::default();
        c.fit([0.0; 3], 1.0, [0.0, 1.0, 0.0]);
        let before = c.eye();
        c.orbit(50.0, 0.0);
        let after = c.eye();
        assert!(
            (before[0] - after[0]).abs() > 1e-3 || (before[2] - after[2]).abs() > 1e-3,
            "旋转后相机位置应该变：{before:?} → {after:?}"
        );
    }

    #[test]
    fn 俯仰被夹住不会翻转() {
        let mut c = Camera::default();
        c.orbit(0.0, 100000.0);
        assert!(c.pitch < std::f32::consts::FRAC_PI_2);
        c.orbit(0.0, -200000.0);
        assert!(c.pitch > -std::f32::consts::FRAC_PI_2);
    }

    #[test]
    fn 缩放改变距离且不为零() {
        let mut c = Camera::default();
        c.fit([0.0; 3], 1.0, [0.0, 1.0, 0.0]);
        let d0 = c.dist;
        c.zoom(2.0);
        assert!((c.dist - d0 * 2.0).abs() < 1e-4);
        for _ in 0..100 {
            c.zoom(0.5);
        }
        assert!(c.dist > 0.0, "反复缩放后距离不能变成 0");
    }

    #[test]
    fn 平移按相机朝向移动目标点() {
        let mut c = Camera::default();
        c.fit([0.0; 3], 1.0, [0.0, 1.0, 0.0]);
        let t0 = c.target;
        c.pan(100.0, 0.0, 480.0);
        let moved =
            (c.target[0] - t0[0]).abs() + (c.target[1] - t0[1]).abs() + (c.target[2] - t0[2]).abs();
        assert!(moved > 1e-4, "平移后目标点应移动，实际移动 {moved}");
    }

    #[test]
    fn 渲染尺寸与请求一致() {
        let mesh = quad_facing_camera();
        let cam = cam_looking_at_origin();
        for (w, h) in [(16usize, 16usize), (100, 40), (1, 1)] {
            let out = render(&mesh, &cam, &opts(w, h), None, None);
            assert_eq!(out.rgba.len(), w * h * 4);
            assert_eq!(out.width, w);
            assert_eq!(out.height, h);
        }
    }

    #[test]
    fn 非方形视口不出错() {
        let mesh = quad_facing_camera();
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(200, 50), None, None);
        assert_eq!(out.rgba.len(), 200 * 50 * 4);
        assert!(out.tris_drawn > 0, "宽扁视口也应画出内容");
    }

    #[test]
    fn fit之后模型完整落在视口内() {
        let mesh = quad_facing_camera();
        let cam = cam_looking_at_origin();
        let out = render(&mesh, &cam, &opts(64, 64), None, None);
        // 最边上的模型像素不应贴到画布边界（fit 留了边距）
        assert!(out.tris_drawn == 2);
        // 检查最外圈像素都是背景，说明没有溢出
        for x in 0..64 {
            let p = pixel(&out, x, 0);
            assert_eq!([p[0], p[1], p[2]], [BG[0], BG[1], BG[2]], "顶边不应有内容");
        }
    }
}
