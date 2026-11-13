//! 记录「texel 采样」与「距离场插值」两条路径的已知差异。
//!
//! 这不是 bug，是量化误差：
//! - `sdf::sample_texels`（预览/导出走这条）：texel 是 u16 满刻度 65535，
//!   角度分辨率约 0.0014°，边界带上的像素会被判到另一侧
//! - `Doc::interpolated_mask_at`（加关键帧的插值起点走这条）：连续距离场
//!
//! 实测最大差 152 像素（1024² 的 0.0145%）。正因如此，
//! `do_add_keyframe_at` 改成直接用 `display_mask` 当起点——
//! 那条路径才和用户屏幕上看到的一致。
//!
//! 用途：若差异突然放大（阈值 1%），说明量化或距离场有一边被改坏了。
//!
//! 用法：`cargo run --release --offline --example verify_keyframe_equiv`

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::sdf;

fn circle(w: u32, h: u32, cx: f32, cy: f32, r: f32) -> Mask {
    let mut m = Mask::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= r * r {
                m.set(x, y, 1);
            }
        }
    }
    m
}

/// 不规则形状（偏心、带缺口），避免圆形掩盖几何误差
fn blob(w: u32, h: u32, cx: f32, cy: f32, r: f32, phase: f32) -> Mask {
    let mut m = Mask::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let dx = (x as f32 - cx) / r;
            let dy = (y as f32 - cy) / r;
            // 极坐标调制出非圆轮廓
            let ang = dy.atan2(dx);
            let rr = 1.0 + 0.28 * (3.0 * ang + phase).sin() + 0.12 * (5.0 * ang).cos();
            if dx * dx + dy * dy <= rr * rr {
                m.set(x, y, 1);
            }
        }
    }
    m
}

fn main() {
    const W: u32 = 1024;
    const H: u32 = 1024;
    let mut pass = 0;
    let mut fail = 0;
    let mut worst = 0usize;

    macro_rules! check {
        ($name:expr, $cond:expr, $detail:expr) => {
            if $cond {
                println!("  ✓ {}", $name);
                pass += 1;
            } else {
                println!("  ✗ {}　{}", $name, $detail);
                fail += 1;
            }
        };
    }

    println!("texel 采样 vs 直接距离场插值 等价性　画布 {W}×{H}");
    println!("================================================================");

    for (label, n_frames, shape) in [
        ("圆形 2 帧", 2usize, 0u8),
        ("圆形 4 帧", 4, 0),
        ("异形 3 帧", 3, 1),
        ("异形 5 帧", 5, 1),
    ] {
        let mut doc = Doc::with_steps(W, H, 32);
        let angles: Vec<f32> = (0..n_frames)
            .map(|i| i as f32 * (90.0 / (n_frames - 1) as f32))
            .collect();
        for (i, a) in angles.iter().enumerate() {
            let m = if shape == 0 {
                circle(W, H, 512.0, 512.0, 120.0 + 60.0 * i as f32)
            } else {
                blob(W, H, 500.0, 520.0, 110.0 + 55.0 * i as f32, i as f32 * 0.7)
            };
            doc.active_channel_mut().insert_frame(*a, m);
        }

        // 全部帧都合法才能烘焙
        let (texels, dir) = match doc.active_channel_mut().texels(W, H) {
            Ok(v) => v,
            Err(e) => {
                println!("  · {label} 烘焙失败：{}", e.lines().next().unwrap_or(""));
                continue;
            }
        };

        // 在若干非关键帧角度比较两条路径
        let mut max_diff = 0usize;
        let mut tested = 0;
        for k in 1..20 {
            let t = k as f32 / 20.0;
            let angle = t * 90.0;
            // 跳过正好落在关键帧上的角度（那里两条路径都退化为"帧本身"）
            if angles.iter().any(|a| (a - angle).abs() < 0.01) {
                continue;
            }
            let mut by_texel = Mask::new(W, H);
            sdf::sample_texels(&texels, t, dir, &mut by_texel, false);
            let by_dist = doc.interpolated_mask_at(angle);

            let d = (0..(W * H) as usize)
                .filter(|&i| by_texel.data()[i] != by_dist.data()[i])
                .count();
            if d > max_diff {
                max_diff = d;
            }
            tested += 1;
        }
        if max_diff > worst {
            worst = max_diff;
        }
        let pct = max_diff as f64 / (W * H) as f64 * 100.0;
        // **这不是回归断言，是记录一个已知差异。**
        //
        // 两条路径本来就不等价：`interpolated_mask_at` 走连续距离场，
        // 预览/导出走 texel 量化（u16 满刻度 65535 → 角度分辨率约 0.0014°）。
        // 量化会在边界带上把少量像素判到另一侧，实测最大 152 像素（0.0145%）。
        //
        // 这个差异正是加关键帧改用 `display_mask` 当起点的理由：
        // 那条路径才和屏幕上看到的一致。若哪天差异突然变大（比如 > 1%），
        // 说明量化或距离场有一边改坏了，这里会报出来。
        const TOLERANCE_PCT: f64 = 1.0;
        check!(
            format!("{label}：{tested} 个角度，差异 {max_diff} 像素（{pct:.4}%）"),
            pct < TOLERANCE_PCT,
            format!("差异 {pct:.3}% 超过 {TOLERANCE_PCT}%，两条路径可能被改坏了")
        );
    }

    println!("\n================================================================");
    println!(
        "最大差异 {worst} 像素（占 {:.4}%）",
        worst as f64 / (W * H) as f64 * 100.0
    );
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
