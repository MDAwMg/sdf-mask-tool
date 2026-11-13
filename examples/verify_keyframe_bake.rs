//! 验证「加关键帧后白烧烘焙」这个假设，并测量解耦方案的可行性。
//!
//! 假设 H1：加关键帧后播放头停在新帧角度上，此时 `bake_auto` + `sample_texels`
//!          得到的结果 == 新帧遮罩本身。若是，则这次 136ms 是完全可以跳过的。
//!
//! 用法：`cargo run --release --offline --example verify_keyframe_bake`

use std::time::Instant;

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::sdf::{self, Direction};

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

fn bench<F: FnMut()>(mut f: F, reps: usize) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        f();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        if ms < best {
            best = ms;
        }
    }
    best
}

fn main() {
    const W: u32 = 1024;
    const H: u32 = 1024;
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

    println!("加关键帧白烧烘焙验证　画布 {W}×{H}");
    println!("================================================================");

    // ---------- H1：新帧位置采样 == 新帧遮罩本身 ----------
    println!("\n[H1] 播放头停在新帧上时，采样结果是否等于新帧遮罩");
    for (n_frames, new_angle) in [(2usize, 45.0f32), (3, 30.0), (4, 11.25), (4, 78.75)] {
        let mut doc = Doc::with_steps(W, H, 32);
        let angles: Vec<f32> = (0..n_frames)
            .map(|i| i as f32 * (90.0 / (n_frames - 1) as f32))
            .collect();
        for (i, a) in angles.iter().enumerate() {
            doc.active_channel_mut()
                .insert_frame(*a, circle(W, H, 512.0, 512.0, 120.0 + 55.0 * i as f32));
        }
        // 模拟 do_add_keyframe_at
        let ok = doc.add_frame_at_angle(new_angle, true);
        if !ok {
            println!("  · {n_frames}帧→{new_angle}° 加帧失败，跳过");
            continue;
        }
        let target = doc
            .active_channel_ref()
            .frame_at(new_angle)
            .unwrap()
            .mask
            .clone();

        let (texels, dir) = doc
            .active_channel_mut()
            .texels(W, H)
            .expect("合法数据应能烘焙");
        let t = new_angle / 90.0;
        let mut sampled = Mask::new(W, H);
        sdf::sample_texels(&texels, t, dir, &mut sampled, false);

        let mut diff = 0usize;
        for i in 0..(W * H) as usize {
            if target.data()[i] != sampled.data()[i] {
                diff += 1;
            }
        }
        check!(
            format!("{n_frames}帧 → 在 {new_angle}° 加帧，采样 == 帧本身"),
            diff == 0,
            format!("{diff} 像素不一致")
        );
    }

    // ---------- 违规检查的成本（用于解耦）----------
    println!("\n[成本] 违规检查 check_direction 能不能独立于烘焙");
    let make_frames = |angles: &[f32], r0: f32, dr: f32| -> Vec<sdf::MaskFrame> {
        angles
            .iter()
            .enumerate()
            .map(|(i, ang)| sdf::MaskFrame {
                angle: *ang,
                mask: circle(W, H, 512.0, 512.0, r0 + dr * i as f32)
                    .data()
                    .to_vec(),
            })
            .collect()
    };
    for n in [2usize, 4] {
        let angles: Vec<f32> = (0..n).map(|i| i as f32 * (90.0 / n as f32)).collect();
        let frames = make_frames(&angles, 120.0, 40.0);
        let chk_ms = bench(
            || {
                let r = sdf::check_direction(&frames, W as usize, H as usize);
                std::hint::black_box(&r);
            },
            10,
        );
        let full_ms = bench(
            || {
                let mut f = make_frames(&angles, 120.0, 40.0);
                let r = sdf::bake_auto(&mut f, W as usize, H as usize);
                std::hint::black_box(&r);
            },
            5,
        );
        println!(
            "  {n} 帧：check_direction {chk_ms:7.3} ms　bake_auto {full_ms:7.3} ms　（检查只占 {:.0}%）",
            chk_ms / full_ms * 100.0
        );
    }

    // ---------- 用缓存的 dist_cache 做插值 ----------
    println!("\n[成本] interpolate 能否复用 dist_cache");
    let a = circle(W, H, 512.0, 512.0, 120.0);
    let b = circle(W, H, 512.0, 512.0, 240.0);
    let cold_ms = bench(
        || {
            let mut out = Mask::new(W, H);
            sdf::interpolate_masks(&a, &b, 0.5, &mut out);
            std::hint::black_box(&out);
        },
        5,
    );
    // 复用：距离场已经算好，只剩 lerp + 阈值
    let da: Vec<f32> = sdf::signed_distance(a.data(), W as usize, H as usize, 1)
        .into_iter()
        .map(|v| -v)
        .collect();
    let db: Vec<f32> = sdf::signed_distance(b.data(), W as usize, H as usize, 1)
        .into_iter()
        .map(|v| -v)
        .collect();
    let warm_ms = bench(
        || {
            let mut out = Mask::new(W, H);
            sdf::interpolate_from_dist(&da, &db, 0.5, &mut out);
            std::hint::black_box(&out);
        },
        20,
    );
    println!("  冷（两次 EDT）: {cold_ms:7.3} ms");
    println!(
        "  暖（复用距离场）: {warm_ms:7.3} ms　→ 可省 {:.1}%",
        (1.0 - warm_ms / cold_ms) * 100.0
    );

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
