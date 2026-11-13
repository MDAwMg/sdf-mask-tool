//! 验证正式版 tile binning：(1) 与单线程逐像素一致 (2) 真实加速比。
//!
//! 这次覆盖全部渲染特性（UV、mask、线框），因为正式实现必须和单线程等价。

use std::time::Instant;

use sdf_mask_tool::model3d::{self, raster};

fn bench<F: FnMut()>(mut f: F, n: usize) -> f32 {
    f();
    let t = Instant::now();
    for _ in 0..n {
        f();
    }
    t.elapsed().as_secs_f32() * 1000.0 / n as f32
}

fn main() {
    // 模型路径从命令行传入：`cargo run --example verify_tile -- <模型1> [模型2] ...`
    let files: Vec<String> = std::env::args().skip(1).collect();
    if files.is_empty() {
        println!("用法：cargo run --example verify_tile -- <模型1> [模型2] ...");
        println!("用真实模型验证 tile 分块渲染与单线程渲染逐像素一致。");
        return;
    }

    println!("rayon 线程：{}", rayon::current_num_threads());

    let tex_w = 512usize;
    let base_tex: Vec<u8> = (0..tex_w * tex_w * 4)
        .map(|i| match i % 4 {
            3 => 255,
            k => ((i / 4 + k * 37) % 200 + 50) as u8,
        })
        .collect();
    let mask_tex: Vec<u8> = (0..tex_w * tex_w * 4)
        .map(|i| match i % 4 {
            3 => {
                if (i / 4) % 7 == 0 {
                    200
                } else {
                    0
                }
            }
            _ => 240,
        })
        .collect();
    let base = raster::Tex {
        data: &base_tex,
        w: tex_w,
        h: tex_w,
    };
    let mask = raster::Tex {
        data: &mask_tex,
        w: tex_w,
        h: tex_w,
    };

    let mut total_fail = 0;

    for f in &files {
        let p = std::path::PathBuf::from(f);
        if !p.exists() {
            continue;
        }
        let mesh = model3d::load_mesh(&p).unwrap();
        let name = p.file_stem().unwrap().to_string_lossy().to_string();
        println!("\n════ {name}  {} 三角 ════", mesh.tri_count);

        for size in [360usize, 720, 1080] {
            let mut cam = raster::Camera::default();
            cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

            for (tag, mut o) in [
                (
                    "基础",
                    raster::RenderOpts {
                        width: size,
                        height: size,
                        ..Default::default()
                    },
                ),
                (
                    "线框",
                    raster::RenderOpts {
                        width: size,
                        height: size,
                        wireframe: true,
                        ..Default::default()
                    },
                ),
                (
                    "贴图+mask",
                    raster::RenderOpts {
                        width: size,
                        height: size,
                        ..Default::default()
                    },
                ),
                (
                    "不剔除",
                    raster::RenderOpts {
                        width: size,
                        height: size,
                        backface_cull: false,
                        ..Default::default()
                    },
                ),
            ] {
                let with_tex = tag == "贴图+mask";
                let (b, m) = if with_tex {
                    (Some(base), Some(mask))
                } else {
                    (None, None)
                };

                o.parallel = false;
                let seq_ms = bench(
                    || {
                        let _ = raster::render(&mesh, &cam, &o, b, m);
                    },
                    5,
                );
                o.parallel = false;
                let seq = raster::render(&mesh, &cam, &o, b, m);

                o.parallel = true;
                let par_ms = bench(
                    || {
                        let _ = raster::render(&mesh, &cam, &o, b, m);
                    },
                    5,
                );
                let par = raster::render(&mesh, &cam, &o, b, m);

                let diff = seq
                    .rgba
                    .iter()
                    .zip(par.rgba.iter())
                    .filter(|(a, b)| a != b)
                    .count();
                // 只要**像素**完全一致就算通过。
                // `tris_drawn` 的统计口径两者本就不同（单线程在像素循环里累加，
                // tile 版本在预处理阶段统计），拿它做判定会产生假阳性。
                if diff != 0 {
                    total_fail += 1;
                    println!(
                        "  {size:>4}² {tag:<10} ✗ 像素差 {diff}（{} vs {} 三角，{} vs {} 剔除）",
                        seq.tris_drawn, par.tris_drawn, seq.culled_back, par.culled_back
                    );
                } else {
                    println!(
                        "  {size:>4}² {tag:<10} 单线程 {seq_ms:6.2} ms → tile {par_ms:6.2} ms  \
                         {:.2}×  ✓ 一致",
                        seq_ms / par_ms,
                    );
                }
            }
        }
    }

    println!("\n════════════════════════════════════");
    if total_fail == 0 {
        println!("全部通过：tile 版本与单线程逐像素一致");
    } else {
        println!("有 {total_fail} 项不一致，需要修复");
        std::process::exit(1);
    }
}
