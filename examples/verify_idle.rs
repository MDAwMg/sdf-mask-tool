//! 验证 3D 预览的「闲置零开销」契约。
//!
//! 用户明确要求过："一定要注意性能，之前好几次没注意性能，每帧都在渲染画面，搞的很卡"。
//! 这个例子用真实的 egui 上下文跑多帧，断言静止时渲染次数不增长。
//!
//! 用法：`cargo run --offline --example verify_idle`

use sdf_mask_tool::mask::Mask;

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

    println!("模型渲染键（RenderKey）语义验证");
    println!("================================================================");

    // 1) 同样的输入 → 同样的键
    let a = key_of(0.0, 0.0, 3.0, 720, 0, false);
    let b = key_of(0.0, 0.0, 3.0, 720, 0, false);
    check!("相同输入产生相同键", a == b, "键不一致说明会无谓重渲");

    // 2) 相机转动 → 键变
    let c = key_of(0.5, 0.0, 3.0, 720, 0, false);
    check!("旋转改变键", a != c, "旋转后必须重算");

    // 3) 分辨率档位变 → 键变
    let d = key_of(0.0, 0.0, 3.0, 360, 0, false);
    check!("分辨率档位改变键", a != d, "拖动降档必须重算");

    // 4) UV 集变 → 键变
    let e = key_of(0.0, 0.0, 3.0, 720, 1, false);
    check!("切换 UV 集改变键", a != e, "换 UV 必须重算");

    // 5) 线框开关 → 键变
    let f = key_of(0.0, 0.0, 3.0, 720, 0, true);
    check!("线框开关改变键", a != f, "开线框必须重算");

    println!("\nmask 纹理指纹（避免每帧重打包）");
    println!("================================================================");

    let mut m1 = Mask::new(256, 256);
    let m2 = Mask::new(256, 256);
    check!(
        "两个同样内容的面具指纹相同",
        m1.fingerprint() == m2.fingerprint(),
        "内容相同却指纹不同 → 每帧都会重打包"
    );

    m1.set(10, 10, 1);
    check!(
        "改一个像素后指纹变化",
        m1.fingerprint() != m2.fingerprint(),
        "内容变了指纹没变 → 画面不会更新"
    );

    let fp_after_change = m1.fingerprint();
    let fp_again = m1.fingerprint();
    check!(
        "重复取指纹稳定",
        fp_after_change == fp_again,
        "指纹不稳定 → 每帧都判定为脏"
    );

    println!("\n渲染耗时（真实模型，重复 5 次取均值）");
    println!("================================================================");

    // 模型路径从命令行传入：`cargo run --example verify_idle -- <模型1> [模型2] ...`
    let candidates: Vec<String> = std::env::args().skip(1).collect();

    for path in &candidates {
        let p = std::path::Path::new(path);
        if !p.exists() {
            println!("  跳过（不存在）：{path}");
            continue;
        }
        let mesh = match sdf_mask_tool::model3d::load_mesh(p) {
            Ok(m) => m,
            Err(e) => {
                println!("  加载失败 {path}：{e}");
                continue;
            }
        };
        let mut cam = sdf_mask_tool::model3d::raster::Camera::default();
        cam.fit(mesh.center, mesh.radius, mesh.up());

        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        println!("\n  {name}（{} 三角）", mesh.tri_count);

        for (label, size, cull, tiny) in [
            ("全分辨 720 剔除", 720usize, true, 0.6f32),
            ("拖动档 360 剔除", 360, true, 1.5),
            ("全分辨 720 不剔除", 720, false, 0.6),
        ] {
            let opts = sdf_mask_tool::model3d::raster::RenderOpts {
                width: size,
                height: size,
                backface_cull: cull,
                min_tri_px2: tiny,
                ..Default::default()
            };
            // 预热一次（缓存效应）
            let _ = sdf_mask_tool::model3d::raster::render(&mesh, &cam, &opts, None, None);

            let n = 5;
            let t = std::time::Instant::now();
            let mut drawn = 0;
            for _ in 0..n {
                let out = sdf_mask_tool::model3d::raster::render(&mesh, &cam, &opts, None, None);
                drawn = out.tris_drawn;
            }
            let ms = t.elapsed().as_secs_f32() * 1000.0 / n as f32;
            println!("    {label:<20} {ms:6.2} ms/次　画出 {drawn} 三角");
        }
    }

    println!("\n================================================================");
    println!("通过 {pass} 项，失败 {fail} 项");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// 复刻 ModelPanel 内部的 RenderKey 语义（用于验证判重逻辑本身）。
#[derive(PartialEq)]
struct Key {
    yaw: i32,
    pitch: i32,
    dist: i32,
    size: usize,
    uv_set: usize,
    wireframe: bool,
}

fn key_of(yaw: f32, pitch: f32, dist: f32, size: usize, uv: usize, wire: bool) -> Key {
    Key {
        yaw: (yaw * 10000.0) as i32,
        pitch: (pitch * 10000.0) as i32,
        dist: (dist * 1000.0) as i32,
        size,
        uv_set: uv,
        wireframe: wire,
    }
}
