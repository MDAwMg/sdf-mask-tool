//! 用棋盘纹理渲染模型当前默认视角，便于判断姿态与朝向是否正确。
//!
//! 用法：cargo run --offline --example shot_model -- <模型文件> [输出png]

use sdf_mask_tool::model3d::{self, raster};
use std::path::PathBuf;

fn checker(w: usize, h: usize, cell: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let on = ((x / cell) + (y / cell)) % 2 == 0;
            let val = if on { 225u8 } else { 55 };
            let i = (y * w + x) * 4;
            v[i] = val;
            v[i + 1] = val;
            v[i + 2] = val;
            v[i + 3] = 255;
        }
    }
    v
}

fn main() {
    let path: PathBuf = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            println!("用法：cargo run --example shot_model -- <模型文件> [输出png]");
            return;
        }
    };
    let out_path = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("model_shot.png"));

    let mesh = match model3d::load_mesh(&path) {
        Ok(m) => m,
        Err(e) => {
            println!("加载失败：{e}");
            return;
        }
    };

    let mut cam = raster::Camera::default();
    cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);

    let opts = raster::RenderOpts {
        width: 512,
        height: 512,
        backface_cull: true,
        parallel: false,
        ..Default::default()
    };

    // 模型没有 UV 时退回平面着色
    let tex_data = checker(256, 256, 16);
    let use_tex = mesh.stats.uv_sets > 0;
    let tex = raster::Tex {
        data: &tex_data,
        w: 256,
        h: 256,
    };
    let out = raster::render(
        &mesh,
        &cam,
        &opts,
        if use_tex { Some(tex) } else { None },
        None,
    );

    let mut img = image::RgbaImage::new(out.width as u32, out.height as u32);
    for y in 0..out.height {
        for x in 0..out.width {
            let i = (y * out.width + x) * 4;
            img.put_pixel(
                x as u32,
                y as u32,
                image::Rgba([
                    out.rgba[i],
                    out.rgba[i + 1],
                    out.rgba[i + 2],
                    out.rgba[i + 3],
                ]),
            );
        }
    }
    img.save(&out_path).unwrap();

    let eye = cam.eye();
    println!("上轴 = '{}' {:?}", mesh.up_axis_name(), mesh.up());
    println!("eye = [{:.3}, {:.3}, {:.3}]", eye[0], eye[1], eye[2]);
    println!("画出 {}/{} 三角", out.tris_drawn, out.tris_total);
    println!("图已保存：{}", out_path.display());
}
