//! 用真实模型文件验证解析器与光栅器。
//!
//! 单元测试用的是合成数据，只能证明"逻辑自洽"；这个例子跑的是磁盘上的
//! 真实文件，能证明"真的能读别人的文件"。
//!
//! 用法：
//! ```text
//! cargo run --offline --example verify_models -- <文件>...
//! ```
//! 不传参数时使用内置的默认路径列表。

use std::path::PathBuf;

use sdf_mask_tool::model3d::{self, raster};
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("用法：cargo run --example verify_models -- <模型文件>...");
        println!("提供若干 .obj / .fbx 路径以验证解析器与光栅器。");
        return;
    }
    let files: Vec<PathBuf> = args.into_iter().map(PathBuf::from).collect();

    let mut ok = 0;
    let mut fail = 0;

    for path in &files {
        println!("================================================================");
        println!("{}", path.display());

        if !path.exists() {
            println!("  文件不存在，跳过");
            continue;
        }

        let t0 = Instant::now();
        match model3d::load_mesh(path) {
            Ok(mesh) => {
                let load_ms = t0.elapsed().as_secs_f32() * 1000.0;
                println!(
                    "  解析成功：{} 三角 / {} 顶点 / {} 套UV　（{load_ms:.1} ms）",
                    mesh.tri_count, mesh.vert_count, mesh.stats.uv_sets
                );
                println!(
                    "  包围球：中心 [{:.3}, {:.3}, {:.3}] 半径 {:.3}",
                    mesh.center[0], mesh.center[1], mesh.center[2], mesh.radius
                );
                if mesh.stats.should_warn() {
                    println!("  ⚠ 面数较多，预览可能卡顿");
                }

                // ---- 光栅器实测：静止全分辨率 + 拖动半分辨率 ----
                let mut cam = raster::Camera::default();
                cam.fit(mesh.center, mesh.radius, mesh.up());

                for (label, size, cull) in [
                    ("全分辨率 720", 720usize, true),
                    ("拖动档 360", 360, true),
                    ("全分辨 不剔除", 720, false),
                ] {
                    let opts = raster::RenderOpts {
                        width: size,
                        height: size,
                        backface_cull: cull,
                        ..Default::default()
                    };
                    let t = Instant::now();
                    let out = raster::render(&mesh, &cam, &opts, None, None);
                    let ms = t.elapsed().as_secs_f32() * 1000.0;

                    // 统计非背景像素比例，确认真的画出了东西
                    let drawn_px = out
                        .rgba
                        .chunks_exact(4)
                        .filter(|p| p[0] > 0x20 || p[1] > 0x20 || p[2] > 0x20)
                        .count();
                    let total = out.width * out.height;

                    println!(
                        "    {label:<14} {ms:7.2} ms  画出 {}/{} 三角 \
                         剔除(背{} 微{}) 裁剪{}  覆盖 {:.1}%",
                        out.tris_drawn,
                        out.tris_total,
                        out.culled_back,
                        out.culled_tiny,
                        out.clipped,
                        drawn_px as f32 / total as f32 * 100.0
                    );
                }

                // ---- 相机操作不 panic ----
                cam.orbit(40.0, 20.0);
                cam.pan(30.0, -15.0, 720.0);
                cam.zoom(0.9);
                let out = raster::render(&mesh, &cam, &raster::RenderOpts::default(), None, None);
                println!("  旋转/平移/缩放后渲染 OK（{} 三角）", out.tris_drawn);

                ok += 1;
            }
            Err(e) => {
                println!("  解析失败：{e}");
                fail += 1;
            }
        }
    }

    println!("================================================================");
    println!("成功 {ok} 个，失败 {fail} 个");
    if fail > 0 {
        std::process::exit(1);
    }
}
