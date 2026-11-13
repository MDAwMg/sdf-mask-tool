//! 二值笔刷：硬边圆形，直接置位/清零，不做软过渡。
//!
//! 硬边是刻意的：遮罩只表示"这块区域有没有被遮"，
//! 软边既会让数据变成灰度、又要在每次涂抹时做混合，得不偿失。

use crate::mask::Mask;

/// 在遮罩上打一个实心圆。
pub fn stamp(mask: &mut Mask, cx: f32, cy: f32, r: f32, erase: bool) {
    if r <= 0.0 {
        return;
    }
    let v = if erase { 0u8 } else { 1u8 };
    let r2 = r * r;
    let x0 = (cx - r).floor().max(0.0) as u32;
    let x1 = ((cx + r).ceil() as i64).clamp(0, mask.width() as i64) as u32;
    let y0 = (cy - r).floor().max(0.0) as u32;
    let y1 = ((cy + r).ceil() as i64).clamp(0, mask.height() as i64) as u32;

    for y in y0..y1 {
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                mask.set(x, y, v);
            }
        }
    }
}

/// 两点之间连线补笔，避免快速拖动时断成一串点。
pub fn stroke(mask: &mut Mask, from: (f32, f32), to: (f32, f32), r: f32, erase: bool) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let dist = (dx * dx + dy * dy).sqrt();
    // 步长取半径的一半，既保证连续又不过度重复
    let step = (r * 0.5).max(1.0);
    let n = (dist / step).ceil() as i32;
    if n <= 0 {
        stamp(mask, to.0, to.1, r, erase);
        return;
    }
    for i in 1..=n {
        let t = i as f32 / n as f32;
        stamp(mask, from.0 + dx * t, from.1 + dy * t, r, erase);
    }
}

/// 扫描线种子填充：翻转与起点连通的同值区域。
pub fn flood_fill(mask: &mut Mask, sx: u32, sy: u32, value: u8) {
    let (w, h) = (mask.width() as i32, mask.height() as i32);
    if sx as i32 >= w || sy as i32 >= h {
        return;
    }
    let target = mask.get(sx, sy);
    if target == value {
        return;
    }

    let mut stack = vec![(sx as i32, sy as i32)];
    while let Some((x, y)) = stack.pop() {
        if x < 0 || y < 0 || x >= w || y >= h {
            continue;
        }
        if mask.get(x as u32, y as u32) != target {
            continue;
        }

        let mut left = x;
        while left > 0 && mask.get((left - 1) as u32, y as u32) == target {
            left -= 1;
        }
        let mut right = x;
        while right < w - 1 && mask.get((right + 1) as u32, y as u32) == target {
            right += 1;
        }

        for xi in left..=right {
            mask.set(xi as u32, y as u32, value);
        }

        for dy in [-1, 1] {
            let ny = y + dy;
            if ny < 0 || ny >= h {
                continue;
            }
            let mut xi = left;
            while xi <= right {
                if mask.get(xi as u32, ny as u32) == target {
                    stack.push((xi, ny));
                    while xi <= right && mask.get(xi as u32, ny as u32) == target {
                        xi += 1;
                    }
                } else {
                    xi += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 笔刷中心被置位() {
        let mut m = Mask::new(32, 32);
        stamp(&mut m, 16.0, 16.0, 8.0, false);
        assert_eq!(m.get(16, 16), 1);
    }

    #[test]
    fn 笔刷半径外不被置位() {
        let mut m = Mask::new(32, 32);
        stamp(&mut m, 16.0, 16.0, 8.0, false);
        assert_eq!(m.get(26, 16), 0, "半径外应保持 0");
        assert_eq!(m.get(0, 0), 0);
    }

    #[test]
    fn 笔刷是实心的() {
        let mut m = Mask::new(32, 32);
        stamp(&mut m, 16.0, 16.0, 8.0, false);
        // 圆内的点都该是 1，不能出现中间空
        for y in 12..20u32 {
            for x in 12..20u32 {
                assert_eq!(m.get(x, y), 1, "({x},{y}) 应为实心");
            }
        }
    }

    #[test]
    fn 橡皮清回零() {
        let mut m = Mask::new(32, 32);
        m.fill(1);
        stamp(&mut m, 16.0, 16.0, 8.0, true);
        assert_eq!(m.get(16, 16), 0);
        assert_eq!(m.get(0, 0), 1, "半径外不受影响");
    }

    #[test]
    fn 描边不断线() {
        let mut m = Mask::new(64, 64);
        stroke(&mut m, (5.0, 5.0), (55.0, 5.0), 4.0, false);
        for x in 5..=55u32 {
            assert_eq!(m.get(x, 5), 1, "x={x} 处断开");
        }
    }

    #[test]
    fn 笔刷不会画出界() {
        let mut m = Mask::new(16, 16);
        stamp(&mut m, 0.0, 0.0, 20.0, false);
        // 不该 panic。圆心在角上、半径 20 覆盖整幅，
        // 但最右/最下一列的中心点距离 > 20，本来就在圆外，所以不是全满。
        assert!(m.count() > 16 * 16 * 3 / 4, "应覆盖大部分");
        assert_eq!(m.get(0, 0), 1);
    }

    #[test]
    fn 填充只染连通区() {
        let mut m = Mask::new(8, 8);
        for y in 0..8 {
            m.set(4, y, 1); // 竖墙
        }
        flood_fill(&mut m, 0, 0, 1);
        assert_eq!(m.get(0, 0), 1);
        assert_eq!(m.get(4, 0), 1, "墙本身是 1，填 1 不变");
        assert_eq!(m.get(5, 0), 0, "另一侧不该被染");
    }

    #[test]
    fn 填充能清空连通区() {
        let mut m = Mask::new(8, 8);
        m.fill(1);
        for y in 0..8 {
            m.set(4, y, 0);
        }
        flood_fill(&mut m, 0, 0, 0);
        assert_eq!(m.get(0, 0), 0);
        assert_eq!(m.get(6, 0), 1, "墙外仍然被遮");
    }
}
