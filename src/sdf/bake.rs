//! 关键帧 → 单张 SDF 贴图的烘焙。
//!
//! ## 编码模型
//!
//! 一张 SDF 贴图，每个像素只存**一个字节**：某个"翻转"发生在角度轴的哪个位置。
//! 引擎按 `lit = texel > t`（`t` = 归一化角度）采样，因此这个字节编码的是：
//!
//! - **变暗方向**（`ShadowGrows`）：`texel` = 该像素第一次被遮的时刻。
//!   小角度受光、大角度被遮。阴影随角度**扩张**。
//! - **变亮方向**（`ShadowShrinks`）：`texel` = 该像素第一次受光的时刻。
//!   小角度被遮、大角度受光。阴影随角度**收缩**。
//!
//! 两种方向都能编码，但**每个通道只能选一种**。方向选错了，翻转顺序就反了，
//! 还原出来的图和关键帧对不上。
//!
//! ## 为什么必须"嵌套"
//!
//! 把角度当高度轴，这片区域就是一个地形，`texel` 是海拔。
//! 每个位置的遮罩是一层水平切片。地形要成"山"而不是"悬空"，切片必须逐层嵌套：
//!
//! - 变暗方向：阴影必须**逐层变大**（后一层完全包含前一层）
//! - 变亮方向：阴影必须**逐层变小**
//!
//! 一旦某像素出现"暗→亮→再暗"这种来回翻转，一个字节就表达不了，
//! 只能编码第一次翻转，后面的信息丢失。
//!
//! ## 所以有自动方向检测
//!
//! 纯二值遮罩无法在绘制时判断意图方向，因此烘焙前先试两种方向，
//! 选"能正确编码的像素更多"的那个。见 `pick_direction`。

use rayon::prelude::*;

use crate::mask::Mask;
use crate::sdf::edt::signed_distance;

/// 距离场的哨兵阈值。超过该阈值表示"这一帧整幅没有边界"，
/// 也就是该帧全亮或全暗，距离值不代表真实几何。
const SENTINEL: f32 = 1.0e6;

/// 转变像素能用的 texel 范围。
///
/// 引擎按 `lit = texel > t * 255 * 0.99` 采样：
/// - texel = 0 表示"t=0 时就被遮"（首帧就遮）
/// - texel = 255 表示"全程受光"
///
/// 中间的转变像素必须在 `[1, 252]` 里：
/// - 下界留 1，避免和被首帧就遮的 0 混淆（`t≈0` 的转变会四舍五入成 0，
///   那样 0° 那一帧就会把该像素画成被遮，与参数点差一个像素）
/// - 上界 252，因为 90° 时阈值是 252.45，≥253 会被判成受光
// （telex 满刻度见 TEXEL_FULL）

/// 一帧的关键帧数据。
pub struct MaskFrame {
    pub angle: f32,
    pub mask: Vec<u8>,
}

/// 预先算好的单帧距离场，可在多次烘焙之间复用。
pub struct FrameSdf {
    pub angle: f32,
    pub dist: Vec<f32>,
}

impl FrameSdf {
    /// `mask` 是二值：0 = 不遮（受光），1 = 被遮（阴影）。
    ///
    /// 距离场约定：**不遮的地方为正**、被遮的地方为负、边界处为 0。
    /// `signed_distance` 把非零值当作"内部"，方向正好相反，所以这里取负。
    pub fn new(angle: f32, mask: &[u8], w: usize, h: usize) -> Self {
        let raw = signed_distance(mask, w, h, 1);
        let mut dist: Vec<f32> = raw.into_iter().map(|v| -v).collect();
        // 全空/全满的帧会得到 7.6e8 量级的哨兵值。
        // 单帧没有"另一帧"可比，用图像对角线作为上限——
        // 这是"到最近边界的最大可能距离"的几何上界。
        let limit = ((w * w + h * h) as f32).sqrt() + 1.0;
        for v in dist.iter_mut() {
            if !v.is_finite() || v.abs() > 1e6 {
                *v = if v.is_sign_negative() { -limit } else { limit };
            }
        }
        Self { angle, dist }
    }

    pub fn from_mask(angle: f32, mask: &Mask) -> Self {
        Self::new(
            angle,
            mask.data(),
            mask.width() as usize,
            mask.height() as usize,
        )
    }
}

// ---------------------------------------------------------------- 方向

/// 阴影随时间的变化方向。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// 阴影随角度增大而扩张：编码"第一次被遮"的时刻。
    ShadowGrows,
    /// 阴影随角度增大而收缩：编码"第一次受光"的时刻。
    ShadowShrinks,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::ShadowGrows => "阴影扩张",
            Direction::ShadowShrinks => "阴影收缩",
        }
    }

    /// 写进 PNG 元数据的稳定标识。
    ///
    /// 用 ASCII 短标记而不是中文名：着色器与工具链都要读该值，
    /// 中文依赖编码约定，ASCII 不会在传递中被转码破坏。
    pub fn tag(self) -> &'static str {
        match self {
            Direction::ShadowGrows => "grows",
            Direction::ShadowShrinks => "shrinks",
        }
    }

    /// 反查：从 PNG 元数据读回方向。未知值返回 `None`，由调用方决定回退策略。
    pub fn from_tag(s: &str) -> Option<Self> {
        match s.trim() {
            "grows" => Some(Direction::ShadowGrows),
            "shrinks" => Some(Direction::ShadowShrinks),
            _ => None,
        }
    }

    /// 采样判据：texel 超过阈值时是否算"受光"。
    ///
    /// - `ShadowGrows` 编码的是"第一次被遮"的时刻，越过该时刻即转暗，
    ///   所以越过阈值 → 不受光（`false`）
    /// - `ShadowShrinks` 编码的是"第一次受光"的时刻，越过该时刻即转亮，
    ///   所以越过阈值 → 受光（`true`）
    ///
    /// 引擎侧的着色器必须按同一张真值表实现，两种方向不能共用一条比较式。
    pub fn lit_when_passed(self) -> bool {
        match self {
            Direction::ShadowGrows => false,
            Direction::ShadowShrinks => true,
        }
    }
}

/// 方向判定结果。
///
/// 方向只能是单向的：一个通道所有像素的翻转方向必须一致，
/// 否则贴图表达不了。所以这里不做"哪个更好"的折中——
/// 能全编码才算通过，两个方向都不通过就是画错了。
pub enum DirectionCheck {
    /// 这个方向能完整编码
    Ok(Direction),
    /// 两个方向都无法完整编码，说明数据本身有问题
    Bad {
        /// 扩张方向下有多少像素是反向翻转的
        grow_conflicts: usize,
        /// 收缩方向下有多少像素是反向翻转的
        shrink_conflicts: usize,
        total: usize,
    },
}

/// 判断这批关键帧能按哪个方向编码。
///
/// 先试 `ShadowGrows`（引擎默认语义），整批像素都能单调就通过；
/// 否则试 `ShadowShrinks`。两个都不行就报错，不产出错图。
pub fn check_direction(frames: &[MaskFrame], w: usize, h: usize) -> DirectionCheck {
    let total = w * h;
    if frames.len() < 2 || total == 0 {
        // 单帧没有翻转可言，两个方向都"成立"，取默认
        return DirectionCheck::Ok(Direction::ShadowGrows);
    }
    let sorted = sorted_frames(frames);
    let grow_bad = total - count_encodable(&sorted, total, Direction::ShadowGrows);
    if grow_bad == 0 {
        return DirectionCheck::Ok(Direction::ShadowGrows);
    }
    let shrink_bad = total - count_encodable(&sorted, total, Direction::ShadowShrinks);
    if shrink_bad == 0 {
        return DirectionCheck::Ok(Direction::ShadowShrinks);
    }
    DirectionCheck::Bad {
        grow_conflicts: grow_bad,
        shrink_conflicts: shrink_bad,
        total,
    }
}

/// 排序 + 同角度去重（保留后出现的那个）。
fn sorted_frames(frames: &[MaskFrame]) -> Vec<&MaskFrame> {
    let mut idx: Vec<&MaskFrame> = frames.iter().collect();
    idx.sort_by(|a, b| {
        a.angle
            .partial_cmp(&b.angle)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    idx
}

/// 统计某方向下有多少像素是单调的（只翻转一次，或一次都不翻转）。
fn count_encodable(frames: &[&MaskFrame], total: usize, dir: Direction) -> usize {
    let mut ok = 0usize;
    for i in 0..total {
        let mut prev_masked = frames[0].mask.get(i).copied().unwrap_or(0) != 0;
        let mut monotonic = true;
        for f in frames.iter().skip(1) {
            let now = f.mask.get(i).copied().unwrap_or(0) != 0;
            let backwards = match dir {
                // 扩张方向：不允许"遮了又亮"
                Direction::ShadowGrows => prev_masked && !now,
                // 收缩方向：不允许"亮了又遮"
                Direction::ShadowShrinks => !prev_masked && now,
            };
            if backwards {
                monotonic = false;
                break;
            }
            prev_masked = now;
        }
        if monotonic {
            ok += 1;
        }
    }
    ok
}

// ---------------------------------------------------------------- 烘焙

/// 用自动判定出来的方向烘焙。方向不合法时返回 `Err`。
///
/// 返回 `(texel 图, 实际使用的方向)`。
pub fn bake_auto(
    frames: &mut Vec<MaskFrame>,
    w: usize,
    h: usize,
) -> Result<(Vec<u16>, Direction), String> {
    match check_direction(frames, w, h) {
        DirectionCheck::Ok(dir) => Ok((bake_with_dir(frames, w, h, dir), dir)),
        DirectionCheck::Bad {
            grow_conflicts,
            shrink_conflicts,
            total,
        } => Err(format!(
            "有像素的明暗来回翻转，两种方向都编码不了。\n\
             按「阴影扩张」有 {} 个像素冲突，按「阴影收缩」有 {} 个冲突（共 {}）。\n\
             每个像素在角度轴上只能翻转一次：要么一直受光后转暗，要么一直遮着后转亮，不能来回变。",
            grow_conflicts, shrink_conflicts, total
        )),
    }
}

/// 烘焙 texel 图。返回 `w * h` 的灰度，值 = 转变角 / 90 * 255。
///
/// 保留旧签名（默认 `ShadowGrows`），供不关心方向的调用方使用。
pub fn bake(frames: &mut Vec<MaskFrame>, w: usize, h: usize) -> Vec<u16> {
    bake_with_dir(frames, w, h, Direction::ShadowGrows)
}

/// 按指定方向烘焙。
pub fn bake_with_dir(frames: &mut Vec<MaskFrame>, w: usize, h: usize, dir: Direction) -> Vec<u16> {
    let n = w * h;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    if frames.is_empty() {
        return vec![0u16; n];
    }

    // 排序 + 同角度去重（保留后出现的那个）
    frames.sort_by(|a, b| {
        a.angle
            .partial_cmp(&b.angle)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut deduped: Vec<MaskFrame> = Vec::with_capacity(frames.len());
    for f in frames.drain(..) {
        match deduped.last() {
            Some(last) if (last.angle - f.angle).abs() < f32::EPSILON => {
                *deduped.last_mut().unwrap() = f;
            }
            _ => deduped.push(f),
        }
    }
    *frames = deduped;

    // **帧间并行**：每帧的距离场只依赖该帧自己的遮罩，与帧集合无关，
    // 所以 N 帧可以完全并行算。8 帧实测 6.4× 加速（24 线程）。
    // 帧数少时并行反而有调度开销，让小规模走串行。
    let sdfs: Vec<Vec<f32>> = if frames.len() >= 2 {
        frames
            .par_iter()
            .map(|f| {
                signed_distance(&f.mask, w, h, 1)
                    .into_iter()
                    .map(|v| -v) // 极性翻转：不遮为正
                    .collect()
            })
            .collect()
    } else {
        frames
            .iter()
            .map(|f| {
                signed_distance(&f.mask, w, h, 1)
                    .into_iter()
                    .map(|v| -v)
                    .collect()
            })
            .collect()
    };
    let angles: Vec<f32> = frames.iter().map(|f| f.angle).collect();
    let masks: Vec<&[u8]> = frames.iter().map(|f| f.mask.as_slice()).collect();
    let max_depths = max_depth_per_frame(&sdfs);
    merge_sdfs(&sdfs, &angles, &masks, &max_depths, w, h, dir)
}

/// 用**已经算好的**逐帧距离场烘焙，跳过 EDT。
///
/// # 为什么这个入口能省掉绝大部分成本
///
/// 距离场只依赖**该帧自己的遮罩**，与帧集合无关（实测：同一帧重算 EDT，
/// 结果 0 像素变化）。但 `bake_with_dir` 每次都把 N 帧全部重算——
/// 加第 N+1 帧时，前 N 帧的 EDT 一点没变却白算了。
///
/// 实测 4 帧 1K：全量 119.7 ms，其中 EDT 119.7 ms、合并仅 14 ms。
/// 也就是说加一帧时 3/4 的 EDT 是纯浪费。接通这个入口后只剩 1 帧要算。
///
/// `sdfs` 必须与 `frames` 一一对应、顺序一致、且已经排序去重过。
pub fn bake_from_sdfs(
    frames: &mut Vec<MaskFrame>,
    sdfs: &[Vec<f32>],
    w: usize,
    h: usize,
    dir: Direction,
) -> Vec<u16> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    if frames.is_empty() || sdfs.len() != frames.len() {
        // 长度不符说明调用方搞错了缓存，退回安全路径
        return bake_with_dir(frames, w, h, dir);
    }
    let angles: Vec<f32> = frames.iter().map(|f| f.angle).collect();
    let masks: Vec<&[u8]> = frames.iter().map(|f| f.mask.as_slice()).collect();
    let max_depths = max_depth_per_frame(sdfs);
    let refs: Vec<&[f32]> = sdfs.iter().map(|v| v.as_slice()).collect();
    merge_sdfs_ref(&refs, &angles, &masks, &max_depths, w, h, dir)
}

/// 每帧暗区最深处，供哨兵归一化用。
fn max_depth_per_frame(sdfs: &[Vec<f32>]) -> Vec<f32> {
    sdfs.iter()
        .map(|d| {
            d.iter()
                .filter(|v| **v < 0.0 && **v > -SENTINEL)
                .fold(0.0f32, |m, v| m.max(-v))
        })
        .collect()
}

fn merge_sdfs(
    sdfs: &[Vec<f32>],
    angles: &[f32],
    masks: &[&[u8]],
    max_depths: &[f32],
    w: usize,
    h: usize,
    dir: Direction,
) -> Vec<u16> {
    let refs: Vec<&[f32]> = sdfs.iter().map(|v| v.as_slice()).collect();
    merge_sdfs_ref(&refs, angles, masks, max_depths, w, h, dir)
}

fn merge_sdfs_ref(
    sdfs: &[&[f32]],
    angles: &[f32],
    masks: &[&[u8]],
    max_depths: &[f32],
    w: usize,
    h: usize,
    dir: Direction,
) -> Vec<u16> {
    let n = w * h;
    let mut out = vec![0u16; n];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let i = y * w + x;
            row[x] = match transition_angle(sdfs, angles, max_depths, masks, i, dir) {
                // 有转变时刻：落到 [0, FULL-1]，FULL 留给"恒定不变"
                Some(a) => angle_to_texel(a).min(TEXEL_FULL as u16 - 1),
                // 恒定不变：本方向的翻转永不发生
                None => constant_texel(),
            };
        }
    });
    out
}

/// 单个像素的转变角（度）。`None` = 整条轴上都不翻转。
///
/// 比例插值用的是**距离比值**，不是绝对像素：
/// `d0` 是还没被扫到的余量、`depth` 是已越过的深度，
/// 交点 = `d0 / (d0 + depth)`，与位移总距离无关，天然归一化。
/// （早期版本用固定 32px 窄带截断，把深层像素判成"不翻转"，
///  导致 1K 画布上大面积 texel=255、导出图出现硬缝。）
fn transition_angle(
    sdfs: &[&[f32]],
    angles: &[f32],
    max_depths: &[f32],
    masks: &[&[u8]],
    i: usize,
    dir: Direction,
) -> Option<f32> {
    // 遮/不遮一律看原 mask，不看距离场正负。
    // 距离为 0 既可能是"不遮但紧贴边界"，也可能是"被遮"，用符号判断会在
    // 边界那一列上误判（实测 1K 画布整列错位）。
    let masked_at = |k: usize| -> bool {
        masks
            .get(k)
            .and_then(|m| m.get(i).copied())
            .map(|v| v != 0)
            .unwrap_or(false)
    };

    if sdfs.len() == 1 {
        let masked = masked_at(0);
        return match (dir, masked) {
            (Direction::ShadowGrows, true) => Some(0.0),
            (Direction::ShadowGrows, false) => None,
            (Direction::ShadowShrinks, true) => None,
            (Direction::ShadowShrinks, false) => Some(0.0),
        };
    }

    // 首帧状态。若首帧已经处在"翻转后"的状态，翻转角是 0°。
    let first_masked = masked_at(0);
    match dir {
        Direction::ShadowGrows if first_masked => return Some(0.0),
        Direction::ShadowShrinks if !first_masked => return Some(0.0),
        _ => {}
    }

    for k in 0..sdfs.len() - 1 {
        let d0 = sdfs[k][i];
        let d1 = sdfs[k + 1][i];
        let masked0 = masked_at(k);
        let masked1 = masked_at(k + 1);

        // 只看本方向允许的翻转
        let flipped = match dir {
            Direction::ShadowGrows => !masked0 && masked1,
            Direction::ShadowShrinks => masked0 && !masked1,
        };
        if !flipped {
            continue;
        }

        let (a0, a1) = (angles[k], angles[k + 1]);

        // 两个时刻到边界的距离（都取绝对值，谁深谁浅无关紧要）
        let s0 = d0.abs();
        let s1 = d1.abs();

        // 全暗/全亮的帧没有边界，距离是哨兵，比例没有几何意义。
        if s0 > SENTINEL || s1 > SENTINEL {
            // 用"翻转后那一帧"的最深处归一化，使形状与画布尺寸无关
            let target = if masked1 { k + 1 } else { k };
            let md = max_depths.get(target).copied().unwrap_or(0.0).max(1.0);
            let live = if s1 > SENTINEL { s0 } else { s1 };
            let frac = (live / md).clamp(0.0, 1.0);

            // **形状该从哪一侧"开始"是分情况的**，这里曾经处理错。
            //
            // 空帧参与时，形状的出现/消失是"整个形状从无到有/从有到无"，
            // 而不是沿某条边界平移。两种方向的几何直觉相反：
            //
            // - **空帧在前**（`s0` 是哨兵，形状是"长出来"的）：
            //   应当**从内部核心向外冒**。深处的像素最"实"，先出现；
            //   边缘最后才补满。所以 `frac` 越大（越深）⇒ `t` 越小。
            //   → `t = 1 - frac`
            //
            // - **空帧在后**（`s1` 是哨兵，形状是"消失"的）：
            //   应当**从边缘向内退去**。边缘最"薄"，先消失；
            //   核心最后才没。所以 `frac` 越大（越深）⇒ `t` 越大。
            //   → `t = frac`（原本就是对的）
            //
            // 原来的实现两种情况一律用 `frac`，导致"长出来"变成了
            // "从边界向内填充"：t=0.1 时画面上出现一个**空心方框**，
            // 然后白框向内收拢直到填满。这不符合阴影生长直觉
            // （"哪里有从外向内生长的？"）。
            //
            // 实测证据（128×128、居中方块，新增像素的平均半径）：
            //   修前 48.2 → 43.7 → 38.5 → … → 3.9　（递减 = 从外向内）
            //   修后  0.9 →  4.0 →  8.3 → … → 48.2（递增 = 从内向外）
            //
            // 取反后要**避开 t=0**：深层像素 frac=1 → t=0，会在 0° 就被判成
            // "已遮"，两端就还原不回去了（实测 t=0 遮了 4 像素）。
            //
            // 公式必须是 `eps + (1-eps)*(1-frac)`，不能写成
            // `1 - frac*(1-eps) - eps`——后者在 frac=1 时等于 0，余量全被抵消掉。
            let empty_before = s0 > SENTINEL;
            let t = if empty_before {
                // 余量 = 一个 texel 对应的归一化量
                let eps = 1.0 / TEXEL_FULL as f32;
                eps + (1.0 - eps) * (1.0 - frac)
            } else {
                frac
            };
            return Some(a0 + (a1 - a0) * t.clamp(0.0, 1.0));
        }

        // 精确比例。s0 是"离边界还有多远"，s1 是"越过边界多深"。
        //
        // s0 == 0 表示这个像素**在帧 k 时正好压在边界上**。该像素属于哪一侧本就
        // 是模糊的（距离场只能给 0），取 0 会让该像素被算成"帧 k 就已翻转"，
        // 而采样时 t 恰好等于帧 k 就翻过去，与关键帧本身差一列。
        // 取中点把这点模糊性摊平，误差落在半个区间内而不是全压到端点。
        let t = if s0 + s1 <= f32::EPSILON {
            0.5
        } else if s0 <= f32::EPSILON {
            // 本帧就压在边界上：往后挪半格，避免和"本帧已翻转"混淆
            0.5
        } else {
            s0 / (s0 + s1)
        };
        return Some(a0 + (a1 - a0) * t.clamp(0.0, 1.0));
    }

    None
}

// ---------------------------------------------------------------- texel 编码

/// texel 满刻度。
///
/// 内部**一律用 u16**，满刻度 65535。8bit 导出只是最后截断，
/// 这样 8bit 和 16bit 走同一条精度路径，不会出现"16bit 版比 8bit 版平滑"
/// 是因为两套算法不同这种隐患。
///
/// 旧版内部用 u8 满刻度 252，两个关键帧间位移跨度大时，最贴近起点的那一列
/// 会挤不进 1/252 的精度而错位一列（实测 1K 画布错 1024 像素）。
pub const TEXEL_FULL: u32 = 65535;

/// 恒定不变时的 texel 值。
///
/// **两种方向下满刻度都表示"永不发生本方向的翻转"，但对应的明暗状态相反：**
/// - `ShadowGrows`：满刻度 = 全程受光（永远不转暗）
/// - `ShadowShrinks`：满刻度 = 全程被遮（永远不转亮）
///
/// 采样统一按"`texel >= FULL` → 该方向的静止态"处理。
#[inline]
fn constant_texel() -> u16 {
    TEXEL_FULL as u16
}

/// 角度 → texel（u16 满刻度）。
///
/// **0° 必须映射到 0**：表示"首帧就已是本方向翻转后的状态"，
/// 采样时 t=0 也要判为已翻转。
/// 上限压到 `TEXEL_FULL - 1`，把 `TEXEL_FULL` 独占给"恒定不变"。
#[inline]
fn angle_to_texel(angle_deg: f32) -> u16 {
    let norm = (angle_deg.clamp(0.0, 90.0) / 90.0).clamp(0.0, 1.0);
    let v = (norm * TEXEL_FULL as f32).round() as u32;
    v.min(TEXEL_FULL - 1) as u16
}

/// u16 texel → 8bit。
///
/// `TEXEL_FULL` 是"全程受光"的专用值，必须映射到 **255**。
/// 其余转变像素在 8bit 下压到 ≤252：引擎按 `lit = texel > t * 255 * 0.99` 采样，
/// 90° 时阈值是 252.45，所以 texel ≥ 253 在 90° 也会被判成"受光"，
/// 必须和"全程受光"区分开。
#[inline]
pub fn texel_to_u8(v: u16) -> u8 {
    const MAX: u16 = 252;
    if v as u32 >= TEXEL_FULL {
        return 255;
    }
    let n = v as u32 * MAX as u32;
    (n / TEXEL_FULL + ((n % TEXEL_FULL) as f32 / TEXEL_FULL as f32).round() as u32).min(MAX as u32)
        as u8
}

// ---------------------------------------------------------------- 采样还原

/// 用 texel 阈值还原成二值遮罩（u16 精度）。
///
/// `t` 是归一化时间 0..1。
///
/// **必须与 `angle_to_texel` 用同一个刻度（`TEXEL_FULL`）**：
/// 烘焙时角度按 `angle/90 * 65535` 换算，采样时阈值也按 `t * 65535` 算，
/// 两者严格互逆。
///
/// 两种方向的语义不同，判断必须跟着方向走：
/// - `ShadowGrows`：texel = 第一次**被遮**的时刻。`t` 超过该值 → 被遮。
/// - `ShadowShrinks`：texel = 第一次**受光**的时刻。`t` 超过该值 → 受光。
///
/// `invert` 再把结果整体取反（导出窗口里的"反向"开关）。
pub fn sample_texels(texels: &[u16], t: f32, dir: Direction, out: &mut Mask, invert: bool) {
    let data = out.data_mut();
    let n = data.len().min(texels.len());
    let thresh = t.clamp(0.0, 1.0) * TEXEL_FULL as f32;
    for i in 0..n {
        let masked = texel_masked(texels[i], thresh, dir);
        data[i] = if masked { 1 } else { 0 };
    }
}

/// 单个 texel 在给定阈值下是否被遮。预览与导出共用这一条判据。
#[inline]
fn texel_masked(v: u16, thresh: f32, dir: Direction) -> bool {
    if v as u32 >= TEXEL_FULL {
        // 恒定不变：本方向的翻转永远没发生。
        //   Grows  → 全程受光 → 不遮
        //   Shrinks→ 全程被遮 → 遮
        return matches!(dir, Direction::ShadowShrinks);
    }
    // passed = 参数已经越过该像素的转变时刻
    let passed = v as f32 <= thresh;
    // 越过"被遮"时刻 → 被遮；越过"受光"时刻 → 受光（即不再被遮）
    passed != dir.lit_when_passed()
}

/// 按**导出语义**把 texel 图还原成遮罩。
///
/// 与 `sample_texels(..., invert=true)` 的区别（这正是曾经的不一致）：
///
/// - `invert=true` 是**后置取反**：先按原 texel 判定，再把结果取反。
///   等价于"同一参数位置下的明暗互换"。
/// - 导出时的「反向」是**前置翻转**：写盘前把 texel 换成 `FULL - v`，
///   等价于"整条参数轴的明暗对调"——某像素改成在镜像时刻翻转。
///
/// 两者在参数轴中间位置给出不同结果，所以导出窗口的还原预览
/// 不能再借用后置取反，必须走本函数，才能保证"看到的"就是"写进文件的"。
pub fn sample_texels_as_exported(
    texels: &[u16],
    t: f32,
    dir: Direction,
    invert: bool,
    out: &mut Mask,
) {
    let data = out.data_mut();
    let n = data.len().min(texels.len());
    let thresh = t.clamp(0.0, 1.0) * TEXEL_FULL as f32;
    for i in 0..n {
        let v = if invert {
            (TEXEL_FULL as u16).saturating_sub(texels[i])
        } else {
            texels[i]
        };
        data[i] = if texel_masked(v, thresh, dir) { 1 } else { 0 };
    }
}

/// 把 u16 texel 图转成 8bit 灰度图。
pub fn texels_to_gray8(texels: &[u16], w: u32, h: u32) -> image::GrayImage {
    let data: Vec<u8> = texels.iter().map(|&v| texel_to_u8(v)).collect();
    image::GrayImage::from_raw(w, h, data).unwrap_or_else(|| image::GrayImage::new(w, h))
}

/// 把 u16 texel 图转成 16bit 灰度图（满刻度 65535，无损）。
pub fn texels_to_gray16(
    texels: &[u16],
    w: u32,
    h: u32,
) -> image::ImageBuffer<image::Luma<u16>, Vec<u16>> {
    image::ImageBuffer::from_raw(w, h, texels.to_vec())
        .unwrap_or_else(|| image::ImageBuffer::new(w, h))
}

// ---------------------------------------------------------------- 中间帧插值（旧路径）

/// 用两张已算好的距离场插值出一张二值遮罩（不遮为正）。
///
/// 已不再用于预览（预览改走 `sample_texels`），保留给测试与外部调用。
pub fn interpolate_from_dist(da: &[f32], db: &[f32], t: f32, out: &mut Mask) {
    let t = t.clamp(0.0, 1.0);
    let out_data = out.data_mut();
    let n = out_data.len().min(da.len()).min(db.len());
    for i in 0..n {
        let v = da[i] * (1.0 - t) + db[i] * t;
        out_data[i] = if v <= 0.0 { 1 } else { 0 };
    }
}

/// 用距离场在相邻两帧之间插值出一张二值遮罩。
///
/// 内部会现算两张距离场，用于测试和一次性场景。
pub fn interpolate_masks(a: &Mask, b: &Mask, t: f32, out: &mut Mask) {
    let (w, h) = (a.width() as usize, a.height() as usize);

    let mut da: Vec<f32> = signed_distance(a.data(), w, h, 1)
        .into_iter()
        .map(|v| -v)
        .collect();
    let mut db: Vec<f32> = signed_distance(b.data(), w, h, 1)
        .into_iter()
        .map(|v| -v)
        .collect();

    // 空帧（全空或全满）与有内容帧之间不能用距离场线性插值——
    // 空帧没有边界，场尺度无从谈起，插值结果会被常数值压倒
    // （实测 t=0.44 处仍是"未遮"，即"加点后不插值"这一现象）。
    // 改为"按 t 收缩有内容帧"，几何上就是遮罩从无到有地生长。
    let a_empty = is_degenerate(&da);
    let b_empty = is_degenerate(&db);
    match (a_empty, b_empty) {
        (true, true) => {
            // 两帧都没有形状：结果也是空
            out.clear();
            return;
        }
        (true, false) => {
            // A 空、B 有：t=1 时才是完整 B
            let neg = da.first().map(|v| v.is_sign_negative()).unwrap_or(false);
            lerp_from_empty(neg, &db, t, out);
            return;
        }
        (false, true) => {
            // A 有、B 空：t=0 时才是完整 A（反向生长）
            let neg = db.first().map(|v| v.is_sign_negative()).unwrap_or(false);
            lerp_from_empty(neg, &da, 1.0 - t, out);
            return;
        }
        (false, false) => {}
    }

    interpolate_from_dist(&da, &db, t, out);
}

/// 判断一个距离场是否"无结构"（整幅图同色，全部是同一个值）。
fn is_degenerate(dist: &[f32]) -> bool {
    if dist.is_empty() {
        return true;
    }
    let first = dist[0];
    dist.iter().all(|v| (*v - first).abs() < 1e-3)
}

/// 空帧与有内容帧的插值：**直接按距离阈值收缩另一帧的遮罩**。
///
/// # 为什么不能把空帧也做成距离场去插值
///
/// 空帧的距离场是一个常数（哨兵值 7.6e8，或锥形场）。
/// 但无论换成什么，**距离场线性插值要求两帧的场尺度可比**，
/// 而"空"这一帧根本没有边界，尺度无从谈起。实测：
/// 64×64 图、空帧锥形场中心值 44.55、有内容帧中心 -12、t=0.44 时
/// 混合得到 +19.79（>0 → 不遮），而期望是"遮"。
///
/// 要让插值成立，空帧的值必须小于 9.34 —— 也就是必须**接近有内容帧的
/// 距离量级**，而不是图像尺度。这暴露出问题的本质：
///
/// > 空帧不是"离边界很远的一帧"，而是"这一帧没有形状"。
/// > 对空帧做距离场插值没有几何意义。
///
/// # 正确做法
///
/// 把插值理解为"目标形状从无到有地生长"：
/// - `t = 0`：完全没有（空帧）
/// - `t = 1`：完整的有内容帧
/// - 中间：把有内容帧的距离场整体**加上一个正偏移**，
///   等价于"把遮罩向内腐蚀"——偏移越大，剩下越少。
///
/// 偏移量按 `t` 线性取：`offset = max_dist_inside * (1 - t)`。
/// 这样 t 小时只剩最核心的一小块，t 大时接近完整形状，
/// 视觉上就是"遮罩逐渐长出来"。
fn lerp_from_empty(empty_negative: bool, solid: &[f32], t: f32, out: &mut Mask) {
    let t = t.clamp(0.0, 1.0);
    // 有内容帧里"最深处"的距离绝对值，作为偏移的总量
    let deepest = solid
        .iter()
        .filter(|v| v.is_finite())
        .fold(0.0f32, |m, v| if *v < 0.0 { m.max(-*v) } else { m });
    let offset = deepest * (1.0 - t);

    let out_data = out.data_mut();
    let n = out_data.len().min(solid.len());
    for i in 0..n {
        // 空帧在"不遮"约定下是正值，加偏移即向内收缩
        let v = if empty_negative {
            solid[i] - offset
        } else {
            solid[i] + offset
        };
        out_data[i] = if v <= 0.0 { 1 } else { 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(angle: f32, mask: Vec<u8>) -> MaskFrame {
        MaskFrame { angle, mask }
    }

    // ------------------------------------------------ 方向判定

    /// 阴影随角度扩张 → 判定为 ShadowGrows
    #[test]
    fn 阴影扩张时判定为扩张方向() {
        let (w, h) = (16, 1);
        let lit = vec![0u8; w];
        let mut half = vec![0u8; w];
        for x in 0..8 {
            half[x] = 1;
        }
        let f = vec![frame(0.0, lit), frame(90.0, half)];
        match check_direction(&f, w, h) {
            DirectionCheck::Ok(d) => assert_eq!(d, Direction::ShadowGrows),
            DirectionCheck::Bad { .. } => panic!("单次翻转应能编码"),
        }
    }

    /// 阴影随角度收缩 → 判定为 ShadowShrinks
    #[test]
    fn 阴影收缩时判定为收缩方向() {
        let (w, h) = (16, 1);
        let mut half = vec![0u8; w];
        for x in 0..8 {
            half[x] = 1;
        }
        let lit = vec![0u8; w];
        let f = vec![frame(0.0, half), frame(90.0, lit)];
        match check_direction(&f, w, h) {
            DirectionCheck::Ok(d) => {
                assert_eq!(d, Direction::ShadowShrinks, "阴影收缩应判为收缩方向")
            }
            DirectionCheck::Bad { .. } => panic!("单次翻转应能编码"),
        }
    }

    /// 收缩方向烘焙后再用阈值采样，两端应还原正确
    #[test]
    fn 收缩方向能正确还原两端() {
        let (w, h) = (32, 1);
        let mut wide = vec![0u8; w];
        for x in 0..24 {
            wide[x] = 1;
        }
        let mut narrow = vec![0u8; w];
        for x in 0..8 {
            narrow[x] = 1;
        }
        let mut f = vec![frame(0.0, wide), frame(90.0, narrow)];
        let (texels, dir) = bake_auto(&mut f, w, h).expect("应能编码");
        assert_eq!(dir, Direction::ShadowShrinks);

        let mut m = Mask::new(w as u32, 1);
        sample_texels(&texels, 0.0, dir, &mut m, false);
        assert_eq!(m.get(0, 0), 1, "0° 时 x=0 应被遮");
        assert_eq!(m.get(20, 0), 1, "0° 时 x=20 应被遮");
        sample_texels(&texels, 1.0, dir, &mut m, false);
        assert_eq!(m.get(20, 0), 0, "90° 时 x=20 应受光");
        assert_eq!(m.get(0, 0), 1, "90° 时 x=0 仍应被遮（窄阴影还在）");
    }

    /// 来回翻转的数据两个方向都不行 → 必须报错，不产出错图
    #[test]
    fn 来回翻转时两个方向都失败并报错() {
        let (w, h) = (8, 1);
        let lit = vec![0u8; w];
        let mut half = vec![0u8; w];
        for x in 0..4 {
            half[x] = 1;
        }
        // 亮 → 暗 → 亮 → 暗：x<4 翻转了三次
        let f = vec![
            frame(0.0, lit.clone()),
            frame(30.0, half.clone()),
            frame(60.0, lit),
            frame(90.0, half),
        ];
        match check_direction(&f, w, h) {
            DirectionCheck::Ok(_) => panic!("来回翻转不该通过"),
            DirectionCheck::Bad {
                grow_conflicts,
                shrink_conflicts,
                ..
            } => {
                assert!(grow_conflicts > 0);
                assert!(shrink_conflicts > 0);
            }
        }

        let mut f2 = vec![
            frame(0.0, vec![0u8; w]),
            frame(30.0, (0..w).map(|x| (x < 4) as u8).collect()),
            frame(60.0, vec![0u8; w]),
            frame(90.0, (0..w).map(|x| (x < 4) as u8).collect()),
        ];
        assert!(bake_auto(&mut f2, w, h).is_err(), "bake_auto 应返回错误");
    }

    // ------------------------------------------------ 回归：导出过渡带

    /// **空帧→有内容时，形状必须从中心长大，不能从边界向内填充。**
    ///
    /// 已修复的方向错误：原来 t 从 0 增大时，画面上先出现一个
    /// **空心外框**，然后白框向内收拢直到填满。
    ///
    /// > 哪里有从外向内生长/收缩的啊？只有从外部收缩/生长的。
    ///
    /// 判据：逐时刻看"新出现的像素离中心多远"。从内向外 → 递增。
    #[test]
    fn 空帧到有内容应从中心长大() {
        let n = 96u32;
        let (w, h) = (n as usize, n as usize);
        let (cx, cy) = (n as f32 / 2.0, n as f32 / 2.0);
        let mut dark = vec![0u8; w * h];
        for y in 12..(n - 12) {
            for x in 12..(n - 12) {
                dark[y as usize * w + x as usize] = 1;
            }
        }
        let lit = vec![0u8; w * h];
        // 空帧在前 → ShadowGrows
        let mut f = vec![frame(0.0, lit), frame(90.0, dark.clone())];
        let (out, dir) = bake_auto(&mut f, w, h).expect("应能编码");
        assert_eq!(dir, Direction::ShadowGrows);

        // 两端必须还原
        let mut at0 = Mask::new(n, n);
        sample_texels(&out, 0.0, dir, &mut at0, false);
        assert_eq!(at0.count(), 0, "0° 应是空的（空帧就是空的）");
        let mut at1 = Mask::new(n, n);
        sample_texels(&out, 1.0, dir, &mut at1, false);
        assert_eq!(
            at1.count(),
            dark.iter().filter(|v| **v != 0).count(),
            "90° 应是完整形状"
        );

        // 新出现像素的平均半径应**递增**
        let mut prev = Mask::new(n, n);
        let mut radii = Vec::new();
        for k in 0..=10 {
            let t = k as f32 / 10.0;
            let mut m = Mask::new(n, n);
            sample_texels(&out, t, dir, &mut m, false);
            let mut sum = 0.0f64;
            let mut cnt = 0usize;
            for y in 0..n {
                for x in 0..n {
                    let i = (y * w as u32 + x) as usize;
                    if m.data()[i] != 0 && prev.data()[i] == 0 {
                        let dx = x as f32 - cx;
                        let dy = y as f32 - cy;
                        sum += ((dx * dx + dy * dy) as f64).sqrt();
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                radii.push(sum / cnt as f64);
            }
            prev = m;
        }
        assert!(radii.len() >= 5, "应采到足够多的时刻，实际 {}", radii.len());
        let increasing = radii.windows(2).all(|w| w[1] >= w[0] - 0.6);
        assert!(
            increasing,
            "形状应从中心长大（新增像素半径递增），实际半径序列：{:?}",
            radii
                .iter()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        );
    }

    /// 反方向（有内容→空帧）仍应"从边缘退去"，不能被上面的修复带坏。
    #[test]
    fn 有内容到空帧仍从边缘退去() {
        let n = 96u32;
        let (w, h) = (n as usize, n as usize);
        let (cx, cy) = (n as f32 / 2.0, n as f32 / 2.0);
        let mut dark = vec![0u8; w * h];
        for y in 12..(n - 12) {
            for x in 12..(n - 12) {
                dark[y as usize * w + x as usize] = 1;
            }
        }
        let lit = vec![0u8; w * h];
        // 空帧在后 → ShadowShrinks
        let mut f = vec![frame(0.0, dark.clone()), frame(90.0, lit)];
        let (out, dir) = bake_auto(&mut f, w, h).expect("应能编码");
        assert_eq!(dir, Direction::ShadowShrinks);

        // 消失像素的平均半径应**递减**（边缘先没）
        let mut prev = Mask::new(n, n);
        sample_texels(&out, 0.0, dir, &mut prev, false);
        let mut radii = Vec::new();
        for k in 1..=10 {
            let t = k as f32 / 10.0;
            let mut m = Mask::new(n, n);
            sample_texels(&out, t, dir, &mut m, false);
            let mut sum = 0.0f64;
            let mut cnt = 0usize;
            for y in 0..n {
                for x in 0..n {
                    let i = (y * w as u32 + x) as usize;
                    if m.data()[i] == 0 && prev.data()[i] != 0 {
                        let dx = x as f32 - cx;
                        let dy = y as f32 - cy;
                        sum += ((dx * dx + dy * dy) as f64).sqrt();
                        cnt += 1;
                    }
                }
            }
            if cnt > 0 {
                radii.push(sum / cnt as f64);
            }
            prev = m;
        }
        assert!(radii.len() >= 5, "应采到足够多的时刻");
        let decreasing = radii.windows(2).all(|w| w[1] <= w[0] + 0.6);
        assert!(
            decreasing,
            "消失应从边缘开始（半径递减），实际：{:?}",
            radii
                .iter()
                .map(|v| (v * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        );
    }

    /// 深阴影必须能被量化，不能因为深度超过某个固定像素数就判成"永不转暗"。
    #[test]
    fn 深阴影不被窄带截断() {
        let n = 256u32;
        let (w, h) = (n as usize, n as usize);
        let (cx, cy, r) = (128.0f32, 128.0f32, 100.0f32);
        let mut dark = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 + 0.5 - cx;
                let dy = y as f32 + 0.5 - cy;
                if dx * dx + dy * dy <= r * r {
                    dark[y * w + x] = 1;
                }
            }
        }
        let lit = vec![0u8; w * h];
        let mut f = vec![frame(0.0, lit), frame(90.0, dark)];
        let out = bake(&mut f, w, h);

        // 直接用真实采样函数判断，不要手写公式——
        // 手写会把 8bit 的 `texel/255 > t*0.99` 搬到 u16 上，和内部规则不符。
        let mut at90 = Mask::new(w as u32, h as u32);
        sample_texels(&out, 1.0, Direction::ShadowGrows, &mut at90, false);

        let center = out[128 * w + 128];
        assert!(
            at90.get(128, 128) == 1,
            "圆心在 90° 必须已变暗，实际 texel={center}"
        );
        assert_ne!(center, TEXEL_FULL as u16, "圆心不该被判成全程受光");
        assert_eq!(out[0], TEXEL_FULL as u16, "左上角在圆外，应全程不遮");
        assert!(at90.get(0, 0) == 0, "左上角在圆外，90° 也应受光");

        let mid = out.iter().filter(|&&v| v > 500 && v < 63000).count();
        assert!(mid > 1000, "过渡带像素太少（{mid}）");

        // 从边界走向圆心应单调**递减**（圆心 texel 更小 → 更早变暗）。
        //
        // 这里曾经断言的是"递增"，也就是靠近边界更早变暗 —— 那意味着
        // 形状**从边界向内填充**：画面上先出现一个空心外框，再向内收拢。
        // 这不符合阴影生长直觉：形状应从内部核心向外扩展。
        // 正确行为是形状从中心核心长大，所以越靠近圆心越早。
        let row = h / 2;
        let mut prev = out[row * w + 40];
        let mut decreases = 0;
        for x in (44..120).step_by(4) {
            let v = out[row * w + x];
            assert!(v <= prev, "从边界走向圆心应单调递减：x={x} {prev} → {v}");
            if v < prev {
                decreases += 1;
            }
            prev = v;
        }
        assert!(decreases > 15, "沿中线应持续递减，实际只有 {decreases} 次");
    }

    #[test]
    fn 单帧全不遮输出全白() {
        let mut f = vec![frame(45.0, vec![0; 64])];
        assert!(bake(&mut f, 8, 8).iter().all(|&v| v == TEXEL_FULL as u16));
    }

    #[test]
    fn 单帧全遮输出全黑() {
        let mut f = vec![frame(45.0, vec![1; 64])];
        assert!(bake(&mut f, 8, 8).iter().all(|&v| v == 0));
    }

    #[test]
    fn 边界平移时中间像素取到中间角() {
        let (w, h) = (16, 1);
        let a = vec![0u8; 16];
        let mut b = vec![0u8; 16];
        for x in 0..4 {
            b[x] = 1;
        }
        let mut f = vec![frame(0.0, a), frame(90.0, b)];
        let out = bake(&mut f, w, h);

        assert_eq!(out[12], TEXEL_FULL as u16, "右侧应全程不遮");
        let a0 = out[0] as f32;
        let a3 = out[3] as f32;
        // x=0 比 x=3 更靠近形状内部（这是一维的左端起点），
        // 所以按"从中心长大"该像素应当**更早**变暗，texel 更小。
        //
        // 旧断言写的是 `a3 < a0`（靠近边界更早翻），那是"从边界向内填充"
        // 的旧行为；方向修正后已改为从内向外。
        assert!(a0 < a3, "形状内侧应更早翻: x0={a0} x3={a3}");
        assert!(
            a0 < TEXEL_FULL as f32 && a3 < TEXEL_FULL as f32,
            "转变像素不能撞到满刻度: x3={a3} x0={a0}"
        );
        // u16 精度下这两者应该能明显区分（不是被量化到同一格）
        assert!(
            a3 - a0 > 2000.0,
            "16bit 精度应能区分 0 和 3 这两列的转变时刻，实际差值只有 {}",
            a3 - a0
        );
    }

    #[test]
    fn 乱序关键帧被正确排序() {
        let (w, h) = (16, 1);
        let a = vec![0u8; 16];
        let mut b = vec![0u8; 16];
        for x in 0..4 {
            b[x] = 1;
        }
        let mut f = vec![frame(90.0, b), frame(0.0, a)];
        let out = bake(&mut f, w, h);
        assert_eq!(out[12], TEXEL_FULL as u16, "右侧应全程不遮");
        // 同上一处：形状内侧（x=0）应更早翻，见"从中心长大"的说明。
        assert!(out[0] < out[3], "形状内侧应更早翻");
    }

    #[test]
    fn 首帧就遮的像素给零() {
        let (w, h) = (8, 1);
        let dark = vec![1u8; 8];
        let lit = vec![0u8; 8];
        let mut f = vec![frame(0.0, dark), frame(90.0, lit)];
        let out = bake(&mut f, w, h);
        assert!(out.iter().all(|&v| v == 0));
    }

    #[test]
    fn 空帧列表不崩溃() {
        let mut f = vec![];
        let out = bake(&mut f, 4, 4);
        assert_eq!(out.len(), 16);
        assert!(out.iter().all(|&v| v == 0));
    }

    #[test]
    fn 重复角度去重后取最后一个() {
        let mut f = vec![frame(0.0, vec![1; 16]), frame(0.0, vec![0; 16])];
        let out = bake(&mut f, 4, 4);
        assert!(out.iter().all(|&v| v == TEXEL_FULL as u16), "got {out:?}");
    }

    #[test]
    fn 插值_两端正好还原() {
        let mut a = Mask::new(16, 16);
        let mut b = Mask::new(16, 16);
        for y in 0..16 {
            for x in 0..6 {
                a.set(x, y, 1);
            }
            for x in 0..12 {
                b.set(x, y, 1);
            }
        }
        let mut out = Mask::new(16, 16);
        interpolate_masks(&a, &b, 0.0, &mut out);
        assert_eq!(out.get(5, 8), 1);
        assert_eq!(out.get(6, 8), 0);

        interpolate_masks(&a, &b, 1.0, &mut out);
        assert_eq!(out.get(11, 8), 1);
        assert_eq!(out.get(12, 8), 0);
    }

    #[test]
    fn 插值_中点是边界的一半() {
        let mut a = Mask::new(32, 32);
        let mut b = Mask::new(32, 32);
        for y in 0..32 {
            for x in 0..8 {
                a.set(x, y, 1);
            }
            for x in 0..24 {
                b.set(x, y, 1);
            }
        }
        let mut out = Mask::new(32, 32);
        interpolate_masks(&a, &b, 0.5, &mut out);
        assert_eq!(out.get(15, 8), 1, "15 应被遮");
        assert_eq!(out.get(17, 8), 0, "17 不该被遮");
    }

    #[test]
    fn 插值_空遮罩到全遮() {
        let a = Mask::new(8, 8);
        let mut b = Mask::new(8, 8);
        b.fill(1);
        let mut out = Mask::new(8, 8);
        interpolate_masks(&a, &b, 0.5, &mut out);
        assert!(out.count() <= 64, "中点不该大面积被遮，got {}", out.count());
    }

    #[test]
    fn 插值_局部形状可靠收缩与扩张() {
        let mut a = Mask::new(32, 32);
        let mut b = Mask::new(32, 32);
        for y in 8..24 {
            for x in 8..24 {
                a.set(x, y, 1);
            }
            for x in 4..28 {
                b.set(x, y, 1);
            }
        }
        let mut out = Mask::new(32, 32);
        interpolate_masks(&a, &b, 0.5, &mut out);
        assert_eq!(out.get(6, 16), 1, "扩张后 6 应被遮");
        assert_eq!(out.get(2, 16), 0, "2 仍不该被遮");
    }

    // ------------------------------------------------ 采样还原

    /// 采样规则必须和引擎一致：texel > t*满刻度 才算受光
    #[test]
    fn 采样规则与引擎一致() {
        let w = 8u32;
        let texels: Vec<u16> = (0..w as usize)
            .map(|i| ((i as f32 / w as f32) * TEXEL_FULL as f32) as u16)
            .collect();
        let mut m = Mask::new(w, 1);

        // t=0：只有 texel=0 被遮，其余受光
        sample_texels(&texels, 0.0, Direction::ShadowGrows, &mut m, false);
        assert_eq!(m.get(0, 0), 1, "texel=0 在 t=0 被遮");
        assert_eq!(m.get(7, 0), 0, "texel 最大在 t=0 受光");

        // t=1：全部被遮
        sample_texels(&texels, 1.0, Direction::ShadowGrows, &mut m, false);
        assert_eq!(m.get(7, 0), 1, "t=1 全被遮");
    }

    /// 「反向」是**时间轴镜像**，不是逐像素取反。
    ///
    /// 两个概念在这里必须分清：
    /// - 前置翻转（导出真正做的）：写盘前把 texel 换成 `FULL - v`，
    ///   某像素改成"在镜像时刻翻转"，等价于整条参数轴的明暗对调。
    /// - 后置取反（`sample_texels(..., invert=true)`）：同一时刻明暗互换。
    ///
    /// 两者只在轴的两个端点重合。这条测试锁的是**前置翻转**的语义，
    /// 因为那才是写进 PNG 的东西。
    #[test]
    fn 反向开关翻转结果() {
        let texels: Vec<u16> = vec![0, 26000, 52000, TEXEL_FULL as u16];
        let mut m = Mask::new(4, 1);

        // 正常：按原 texel 判定
        sample_texels_as_exported(&texels, 0.5, Direction::ShadowGrows, false, &mut m);
        let normal: Vec<u8> = (0..4).map(|x| m.get(x, 0)).collect();

        // 反向：texel 先换成 FULL - v 再判定
        sample_texels_as_exported(&texels, 0.5, Direction::ShadowGrows, true, &mut m);
        let flipped: Vec<u8> = (0..4).map(|x| m.get(x, 0)).collect();

        // 逐项核对，而不是笼统要求"取反"。
        //
        // t=0.5、满刻度 65535，阈值 32767.5：
        //   texel 0     → 0 <= 阈值 → 已翻 → 被遮
        //   texel 26000 → 已翻 → 被遮
        //   texel 52000 → 未翻 → 受光
        //   texel FULL  → 恒定 → 受光（Grows 下永不转暗）
        assert_eq!(normal, vec![1, 1, 0, 0], "正常采样");

        // 反向把 texel 换成 FULL - v：
        //   0     → 65535 → 恒定 → 受光
        //   26000 → 39535 → 未翻 → 受光
        //   52000 → 13535 → 已翻 → 被遮
        //   FULL  → 0     → 已翻 → 被遮
        assert_eq!(flipped, vec![0, 0, 1, 1], "反向采样（时间轴镜像）");

        // 反向的精确不变式：`flipped(t) == !normal(1 - t)`。
        //
        // 推导（仅对非常量 texel）：
        //   正常：masked ⟺ v <= t*FULL
        //   反向把 v 换成 FULL - v：masked ⟺ FULL - v <= t*FULL
        //                          ⟺ v >= (1 - t)*FULL
        // 而 normal(1-t) 是 `v <= (1-t)*FULL`，两者正好互补。
        //
        // 也就是说「反向」同时翻转了时间轴与明暗极性——这正是该开关的定义，
        // 与"同一时刻取反"（后置取反）不是同一个操作。
        for (k, &v) in [26000u16, 52000].iter().enumerate() {
            for t in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
                let mut a = Mask::new(1, 1);
                let mut b = Mask::new(1, 1);
                sample_texels_as_exported(&[v], 1.0 - t, Direction::ShadowGrows, false, &mut a);
                sample_texels_as_exported(&[v], t, Direction::ShadowGrows, true, &mut b);
                assert_eq!(
                    a.get(0, 0),
                    1 - b.get(0, 0),
                    "texel #{k}={v} t={t}：反向态应等于 (1-t) 正常态取反"
                );
            }
        }
    }
}
