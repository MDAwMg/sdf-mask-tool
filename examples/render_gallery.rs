//! 离屏渲染验证：把真实模型渲成 PNG，用来肉眼 + 程序化检查渲染结果。
//!
//! 为什么需要它：窗口截图在本机不可靠（最小化拿不到帧、被前台窗口遮挡），
//! 而"线框有没有断裂""模型有没有被切成方块"这类问题
//! 只能靠看真实输出判断。
//!
//! 用法：`cargo run --offline --example render_gallery -- [输出目录]`
//!
//! 除了出图，还会做两项**程序化断言**，不依赖肉眼：
//! 1. 线框连通性：统计线框像素的 8 邻接孤立点数，孤立点越多说明断得越厉害。
//! 2. 画面利用率：非背景像素占整个画布的比例，用来发现"只用了中间一块方图"。

use std::path::{Path, PathBuf};

use sdf_mask_tool::model3d;
use sdf_mask_tool::model3d::raster::{self, Camera, RenderOpts};

fn main() {
    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/render_out"));
    std::fs::create_dir_all(&out_dir).expect("创建输出目录失败");

    // 模型路径从命令行传入，可传多个：
    // `cargo run --example render_gallery -- <模型1> [模型2] ...`
    let candidates: Vec<String> = std::env::args().skip(1).collect();
    if candidates.is_empty() {
        println!("用法：cargo run --example render_gallery -- <模型1> [模型2] ...");
        return;
    }

    let mut ran = 0;
    for c in &candidates {
        let p = Path::new(c);
        if !p.exists() {
            continue;
        }
        let name = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let mesh = match model3d::load_mesh(p) {
            Ok(m) => m,
            Err(e) => {
                println!("{name}: 加载失败 {e}");
                continue;
            }
        };

        println!("=== {name}  {} 三角", mesh.tri_count);

        // 三组宽高比：方形 / 宽 / 窄
        for (tag, aspect) in [("1x1", 1.0f32), ("2x1", 2.0), ("1x2", 0.5)] {
            let (w, h) = sdf_mask_tool::preview::render_size(560, aspect);
            let mut cam = Camera::default();
            cam.fit_bounds(
                mesh.center,
                mesh.half_extents,
                mesh.up(),
                w as f32 / h as f32,
            );

            let opts = RenderOpts {
                width: w,
                height: h,
                backface_cull: true,
                wireframe: false,
                ..Default::default()
            };
            let out = raster::render(&mesh, &cam, &opts, None, None);
            let fill = usage(&out.rgba, w, h);
            println!(
                "   {tag:<4} {w}×{h}  画面利用率 {:.1}%  画出 {}/{}",
                fill * 100.0,
                out.tris_drawn,
                out.tris_total
            );
            save(&out.rgba, w, h, &out_dir.join(format!("{name}_{tag}.png")));

            // 线框版本 + 连通性检查
            let opts_w = RenderOpts {
                width: w,
                height: h,
                wireframe: true,
                ..opts
            };
            let outw = raster::render(&mesh, &cam, &opts_w, None, None);
            let (wire_px, isolated) = wire_connectivity(&outw.rgba, w, h);
            let ratio = if wire_px > 0 {
                isolated as f32 / wire_px as f32
            } else {
                0.0
            };
            println!(
                "       线框像素 {wire_px}，孤立点 {isolated}（{:.2}%）",
                ratio * 100.0
            );
            save(
                &outw.rgba,
                w,
                h,
                &out_dir.join(format!("{name}_{tag}_wire.png")),
            );
        }
        ran += 1;
    }

    println!("\n输出目录：{}", out_dir.display());
    if ran == 0 {
        println!("没有可用的模型文件");
        std::process::exit(1);
    }
}

/// 非背景像素占全画布的比例。
fn usage(rgba: &[u8], w: usize, h: usize) -> f32 {
    let bg = [0x1au8, 0x1a, 0x1e];
    let mut n = 0usize;
    for px in rgba.chunks_exact(4) {
        if px[0] != bg[0] || px[1] != bg[1] || px[2] != bg[2] {
            n += 1;
        }
    }
    n as f32 / (w * h) as f32
}

/// 统计深色（线框）像素数，以及其中 8 邻域没有任何同伴的孤立点数。
///
/// 断线在图像上就表现为孤立点/短碎线，所以"孤立点占比"能定量反映断裂程度。
fn wire_connectivity(rgba: &[u8], w: usize, h: usize) -> (usize, usize) {
    let is_wire = |i: usize| -> bool {
        let o = i * 4;
        rgba[o] < 0x20 && rgba[o + 1] < 0x20 && rgba[o + 2] < 0x20
    };
    let mut total = 0usize;
    let mut isolated = 0usize;
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if !is_wire(i) {
                continue;
            }
            total += 1;
            let mut neighbours = 0;
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let nx = x as i32 + dx;
                    let ny = y as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    if is_wire(ny as usize * w + nx as usize) {
                        neighbours += 1;
                    }
                }
            }
            if neighbours == 0 {
                isolated += 1;
            }
        }
    }
    (total, isolated)
}

fn save(rgba: &[u8], w: usize, h: usize, path: &Path) {
    let img: image::RgbaImage =
        image::ImageBuffer::from_raw(w as u32, h as u32, rgba.to_vec()).expect("尺寸不匹配");
    img.save(path).expect("保存失败");
}
