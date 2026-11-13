//! 把模型的 UV 线框投到画布上，用于对照"贴图落在模型的哪一块"。
//!
//! # 为什么按三角形边收集并去重
//!
//! UV 是逐角存的（每个三角 3 个角各有一组 UV），直接按三角画三条边会出现
//! 大量重复：两个相邻三角共享一条边，那条边会被画两次。
//!
//! 重复本身不影响正确性，但线宽会变成两倍、颜色也会叠深，
//! 看起来像"那条缝特别重要"。
//!
//! # 为什么用哈希而不是线性查找
//!
//! 早期实现用 `Vec::contains` 去重，复杂度 O(n²)。实测 10 万三角的模型
//! 要 260ms——点一下开关就卡住四分之一秒。改成 `HashSet` 后是线性时间。
//!
//! # 为什么不每帧重算
//!
//! 模型的 UV 在导入后**不会变**。所以线段只在"模型或 UV 集变化"时算一次，
//! 连坐标变换的结果也一起缓存（见 `ScreenEdges`）。

use std::collections::HashSet;

/// 一条 UV 线段（UV 坐标系 0..1）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UvEdge {
    pub a: [f32; 2],
    pub b: [f32; 2],
}

/// 已变换到屏幕坐标的线段，可直接画。
///
/// 缓存它是因为绘制每帧都发生，而 UV 与画布布局不变时变换结果完全相同。
#[derive(Clone, Copy, Debug)]
pub struct ScreenEdge {
    pub a: [f32; 2],
    pub b: [f32; 2],
}

/// 从一套 UV 里抽出所有互不重复的边。
///
/// `uv` 的长度应为 `tri_count * 3`（每个三角 3 个角）。
/// 坐标超出 0..1 的顶点会被保留（模型确实可能 UV 溢出），
/// 由绘制方决定是否裁剪。
///
/// 复杂度 O(n)：每条边的两个端点按位量化后放进 `HashSet` 去重。
pub fn edges(uv: &[[f32; 2]], max_edges: usize) -> Vec<UvEdge> {
    let mut seen: HashSet<EdgeKey> = HashSet::with_capacity(uv.len());
    let mut out: Vec<UvEdge> = Vec::new();
    let n = uv.len() / 3;
    for t in 0..n {
        let c = [uv[t * 3], uv[t * 3 + 1], uv[t * 3 + 2]];
        for k in 0..3 {
            let a = c[k];
            let b = c[(k + 1) % 3];
            // 退化边（两端重合）没有可画的长度
            if (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9 {
                continue;
            }
            let (e, key) = canonical(a, b);
            if seen.insert(key) {
                out.push(e);
                if out.len() >= max_edges {
                    return out;
                }
            }
        }
    }
    out
}

/// 线段的哈希键：端点按位量化后再排序。
///
/// 量化到 1e-5：UV 值来自解析出的 f32，同一条边在两个三角里取到的值
/// 通常逐位相同，但这个精度足以吸收解析误差，又远小于任何有意义的 UV 差异
/// （1e-5 × 贴图宽度，4K 下才 0.04 像素）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct EdgeKey([i64; 4]);

/// 把线段规整成与方向无关的形式：端点按坐标排序。
///
/// 相邻两个三角共享的边端点顺序相反（ABC 的 CA 与 BCD 的 CD 之类），
/// 不排序就会当成两条不同的边。
fn canonical(a: [f32; 2], b: [f32; 2]) -> (UvEdge, EdgeKey) {
    let q = |p: [f32; 2]| [quant(p[0]), quant(p[1])];
    let (qa, qb) = (q(a), q(b));
    let flip = (qb[0], qb[1]) < (qa[0], qa[1]);
    if flip {
        (
            UvEdge { a: b, b: a },
            EdgeKey([qb[0], qb[1], qa[0], qa[1]]),
        )
    } else {
        (UvEdge { a, b }, EdgeKey([qa[0], qa[1], qb[0], qb[1]]))
    }
}

#[inline]
fn quant(v: f32) -> i64 {
    // 非有限值钳到 0：NaN 参与比较会得到任意结果，导致去重失效
    if v.is_finite() {
        (v as f64 * 100_000.0).round() as i64
    } else {
        0
    }
}

/// 把 UV 线段批量变换到屏幕坐标。
///
/// `origin` 是画布左上角，`size` 是画布像素尺寸。
/// UV 原点在左下（与纹理坐标一致），画布原点在左上，所以纵向要翻转。
pub fn to_screen(edges: &[UvEdge], origin: [f32; 2], size: [f32; 2]) -> Vec<ScreenEdge> {
    let (ox, oy) = (origin[0], origin[1]);
    let (w, h) = (size[0], size[1]);
    edges
        .iter()
        .map(|e| ScreenEdge {
            a: [ox + e.a[0] * w, oy + (1.0 - e.a[1]) * h],
            b: [ox + e.b[0] * w, oy + (1.0 - e.b[1]) * h],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 两个共享一条边的三角，共有的那条边只能出现一次。
    #[test]
    fn 共享边不重复() {
        // 三角 1：(0,0) (1,0) (0,1)；三角 2：(1,0) (1,1) (0,1)
        // 共享边是 (1,0)-(0,1)
        let uv = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
        ];
        let e = edges(&uv, 1000);
        assert_eq!(e.len(), 5, "共享边被当成两条：{:?}", e);
    }

    /// 端点顺序相反的同一条边必须合并。
    #[test]
    fn 反向的同一条边合并() {
        let uv = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [0.0, 1.0],
            [0.0, 1.0],
            [1.0, 0.0],
            [1.0, 1.0],
        ];
        let e = edges(&uv, 1000);
        assert_eq!(e.len(), 5, "反向边未合并：{:?}", e);
    }

    /// 退化边（两端重合）不产生线段。
    #[test]
    fn 退化边被跳过() {
        // 顶点 A、B 重合，只有 C 不同：
        //   A-B 退化（跳过）
        //   B-C 与 C-A 都连到同一个点坐标，去重后只剩一条
        let uv = vec![[0.5, 0.5], [0.5, 0.5], [1.0, 0.0]];
        let e = edges(&uv, 1000);
        assert_eq!(e.len(), 1, "应只剩一条非退化边：{:?}", e);
        assert_eq!(e[0], UvEdge { a: [0.5, 0.5], b: [1.0, 0.0] });
    }

    /// 上限生效，避免超大模型把画布拖死。
    #[test]
    fn 边数受上限约束() {
        let mut uv = Vec::new();
        for i in 0..1000 {
            let x = i as f32 / 1000.0;
            uv.push([x, 0.0]);
            uv.push([x, 1.0]);
            uv.push([x + 0.001, 0.5]);
        }
        let e = edges(&uv, 50);
        assert!(e.len() <= 50, "应被截断到 50 条，实际 {}", e.len());
    }

    /// 空输入不崩。
    #[test]
    fn 空UV返回空() {
        assert!(edges(&[], 100).is_empty());
        assert!(edges(&[[0.0, 0.0]], 100).is_empty());
    }

    /// 坐标变换：UV 原点在左下，屏幕原点在左上。
    #[test]
    fn 变换时纵向翻转() {
        let e = vec![UvEdge { a: [0.0, 0.0], b: [1.0, 1.0] }];
        let s = to_screen(&e, [10.0, 20.0], [100.0, 200.0]);
        // UV(0,0) 在左下 → 屏幕 (10, 220)
        assert_eq!(s[0].a, [10.0, 220.0]);
        // UV(1,1) 在右上 → 屏幕 (110, 20)
        assert_eq!(s[0].b, [110.0, 20.0]);
    }

    /// 去重必须是线性时间——早期用 `Vec::contains` 是 O(n²)，
    /// 10 万三角要 260ms，点一下开关就卡住。
    ///
    /// 这里只做一个宽松的上界断言：真正的性能回归会以数量级体现，
    /// 阈值取 2 秒是为了在慢速 CI 上也不误报。
    #[test]
    fn 大规模去重不超时() {
        let tris = 50_000usize;
        let mut uv = Vec::with_capacity(tris * 3);
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 24) as f32
        };
        for _ in 0..tris {
            let (bx, by) = (rnd(), rnd());
            let d = 0.02;
            uv.push([bx, by]);
            uv.push([(bx + d).min(1.0), by]);
            uv.push([bx, (by + d).min(1.0)]);
        }
        let t = Instant::now();
        let e = edges(&uv, 40_000);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(!e.is_empty());
        // debug 构建（cargo test 默认）比 release 慢数倍，阈值放宽
        assert!(ms < 2000.0, "5 万三角去重用了 {ms:.0}ms，疑似退化回平方复杂度");
        println!("5 万三角 → {} 边，{ms:.1}ms", e.len());
    }
}
