//! 性能对比：旧的"距离场混合" vs 新的"texel 阈值比较"。

use std::time::Instant;

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::Doc;
use sdf_mask_tool::model::doc::{DEFAULT_STEPS, step_angle};
use sdf_mask_tool::sdf;

/// 测试统一用默认步数
const TS: usize = DEFAULT_STEPS;

fn main() {
    let n = 1024u32;
    println!("画布 {n}²，测每次刷新耗时\n");

    let mut doc = Doc::new(n, n);
    let mut a = Mask::new(n, n);
    for y in 0..n {
        for x in 0..(n / 4) {
            a.set(x, y, 1);
        }
    }
    doc.layers[0].channels[0].insert_frame(step_angle(0, TS), a);
    let mut b = Mask::new(n, n);
    for y in 0..n {
        for x in 0..(n * 3 / 4) {
            b.set(x, y, 1);
        }
    }
    doc.layers[0].channels[0].insert_frame(step_angle(TS - 1, TS), b);

    // ---- 首次烘焙（不可避免的一次性成本）----
    let t0 = Instant::now();
    let (texels, dir) = doc.layers[0].channels[0].texels(n, n).expect("烘焙失败");
    let bake_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!("首次烘焙 texel：{bake_ms:.1} ms（一次性，之后走缓存）");
    println!("编码方向：{}\n", dir.label());

    // ---- 新路径：texel 阈值采样，缓存命中 ----
    let mut out = Mask::new(n, n);
    let mut total = 0.0f64;
    let iters = 32;
    for s in 0..iters {
        let t = s as f32 / (iters - 1) as f32;
        let t0 = Instant::now();
        sdf::sample_texels(&texels, t, dir, &mut out, false);
        total += t0.elapsed().as_secs_f64() * 1000.0;
    }
    println!(
        "新路径（texel 阈值）：单次 {:.2} ms，{} 步共 {:.1} ms",
        total / iters as f64,
        iters,
        total
    );

    // ---- 旧路径：两次距离场 to_vec + 混合 ----
    let da = doc.layers[0].channels[0]
        .dist_of(0, n as usize, n as usize)
        .to_vec();
    let db = doc.layers[0].channels[0]
        .dist_of(1, n as usize, n as usize)
        .to_vec();
    let mut total_old = 0.0f64;
    for s in 0..iters {
        let t = s as f32 / (iters - 1) as f32;
        let t0 = Instant::now();
        let a = doc.layers[0].channels[0]
            .dist_of(0, n as usize, n as usize)
            .to_vec();
        let b = doc.layers[0].channels[0]
            .dist_of(1, n as usize, n as usize)
            .to_vec();
        sdf::interpolate_from_dist(&a, &b, t, &mut out);
        total_old += t0.elapsed().as_secs_f64() * 1000.0;
    }
    println!(
        "旧路径（距离场混合）：单次 {:.2} ms，{} 步共 {:.1} ms",
        total_old / iters as f64,
        iters,
        total_old
    );

    println!("\n提升：{:.1}× 更快", total_old / total);

    // ---- 内存占用对比 ----
    let texel_mem = texels.len() as f64 / 1024.0 / 1024.0;
    let dist_mem = (da.len() * 4) as f64 / 1024.0 / 1024.0 * 2.0;
    println!("\n内存：texel {texel_mem:.1} MB（1 帧） vs 距离场 {dist_mem:.1} MB（2 帧）");
    println!(
        "       {} 帧关键帧时：texel {:.1} MB vs 距离场 {:.1} MB",
        8,
        texel_mem,
        (da.len() * 4) as f64 / 1024.0 / 1024.0 * 8.0
    );

    // ---- 正确性：两端应还原等于关键帧 ----
    println!("\n还原校验：");
    for (i, t) in [(0usize, 0.0f32), (1, 1.0)] {
        sdf::sample_texels(&texels, t, dir, &mut out, false);
        let orig = &doc.layers[0].channels[0].frames[i].mask;
        let diff = (0..(n * n) as usize)
            .filter(|&k| out.data()[k] != orig.data()[k])
            .count();
        println!("  t={t:.0}：与原帧差异 {diff} / {}", n * n);
    }
}
