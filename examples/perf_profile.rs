//! 性能剖析：把 3D 渲染的耗时拆到各个阶段，用于判断优化方向。
//!
//! 不做微基准游戏——这里测的是"真实模型 + 真实参数"下各阶段占比，
//! 因为优化该往哪走取决于哪个阶段最贵，而不是直觉。
//!
//! 用法：`cargo run --release --offline --example perf_profile`

use std::time::Instant;

use sdf_mask_tool::model3d::{self, raster};

/// 统计目标：一次渲染里各阶段的时间。
///
/// 由于 `raster::render` 是黑盒，这里用"改参数看差异"的差分法：
/// 关掉某个特性后的耗时差，就是该特性的成本。
fn bench<F: FnMut()>(n: usize, mut f: F) -> f32 {
    // 预热
    f();
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    t.elapsed().as_secs_f32() * 1000.0 / n as f32
}

fn main() {
    // 模型路径从命令行传入：`cargo run --example perf_profile -- <模型文件>`
    let path = match std::env::args().nth(1) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            println!("用法：cargo run --example perf_profile -- <模型文件>");
            return;
        }
    };
    let mesh = match model3d::load_mesh(&path) {
        Ok(m) => m,
        Err(e) => {
            println!("加载失败：{e}");
            return;
        }
    };

    println!("════════ 模型 ════════");
    println!("  三角 {}  顶点 {}", mesh.tri_count, mesh.vert_count);
    println!(
        "  数据量：位置 {} KB，UV {} 套 {} KB，法线 {} KB，平滑法线 {} KB",
        mesh.pos.len() * 12 / 1024,
        mesh.uv.len(),
        mesh.uv.iter().map(|s| s.len() * 8).sum::<usize>() / 1024,
        mesh.normals.len() * 12 / 1024,
        mesh.smooth_normals.len() * 12 / 1024
    );

    // ---------------- 各分辨率基线 ----------------
    println!("\n════════ 分辨率 vs 耗时（正视，背面剔除开）════════");
    let mut cam = raster::Camera::default();
    cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

    let mut baseline = Vec::new();
    for size in [240usize, 360, 480, 720, 960, 1440] {
        let opts = raster::RenderOpts {
            width: size,
            height: size,
            ..Default::default()
        };
        let ms = bench(6, || {
            let _ = raster::render(&mesh, &cam, &opts, None, None);
        });
        let px = (size * size) as f32;
        println!(
            "  {size:>4}×{size:<4} {ms:7.2} ms   {:.1} ns/px",
            ms * 1e6 / px
        );
        baseline.push((size, ms));
    }

    // ---------------- 特性成本分解（差分法）----------------
    println!("\n════════ 特性成本（720×720，与基线对比）════════");
    let size = 720usize;
    let base_opts = raster::RenderOpts {
        width: size,
        height: size,
        ..Default::default()
    };
    let base_ms = bench(6, || {
        let _ = raster::render(&mesh, &cam, &base_opts, None, None);
    });
    println!("  基线（剔除开、无线框、无纹理）        {base_ms:7.2} ms");

    // 逐项：
    let mut variants: Vec<(String, raster::RenderOpts)> = Vec::new();

    let mut o = base_opts;
    o.backface_cull = false;
    variants.push(("关背面剔除（多画一倍三角）".into(), o));

    let mut o = base_opts;
    o.wireframe = true;
    variants.push(("开线框".into(), o));

    // 纹理：造一张 1K 的底图
    let tex_w = 1024usize;
    let tex_data = vec![180u8; tex_w * tex_w * 4];
    let tex = raster::Tex {
        data: &tex_data,
        w: tex_w,
        h: tex_w,
    };

    for (tag, o) in &variants {
        let ms = bench(6, || {
            let _ = raster::render(&mesh, &cam, o, None, None);
        });
        let delta = ms - base_ms;
        println!(
            "  {tag:<34} {ms:7.2} ms  ({:+.2} ms, {:+.0}%)",
            delta,
            delta / base_ms * 100.0
        );
    }

    // mask 叠加（带 alpha 混合）
    let mask_data = vec![255u8; tex_w * tex_w * 4];
    let mask_tex = raster::Tex {
        data: &mask_data,
        w: tex_w,
        h: tex_w,
    };
    let ms = bench(6, || {
        let _ = raster::render(&mesh, &cam, &base_opts, Some(tex), Some(mask_tex));
    });
    println!(
        "  加底图 + mask（1024² 采样+混合）      {ms:7.2} ms  ({:+.2} ms, {:+.0}%)",
        ms - base_ms,
        (ms - base_ms) / base_ms * 100.0
    );

    // ---------------- 加载与预处理 ----------------
    println!("\n════════ 加载与预处理 ════════");
    let t = Instant::now();
    let _ = model3d::load_mesh(&path);
    println!(
        "  完整加载（解析 + 平滑法线）  {:.1} ms",
        t.elapsed().as_secs_f32() * 1000.0
    );

    // 单独测平滑法线
    let mut m2 = mesh.clone();
    let t = Instant::now();
    m2.compute_smooth_normals(40.0);
    println!(
        "  其中平滑法线重算            {:.1} ms",
        t.elapsed().as_secs_f32() * 1000.0
    );

    // 纹理上传（等价于 ColorImage 构造：一次 RGBA 拷贝）
    println!("\n════════ 每次重渲的固定开销 ════════");
    let t = Instant::now();
    for _ in 0..6 {
        let out = raster::render(&mesh, &cam, &base_opts, None, None);
        // ColorImage::from_rgba_unmultiplied 做的就是这个：一次连续拷贝。
        // 这里用普通 Vec 模拟，避免 examples 依赖 GUI 库。
        let copy: Vec<u8> = out.rgba.to_vec();
        std::hint::black_box(&copy);
    }
    let with_img = t.elapsed().as_secs_f32() * 1000.0 / 6.0;
    println!("  渲染 + RGBA 拷贝            {with_img:.2} ms");
    println!(
        "  纯渲染                      {base_ms:.2} ms  → 拷贝约 {:.2} ms",
        with_img - base_ms
    );

    // ---------------- 结论 ----------------
    println!("\n════════ 数据摘要 ════════");
    println!("  720×720 单次渲染：{base_ms:.2} ms");
    println!(
        "  → 60fps 预算 16.7ms，当前占 {:.0}%",
        base_ms / 16.7 * 100.0
    );
    println!("  → 拖动档 360×360：见上表");
}
