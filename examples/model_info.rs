//! 打印模型沿每个轴的正负两端顶点数，配合可视化用来认朝向。
//!
//! 用法：cargo run --offline --example model_info -- <模型文件>

use sdf_mask_tool::model3d;
use std::path::PathBuf;

fn main() {
    let path: PathBuf = match std::env::args().nth(1) {
        Some(p) => PathBuf::from(p),
        None => {
            println!("用法：cargo run --example model_info -- <模型文件>");
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

    println!("文件：{}", path.display());
    println!(
        "三角 {}  顶点 {}  UV 套数 {}",
        mesh.tri_count, mesh.vert_count, mesh.stats.uv_sets
    );
    println!(
        "包围盒中心 [{:.4}, {:.4}, {:.4}]  半轴 [{:.4}, {:.4}, {:.4}]",
        mesh.center[0], mesh.center[1], mesh.center[2],
        mesh.half_extents[0], mesh.half_extents[1], mesh.half_extents[2]
    );
    println!("上轴 = '{}'  {:?}", mesh.up_axis_name(), mesh.up());

    let ex = mesh.half_extents;
    let guess = if ex[0] > ex[1] && ex[0] > ex[2] {
        0
    } else if ex[2] > ex[1] {
        2
    } else {
        1
    };
    println!(
        "若按「包围盒最长边」猜会得到 '{}' 轴：{}",
        ["X", "Y", "Z"][guess],
        if guess == mesh.up_axis as usize {
            "与当前一致"
        } else {
            "与当前不同 → 用的是文件里的权威 UpAxis"
        }
    );

    let mut cam = model3d::raster::Camera::default();
    cam.fit_bounds(mesh.center, mesh.half_extents, mesh.up(), 1.0);
    let eye = cam.eye();
    println!(
        "\n默认取景：eye = [{:.3}, {:.3}, {:.3}]  dist = {:.3}",
        eye[0], eye[1], eye[2], cam.dist
    );
}
