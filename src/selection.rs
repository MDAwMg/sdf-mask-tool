//! 选区：文档级的二值掩码 + 蚂蚁线所需的边缘提取。
//!
//! 选区不是图层的一部分，也不是参数点的一部分——选区是**编辑期的临时状态**，
//! 决定"哪些像素允许被改动"。所以选区不进工程存盘，也不进撤销快照。
//!
//! 内部用 `Option<Mask>` 而不是 `Mask + 空标志`：
//! "有没有选区"是一个明确的二值状态，用 Option 让编译器帮忙强制判断。
//!
//! ## 为什么蚂蚁线要自己算边缘
//!
//! 选区是一张二值图，直接画会得到一块实色区域。PS 里那种流动虚线
//! 只画在**边界**上，所以要从二值图里提取出边界线段。
//! 提取规则：某个被选中的像素，其上/下/左/右邻居里只要有一个没被选中，
//! 那一条边就属于边界。

use crate::mask::Mask;

/// 一条边缘线段（画布像素坐标）
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

#[derive(Clone, Default)]
pub struct Selection {
    inner: Option<Mask>,
}

impl Selection {
    pub fn none() -> Self {
        Self { inner: None }
    }

    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }

    pub fn mask(&self) -> Option<&Mask> {
        self.inner.as_ref()
    }

    pub fn count(&self) -> usize {
        self.inner.as_ref().map(|m| m.count()).unwrap_or(0)
    }

    /// 用一张新掩码替换选区。全空等价于"没有选区"，顺手归一化成 None。
    pub fn replace(&mut self, m: Mask) {
        self.inner = if m.any() { Some(m) } else { None };
    }

    pub fn clear(&mut self) {
        self.inner = None;
    }

    pub fn select_all(&mut self, w: u32, h: u32) {
        let mut m = Mask::new(w, h);
        m.fill(1);
        self.inner = Some(m);
    }

    /// 反相选区。没有任何选区时，PS 的行为是"全选"。
    pub fn invert(&mut self, w: u32, h: u32) {
        match &mut self.inner {
            Some(m) => m.invert(),
            None => self.select_all(w, h),
        }
        // 反相后如果全空（原来全选），归一化成无选区
        if self.inner.as_ref().is_some_and(|m| !m.any()) {
            self.inner = None;
        }
    }

    /// 平移选区（移动选区功能）
    pub fn translate(&mut self, dx: i32, dy: i32) {
        if let Some(m) = &self.inner {
            self.inner = Some(shift_mask(m, dx, dy));
        }
    }

    /// 矩形选区。两个角可以对角给出，自动排序。
    pub fn from_rect(w: u32, h: u32, ax: f32, ay: f32, bx: f32, by: f32) -> Mask {
        let mut m = Mask::new(w, h);
        let x0 = ax.min(bx).round().max(0.0) as u32;
        let y0 = ay.min(by).round().max(0.0) as u32;
        let x1 = (ax.max(bx).round().max(0.0) as u32).min(w);
        let y1 = (ay.max(by).round().max(0.0) as u32).min(h);
        for y in y0..y1 {
            for x in x0..x1 {
                m.set(x, y, 1);
            }
        }
        m
    }

    /// 套索选区：把自由曲线围成的多边形填成掩码（奇偶规则扫描线填充）。
    //
    /// 用奇偶规则而不是非零绕数：套索是随手画的，可能自我交叉；
    /// 奇偶规则下交叉处会形成"洞"，这正是画笔软件里套索的观感。
    pub fn from_polygon(w: u32, h: u32, pts: &[(f32, f32)]) -> Mask {
        let mut m = Mask::new(w, h);
        if pts.len() < 3 {
            return m;
        }
        let mut xs: Vec<f32> = Vec::new();
        for y in 0..h {
            // 取像素中心做扫描线，避免和顶点重合导致 ±1 的误差
            let sy = y as f32 + 0.5;
            xs.clear();
            let n = pts.len();
            for i in 0..n {
                let (x0, y0) = pts[i];
                let (x1, y1) = pts[(i + 1) % n];
                // 半开区间 [y0, y1) 判定，保证顶点只算一次
                if (y0 <= sy && sy < y1) || (y1 <= sy && sy < y0) {
                    let t = (sy - y0) / (y1 - y0);
                    xs.push(x0 + (x1 - x0) * t);
                }
            }
            if xs.is_empty() {
                continue;
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            for pair in xs.chunks(2) {
                if pair.len() < 2 {
                    break;
                }
                let a = pair[0].max(0.0);
                let b = pair[1].min(w as f32);
                let xa = a.round() as u32;
                let xb = b.round().max(0.0) as u32;
                for x in xa..xb.min(w) {
                    m.set(x, y, 1);
                }
            }
        }
        m
    }

    /// 选区的包围盒 (x0, y0, x1, y1)，右开区间。没有选区返回 None。
    pub fn bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let m = self.inner.as_ref()?;
        let (w, h) = (m.width(), m.height());
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                if m.get(x, y) != 0 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        if x0 == u32::MAX {
            None
        } else {
            Some((x0, y0, x1, y1))
        }
    }

    /// 提取蚂蚁线所需的边界线段。
    //
    /// 只统计"自己是选中、邻居没选中"的那条边，所以每条边界只出现一次，
    /// 不会像画轮廓那样重复描两遍。
    pub fn edges(&self) -> Vec<Edge> {
        let Some(m) = &self.inner else {
            return Vec::new();
        };
        let (w, h) = (m.width(), m.height());
        let mut out = Vec::new();
        for y in 0..h {
            for x in 0..w {
                if m.get(x, y) == 0 {
                    continue;
                }
                let (fx, fy) = (x as f32, y as f32);
                // 上边：上方没选中
                if y == 0 || m.get(x, y - 1) == 0 {
                    out.push(Edge {
                        x0: fx,
                        y0: fy,
                        x1: fx + 1.0,
                        y1: fy,
                    });
                }
                // 下边
                if y + 1 >= h || m.get(x, y + 1) == 0 {
                    out.push(Edge {
                        x0: fx,
                        y0: fy + 1.0,
                        x1: fx + 1.0,
                        y1: fy + 1.0,
                    });
                }
                // 左边
                if x == 0 || m.get(x - 1, y) == 0 {
                    out.push(Edge {
                        x0: fx,
                        y0: fy,
                        x1: fx,
                        y1: fy + 1.0,
                    });
                }
                // 右边
                if x + 1 >= w || m.get(x + 1, y) == 0 {
                    out.push(Edge {
                        x0: fx + 1.0,
                        y0: fy,
                        x1: fx + 1.0,
                        y1: fy + 1.0,
                    });
                }
            }
        }
        out
    }
}

/// 平移一张二值遮罩，越界部分丢弃。
pub fn shift_mask(m: &Mask, dx: i32, dy: i32) -> Mask {
    let (w, h) = (m.width() as i32, m.height() as i32);
    let mut out = Mask::new(m.width(), m.height());
    for y in 0..h {
        for x in 0..w {
            if m.get(x as u32, y as u32) == 0 {
                continue;
            }
            let nx = x + dx;
            let ny = y + dy;
            if nx >= 0 && ny >= 0 && nx < w && ny < h {
                out.set(nx as u32, ny as u32, 1);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect_filled(w: u32, h: u32, cols: u32) -> Mask {
        let mut m = Mask::new(w, h);
        for y in 0..h {
            for x in 0..cols.min(w) {
                m.set(x, y, 1);
            }
        }
        m
    }

    #[test]
    fn 空选区不算激活() {
        let s = Selection::none();
        assert!(!s.is_active());
        assert_eq!(s.count(), 0);
        assert!(s.bounds().is_none());
        assert!(s.edges().is_empty());
    }

    #[test]
    fn 全空掩码归一化成无选区() {
        let mut s = Selection::none();
        s.replace(Mask::new(8, 8));
        assert!(!s.is_active(), "全空的掩码不该算有选区");
    }

    #[test]
    fn 矩形选区覆盖指定范围() {
        let m = Selection::from_rect(16, 16, 4.0, 4.0, 10.0, 10.0);
        assert_eq!(m.count(), 6 * 6, "4..10 是 6×6");
        assert_eq!(m.get(4, 4), 1);
        assert_eq!(m.get(9, 9), 1);
        assert_eq!(m.get(10, 10), 0, "右边界是开的");
    }

    #[test]
    fn 矩形选区自动对角排序() {
        // 从右下往左上拖
        let m = Selection::from_rect(16, 16, 12.0, 12.0, 6.0, 6.0);
        assert_eq!(m.count(), 6 * 6);
        assert_eq!(m.get(6, 6), 1);
    }

    #[test]
    fn 全选和反相() {
        let mut s = Selection::none();
        s.select_all(8, 8);
        assert_eq!(s.count(), 64);
        s.invert(8, 8);
        assert!(!s.is_active(), "全选反相后应是无选区");
    }

    #[test]
    fn 无选区时反相等于全选() {
        let mut s = Selection::none();
        s.invert(8, 8);
        assert_eq!(s.count(), 64);
    }

    #[test]
    fn 套索多边形填充成掩码() {
        // 三角形
        let pts = [(2.0, 2.0), (14.0, 2.0), (8.0, 14.0)];
        let m = Selection::from_polygon(16, 16, &pts);
        assert!(m.count() > 0);
        assert_eq!(m.get(8, 6), 1, "三角形内部应被选中");
        assert_eq!(m.get(0, 0), 0, "外部不该选中");
        assert_eq!(m.get(8, 15), 0, "底尖之下不该选中");
    }

    #[test]
    fn 顶点不足三个的套索为空() {
        let m = Selection::from_polygon(8, 8, &[(1.0, 1.0), (4.0, 4.0)]);
        assert_eq!(m.count(), 0);
    }

    #[test]
    fn 平移选区越界部分被裁掉() {
        let mut s = Selection::none();
        s.replace(rect_filled(8, 8, 3));
        let before = s.count();
        s.translate(2, 0);
        assert_eq!(s.count(), before, "没撞到边界，数量不变");
        s.translate(10, 0);
        assert_eq!(s.count(), 0, "整体移出画布应清空");
    }

    #[test]
    fn 包围盒正确() {
        let mut s = Selection::none();
        s.replace(rect_filled(16, 16, 5));
        assert_eq!(s.bounds(), Some((0, 0, 5, 16)));
    }

    #[test]
    fn 实心矩形的边界只有一圈() {
        // 4×4 实心，边界线段数应为 4 条边各 4 段 = 16
        let mut s = Selection::none();
        let mut m = Mask::new(8, 8);
        for y in 2..6 {
            for x in 2..6 {
                m.set(x, y, 1);
            }
        }
        s.replace(m);
        assert_eq!(s.edges().len(), 16, "实心块只有外圈 16 条边");
    }

    #[test]
    fn 全选时边界贴着画布四边() {
        let mut s = Selection::none();
        s.select_all(4, 4);
        // 4×4 全选：上 4 + 下 4 + 左 4 + 右 4 = 16
        assert_eq!(s.edges().len(), 16);
    }

    #[test]
    fn 平移遮罩内容() {
        let m = rect_filled(8, 8, 2);
        let out = shift_mask(&m, 3, 0);
        assert_eq!(out.get(0, 0), 0, "原位置应空");
        assert_eq!(out.get(3, 0), 1, "应移到 x=3");
        assert_eq!(out.count(), m.count());
    }
}
