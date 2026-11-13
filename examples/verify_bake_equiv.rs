//! 验证优化后的烘焙与优化前**逐像素完全一致**。
//!
//! 优化有三处，每处都必须零精度影响：
//!  1. 帧间并行 EDT —— 每帧独立，结果应当位相同
//!  2. 单帧 EDT 缓存复用 —— 距离场只依赖自身遮罩，复用等同重算
//!  3. 从已算好的 sdfs 合并 —— 纯数据搬运
//!
//! 用法：`cargo run --release --offline --example verify_bake_equiv`

use std::time::Instant;

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::sdf;

const W: u32 = 1024;
const H: u32 = 1024;

fn circle(cx: f32, cy: f32, r: f32) -> Mask {
    let mut m = Mask::new(W, H);
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

/// 不规则形状，避免圆形掩盖几何问题
fn blob(cx: f32, cy: f32, r: f32, phase: f32) -> Mask {
    let mut m = Mask::new(W, H);
    for y in 0..H {
        for x in 0..W {
            let dx = (x as f32 - cx) / r;
            let dy = (y as f32 - cy) / r;
            let ang = dy.atan2(dx);
            let rr = 1.0 + 0.28 * (3.0 * ang + phase).sin() + 0.12 * (5.0 * ang).cos();
            if dx * dx + dy * dy <= rr * rr {
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

    println!(
        "烘焙优化等价性验证　画布 {W}×{H}　线程 {}",
        rayon::current_num_threads()
    );
    println!("================================================================");

    // ---------- 1. 帧间并行 vs 串行 ----------
    println!("\n[1] 帧间并行 EDT 与串行是否位相同");
    for n in [2usize, 4, 8] {
        let masks: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    circle(512.0, 512.0, 100.0 + 30.0 * i as f32)
                        .data()
                        .to_vec()
                } else {
                    blob(500.0, 520.0, 110.0 + 25.0 * i as f32, i as f32 * 0.7)
                        .data()
                        .to_vec()
                }
            })
            .collect();
        let mut mf: Vec<sdf::MaskFrame> = masks
            .iter()
            .enumerate()
            .map(|(i, m)| sdf::MaskFrame {
                angle: i as f32 * (90.0 / n as f32),
                mask: m.clone(),
            })
            .collect();
        // bake_with_dir 现在是并行的（>=2 帧）
        let par = sdf::bake_with_dir(&mut mf, W as usize, H as usize, sdf::Direction::ShadowGrows);

        // 手工串行算一遍作为基准
        let serial_sdfs: Vec<Vec<f32>> = masks
            .iter()
            .map(|m| {
                sdf::signed_distance(m, W as usize, H as usize, 1)
                    .into_iter()
                    .map(|v| -v)
                    .collect()
            })
            .collect();
        let mut mf2: Vec<sdf::MaskFrame> = masks
            .iter()
            .enumerate()
            .map(|(i, m)| sdf::MaskFrame {
                angle: i as f32 * (90.0 / n as f32),
                mask: m.clone(),
            })
            .collect();
        let serial = sdf::bake_from_sdfs(
            &mut mf2,
            &serial_sdfs,
            W as usize,
            H as usize,
            sdf::Direction::ShadowGrows,
        );

        let diff = par
            .iter()
            .zip(serial.iter())
            .filter(|(a, b)| a != b)
            .count();
        check!(
            format!("{n} 帧：并行 == 串行（{} 个 texel）", par.len()),
            diff == 0,
            format!("{diff} 个 texel 不一致")
        );
    }

    // ---------- 2. 缓存复用 vs 全量重算 ----------
    println!("\n[2] 加帧时复用缓存 vs 全量重算，结果是否一致");
    for n in [2usize, 3, 4] {
        // 基准：全新文档，全量算
        let mut fresh = Doc::with_steps(W, H, 32);
        for i in 0..n {
            fresh.active_channel_mut().insert_frame(
                i as f32 * (90.0 / (n - 1) as f32),
                circle(512.0, 512.0, 100.0 + 60.0 * i as f32),
            );
        }
        let fresh_texels = fresh
            .active_channel_mut()
            .texels(W, H)
            .expect("基准应能烘焙")
            .0;

        // 复用路径：先建 n-1 帧并烘一次（填满缓存），再加最后一帧
        let mut cached = Doc::with_steps(W, H, 32);
        for i in 0..(n - 1) {
            cached.active_channel_mut().insert_frame(
                i as f32 * (90.0 / (n - 1) as f32),
                circle(512.0, 512.0, 100.0 + 60.0 * i as f32),
            );
        }
        let _ = cached.active_channel_mut().texels(W, H); // 预热缓存
        // 加最后一帧
        cached.active_channel_mut().insert_frame(
            (n - 1) as f32 * (90.0 / (n - 1) as f32),
            circle(512.0, 512.0, 100.0 + 60.0 * (n - 1) as f32),
        );
        let cached_texels = cached
            .active_channel_mut()
            .texels(W, H)
            .expect("复用路径应能烘焙")
            .0;

        let diff = fresh_texels
            .iter()
            .zip(cached_texels.iter())
            .filter(|(a, b)| a != b)
            .count();
        check!(
            format!("{n} 帧：复用缓存 == 全量重算"),
            diff == 0,
            format!("{diff} 个 texel 不一致")
        );
    }

    // ---------- 3. 插入中间帧后缓存不错位 ----------
    println!("\n[3] 插入中间帧（下标位移）后缓存仍正确");
    {
        let mut a = Doc::with_steps(W, H, 32);
        a.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        a.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let _ = a.active_channel_mut().texels(W, H); // 填充缓存

        // 在中间插入一帧：原 90° 帧的下标从 1 变成 2
        a.active_channel_mut()
            .insert_frame(45.0, circle(512.0, 512.0, 200.0));
        let reused = a.active_channel_mut().texels(W, H).expect("应能烘焙").0;

        // 基准：同样内容，全新文档全量算
        let mut b = Doc::with_steps(W, H, 32);
        b.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        b.active_channel_mut()
            .insert_frame(45.0, circle(512.0, 512.0, 200.0));
        b.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let fresh = b.active_channel_mut().texels(W, H).expect("应能烘焙").0;

        let diff = reused
            .iter()
            .zip(fresh.iter())
            .filter(|(x, y)| x != y)
            .count();
        check!(
            "中间插入帧后（下标位移）结果依然正确",
            diff == 0,
            format!("{diff} 个 texel 不一致——缓存按下标存会张冠李戴")
        );
    }

    // ---------- 4. 画一笔后缓存失效 ----------
    println!("\n[4] 改一帧内容后缓存必须失效");
    {
        let mut d = Doc::with_steps(W, H, 32);
        d.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        d.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let before = d.active_channel_mut().texels(W, H).expect("应能烘焙").0;

        // 改第 0 帧：**扩大**它的遮罩（保持"扩张"合法）。
        // 上一版在角落新画一块，而 90° 帧那里没有 → 构成"遮了又亮"，数据非法，
        // 烘焙直接报错。那是测试造错了数据，不是实现问题。
        let mut m = d.active_channel_ref().frames[0].mask.clone();
        // 把 0° 帧扩大到半径 160（原本 100），仍小于 90° 帧的 300，保持嵌套
        m = circle(512.0, 512.0, 160.0);
        assert_ne!(
            m.fingerprint(),
            d.active_channel_ref().frames[0].mask.fingerprint(),
            "前提：内容确实变了"
        );
        d.active_channel_mut().frames[0].mask = m;
        let after = d.active_channel_mut().texels(W, H).expect("应能烘焙").0;

        let diff = before
            .iter()
            .zip(after.iter())
            .filter(|(x, y)| x != y)
            .count();
        check!(
            format!("扩大 0° 帧后 texel 应变化（变了 {diff} 个）"),
            diff > 0,
            "内容改了但 texel 没变——说明缓存脏了".to_string()
        );
    }

    // ---------- 5. 速度 ----------
    println!("\n[5] 速度对比");
    for n in [2usize, 4, 8] {
        let angles: Vec<f32> = (0..n).map(|i| i as f32 * (90.0 / n as f32)).collect();
        let mk = || -> Vec<sdf::MaskFrame> {
            angles
                .iter()
                .enumerate()
                .map(|(i, a)| sdf::MaskFrame {
                    angle: *a,
                    mask: circle(512.0, 512.0, 100.0 + 220.0 * i as f32 / n as f32)
                        .data()
                        .to_vec(),
                })
                .collect()
        };
        let full = bench(
            || {
                let mut f = mk();
                std::hint::black_box(sdf::bake_auto(&mut f, W as usize, H as usize));
            },
            5,
        );
        let one = bench(
            || {
                let v = sdf::signed_distance(&mk()[0].mask, W as usize, H as usize, 1);
                std::hint::black_box(&v);
            },
            5,
        );
        println!(
            "  {n} 帧：全量 {full:7.2} ms　加帧时（复用 {} 帧）约 {one:6.2} ms + 合并",
            n - 1
        );
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
