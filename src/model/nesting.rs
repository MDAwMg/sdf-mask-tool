//! 嵌套检测与修复。
//!
//! ## 问题
//!
//! 单通道 SDF 只能表达**单调**的时间轴：阴影要么一直变大（扩张），要么一直变小（收缩）。
//! 若某帧的遮罩不是前一帧的超集/子集，那块区域就"没有地基"，贴图表达不了。
//!
//! 典型症状是形状**平移**：左上角的大块在下一帧跑到右下角，
//! 两个区域互不包含，冲突像素就来自"消失的那半"和"新出现的那半"。
//!
//! ## 修复
//!
//! 把整条时间轴**层层嵌套化**：
//! - 扩张方向：`第 k 帧 = 第 0..k 帧的并集`（后面的帧只会更大）
//! - 收缩方向：`第 k 帧 = 第 k..n 帧的交集`（后面的帧只会更小）
//!
//! 两个方向各有代价（一个只会"加"、一个只会"减"），所以先估各自要改多少像素，
//! 选改动少的那个。修复前的预计影响会显示在界面上。

use crate::mask::Mask;

/// 阴影随时间的变化方向（和 `sdf::Direction` 语义相同，这里独立定义避免循环依赖）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NestDir {
    /// 阴影随角度增大而扩张：后一帧必须是前一帧的超集
    Grows,
    /// 阴影随角度增大而收缩：后一帧必须是前一帧的子集
    Shrinks,
}

impl NestDir {
    pub fn label(self) -> &'static str {
        match self {
            NestDir::Grows => "阴影扩张",
            NestDir::Shrinks => "阴影收缩",
        }
    }
}

/// 违规像素的分布：一张"哪里没有嵌套"的标记图。
#[derive(Clone)]
pub struct Violations {
    /// w*h，true = 该像素破坏了嵌套
    pub bad: Vec<bool>,
    /// 违规像素总数
    pub count: usize,
    /// 总像素数
    pub total: usize,
}

impl Violations {
    pub fn ratio(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.count as f32 / self.total as f32
        }
    }

    pub fn is_clean(&self) -> bool {
        self.count == 0
    }
}

/// 检测：按 `dir` 这个方向，哪些像素破坏了嵌套。
///
/// "破坏"的定义：
/// - `Grows` 下，某像素在帧 k 是受光、在帧 k+1 却被遮 → 该像素在帧 k 的状态
///   与"扩张"矛盾（该像素本该在更晚才转暗，或一直受光）
/// - `Shrinks` 下，某像素在帧 k 被遮、在帧 k+1 却受光，但之后又变回被遮 → 来回翻
///
/// 实际上更直接的判定是**单调性**：某像素的明暗序列若出现本方向不允许的翻转，就是违规。
pub fn detect(masks: &[&Mask], dir: NestDir, w: usize, h: usize) -> Violations {
    let n = w * h;
    let mut bad = vec![false; n];

    if masks.len() < 2 {
        return Violations {
            bad,
            count: 0,
            total: n,
        };
    }

    for i in 0..n {
        let mut prev = masks[0].data()[i] != 0;
        for m in masks.iter().skip(1) {
            let now = m.data()[i] != 0;
            let illegal = match dir {
                // 扩张方向不允许"遮了又亮"
                NestDir::Grows => prev && !now,
                // 收缩方向不允许"亮了又遮"
                NestDir::Shrinks => !prev && now,
            };
            if illegal {
                bad[i] = true;
                break;
            }
            prev = now;
        }
    }

    let count = bad.iter().filter(|b| **b).count();
    Violations {
        bad,
        count,
        total: n,
    }
}

/// 修复：把帧改成严格嵌套。
///
/// - `Grows`：**从前往后累积并集**，`第 k 帧 = 第 0..=k 帧的并集`。
///   这样每帧都是前一帧的超集，后面的帧只会越来越大。
/// - `Shrinks`：**从前往后累积交集**，`第 k 帧 = 第 0..=k 帧的交集`。
///   这样每帧都是前一帧的子集，后面的帧只会越来越小。
///
/// 两个方向都必须在**前向**传播：因为约束是"后一帧包含/被包含于前一帧"，
/// 即 `m[k+1] ⊇ m[k]` 或 `m[k+1] ⊆ m[k]`，只有沿时间顺序累积才成立。
/// （早期版本把收缩写成从后往前，结果最后一帧反而比前面大，仍然违规。）
///
/// 返回被改动的像素总数（供界面提示"这次改了 X 个像素"）。
pub fn repair(masks: &mut [Mask], dir: NestDir, w: usize, h: usize) -> usize {
    if masks.len() < 2 {
        return 0;
    }
    let mut changed = 0usize;

    for k in 1..masks.len() {
        // 先取出前一帧的累积结果，避免同时借用两帧
        let prev: Vec<u8> = masks[k - 1].data().to_vec();
        for i in 0..w * h {
            let cur = masks[k].data()[i];
            match dir {
                // 扩张：前一帧有、本帧没有 → 本帧补上
                NestDir::Grows => {
                    if prev[i] != 0 && cur == 0 {
                        masks[k].set((i % w) as u32, (i / w) as u32, 1);
                        changed += 1;
                    }
                }
                // 收缩：前一帧没有、本帧有 → 本帧挖掉
                NestDir::Shrinks => {
                    if prev[i] == 0 && cur != 0 {
                        masks[k].set((i % w) as u32, (i / w) as u32, 0);
                        changed += 1;
                    }
                }
            }
        }
    }

    changed
}

/// 两个方向各自要改多少像素，用于"选改动少的"。
pub struct RepairPlan {
    pub dir: NestDir,
    pub change_count: usize,
    pub violations: Violations,
}

/// 估算两个方向的修复代价，挑改动少的。
///
/// 注意这里**不真的改数据**，只在计数：复制一份帧来试算代价太高，
/// 直接用帧间差累加即可——
/// - 扩张方向要改的像素数 = 各相邻对里"前一帧有、后一帧没有"的像素之和
/// - 收缩方向要改的像素数 = 各相邻对里"前一帧没有、后一帧有"的像素之和
pub fn plan(masks: &[&Mask], w: usize, h: usize) -> RepairPlan {
    let n = w * h;
    let mut grow_fix = 0usize;
    let mut shrink_fix = 0usize;

    for k in 0..masks.len().saturating_sub(1) {
        let a = masks[k].data();
        let b = masks[k + 1].data();
        for i in 0..n {
            let ma = a[i] != 0;
            let mb = b[i] != 0;
            // 扩张：a 有 b 没有 → b 要补上
            if ma && !mb {
                grow_fix += 1;
            }
            // 收缩：a 没有 b 有 → a 要挖掉
            if !ma && mb {
                shrink_fix += 1;
            }
        }
    }

    // 选改动少的；平手时选扩张（引擎默认语义）
    let dir = if shrink_fix < grow_fix {
        NestDir::Shrinks
    } else {
        NestDir::Grows
    };
    let violations = detect(masks, dir, w, h);
    RepairPlan {
        dir,
        change_count: grow_fix.min(shrink_fix),
        violations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(w: u32, h: u32, dark_cols: u32) -> Mask {
        let mut m = Mask::new(w, h);
        for y in 0..h {
            for x in 0..dark_cols.min(w) {
                m.set(x, y, 1);
            }
        }
        m
    }

    #[test]
    fn 单调扩张不算违规() {
        let (w, h) = (16, 1);
        let a = mk(w, h, 4);
        let b = mk(w, h, 8);
        let v = detect(&[&a, &b], NestDir::Grows, w as usize, h as usize);
        assert!(v.is_clean(), "4列→8列 是合法扩张");
    }

    #[test]
    fn 收缩算违规() {
        let (w, h) = (16, 1);
        let a = mk(w, h, 8);
        let b = mk(w, h, 4);
        let v = detect(&[&a, &b], NestDir::Grows, w as usize, h as usize);
        assert_eq!(v.count, 4, "8列→4列 在扩张方向下有 4 个像素违规");
    }

    #[test]
    fn 收缩方向下相反() {
        let (w, h) = (16, 1);
        let a = mk(w, h, 8);
        let b = mk(w, h, 4);
        let v = detect(&[&a, &b], NestDir::Shrinks, w as usize, h as usize);
        assert!(v.is_clean(), "8列→4列 是合法收缩");
    }

    #[test]
    fn 平移形状两个方向都违规() {
        let (w, h) = (32, 1);
        // 左半 → 右半：既不包含也不被包含
        let mut a = Mask::new(w, h);
        for x in 0..16 {
            a.set(x, 0, 1);
        }
        let mut b = Mask::new(w, h);
        for x in 16..32 {
            b.set(x, 0, 1);
        }
        let vg = detect(&[&a, &b], NestDir::Grows, w as usize, h as usize);
        let vs = detect(&[&a, &b], NestDir::Shrinks, w as usize, h as usize);
        assert!(vg.count > 0 && vs.count > 0, "平移两个方向都有违规");
        assert_eq!(vg.count, 16);
        assert_eq!(vs.count, 16);
    }

    #[test]
    fn 修复后扩张方向必定干净() {
        let (w, h) = (16, 1);
        // 8 → 4 → 12（列数）
        let mut frames = vec![mk(w, h, 8), mk(w, h, 4), mk(w, h, 12)];
        let changed = repair(&mut frames, NestDir::Grows, w as usize, h as usize);
        let refs: Vec<&Mask> = frames.iter().collect();
        let v = detect(&refs, NestDir::Grows, w as usize, h as usize);
        assert!(v.is_clean(), "修复后必须无违规");
        // 前向累积并集：8 → 8 → 12
        assert_eq!(frames[0].count() as u32, 8);
        assert_eq!(frames[1].count() as u32, 8, "第1帧吸收第0帧后从4变8");
        assert_eq!(frames[2].count() as u32, 12, "第2帧本来就是12，无需改");
        assert_eq!(changed, 4, "只有第1帧补了 4 个像素");
    }

    #[test]
    fn 修复后收缩方向必定干净() {
        let (w, h) = (16, 1);
        let mut frames = vec![mk(w, h, 8), mk(w, h, 4), mk(w, h, 12)];
        repair(&mut frames, NestDir::Shrinks, w as usize, h as usize);
        let refs: Vec<&Mask> = frames.iter().collect();
        let v = detect(&refs, NestDir::Shrinks, w as usize, h as usize);
        assert!(v.is_clean(), "修复后必须无违规");
        // 前向累积交集：8 → min(8,4)=4 → min(4,12)=4
        assert_eq!(frames[0].count() as u32, 8, "第0帧不受影响");
        assert_eq!(frames[1].count() as u32, 4, "第1帧本就是4");
        assert_eq!(frames[2].count() as u32, 4, "第2帧从12被挖成4");
    }

    #[test]
    fn 选改动少的方向() {
        let (w, h) = (16, 1);
        // 8 → 6：收缩代价 0（本来就收缩），扩张代价 2
        let a = mk(w, h, 8);
        let b = mk(w, h, 6);
        let p = plan(&[&a, &b], w as usize, h as usize);
        assert_eq!(p.dir, NestDir::Shrinks, "应选收缩（改动更少）");
    }

    #[test]
    fn 单帧不算违规() {
        let (w, h) = (8, 8);
        let a = mk(w, h, 3);
        let v = detect(&[&a], NestDir::Grows, w as usize, h as usize);
        assert!(v.is_clean());
    }

    #[test]
    fn 空列表不崩溃() {
        let v = detect(&[], NestDir::Grows, 4, 4);
        assert_eq!(v.total, 16);
        assert!(v.is_clean());
    }
}
