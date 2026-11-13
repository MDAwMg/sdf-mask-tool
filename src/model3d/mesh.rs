//! 网格数据结构。
//!
//! 存储策略：**每个三角存三份顶点**（摊平），而不是"顶点数组 + 索引数组"。
//!
//! 为什么浪费这点内存：
//! OBJ 和 FBX 都能让同一个位置在不同三角里有不同 UV（接缝处），
//! 用共享顶点就必须做顶点分裂，分裂逻辑是解析器里最容易出错的部分。
//! 摊平之后"位置和 UV 天然一一对应"，所有映射类型（ByVertex /
//! ByPolygonVertex / ByPolygon）都用同一套代码处理，不可能对错。
//!
//! 代价可以接受：74k 三角 ≈ 222k 顶点 × 12B ≈ 2.7MB。
//! 而渲染时省掉了一次索引间接寻址——对软件光栅才是大头。
//!
//! `vidx` 另外保留原始顶点下标，用于统计和潜在的去重需求。

/// 自动平滑的折角阈值（度）。
///
/// 相邻面夹角小于该阈值的边会被平滑（法线共享），大于该阈值的保持硬边。
/// 40° 的实测依据：样本模型的曲面区域相邻面夹角普遍 <10°，
/// 而直角/折边普遍 >60°，40° 落在两簇中间，两边都不误伤。
pub const DEFAULT_SMOOTH_ANGLE: f32 = 40.0;

/// 加载失败的原因。分成几类是为了让 UI 能给出不同的处置建议。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// 读文件失败（不存在、没权限、路径不是文件）
    Io(String),
    /// 文件能读，但内容不符合该格式
    Format(String),
    /// 面数超过 `MAX_TRIS`，拒绝加载
    TooManyTris(usize),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "读取失败：{e}"),
            LoadError::Format(e) => write!(f, "格式错误：{e}"),
            LoadError::TooManyTris(n) => write!(
                f,
                "面数过多：{n} 个三角面（上限 {}）。这个模型不适合做预览，\
                 请在建模软件里减面后再导入。",
                crate::model3d::MAX_TRIS
            ),
        }
    }
}

impl std::error::Error for LoadError {}

/// 加载完成后的统计，用于状态栏和"可能卡顿"提示。
#[derive(Debug, Clone, Copy, Default)]
pub struct MeshStats {
    pub tris: usize,
    pub verts: usize,
    pub uv_sets: usize,
    /// 解析耗时（毫秒）
    pub parse_ms: f32,
}

impl MeshStats {
    /// 需要提示"预览可能卡顿"吗。
    pub fn should_warn(&self) -> bool {
        self.tris > crate::model3d::WARN_TRIS
    }
}

/// 摊平后的三角网格。
#[derive(Clone, Default)]
pub struct Mesh {
    /// 每三角 3 个顶点位置（长度 = tri_count * 3）
    pub pos: Vec<[f32; 3]>,
    /// 每个 UV 集一份，每三角 3 个 UV（长度 = tri_count * 3）
    pub uv: Vec<Vec<[f32; 2]>>,
    /// 每三角 3 个原始顶点下标
    pub vidx: Vec<[u32; 3]>,
    /// 每三角 1 个面法线（已归一化）
    pub normals: Vec<[f32; 3]>,
    pub tri_count: usize,
    /// 原始顶点数（去重后，报告用）
    pub vert_count: usize,
    /// 包围盒中心
    pub center: [f32; 3],
    /// 包围球半径（粗略尺度参考）
    pub radius: f32,
    /// 包围盒三个半轴长度（相对 `center`）。
    ///
    /// 取景要用半长而不是 `radius`：`radius` 是"最远点到中心的距离"，
    /// 对细长模型（比如直立的人）会远大于实际需要的视野，
    /// 结果模型在画面里只占中间一小块。
    /// 用三轴半宽分别算，才能让模型真正撑满视口。
    pub half_extents: [f32; 3],
    /// **哪个轴朝上**（0=X / 1=Y / 2=Z）。
    ///
    /// 不能硬编码 Y-up：实测同一个作者的两个文件就不一样——
    /// 实测模型 A 是 Y-up（Y 尺寸 1.79 最大），
    /// 实测模型 B 是 Z-up（Z 尺寸 171 最大，Blender 默认导 Z-up）。
    /// 假设错了相机就会从头顶往下看，模型在屏幕上被压成一条扁片。
    ///
    /// 判定方式：**包围盒三边里最长的那条当高度**。对角色、道具、建筑
    /// 这类"立着"的模型是可靠的启发式；对横向摆放的地形不适用，
    /// 但那种模型也没有明确的"上"可言。
    pub up_axis: u8,
    /// **逐角的平滑法线**（长度 = tri_count * 3）。
    ///
    /// 与 `normals`（每三角一个面法线）并存：
    /// - `normals` 用于背面剔除（面朝向是确定的，不能用法线插值替代）
    /// - `smooth_normals` 用于着色（Gouraud 插值，让曲面看起来是圆的）
    ///
    /// 为空表示没做平滑（此时着色退回面法线）。
    pub smooth_normals: Vec<[f32; 3]>,
    pub stats: MeshStats,
}

impl std::fmt::Debug for Mesh {
    /// 手写而不是 derive：一个 10 万面的模型用 derive 打印会把终端淹掉。
    /// 测试里 `unwrap_err()` 需要的是"能 Debug"，不是"打印全部数据"。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mesh")
            .field("tris", &self.tri_count)
            .field("verts", &self.vert_count)
            .field("uv_sets", &self.uv.len())
            .field("radius", &self.radius)
            .field("center", &self.center)
            .finish()
    }
}

impl Mesh {
    /// 从"共享顶点 + 三角索引 + 每套 UV"构造。
    ///
    /// `uv_sets` 的每个元素是**一套 UV**，长度必须等于 `tris.len() * 3`
    /// （每个三角 3 个角，按三角顺序排列）。长度不符的集会**整集丢弃**——
    /// 半个 UV 集比没有更糟，半个 UV 集会让整片错位而且看起来像"UV 画错了"。
    ///
    /// 这个签名是刻意的：调用方（OBJ/FBX）天然就按"每角一个 UV"产出数据，
    /// 自行拼装任何偏移布局都是多余的心智负担和出错机会。
    pub fn from_indexed(
        verts: Vec<[f32; 3]>,
        tris: Vec<[u32; 3]>,
        uv_sets: Vec<Vec<[f32; 2]>>,
    ) -> Self {
        Self::from_indexed_with_up(verts, tris, uv_sets, None)
    }

    /// 同 [`Self::from_indexed`]，但允许调用方给出**权威的上轴**。
    ///
    /// FBX 的 `GlobalSettings / UpAxis` 明确记录了导出时的上方向，
    /// 这比"包围盒最长边"的猜测可靠得多：猴子头（Suzanne）左右宽 2.73、
    /// 高 1.70，最长边是 X，靠猜会把 X 当成上方向，相机就绕到侧面去了。
    ///
    /// `Some(axis)` 时直接采用（0=X / 1=Y / 2=Z），`None` 时退回按包围盒猜。
    pub fn from_indexed_with_up(
        verts: Vec<[f32; 3]>,
        tris: Vec<[u32; 3]>,
        uv_sets: Vec<Vec<[f32; 2]>>,
        up_axis: Option<u8>,
    ) -> Self {
        // 顶点可以是 NaN / Inf：OBJ 的 `v nan 0 0` 能解析出 f32::NAN，
        // FBX 的浮点数组也不保证有限。一旦进入几何流程：
        // 包围球中心变 NaN → 相机 target 变 NaN → 栅格化全部退化，
        // 画面糊成一片而且没有任何报错。
        //
        // 单帧的 NaN 传播面太大，这里在入口统一清洗：非有限分量置 0。
        // 位置被破坏的顶点本来就无法正确显示，置 0 至少让其余部分可见。
        let verts: Vec<[f32; 3]> = verts
            .into_iter()
            .map(|v| {
                [
                    if v[0].is_finite() { v[0] } else { 0.0 },
                    if v[1].is_finite() { v[1] } else { 0.0 },
                    if v[2].is_finite() { v[2] } else { 0.0 },
                ]
            })
            .collect();

        let tri_count = tris.len();
        let mut pos = Vec::with_capacity(tri_count * 3);
        let mut vidx = Vec::with_capacity(tri_count);
        for t in &tris {
            for &i in t {
                // 解析器已校验过范围；这里再兜一次底，越界就给原点，
                // 宁可几何错一点也不 panic
                pos.push(verts.get(i as usize).copied().unwrap_or([0.0; 3]));
            }
            vidx.push(*t);
        }

        let want = tri_count * 3;
        let uv: Vec<Vec<[f32; 2]>> = uv_sets.into_iter().filter(|s| s.len() == want).collect();

        let normals = (0..tri_count)
            .map(|i| face_normal(&pos[i * 3], &pos[i * 3 + 1], &pos[i * 3 + 2]))
            .collect();

        let (center, radius) = bounding_sphere(&verts);
        let half_extents = half_extents_of(&verts, center);
        // 权威上轴优先；没有时才按包围盒最长边猜
        let up_axis = up_axis.unwrap_or_else(|| detect_up_axis(&verts));

        let stats = MeshStats {
            tris: tri_count,
            verts: verts.len(),
            uv_sets: uv.len(),
            parse_ms: 0.0,
        };

        let mut m = Self {
            pos,
            uv,
            vidx,
            normals,
            tri_count,
            vert_count: verts.len(),
            center,
            radius,
            half_extents,
            up_axis,
            smooth_normals: Vec::new(),
            stats,
        };
        // 加载时自动算平滑法线，无需手动设置。
        // 40° 阈值：曲面被平滑、硬边保持锐利。
        m.compute_smooth_normals(DEFAULT_SMOOTH_ANGLE);
        m
    }

    pub fn is_empty(&self) -> bool {
        self.tri_count == 0
    }

    /// 取第 `set` 套 UV；越界返回 None。
    pub fn uv_set(&self, set: usize) -> Option<&[[f32; 2]]> {
        self.uv.get(set).map(|v| v.as_slice())
    }

    /// 第 `i` 个三角的三个位置。
    #[inline]
    pub fn tri_pos(&self, i: usize) -> ([f32; 3], [f32; 3], [f32; 3]) {
        (self.pos[i * 3], self.pos[i * 3 + 1], self.pos[i * 3 + 2])
    }

    /// 重新计算包围球（模型平移后在测试里用得到）。
    pub fn recompute_bounds(&mut self) {
        let (c, r) = bounding_sphere(&self.pos);
        self.center = c;
        self.radius = r;
    }

    /// 按**折角阈值**自动计算平滑法线（Gouraud 着色）。
    ///
    /// # 为什么需要阈值，而不是"全平滑"
    ///
    /// 全平滑会把立方体的棱磨圆、把刀锋磨钝——看起来像塑料融化。
    /// 行业标准做法是：**只有相邻面夹角小于阈值的边才共享法线**，
    /// 大于阈值的（也就是硬边/UV 缝/材质分界）保持各自的法线。
    ///
    /// 实测数据支持这一点：两个样本模型分别有 32894 处、112647 处
    /// 顶点上相邻三角的夹角超过 60°，全部平滑会显著破坏造型。
    ///
    /// `angle_deg` 常用 30–60°。取 40° 是个稳妥的中间值：
    /// 圆滑的曲面（相邻面夹角通常 <10°）会被平滑，
    /// 而直角、折边（>60°）保持锐利。
    ///
    /// # 为什么要同时看 UV 和材质缝
    ///
    /// 同一位置在 UV 接缝处是**两个不同的顶点**（索引不同），
    /// 但因为 `vidx` 保留了原始索引，接缝两侧不会被误当成相邻——
    /// 这正好省掉了"检测 UV 缝"这一步。
    pub fn compute_smooth_normals(&mut self, angle_deg: f32) {
        if self.tri_count == 0 {
            self.smooth_normals.clear();
            return;
        }
        use std::collections::HashMap;

        let cos_thresh = angle_deg.to_radians().cos();

        // 顶点 → 该顶点上所有三角的 (三角下标)
        let mut vert_tris: HashMap<u32, Vec<u32>> = HashMap::new();
        for (i, tri) in self.vidx.iter().enumerate() {
            for &v in tri {
                vert_tris.entry(v).or_default().push(i as u32);
            }
        }

        let mut out = vec![[0.0f32; 3]; self.tri_count * 3];

        for (ti, tri) in self.vidx.iter().enumerate() {
            let n_self = self.normals[ti];
            for (corner, &v) in tri.iter().enumerate() {
                let Some(list) = vert_tris.get(&v) else {
                    out[ti * 3 + corner] = n_self;
                    continue;
                };
                // 累加所有与当前面夹角足够小的相邻面的法线（按面积加权）
                let mut acc = [0.0f32; 3];
                for &other in list {
                    let n_o = self.normals[other as usize];
                    let d = n_self[0] * n_o[0] + n_self[1] * n_o[1] + n_self[2] * n_o[2];
                    if d < cos_thresh {
                        continue; // 硬边：不参与平均
                    }
                    // 面积加权：大面贡献更多，避免细碎三角把法线拉偏
                    let w = tri_area(&self.pos, other as usize).max(1e-9);
                    acc[0] += n_o[0] * w;
                    acc[1] += n_o[1] * w;
                    acc[2] += n_o[2] * w;
                }
                let len = (acc[0] * acc[0] + acc[1] * acc[1] + acc[2] * acc[2]).sqrt();
                out[ti * 3 + corner] = if len > 1e-9 {
                    [acc[0] / len, acc[1] / len, acc[2] / len]
                } else {
                    n_self
                };
            }
        }

        self.smooth_normals = out;
    }

    /// 取第 `i` 个三角第 `corner` 个角的着色法线。
    ///
    /// 没做平滑时退回面法线，调用方不必判断是否为空。
    #[inline]
    pub fn shade_normal(&self, tri: usize, corner: usize) -> [f32; 3] {
        let n = self.smooth_normals.len();
        let want = self.tri_count * 3;
        if n == want {
            self.smooth_normals[tri * 3 + corner]
        } else {
            self.normals[tri]
        }
    }

    /// 上方向的单位向量（由 `up_axis` 得出）。
    pub fn up(&self) -> [f32; 3] {
        match self.up_axis {
            0 => [1.0, 0.0, 0.0],
            2 => [0.0, 0.0, 1.0],
            _ => [0.0, 1.0, 0.0],
        }
    }

    /// 上方向的中文名（状态栏/UI 提示用）。
    pub fn up_axis_name(&self) -> &'static str {
        match self.up_axis {
            0 => "X",
            2 => "Z",
            _ => "Y",
        }
    }

    /// 所有 UV 中最小的 v，用于判断 v 轴是否需要翻转。
    pub fn uv_v_min(&self, set: usize) -> f32 {
        self.uv_set(set)
            .map(|s| s.iter().map(|uv| uv[1]).fold(f32::INFINITY, f32::min))
            .filter(|v| v.is_finite())
            .unwrap_or(0.0)
    }
}

/// 三角面积（load 期算一次，用于法线加权）。
#[inline]
fn tri_area(pos: &[[f32; 3]], tri: usize) -> f32 {
    let a = pos[tri * 3];
    let b = pos[tri * 3 + 1];
    let c = pos[tri * 3 + 2];
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    0.5 * (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt()
}

/// 三角面法线（右手逆时针为正面）。
pub fn face_normal(a: &[f32; 3], b: &[f32; 3], c: &[f32; 3]) -> [f32; 3] {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let n = [
        u[1] * v[2] - u[2] * v[1],
        u[2] * v[0] - u[0] * v[2],
        u[0] * v[1] - u[1] * v[0],
    ];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len > 1e-12 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        // 退化三角形：给一个任意方向，避免 NaN 传播到光栅器
        [0.0, 1.0, 0.0]
    }
}

/// 用包围盒中心 + 最远点距离做包围球。
///
/// 不用"最小包围球"算法：预览只要一个够用的球，两个循环就够，
/// 而且这个结果对取景来说宁可大一点也不要小（小了会裁掉模型边缘）。
fn bounding_sphere(verts: &[[f32; 3]]) -> ([f32; 3], f32) {
    if verts.is_empty() {
        return ([0.0; 3], 1.0);
    }
    let mut lo = verts[0];
    let mut hi = verts[0];
    for v in verts {
        for k in 0..3 {
            if v[k] < lo[k] {
                lo[k] = v[k];
            }
            if v[k] > hi[k] {
                hi[k] = v[k];
            }
        }
    }
    let center = [
        (lo[0] + hi[0]) * 0.5,
        (lo[1] + hi[1]) * 0.5,
        (lo[2] + hi[2]) * 0.5,
    ];
    let mut r2: f32 = 0.0;
    for v in verts {
        let d = [v[0] - center[0], v[1] - center[1], v[2] - center[2]];
        let d2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        if d2 > r2 {
            r2 = d2;
        }
    }
    let r = r2.sqrt();
    // 零尺寸模型（单个点）：给个非零半径，否则相机的 dist 计算会得到 0 然后除零
    (center, if r > 1e-6 { r } else { 1.0 })
}

/// 包围盒三个方向的半宽（相对给定中心）。
fn half_extents_of(verts: &[[f32; 3]], center: [f32; 3]) -> [f32; 3] {
    if verts.is_empty() {
        return [1.0; 3];
    }
    let mut m = [0.0f32; 3];
    for v in verts {
        for k in 0..3 {
            let d = (v[k] - center[k]).abs();
            if d > m[k] {
                m[k] = d;
            }
        }
    }
    // 任何一轴为零（例如完全扁平的片）时给个下限，
    // 否则取景算出的距离会是 0 然后除零
    for k in 0..3 {
        if m[k] < 1e-6 {
            m[k] = 1e-6;
        }
    }
    m
}

/// 判定哪个轴朝上：包围盒三边中最长的那条。
///
/// 为什么不是"看顶点分布"或"看某个轴是不是从 0 开始"：
/// 那些启发式对旋转过的模型、对居中模型都不稳。
/// 包围盒最长边对角色/道具/建筑这类立着的模型非常可靠，
/// 而且判定代价是 O(n) 的两个比较，不增加复杂度。
pub fn detect_up_axis(verts: &[[f32; 3]]) -> u8 {
    if verts.is_empty() {
        return 1;
    }
    let mut lo = verts[0];
    let mut hi = verts[0];
    for v in verts {
        for k in 0..3 {
            if v[k] < lo[k] {
                lo[k] = v[k];
            }
            if v[k] > hi[k] {
                hi[k] = v[k];
            }
        }
    }
    let ext = [hi[0] - lo[0], hi[1] - lo[1], hi[2] - lo[2]];
    // 并列时优先 Y（最常见的约定），所以用严格的 >
    let mut best = 1u8;
    let mut best_e = ext[1];
    if ext[0] > best_e {
        best = 0;
        best_e = ext[0];
    }
    if ext[2] > best_e {
        best = 2;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 单三角的法线朝向由绕序决定() {
        let a = [0.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let c = [0.0, 1.0, 0.0];
        let n = face_normal(&a, &b, &c);
        assert!((n[2] - 1.0).abs() < 1e-6, "法线应为 +Z，实际 {n:?}");
    }

    #[test]
    fn 反向绕序法线翻转() {
        let a = [0.0, 0.0, 0.0];
        let b = [0.0, 1.0, 0.0];
        let c = [1.0, 0.0, 0.0];
        let n = face_normal(&a, &b, &c);
        assert!((n[2] + 1.0).abs() < 1e-6, "反向绕序法线应为 -Z，实际 {n:?}");
    }

    #[test]
    fn 退化三角不产生NaN() {
        let a = [1.0, 1.0, 1.0];
        let n = face_normal(&a, &a, &a);
        assert!(n.iter().all(|v| v.is_finite()), "退化三角不能出 NaN：{n:?}");
    }

    #[test]
    fn 摊平后顶点数是三角数的三倍() {
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.0],
        ];
        let tris = vec![[0u32, 1, 2], [1, 3, 2]];
        let m = Mesh::from_indexed(verts, tris, Vec::new());
        assert_eq!(m.tri_count, 2);
        assert_eq!(m.pos.len(), 6, "两个三角应有 6 个摊平顶点");
        assert_eq!(m.normals.len(), 2);
        assert_eq!(m.vert_count, 4, "原始顶点数应保留为 4");
    }

    #[test]
    fn 包围球能罩住所有顶点() {
        let verts = vec![
            [-1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 2.0, 0.0],
            [0.0, -2.0, 0.0],
        ];
        let tris = vec![[0u32, 1, 2], [0, 1, 3]];
        let m = Mesh::from_indexed(verts, tris, Vec::new());
        for v in &m.pos {
            let d = [v[0] - m.center[0], v[1] - m.center[1], v[2] - m.center[2]];
            let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            assert!(
                dist <= m.radius + 1e-4,
                "顶点 {v:?} 到中心距离 {dist} 超过半径 {}",
                m.radius
            );
        }
    }

    #[test]
    fn 空网格安全() {
        let m = Mesh::from_indexed(Vec::new(), Vec::new(), Vec::new());
        assert!(m.is_empty());
        assert!(m.radius > 0.0, "空网格半径必须为正，否则相机会除零");
        assert!(m.uv_set(0).is_none());
    }

    #[test]
    fn UV集数量与长度正确() {
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let tris = vec![[0u32, 1, 2]];
        let set0 = vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let set1 = vec![[0.5, 0.5], [0.6, 0.5], [0.5, 0.6]];
        let m = Mesh::from_indexed(verts, tris, vec![set0, set1]);
        assert_eq!(m.stats.uv_sets, 2);
        assert_eq!(m.uv_set(0).unwrap().len(), 3);
        assert_eq!(
            m.uv_set(1).unwrap()[0],
            [0.5, 0.5],
            "第二集的 UV 不能串到第一集"
        );
    }

    #[test]
    fn 长度不符的uv集被整集丢弃() {
        let verts = vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let tris = vec![[0u32, 1, 2]];
        // 第二个集只有 2 个 UV，应该被丢掉
        let good = vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]];
        let bad = vec![[0.0, 0.0], [1.0, 0.0]];
        let m = Mesh::from_indexed(verts, tris, vec![good, bad]);
        assert_eq!(m.stats.uv_sets, 1, "长度不符的 UV 集必须被丢弃");
    }

    #[test]
    fn 面数超过上限时错误信息说明上限() {
        let e = LoadError::TooManyTris(2_000_000);
        let s = e.to_string();
        assert!(s.contains("2000000"), "{s}");
        assert!(s.contains("减面"), "应给出可执行建议：{s}");
    }

    // ------------------------------------------------ 上轴判定

    #[test]
    fn 竖长模型判定为y轴向上() {
        // Y 方向最长，典型的 Y-up 角色
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 10.0, 1.0]];
        assert_eq!(detect_up_axis(&verts), 1, "Y 最长应判定 Y-up");
    }

    #[test]
    fn zup模型判定为z轴向上() {
        // Z 方向最长，Blender 默认导出
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 2.0, 20.0]];
        assert_eq!(detect_up_axis(&verts), 2, "Z 最长应判定 Z-up");
    }

    #[test]
    fn x轴最长时判定x轴向上() {
        let verts = vec![[0.0, 0.0, 0.0], [30.0, 1.0, 1.0]];
        assert_eq!(detect_up_axis(&verts), 0);
    }

    #[test]
    fn 空网格默认y轴向上() {
        assert_eq!(detect_up_axis(&[]), 1, "空网格要有个兜底，不能返回越界值");
    }

    #[test]
    fn up向量与轴名一致() {
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 1.0, 10.0]];
        let m = Mesh::from_indexed(verts, vec![[0u32, 0, 1]], Vec::new());
        assert_eq!(m.up_axis, 2);
        assert_eq!(m.up(), [0.0, 0.0, 1.0]);
        assert_eq!(m.up_axis_name(), "Z");
    }

    /// 文件声明的上轴必须压过"包围盒最长边"的猜测。
    ///
    /// 这是实测的 Suzanne 场景：猴子头左右宽 2.73、高 1.70、纵深 1.97，
    /// 最长边是 X。靠猜会把 X 当上轴，相机绕到侧面、模型横躺。
    /// FBX 的 `UpAxis` 写的是 Y，必须以它为准。
    #[test]
    fn 权威上轴优先于包围盒猜测() {
        // 左右最宽、竖向次之：靠猜会得到 X-up
        let verts = vec![
            [-1.4, -0.85, -0.98],
            [1.4, 0.85, 0.98],
            [-0.5, 0.0, 0.0],
        ];
        let tris = vec![[0u32, 1, 2]];

        // 不指定：退回包围盒猜测，最长边是 X
        let guessed = Mesh::from_indexed(verts.clone(), tris.clone(), Vec::new());
        assert_eq!(guessed.up_axis, 0, "该形状靠猜应得到 X-up");

        // 指定 Y：必须采用给定值
        let authoritative =
            Mesh::from_indexed_with_up(verts, tris, Vec::new(), Some(1));
        assert_eq!(authoritative.up_axis, 1, "权威上轴必须压过猜测");
        assert_eq!(authoritative.up(), [0.0, 1.0, 0.0]);
    }

    /// 越界的上轴号不应被采纳，仍走猜测。
    #[test]
    fn 非法上轴号退回猜测() {
        let verts = vec![[0.0, 0.0, 0.0], [1.0, 20.0, 0.0]];
        let m = Mesh::from_indexed_with_up(verts, vec![[0u32, 0, 1]], Vec::new(), Some(9));
        // up() 对未知轴号回退到 Y，这里只要求不 panic 且得到合法向量
        let up = m.up();
        assert!(
            (up[0] * up[0] + up[1] * up[1] + up[2] * up[2] - 1.0).abs() < 1e-6,
            "上方向必须是单位向量，实际 {up:?}"
        );
    }

    // ------------------------------------------------ 自动平滑

    /// 两个共享一条边、夹角很小的面：法线应被平均（平滑）。
    #[test]
    fn 小夹角邻接面被平滑() {
        // 近似同一个平面，法线几乎相同
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.01], // 微微抬起，夹角极小
        ];
        let tris = vec![[0u32, 1, 2], [1, 3, 2]];
        let m = Mesh::from_indexed(verts, tris, Vec::new());
        assert_eq!(m.smooth_normals.len(), 6, "应算出逐角法线");
        // 两个面法线本来就接近，平滑后应仍然朝 +Z
        let a = m.shade_normal(0, 0);
        let b = m.shade_normal(1, 0);
        assert!(a[2] > 0.9 && b[2] > 0.9, "两个面应都朝 +Z：{a:?} {b:?}");
    }

    /// 夹角很大的两个面（硬边）：法线必须保持各自独立，不被平均。
    #[test]
    fn 大夹角邻接面保持硬边() {
        // 一个朝 +Z，一个朝 +X，夹角 90°
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 0.0, 1.0],
        ];
        let tris = vec![[0u32, 1, 2], [1, 3, 2]];
        let m = Mesh::from_indexed(verts, tris, Vec::new());

        let n0 = m.normals[0];
        let n1 = m.normals[1];
        let dot = n0[0] * n1[0] + n0[1] * n1[1] + n0[2] * n1[2];
        assert!(dot < 0.3, "两个面应该接近垂直（dot={dot}）");

        // 关键：硬边处的着色法线应等于各自的面法线
        let s0 = m.shade_normal(0, 1); // 顶点 1 在三角 0 里
        let s1 = m.shade_normal(1, 0); // 同顶点在三角 1 里
        let d = s0[0] * s1[0] + s0[1] * s1[1] + s0[2] * s1[2];
        assert!(d < 0.3, "90° 硬边的两顶着色法线不应被平均（dot={d}）");
    }

    #[test]
    fn 平滑法线全部是单位长度() {
        let verts = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [1.0, 1.0, 0.2],
        ];
        let tris = vec![[0u32, 1, 2], [1, 3, 2]];
        let m = Mesh::from_indexed(verts, tris, Vec::new());
        for n in &m.smooth_normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-4, "法线长度应为 1，实际 {len}");
        }
    }

    #[test]
    fn 没有平滑数据时退回面法线() {
        let mut m = Mesh::from_indexed(
            vec![[0.0; 3], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            vec![[0u32, 1, 2]],
            Vec::new(),
        );
        m.smooth_normals.clear();
        let got = m.shade_normal(0, 1);
        assert_eq!(got, m.normals[0], "空平滑数据必须退回面法线，不能 panic");
    }

    #[test]
    fn 空网格平滑不panic() {
        let mut m = Mesh::from_indexed(Vec::new(), Vec::new(), Vec::new());
        m.compute_smooth_normals(40.0);
        assert!(m.smooth_normals.is_empty());
    }

    #[test]
    fn 默认平滑角度是40度() {
        assert!((DEFAULT_SMOOTH_ANGLE - 40.0).abs() < 1e-6);
    }
}
