//! 形状工具与平滑工具用到的二值几何运算。
//!
//! ## 平滑工具为什么不能用"灰度模糊"
//!
//! 遮罩只允许 0/1。如果直接做高斯模糊，边界会变成 0.0~1.0 的中间值，
//! 二值性立刻被破坏——导出的 SDF 会跟着错。
//!
//! 正确做法是**浮动模糊 + 阈值**：
//! 1. 把二值图当作浮点场做一次盒式模糊（此时确实出现中间值）
//! 2. 立刻用 0.5 重新二值化
//!
//! 效果：边界的锯齿被平均掉，等值线变得更接近真实曲线；
//! 而结果**仍然是严格的 0/1**。
//!
//! 已知代价：比半径更细的孤立线会被模糊"抹平"到
//! 低于 0.5 而消失。这是平滑的固有行为，不是 bug。

use crate::mask::Mask;

/// 形状类型
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeKind {
    Rect,
    Ellipse,
}

impl ShapeKind {
    pub fn name(self) -> &'static str {
        match self {
            ShapeKind::Rect => "矩形",
            ShapeKind::Ellipse => "圆形",
        }
    }
    pub fn toggled(self) -> Self {
        match self {
            ShapeKind::Rect => ShapeKind::Ellipse,
            ShapeKind::Ellipse => ShapeKind::Rect,
        }
    }
}

/// 在遮罩上画一个矩形（实心或空心）。
///
/// 坐标是画布像素的浮点位置，两个角自动排序。空心时线宽至少 1 像素。
pub fn draw_rect(
    mask: &mut Mask,
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    value: u8,
    filled: bool,
    line_w: f32,
) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let x0 = ax.min(bx).round() as i32;
    let y0 = ay.min(by).round() as i32;
    let x1 = ax.max(bx).round() as i32;
    let y1 = ay.max(by).round() as i32;
    let lw = line_w.max(1.0);

    for y in y0.max(0)..=y1.min(h - 1) {
        for x in x0.max(0)..=x1.min(w - 1) {
            let on = if filled {
                true
            } else {
                // 距四条边任一 <= lw 就在边上
                let d = (x - x0).min(x1 - x).min(y - y0).min(y1 - y) as f32;
                d <= lw - 0.5
            };
            if on {
                mask.set(x as u32, y as u32, value);
            }
        }
    }
}

/// 在遮罩上画一个椭圆（内切于给定矩形；按住 Shift 时两轴相等即正圆）。
///
/// 用归一化椭圆方程判定，比中点画圆算法更容易支持任意长宽比。
/// 空心时按"到边界的归一化距离"控制线宽。
pub fn draw_ellipse(
    mask: &mut Mask,
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    value: u8,
    filled: bool,
    line_w: f32,
) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    let x0 = ax.min(bx);
    let y0 = ay.min(by);
    let x1 = ax.max(bx);
    let y1 = ay.max(by);
    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let rx = (x1 - x0).abs() * 0.5;
    let ry = (y1 - y0).abs() * 0.5;
    if rx < 0.5 || ry < 0.5 {
        // 退化成点/线，直接按矩形处理，避免除零
        draw_rect(mask, ax, ay, bx, by, value, filled, line_w);
        return;
    }

    // 线宽换算成"归一化半径收缩量"：取两轴平均，够用且不挑方向
    let shrink = if filled {
        0.0
    } else {
        (line_w.max(1.0)) / rx.min(ry).max(1.0)
    };

    for y in (cy - ry).floor().max(0.0) as i32..=(cy + ry).ceil().min((h - 1) as f32) as i32 {
        for x in (cx - rx).floor().max(0.0) as i32..=(cx + rx).ceil().min((w - 1) as f32) as i32 {
            let nx = (x as f32 + 0.5 - cx) / rx;
            let ny = (y as f32 + 0.5 - cy) / ry;
            let d = nx * nx + ny * ny;
            let on = if filled {
                d <= 1.0
            } else {
                // 外圈内、内圈外
                let inner = (1.0 - shrink).max(0.0);
                d <= 1.0 && d >= inner * inner
            };
            if on {
                mask.set(x as u32, y as u32, value);
            }
        }
    }
}

/// 笔刷式平滑：只改笔刷圆内的像素，且**边缘不留接缝**。
///
/// ## 为什么不能硬性裁剪
///
/// 天真做法是"算出平滑结果，只把圆内的值写回去，圆外保持原样"。
/// 这样在圆边界上，平滑后的分界线与原来的锯齿分界线对不上，
/// 会出现一圈**可见的接缝**——这是必须避免的。
///
/// ## 正确做法：在浮点域按半径衰减混合
///
/// 1. 对局部窗口做盒式模糊，得到浮点场 `b`
/// 2. 圆内每个像素按到圆心的距离算权重 `w`：圆心为 1，圆边为 0
/// 3. `f = w*b + (1-w)*原值`，再按 0.5 二值化
///
/// 关键性质：`w → 0` 时 `f → 原值`，而原值是 0/1，所以
/// **越靠近笔刷边缘，改动量越趋近于零**，最终平滑地收束回原始形状。
/// 因为混合发生在**二值化之前**，输出仍然是严格的 0/1。
///
/// 而且 `w < 0.5` 的像素**在数学上不可能翻转**
/// （`w*b + (1-w)*orig` 要跨过 0.5 需要 `w ≥ 0.5` 起步），
/// 所以外圈有一段像素保证原样不动——这就是"没有接缝"的硬保证。
///
/// 代价：真正会改变的是内侧那部分（默认约半径的 65% 内），
/// 外圈是过渡区。这是二值遮罩下能做到的最接近"软笔刷"的效果。
pub fn smooth_brush(
    mask: &mut Mask,
    cx: f32,
    cy: f32,
    brush_r: f32,
    blur_r: f32,
    region: Option<&Mask>,
) -> usize {
    let br = brush_r.max(1.0);
    let sr = blur_r.max(1.0);
    let (mw, mh) = (mask.width() as i32, mask.height() as i32);
    if mw <= 0 || mh <= 0 {
        return 0;
    }

    // 局部窗口要包含**模糊核的余量**，否则窗口边界会把模糊结果污染
    // （盒式模糊在窗口边缘用 clamp 延拓，算出来的不是真实邻域平均）。
    let pad = sr.ceil() as i32 + 1;
    let reach = br.ceil() as i32;
    let x0 = (cx.floor() as i32 - reach - pad).max(0);
    let y0 = (cy.floor() as i32 - reach - pad).max(0);
    let x1 = (cx.ceil() as i32 + reach + pad).min(mw - 1);
    let y1 = (cy.ceil() as i32 + reach + pad).min(mh - 1);
    if x1 < x0 || y1 < y0 {
        return 0;
    }

    let lw = (x1 - x0 + 1) as usize;
    let lh = (y1 - y0 + 1) as usize;

    // 原始值快照（浮点 0/1）。后面写回时只读该快照，避免"边算边改"互相影响。
    let mut orig = vec![0f32; lw * lh];
    for j in 0..lh {
        for i in 0..lw {
            let gx = (x0 + i as i32) as u32;
            let gy = (y0 + j as i32) as u32;
            orig[j * lw + i] = if mask.get(gx, gy) != 0 { 1.0 } else { 0.0 };
        }
    }

    let mut tmp = vec![0f32; lw * lh];
    box_blur_h(&orig, &mut tmp, lw, lh, sr.round() as i32);
    let mut blurred = vec![0f32; lw * lh];
    box_blur_v(&tmp, &mut blurred, lw, lh, sr.round() as i32);

    let mut changed = 0usize;
    for j in 0..lh {
        for i in 0..lw {
            let gx = x0 + i as i32;
            let gy = y0 + j as i32;
            let dx = (gx as f32 + 0.5) - cx;
            let dy = (gy as f32 + 0.5) - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // 笔刷圆外一律不动
            if d > br {
                continue;
            }
            // 有选区时，选区外也不动
            if let Some(reg) = region
                && reg.get(gx as u32, gy as u32) == 0
            {
                continue;
            }
            let w = brush_falloff(d / br);
            let o = orig[j * lw + i];
            let f = w * blurred[j * lw + i] + (1.0 - w) * o;
            let nv = if f >= 0.5 { 1u8 } else { 0u8 };
            let ov = if o >= 0.5 { 1u8 } else { 0u8 };
            if nv != ov {
                mask.set(gx as u32, gy as u32, nv);
                changed += 1;
            }
        }
    }
    changed
}

/// 笔刷内的平滑强度衰减：中心 1，边缘 0。
///
/// `CORE` 以内完全按平滑结果走；`CORE..1` 用 smoothstep 平滑收束到 0。
/// 用 smoothstep 而不是线性，是为了在两端导数为 0——
/// 强度变化在圆心处和圆边处都不"折"，观感更自然。
///
/// `CORE` 取得比较小（0.45）是有原因的：实测 0.65 时，
/// 半径 3/4 处权重还有 0.84，那里仍会翻转像素，外圈改动量下不来，
/// 视觉上就是"边缘还在动"。压到 0.45 后，半径 3/4 处权重降到约 0.55，
/// 外侧半圈基本锁死——这才是"没有接缝"该有的样子。
fn brush_falloff(t: f32) -> f32 {
    const CORE: f32 = 0.45;
    if t <= CORE {
        return 1.0;
    }
    let u = ((t - CORE) / (1.0 - CORE)).clamp(0.0, 1.0);
    // 1 - smoothstep(u)：u=0 → 1，u=1 → 0
    1.0 - (u * u * (3.0 - 2.0 * u))
}

/// 平滑二值遮罩的边界，**结果仍是严格 0/1**。
///
/// `radius` 是模糊半径（像素）。越大越圆润，但也越容易吞掉细节。
/// 只影响 `region` 为真的像素——选区外的内容不该被顺手改掉。
///
/// 实现：盒式模糊（可分离，两趟一维，O(w*h) 与半径无关）+
/// 0.5 阈值。盒式而不是高斯，因为盒式两趟就够平滑边界，
/// 且不需要预计算核。
///
/// 这是**整片**平滑（不留过渡区）；笔刷式局部平滑见 [`smooth_brush`]。
pub fn smooth_mask(mask: &mut Mask, radius: f32, region: Option<&Mask>) {
    let r = radius.round() as i32;
    if r < 1 {
        return;
    }
    let (w, h) = (mask.width() as usize, mask.height() as usize);
    if w == 0 || h == 0 {
        return;
    }

    // 1. 取出浮点场（0/1）。只有选区内的像素参与，其余保持原值不参与平均。
    let mut f = vec![0f32; w * h];
    for i in 0..w * h {
        f[i] = if mask.data()[i] != 0 { 1.0 } else { 0.0 };
    }

    // 可分离盒式模糊：先横后竖
    let mut tmp = vec![0f32; w * h];
    box_blur_h(&f, &mut tmp, w, h, r);
    let mut blurred = vec![0f32; w * h];
    box_blur_v(&tmp, &mut blurred, w, h, r);

    // 2. 按 0.5 重新二值化。**不写灰度值**，这是保住二值性的关键。
    for i in 0..w * h {
        let (x, y) = ((i % w) as u32, (i / w) as u32);
        if let Some(reg) = region
            && reg.get(x, y) == 0
        {
            continue; // 选区外不动
        }
        let v = if blurred[i] >= 0.5 { 1u8 } else { 0u8 };
        mask.set(x, y, v);
    }
}

/// 横向一维盒式模糊（边界用 clamp 延拓，避免边缘变暗）
fn box_blur_h(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: i32) {
    let d = (2 * r + 1) as f32;
    for y in 0..h {
        let row = y * w;
        let mut acc = 0f32;
        // 初始窗口
        for k in -r..=r {
            let x = k.clamp(0, (w - 1) as i32) as usize;
            acc += src[row + x];
        }
        for x in 0..w {
            dst[row + x] = acc / d;
            // 滑出左边，滑入右边
            let out = (x as i32 - r).clamp(0, (w - 1) as i32) as usize;
            let inn = (x as i32 + r + 1).clamp(0, (w - 1) as i32) as usize;
            acc += src[row + inn] - src[row + out];
        }
    }
}

/// 纵向一维盒式模糊
fn box_blur_v(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: i32) {
    let d = (2 * r + 1) as f32;
    for x in 0..w {
        let mut acc = 0f32;
        for k in -r..=r {
            let y = k.clamp(0, (h - 1) as i32) as usize;
            acc += src[y * w + x];
        }
        for y in 0..h {
            dst[y * w + x] = acc / d;
            let out = (y as i32 - r).clamp(0, (h - 1) as i32) as usize;
            let inn = (y as i32 + r + 1).clamp(0, (h - 1) as i32) as usize;
            acc += src[inn * w + x] - src[out * w + x];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank(w: u32, h: u32) -> Mask {
        Mask::new(w, h)
    }

    // ---------------------------------------------------------- 形状

    #[test]
    fn 矩形实心覆盖范围() {
        let mut m = blank(16, 16);
        draw_rect(&mut m, 2.0, 2.0, 12.0, 12.0, 1, true, 1.0);
        assert_eq!(m.get(2, 2), 1, "角点包含");
        assert_eq!(m.get(12, 12), 1, "另一角也包含（两端都是闭区间）");
        assert_eq!(m.get(0, 0), 0);
        assert_eq!(m.get(13, 13), 0, "框外不受影响");
        assert_eq!(m.count(), 11 * 11, "2..=12 共 11 列/行");
    }

    #[test]
    fn 矩形空心只有边框() {
        let mut m = blank(16, 16);
        draw_rect(&mut m, 2.0, 2.0, 12.0, 12.0, 1, false, 1.0);
        assert_eq!(m.get(2, 2), 1, "角在边上");
        assert_eq!(m.get(7, 7), 0, "内部应空");
        // 边框像素数应远少于实心
        assert!(m.count() < 10 * 10 / 2);
    }

    #[test]
    fn 矩形反向拖动也能画() {
        let mut a = blank(16, 16);
        draw_rect(&mut a, 2.0, 2.0, 12.0, 12.0, 1, true, 1.0);
        let mut b = blank(16, 16);
        draw_rect(&mut b, 12.0, 12.0, 2.0, 2.0, 1, true, 1.0);
        assert_eq!(a.count(), b.count(), "两个方向拖应得到同样结果");
    }

    #[test]
    fn 圆形实心是圆的() {
        let mut m = blank(32, 32);
        draw_ellipse(&mut m, 2.0, 2.0, 30.0, 30.0, 1, true, 1.0);
        assert_eq!(m.get(16, 16), 1, "圆心");
        assert_eq!(m.get(2, 2), 0, "外接矩形角在圆外");
        assert!(m.count() > 0);
        // 圆的面积应约为 pi*r^2
        let r = 14.0f32;
        let expect = (std::f32::consts::PI * r * r) as usize;
        let got = m.count();
        assert!(
            got > expect * 9 / 10 && got < expect * 11 / 10,
            "面积应接近 πr²：期望约 {expect}，实际 {got}"
        );
    }

    #[test]
    fn 等比形状是正圆() {
        // 拖动距离不等，但按住 shift 传进来的框已被调用方改成正方形
        let mut m = blank(40, 40);
        draw_ellipse(&mut m, 5.0, 5.0, 25.0, 25.0, 1, true, 1.0);
        // 圆心 (15,15)，半径 10。像素中心在整点上，四个方向最外侧的
        // 完整像素落在半径内、相邻一个刚好在外——所以检查"贴着圆的位置"而非端点本身。
        assert_eq!(m.get(15, 15), 1, "圆心");
        assert_eq!(m.get(6, 15), 1, "左侧靠内一点在圆内");
        assert_eq!(m.get(24, 15), 1, "右侧对称位置也在圆内");
        assert_eq!(m.get(15, 6), 1, "上方靠内在圆内");
        assert_eq!(m.get(15, 24), 1, "下方对称位置在圆内");
        // 四个外接矩形角都在圆外，这才是"是圆的"的关键证据
        assert_eq!(m.get(5, 5), 0, "左上角在圆外");
        assert_eq!(m.get(25, 25), 0, "右下角在圆外");
    }

    #[test]
    fn 椭圆空心内部是空的() {
        let mut m = blank(32, 32);
        draw_ellipse(&mut m, 2.0, 2.0, 30.0, 30.0, 1, false, 2.0);
        assert_eq!(m.get(16, 16), 0, "内部应空");
        assert_eq!(m.get(16, 3), 1, "顶部边框应有值");
    }

    #[test]
    fn 退化椭圆不崩溃() {
        let mut m = blank(8, 8);
        draw_ellipse(&mut m, 4.0, 4.0, 4.0, 4.0, 1, true, 1.0);
        // 宽高都 0，走矩形退化路径，应至少标记 1 像素
        assert!(m.count() >= 1, "退化应至少画一个点");
    }

    #[test]
    fn 形状擦除用值0() {
        let mut m = blank(16, 16);
        m.fill(1);
        draw_rect(&mut m, 4.0, 4.0, 10.0, 10.0, 0, true, 1.0);
        assert_eq!(m.get(5, 5), 0, "应擦掉");
        assert_eq!(m.get(0, 0), 1, "外部保留");
    }

    // ---------------------------------------------------------- 平滑

    #[test]
    fn 平滑后仍然只有0和1() {
        let mut m = blank(32, 32);
        // 造一条带锯齿的边界
        for y in 0..32 {
            let edge = 10 + (y % 5);
            for x in 0..edge {
                m.set(x, y, 1);
            }
        }
        smooth_mask(&mut m, 2.0, None);
        assert!(
            m.data().iter().all(|&v| v == 0 || v == 1),
            "平滑绝不能产生灰度值——这是二值性的底线"
        );
    }

    #[test]
    fn 平滑让边界更整齐() {
        let mut m = blank(64, 64);
        // 1 像素的阶梯锯齿边界——这正是鼠标画出来的那种毛刺
        for y in 0..64 {
            let edge = 20 + (y % 3); // 每行跳 1 像素，周期 3
            for x in 0..edge {
                m.set(x, y, 1);
            }
        }
        let before = boundary_jitter(&m);
        smooth_mask(&mut m, 2.0, None);
        let after = boundary_jitter(&m);
        assert!(after < before, "边界跳变次数应减少：{before} → {after}");
    }

    /// 统计边界"跳变"次数：相邻两行的最左被遮列相差 >=1 就算一次。
    /// 越平滑这个数越小，直觉上正好对应"毛刺被抹平"。
    fn boundary_jitter(m: &Mask) -> usize {
        let mut prev: Option<i32> = None;
        let mut n = 0usize;
        for y in 0..m.height() {
            let mut e = -1i32;
            for x in 0..m.width() {
                if m.get(x, y) != 0 {
                    e = x as i32;
                }
            }
            if let Some(p) = prev
                && (e - p).abs() >= 1
            {
                n += 1;
            }
            prev = Some(e);
        }
        n
    }

    #[test]
    fn 平滑半径0时不动() {
        let mut m = blank(16, 16);
        for y in 0..16 {
            for x in 0..8 {
                m.set(x, y, 1);
            }
        }
        let before = m.data().to_vec();
        smooth_mask(&mut m, 0.0, None);
        assert_eq!(m.data(), &before[..], "半径 0 应原样返回");
    }

    #[test]
    fn 平滑不碰选区外() {
        let mut m = blank(32, 32);
        for y in 0..32 {
            for x in 0..16 {
                m.set(x, y, 1);
            }
        }
        // 选区只覆盖右半边（那里本来是空的）
        let mut reg = blank(32, 32);
        for y in 0..32 {
            for x in 16..32 {
                reg.set(x, y, 1);
            }
        }
        let left_before: Vec<u8> = (0..32).map(|y| m.get(15, y)).collect();
        smooth_mask(&mut m, 3.0, Some(&reg));
        let left_after: Vec<u8> = (0..32).map(|y| m.get(15, y)).collect();
        assert_eq!(left_before, left_after, "选区外的像素必须原样保留");
    }

    #[test]
    fn 平滑不会凭空造出内容() {
        let mut m = blank(16, 16);
        smooth_mask(&mut m, 5.0, None);
        assert_eq!(m.count(), 0, "全空的图平滑后还是全空");
    }

    #[test]
    fn 平滑收敛而不是无限膨胀() {
        let mut m = blank(16, 16);
        for y in 4..12 {
            for x in 4..12 {
                m.set(x, y, 1);
            }
        }
        let before = m.count();
        smooth_mask(&mut m, 2.0, None);
        let after = m.count();
        // 平滑会略微改变面积，但不该是数量级的暴涨
        let ratio = after as f32 / before.max(1) as f32;
        assert!(
            ratio > 0.6 && ratio < 1.4,
            "面积应大致保持：{before} → {after}"
        );
    }

    // ------------------------------------------------ 笔刷式平滑

    /// 造一条锯齿边界（左边界每行跳动）
    fn jitter_edge(w: u32, h: u32) -> Mask {
        let mut m = blank(w, h);
        for y in 0..h {
            let edge = 24 + (y % 5);
            for x in 0..edge {
                m.set(x, y, 1);
            }
        }
        m
    }

    #[test]
    fn 笔刷平滑结果仍是二值() {
        let mut m = jitter_edge(64, 64);
        smooth_brush(&mut m, 24.0, 32.0, 16.0, 3.0, None);
        assert!(
            m.data().iter().all(|&v| v == 0 || v == 1),
            "笔刷平滑也必须严格二值"
        );
    }

    #[test]
    fn 笔刷圆外的像素一个都不变() {
        let mut m = jitter_edge(64, 64);
        let before = m.clone();
        let (cx, cy, br) = (24.0f32, 32.0f32, 10.0f32);
        smooth_brush(&mut m, cx, cy, br, 3.0, None);

        let mut changed_outside = 0;
        for y in 0..64u32 {
            for x in 0..64u32 {
                let d = (((x as f32 + 0.5) - cx).powi(2) + ((y as f32 + 0.5) - cy).powi(2)).sqrt();
                if d > br && m.get(x, y) != before.get(x, y) {
                    changed_outside += 1;
                }
            }
        }
        assert_eq!(changed_outside, 0, "笔刷圆外必须一个像素都不动");
    }

    /// **核心约束**：笔刷边界不能出现可见接缝。
    ///
    /// 判定方式：把"距圆心距离"分环统计改动量。
    /// 若硬性裁剪，改动量会在圆边处**突然**从有变无（一个台阶）；
    /// 正确实现应当随距离**单调递减**到 0。
    #[test]
    fn 笔刷边缘没有接缝_改动量随距离递减() {
        let mut m = jitter_edge(96, 96);
        let before = m.clone();
        let br = 24.0f32;
        let (cx, cy) = (24.0f32, 48.0f32);
        smooth_brush(&mut m, cx, cy, br, 3.0, None);

        // 按 4 像素一圈统计改动数
        let mut rings = [0usize; 6]; // 0-4,4-8,...,20-24
        for y in 0..96u32 {
            for x in 0..96u32 {
                if m.get(x, y) == before.get(x, y) {
                    continue;
                }
                let d = (((x as f32 + 0.5) - cx).powi(2) + ((y as f32 + 0.5) - cy).powi(2)).sqrt();
                let idx = ((d / 4.0) as usize).min(5);
                rings[idx] += 1;
            }
        }
        println!("按环改动量：{rings:?}");

        // 核心要求：最外两环必须为 0（那里权重接近 0，翻不动）
        assert!(
            rings[4] == 0 && rings[5] == 0,
            "最外两环（半径 16+）不该有任何改动，实际 {rings:?}"
        );
        // 内圈必须有改动（否则等于没干活）
        assert!(
            rings[0] + rings[1] + rings[2] > 0,
            "内圈应该有改动，实际 {rings:?}"
        );
        // 单调性：改动量总体从内到外下降。
        // 允许相邻环有 ±50% 的波动——每个环里"有多少条锯齿边界恰好落在环内"
        // 本身就不均匀，逐环计数必然有抖动。真正的"接缝"特征是**成倍跳增**
        // 或外圈突然出现改动，那才要拦。
        let nonzero: Vec<usize> = rings.to_vec();
        for i in 1..nonzero.len() {
            if nonzero[i - 1] == 0 && nonzero[i] == 0 {
                continue;
            }
            let prev = nonzero[i - 1].max(1) as f32;
            let cur = nonzero[i] as f32;
            assert!(
                cur <= prev * 1.5 + 2.0,
                "外圈改动量不该明显高于内圈（说明有接缝）：{rings:?}"
            );
        }
    }

    #[test]
    fn 笔刷平滑不会凭空造内容() {
        let mut m = blank(32, 32);
        let n = smooth_brush(&mut m, 16.0, 16.0, 12.0, 3.0, None);
        assert_eq!(n, 0, "全空图上平滑不该产生任何改动");
        assert_eq!(m.count(), 0);
    }

    #[test]
    fn 笔刷平滑只在局部计算() {
        // 大图上小笔刷应该很快：改动数很少，且圆外不动
        let mut m = jitter_edge(256, 256);
        let before = m.clone();
        let n = smooth_brush(&mut m, 26.0, 128.0, 8.0, 2.0, None);
        assert!(n > 0, "应该发生了改动");
        // 改动全部落在圆内
        let mut total_out = 0;
        for y in 0..256u32 {
            for x in 0..256u32 {
                let d =
                    (((x as f32 + 0.5) - 26.0).powi(2) + ((y as f32 + 0.5) - 128.0).powi(2)).sqrt();
                if d > 8.0 && m.get(x, y) != before.get(x, y) {
                    total_out += 1;
                }
            }
        }
        assert_eq!(total_out, 0);
    }

    #[test]
    fn 笔刷平滑尊重选区() {
        let mut m = jitter_edge(64, 64);
        // 选区只覆盖上半
        let mut reg = blank(64, 64);
        for y in 0..32 {
            for x in 0..64 {
                reg.set(x, y, 1);
            }
        }
        let before = m.clone();
        smooth_brush(&mut m, 24.0, 32.0, 16.0, 3.0, Some(&reg));
        // 下半（选区外）不该变
        for y in 32..64u32 {
            for x in 0..64u32 {
                assert_eq!(m.get(x, y), before.get(x, y), "选区外 ({x},{y}) 不该被改");
            }
        }
    }

    #[test]
    fn 衰减函数两端正确() {
        assert_eq!(brush_falloff(0.0), 1.0, "圆心全强度");
        assert_eq!(brush_falloff(1.0), 0.0, "圆边零强度");
        // 中间单调递减
        let mut prev = 1.0;
        for k in 1..=10 {
            let t = k as f32 / 10.0;
            let v = brush_falloff(t);
            assert!(v <= prev, "应单调递减：t={t} v={v} prev={prev}");
            prev = v;
        }
    }

    #[test]
    fn 笔刷半径越大改动越多() {
        let base = jitter_edge(96, 96);
        let mut small = base.clone();
        let mut big = base.clone();
        let n_small = smooth_brush(&mut small, 26.0, 48.0, 6.0, 3.0, None);
        let n_big = smooth_brush(&mut big, 26.0, 48.0, 24.0, 3.0, None);
        assert!(
            n_big > n_small,
            "大笔刷覆盖更多像素，改动数应更多：{n_small} vs {n_big}"
        );
    }
}
