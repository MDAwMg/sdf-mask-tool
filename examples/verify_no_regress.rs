//! 验证修复的副作用没有把成本转移到别处。
//!
//! 重点看三条真实高频路径，修复后都不能比修复前慢：
//! 1. 落笔（松手后要重新烘焙预览）
//! 2. 撤销 / 重做（要 clone 文档进出栈）
//! 3. 切图层（clone 不涉及，但遍历所有通道）
//!
//! 用法：`cargo run --release --offline --example verify_no_regress`

use std::time::Instant;

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;

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
    println!("修复副作用回归　画布 {W}×{H}（1 图层 4 通道 × 4 帧）");
    println!("================================================================");

    // 造一个接近真实的文档：1 图层，4 通道各 4 帧
    let mut doc = Doc::new(W, H);
    for c in 0..4usize {
        for i in 0..4usize {
            let ch = &mut doc.layers[0].channels[c];
            ch.insert_frame(
                i as f32 * 22.5,
                circle(400.0 + 40.0 * c as f32, 512.0, 120.0 + 55.0 * i as f32),
            );
        }
    }

    // ---------- 1. 快照（撤销栈）----------
    println!("\n[1] push_undo 快照（doc.clone）");
    let snap_ms = bench(
        || {
            std::hint::black_box(doc.clone());
        },
        20,
    );
    println!("   {snap_ms:7.3} ms　（修复前同规模约 14.5ms：16 通道 × 4MB 距离场）");
    let ok1 = snap_ms < 6.0;
    println!(
        "   {}",
        if ok1 {
            "✓ 达标（< 6ms）"
        } else {
            "✗ 太慢"
        }
    );

    // ---------- 2. 落笔后重烘预览 ----------
    println!("\n[2] 落笔松手后重烘当前通道预览");
    // 模拟：改一帧 → 缓存失效 → 重新 texels()
    let bake_ms = bench(
        || {
            let mut d = doc.clone();
            let ch = &mut d.layers[0].channels[0];
            // 改一个像素，指纹变化 → 缓存失效
            let mut m = ch.frames[3].mask.clone();
            m.set(10, 10, if m.get(10, 10) == 0 { 1 } else { 0 });
            ch.frames[3].mask = m;
            let r = ch.texels(W, H);
            std::hint::black_box(&r);
        },
        5,
    );
    println!("   {bake_ms:7.3} ms　（这是真实重烘成本，修复前后应一致）");
    // 关键：这个值本来就该是"烘焙一次"的成本，不该因修复而增加
    let ok2 = bake_ms < 200.0;
    println!(
        "   {}",
        if ok2 {
            "✓ 在预期内（单次重烘，非新增开销）"
        } else {
            "✗ 异常"
        }
    );

    // ---------- 3. 缓存命中（不该重烘）----------
    println!("\n[3] 缓存命中：连续调用 texels() 应几乎零开销");
    let mut warm = doc.clone();
    let _ = warm.layers[0].channels[0].texels(W, H); // 预热
    let hit_ms = bench(
        || {
            let r = warm.layers[0].channels[0].texels(W, H);
            std::hint::black_box(&r);
        },
        20,
    );
    println!("   {hit_ms:7.3} ms　（含 2MB texel 图拷贝）");
    let ok3 = hit_ms < 5.0;
    println!(
        "   {}",
        if ok3 {
            "✓ 达标（< 5ms）"
        } else {
            "✗ 太慢"
        }
    );

    // ---------- 4. clone 后缓存仍命中 ----------
    println!("\n[4] clone 后 texel 缓存仍命中（这是我特意保留的）");
    let src = warm.clone();
    let post_clone_ms = bench(
        || {
            let mut d = src.clone();
            let r = d.layers[0].channels[0].texels(W, H);
            std::hint::black_box(&r);
        },
        10,
    );
    println!("   {post_clone_ms:7.3} ms　（若缓存被丢会变成 ~140ms）");
    let ok4 = post_clone_ms < 8.0;
    println!(
        "   {}",
        if ok4 {
            "✓ 达标（缓存被正确保留）"
        } else {
            "✗ 缓存丢了，会拖慢每次快照后的预览"
        }
    );

    // ---------- 5. 距离场缓存已丢（预期行为）----------
    println!("\n[5] clone 后 dist_cache 为空（预期，它是死缓存）");
    let c = doc.clone();
    let empty = c
        .layers
        .iter()
        .all(|l| l.channels.iter().all(|ch| ch.dist_cache.is_empty()));
    println!(
        "   {}",
        if empty {
            "✓ 已丢弃"
        } else {
            "✗ 没丢，白费内存带宽"
        }
    );
    let ok5 = empty;

    println!("\n================================================================");
    let all = ok1 && ok2 && ok3 && ok4 && ok5;
    println!(
        "{}",
        if all {
            "全部达标"
        } else {
            "有项目未达标"
        }
    );
    if !all {
        std::process::exit(1);
    }
}
