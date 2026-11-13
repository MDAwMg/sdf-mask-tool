//! 判定 3D 预览当前用的是左手还是右手坐标系。
//!
//! 判据不依赖形状对称性：把**单个**标记放在世界 +X（或 +Y）一侧，
//! 量它在屏幕上的质心位置。
//!
//! - 世界 +X 落在屏幕右侧、+Y 落在屏幕上方，且相机位于 +Z：
//!   (X, Y, Z) = (右, 上, 朝观察者)，叉积 X × Y = Z，**右手系**
//! - 世界 +X 落在屏幕左侧（相机仍在 +Z）：等价于 Z 轴逆序，**左手系**
//!
//! 用法：cargo run --offline --example verify_handedness

use sdf_mask_tool::model3d::mesh::Mesh;
use sdf_mask_tool::model3d::raster::{self, Camera, RenderOpts};

/// 造一个只含一个三角形的网格，三角形中心位于 `origin`。
fn marker_at(origin: [f32; 3], size: f32) -> Mesh {
    let mut m = Mesh::default();
    let [ox, oy, oz] = origin;
    let tri = [
        [ox - size, oy - size, oz],
        [ox + size, oy - size, oz],
        [ox, oy + size, oz],
    ];
    for v in tri {
        m.pos.push(v);
    }
    m.normals.push([0.0, 0.0, 1.0]);
    m.vidx.push([0, 1, 2]);
    m.tri_count = 1;
    m.vert_count = 3;
    m.center = [0.0, 0.0, 0.0];
    m.radius = 3.0;
    m.half_extents = [3.0, 3.0, 0.0];
    m.up_axis = 1; // Y-up
    m
}

/// 亮像素的质心（以像素为单位），没有内容时返回 None。
///
/// 背景是 `[0x1a, 0x1a, 0x1e, 0xff]`（**不透明**），所以不能用 alpha 判断
/// 有没有内容——整张图 alpha 都是 255，质心会恒等于画面中心。
/// 必须与背景色做差异比较。
fn centroid(out: &raster::RenderOut) -> Option<(f64, f64)> {
    const BG: [u8; 3] = [0x1a, 0x1a, 0x1e];
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut n = 0u32;
    for y in 0..out.height {
        for x in 0..out.width {
            let i = (y * out.width + x) * 4;
            let d = (out.rgba[i] as i32 - BG[0] as i32).abs()
                + (out.rgba[i + 1] as i32 - BG[1] as i32).abs()
                + (out.rgba[i + 2] as i32 - BG[2] as i32).abs();
            // 抗锯齿边缘差异很小，阈值 12 只保留实质着色区域
            if d > 12 {
                sx += x as f64;
                sy += y as f64;
                n += 1;
            }
        }
    }
    if n == 0 {
        None
    } else {
        Some((sx / n as f64, sy / n as f64))
    }
}

fn render_marker(origin: [f32; 3]) -> raster::RenderOut {
    let mesh = marker_at(origin, 0.5);
    let mut cam = Camera::default();
    cam.fit([0.0, 0.0, 0.0], 3.0, [0.0, 1.0, 0.0]);
    let opts = RenderOpts {
        width: 400,
        height: 400,
        backface_cull: false, // 只测位置，不受法线朝向影响
        parallel: false,
        ..Default::default()
    };
    raster::render(&mesh, &cam, &opts, None, None)
}

fn main() {
    let mut cam = Camera::default();
    cam.fit([0.0, 0.0, 0.0], 3.0, [0.0, 1.0, 0.0]);
    let eye = cam.eye();
    let (r, u, f) = cam.basis();

    println!("相机 eye     = [{:.3}, {:.3}, {:.3}]", eye[0], eye[1], eye[2]);
    println!("相机 forward = [{:.3}, {:.3}, {:.3}]", f[0], f[1], f[2]);
    println!("相机 right   = [{:.3}, {:.3}, {:.3}]", r[0], r[1], r[2]);
    println!("相机 up      = [{:.3}, {:.3}, {:.3}]", u[0], u[1], u[2]);

    // 基向量手性：right × up 应等于 -forward（相机看向自身 -Z）
    let cr = [
        r[1] * u[2] - r[2] * u[1],
        r[2] * u[0] - r[0] * u[2],
        r[0] * u[1] - r[1] * u[0],
    ];
    let matches = (cr[0] + f[0]).abs() < 1e-4
        && (cr[1] + f[1]).abs() < 1e-4
        && (cr[2] + f[2]).abs() < 1e-4;
    println!(
        "right × up = [{:.3}, {:.3}, {:.3}]   -forward = [{:.3}, {:.3}, {:.3}]  → {}",
        cr[0],
        cr[1],
        cr[2],
        -f[0],
        -f[1],
        -f[2],
        if matches {
            "右手（right × up = -forward）"
        } else {
            "左手（right × up = +forward）"
        }
    );

    // 世界 +X 标记：应当落在屏幕右半
    let plus_x = render_marker([1.8, 0.0, 0.0]);
    let cx = centroid(&plus_x).expect("+X 标记没有画出来");
    let x_on_right = cx.0 > plus_x.width as f64 / 2.0;
    println!(
        "\n世界 +X 标记：屏幕质心 x = {:.1} / {}  → {}侧",
        cx.0,
        plus_x.width,
        if x_on_right { "右" } else { "左" }
    );

    // 世界 +Y 标记：应当落在屏幕上半（图像 y 向下，所以质心 y 小于中位）
    let plus_y = render_marker([0.0, 1.8, 0.0]);
    let cy = centroid(&plus_y).expect("+Y 标记没有画出来");
    let y_on_top = cy.1 < plus_y.height as f64 / 2.0;
    println!(
        "世界 +Y 标记：屏幕质心 y = {:.1} / {}  → {}方",
        cy.1,
        plus_y.height,
        if y_on_top { "上" } else { "下" }
    );

    println!("\n相机位于 world Z = {:.3}（{}）", eye[2], if eye[2] > 0.0 { "+Z 侧" } else { "-Z 侧" });

    // 综合判定
    // 右手系：从 +Z 看，(X 右, Y 上)，即 X × Y = Z 指向观察者
    // 左手系：同样从 +Z 看，(X 左, Y 上)，等价于 Z 指向屏幕内部
    let right_handed = (eye[2] > 0.0) == x_on_right;
    println!(
        "综合判定：世界坐标系是 {}",
        if right_handed { "右手系" } else { "左手系" }
    );
    println!(
        "         （从 {} 侧观察时，世界 +X 落在屏幕{}侧）",
        if eye[2] > 0.0 { "+Z" } else { "-Z" },
        if x_on_right { "右" } else { "左" }
    );
}
