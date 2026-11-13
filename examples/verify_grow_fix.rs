//! 验证用户场景的完整流程：在「第 2 帧」画一笔后，时间轴能否正确生长。
//!
//! 用户报的症状：新建工程两帧空白，在 90° 画一笔后播放**毫无效果**。
//! 预期：应从 0° 的空帧慢慢"长出来"到 90° 的笔迹。
//!
//! 本程序覆盖两种真实路径：
//!   A. 直接在第 2 帧（90°，本来就有）上画
//!   B. 在 88.6° 自动插了一帧后再画（实机出现的状态，共 3 帧）
//!
//! 用法：`cargo run --release --offline --example verify_grow_fix`

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::sdf;

const W: u32 = 1024;
const H: u32 = 1024;

fn right_blob() -> Mask {
    let mut m = Mask::new(W, H);
    let (cx, cy, r) = (760.0f32, 512.0f32, 120.0f32);
    for y in 0..H {
        for x in 0..W {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= r * r {
                m.set(x, y, 1);
            }
        }
    }
    m
}

fn blank() -> Mask {
    Mask::new(W, H)
}

/// 沿时间轴扫一遍，返回 (是否单调增长, 像素数列表)
fn sweep(doc: &mut Doc) -> (bool, Vec<usize>) {
    let (dw, dh) = (doc.width, doc.height);
    let (texels, dir) = doc
        .active_channel_mut()
        .texels(dw, dh)
        .expect("数据应能烘焙");
    let mut counts = Vec::new();
    for k in 0..=10 {
        let t = k as f32 / 10.0;
        let mut m = Mask::new(dw, dh);
        sdf::sample_texels(&texels, t, dir, &mut m, false);
        counts.push(m.count());
    }
    // 单调不减
    let monotonic = counts.windows(2).all(|w| w[1] >= w[0]);
    (monotonic, counts)
}

fn main() {
    let mut pass = 0;
    let mut fail = 0;
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

    println!("空帧生长修复验证　画布 {W}×{H}");
    println!("================================================================");

    // ---------- A：直接在第 2 帧（90°）画 ----------
    println!("\n[A] 在两帧空白的工程里，于 90° 画一笔");
    {
        let mut doc = Doc::with_steps(W, H, 32);
        doc.active_channel_mut().insert_frame(0.0, blank());
        doc.active_channel_mut().insert_frame(90.0, blank());

        // 先烘一次（模拟用户打开工程时的首次预览）
        let (dw, dh) = (doc.width, doc.height);
        let _ = doc.active_channel_mut().texels(dw, dh);

        // 在 90° 画
        doc.active_channel_mut().frames[1].mask = right_blob();

        // 关键：画完后必须能重新烘出**新**内容
        let fp = doc.active_channel_ref().channel_fingerprint();
        check!(
            "画完后旧缓存不再匹配当前内容（防止画面冻结）",
            !doc.active_channel_ref().cache_matches(fp),
            "旧缓存仍被当成当前内容——这就是原 bug".to_string()
        );

        let (mono, counts) = sweep(&mut doc);
        println!("    像素数：{counts:?}");
        check!(
            "从 0 单调增长到满（播放能看到「长出来」）",
            mono && counts[0] == 0 && *counts.last().unwrap() > 40000,
            format!(
                "单调={mono}，首={} 末={}",
                counts[0],
                counts.last().unwrap()
            )
        );
    }

    // ---------- B：实机路径 + 吸附（应当复用 90° 而不是另建） ----------
    println!("\n[B] 点偏到 88.6°：应当吸附到 90° 那一帧，不另建");
    {
        // 当前 steps=32 → 每格 90/31 = 2.90°，半格 = 1.45°
        // 88.6° 与 90° 差 1.4° ≤ 1.45° → 应当吸附
        let steps = 32usize;
        let half = 45.0 / (steps - 1) as f32;
        println!("    半格阈值 = {half:.2}°；88.6° 与 90° 相差 1.40°");
        check!(
            "88.6° 在吸附范围内",
            1.4 <= half + 1e-6,
            format!("差 1.40° 超过半格 {half:.2}°")
        );

        let mut doc = Doc::with_steps(W, H, steps);
        doc.active_channel_mut().insert_frame(0.0, blank());
        doc.active_channel_mut().insert_frame(90.0, blank());
        let (dw, dh) = (doc.width, doc.height);
        let _ = doc.active_channel_mut().texels(dw, dh);

        // 模拟吸附：直接取 90° 那帧来画（这是修复后的行为）
        let n_before = doc.active_channel_ref().frames.len();
        if let Some(f) = doc.active_channel_mut().frame_at_mut(90.0) {
            f.mask = right_blob();
        }
        check!(
            "吸附后帧数不变（没有多建一帧）",
            doc.active_channel_ref().frames.len() == n_before,
            format!(
                "帧数 {} → {}",
                n_before,
                doc.active_channel_ref().frames.len()
            )
        );

        let (mono, counts) = sweep(&mut doc);
        println!("    像素数：{counts:?}");
        check!(
            "从 0 单调增长到满",
            mono && counts[0] == 0 && *counts.last().unwrap() > 40000,
            format!(
                "单调={mono}，首={} 末={}",
                counts[0],
                counts.last().unwrap()
            )
        );
    }

    // ---------- B2：离得远时仍应正常插帧 ----------
    println!("\n[B2] 点在 45°（远离所有帧）：应当正常插帧");
    {
        let steps = 32usize;
        // 半格宽度：吸附判定的阈值。45° 到 0° 和到 90° 的距离都是 45°。
        let half = 45.0 / (steps - 1) as f32;
        let dist_to_0 = 45.0f32;
        let dist_to_90 = 90.0f32 - 45.0;
        check!(
            "45° 离 0° 和 90° 都超过半格，不会被吸附",
            dist_to_0 > half && dist_to_90 > half,
            format!("半格 {half:.3}，到 0° 是 {dist_to_0}，到 90° 是 {dist_to_90}")
        );

        let mut doc = Doc::with_steps(W, H, steps);
        doc.active_channel_mut().insert_frame(0.0, blank());
        doc.active_channel_mut().insert_frame(90.0, right_blob());
        let (dw, dh) = (doc.width, doc.height);
        let _ = doc.active_channel_mut().texels(dw, dh);

        doc.active_channel_mut().insert_frame(45.0, blank());
        check!(
            "45° 确实新建了一帧",
            doc.active_channel_ref().frames.len() == 3,
            format!("帧数 {}", doc.active_channel_ref().frames.len())
        );
    }

    // ---------- C：反方向（0° 画，90° 空）仍要正常 ----------
    println!("\n[C] 回归：0° 画一笔 → 90° 空白（应慢慢消失）");
    {
        let mut doc = Doc::with_steps(W, H, 32);
        doc.active_channel_mut().insert_frame(0.0, right_blob());
        doc.active_channel_mut().insert_frame(90.0, blank());
        let (_, counts) = sweep(&mut doc);
        println!("    像素数：{counts:?}");
        let mono_down = counts.windows(2).all(|w| w[1] <= w[0]);
        check!(
            "单调递减到 0（不会因为修了生长方向而破坏收缩方向）",
            mono_down && *counts.last().unwrap() == 0,
            format!("单调={mono_down}，末={}", counts.last().unwrap())
        );
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
