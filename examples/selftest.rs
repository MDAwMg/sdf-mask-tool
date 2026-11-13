//! 端到端自检：管线 + 本轮新增的编辑/导出功能。
//!
//! 运行：cargo run --example selftest
//! 输出图写到 target/selftest/，可以直接打开看。
//!
//! 所有断言用 ok &= 累加，最后给出总结论——中途失败也能看完全部结果。

use sdf_mask_tool::io::{self, BitDepth, ExportOptions};
use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::{
    DEFAULT_STEPS, Doc, angle_to_step, norm_to_step, step_angle, step_norm,
};
use sdf_mask_tool::paint;
use sdf_mask_tool::sdf;

/// 测试统一用默认步数
const TS: usize = DEFAULT_STEPS;

fn left_half(w: u32, h: u32, cols: u32) -> Mask {
    let mut m = Mask::new(w, h);
    for y in 0..h {
        for x in 0..cols.min(w) {
            m.set(x, y, 1);
        }
    }
    m
}

fn main() {
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/selftest");
    std::fs::create_dir_all(&out).unwrap();
    let n = 256u32;
    let mut ok = true;

    let mut doc = Doc::new(n, n);

    // 用真实步角，别写 45°——45 不在 31 等分点上，会被吸附走
    let a_mid = step_angle(angle_to_step(45.0, TS), TS);

    // 0°：右下角一小块固有遮罩（用笔刷画）
    let mut m0 = Mask::new(n, n);
    paint::stamp(&mut m0, 200.0, 200.0, 30.0, false);
    doc.active_channel_mut().insert_frame(step_angle(0, TS), m0);

    // 中段：左半边加遮罩
    let mut m1 = left_half(n, n, n / 2);
    paint::stamp(&mut m1, 200.0, 200.0, 30.0, false);
    doc.active_channel_mut().insert_frame(a_mid, m1);

    // 90°：左三分之二加遮罩
    let mut m2 = left_half(n, n, n * 2 / 3);
    paint::stamp(&mut m2, 200.0, 200.0, 30.0, false);
    doc.active_channel_mut()
        .insert_frame(step_angle(TS - 1, TS), m2);

    println!(
        "关键帧：{:?}",
        doc.active_channel_ref()
            .frames
            .iter()
            .map(|f| format!("{:.2}°", f.angle))
            .collect::<Vec<_>>()
    );

    // ---------------------------------------------------------- 烘焙
    let baked = io::bake_channel(doc.active_channel_ref(), n, n).expect("烘焙失败");
    println!("编码方向：{}", baked.direction.label());
    let sdf_img = baked.to_gray8();
    let p = out.join("sdf.png");
    sdf_img.save(&p).unwrap();
    println!("SDF 贴图 → {}", p.display());

    // 各角度还原拼成对比图
    let angles = [0.0f32, 15.0, 30.0, 45.0, 60.0, 75.0, 90.0];
    let cols = angles.len() as u32;
    let mut sheet = image::GrayImage::new(n * cols, n);
    for (i, a) in angles.iter().enumerate() {
        let r = baked.restore_at(*a / 90.0);
        for y in 0..n {
            for x in 0..n {
                sheet.put_pixel(i as u32 * n + x, y, *r.get_pixel(x, y));
            }
        }
    }
    let p2 = out.join("restored_sheet.png");
    sheet.save(&p2).unwrap();
    println!("还原对比 → {}", p2.display());

    // 导出（8bit / 16bit）
    let p8 = out.join("export8.png");
    io::export_layer_opts(
        &doc,
        0,
        &p8,
        &ExportOptions {
            bit_depth: BitDepth::Bit8,
            invert: false,
        },
    )
    .unwrap();
    let p16 = out.join("export16.png");
    io::export_layer_opts(
        &doc,
        0,
        &p16,
        &ExportOptions {
            bit_depth: BitDepth::Bit16,
            invert: false,
        },
    )
    .unwrap();
    println!("导出 8bit → {}  16bit → {}", p8.display(), p16.display());

    // ---------------------------------------------------------- 校验
    println!("\n--- SDF 还原校验 ---");
    let check = |a: f32, x: u32, y: u32, want_lit: bool, why: &str| {
        let got = baked.restore_at(a / 90.0).get_pixel(x, y).0[0] > 127;
        let pass = got == want_lit;
        println!(
            "  {a}° @({x},{y}) 期望{} 实得{} {} {}",
            if want_lit { "不遮" } else { "被遮" },
            if got { "不遮" } else { "被遮" },
            if pass { "OK" } else { "!!" },
            why
        );
        pass
    };
    ok &= check(0.0, 10, 128, true, "左侧 0° 不遮");
    ok &= check(90.0, 10, 128, false, "左侧 90° 被遮");
    ok &= check(0.0, 240, 128, true, "右侧 0° 不遮");
    ok &= check(90.0, 240, 128, true, "右侧 90° 仍不遮");
    ok &= check(0.0, 200, 200, false, "固有遮罩 0° 就被遮");

    // ---------------------------------------------------------- 本轮功能
    println!("\n--- 本轮新增功能 ---");

    // 1. 时间轴归一化：0..1
    {
        let pass = (step_norm(0, TS) - 0.0).abs() < 1e-6
            && (step_norm(DEFAULT_STEPS - 1, TS) - 1.0).abs() < 1e-6
            && norm_to_step(1.0, TS) == DEFAULT_STEPS - 1
            && norm_to_step(0.0, TS) == 0;
        println!("  时间轴 0..1 归一化 {} ", if pass { "OK" } else { "!!" });
        ok &= pass;
    }

    // 2. 步角往返一致
    {
        let mut pass = true;
        for s in 0..DEFAULT_STEPS {
            if angle_to_step(step_angle(s, TS), TS) != s {
                pass = false;
            }
        }
        println!("  步角往返一致 {} ", if pass { "OK" } else { "!!" });
        ok &= pass;
    }

    // 3. 16bit 精度确实高于 8bit
    {
        let img8 = image::open(&p8).unwrap().to_luma8();
        let img16 = image::open(&p16).unwrap().to_luma16();
        let v8 = img8.get_pixel(64, 128).0[0] as u32;
        let v16 = img16.get_pixel(64, 128).0[0] as u32;
        let pass = v16 > v8 as u32 * 4 && v16 < 65535;
        println!(
            "  16bit 精度：同一点 8bit={v8} 16bit={v16} {} ",
            if pass { "OK" } else { "!!" }
        );
        ok &= pass;
    }

    // 4. 反向开关
    {
        let pi = out.join("export_inv.png");
        io::export_layer_opts(
            &doc,
            0,
            &pi,
            &ExportOptions {
                bit_depth: BitDepth::Bit8,
                invert: true,
            },
        )
        .unwrap();
        let a = image::open(&p8).unwrap().to_luma8().get_pixel(128, 128).0[0];
        let b = image::open(&pi).unwrap().to_luma8().get_pixel(128, 128).0[0];
        let pass = b < a;
        println!(
            "  反向导出：正常={a} 反向={b} {} ",
            if pass { "OK" } else { "!!" }
        );
        ok &= pass;
    }

    // 5. 阴影收缩自动改方向
    {
        let mut d = Doc::new(64, 64);
        d.active_channel_mut()
            .insert_frame(step_angle(0, TS), left_half(64, 64, 48));
        d.active_channel_mut()
            .insert_frame(step_angle(TS - 1, TS), left_half(64, 64, 16));
        let r = io::bake_channel(d.active_channel_ref(), 64, 64);
        let pass = matches!(r, Ok(ref b) if b.direction == sdf::Direction::ShadowShrinks);
        println!("  收缩方向自动判定 {} ", if pass { "OK" } else { "!!" });
        ok &= pass;
    }

    // 6. 来回翻转必须拒绝导出
    {
        let mut d = Doc::new(64, 64);
        d.active_channel_mut()
            .insert_frame(step_angle(0, TS), left_half(64, 64, 48));
        d.active_channel_mut()
            .insert_frame(step_angle(TS / 6, TS), left_half(64, 64, 16));
        d.active_channel_mut()
            .insert_frame(step_angle(TS / 3, TS), left_half(64, 64, 48));
        d.active_channel_mut()
            .insert_frame(step_angle(TS - 1, TS), left_half(64, 64, 16));
        let r = io::bake_channel(d.active_channel_ref(), 64, 64);
        let pass = r.is_err();
        println!("  来回翻转被拒绝 {} ", if pass { "OK" } else { "!!" });
        ok &= pass;
    }

    // 7. 笔刷半径（回归：曾经"改不了大小"）
    {
        let mut small = Mask::new(128, 128);
        paint::stamp(&mut small, 64.0, 64.0, 4.0, false);
        let mut big = Mask::new(128, 128);
        paint::stamp(&mut big, 64.0, 64.0, 32.0, false);
        let ratio = big.count() as f64 / small.count() as f64;
        let pass = (ratio - 64.0).abs() < 8.0;
        println!(
            "  笔刷半径：r4={} r32={} 面积比 {:.1}（理论 64） {} ",
            small.count(),
            big.count(),
            ratio,
            if pass { "OK" } else { "!!" }
        );
        ok &= pass;
    }

    // 8. 关键帧复制、导航
    {
        let mut d = Doc::new(32, 32);
        let mut m = Mask::new(32, 32);
        paint::stamp(&mut m, 16.0, 16.0, 6.0, false);
        d.active_channel_mut()
            .insert_frame(step_angle(0, TS), m.clone());
        d.active_channel_mut()
            .insert_frame(step_angle(TS - 1, TS), m);
        let before = d.active_channel_ref().frames.len();
        let na = d.duplicate_keyframe(0, TS);
        let pass = na.is_some() && d.active_channel_ref().frames.len() == before + 1;
        println!(
            "  复制关键帧 → {:?} {} ",
            na.map(|a| format!("{a:.1}°")),
            if pass { "OK" } else { "!!" }
        );
        ok &= pass;
    }

    // 9. 通道交换
    {
        let mut d = Doc::new(32, 32);
        d.add_layer("B");
        d.layers[0].channels[0].insert_frame(step_angle(0, TS), left_half(32, 32, 8));
        d.layers[1].channels[1].insert_frame(step_angle(0, TS), left_half(32, 32, 20));
        let a0 = d.layers[0].channels[0].frames[0].mask.count();
        let b0 = d.layers[1].channels[1].frames[0].mask.count();
        d.swap_channel((0, 0), (1, 1));
        let pass = d.layers[0].channels[0].frames[0].mask.count() == b0
            && d.layers[1].channels[1].frames[0].mask.count() == a0;
        println!("  通道交换 {} ", if pass { "OK" } else { "!!" });
        ok &= pass;
    }

    // 10. 改画布尺寸保持二值
    {
        let mut d = Doc::new(64, 64);
        d.active_channel_mut()
            .insert_frame(step_angle(0, TS), left_half(64, 64, 32));
        d.resize(37, 37);
        let f = d.active_channel_ref().frames[0].mask.clone();
        let pass = f.data().iter().all(|&v| v == 0 || v == 1) && f.width() == 37;
        println!("  改画布尺寸仍二值 {} ", if pass { "OK" } else { "!!" });
        ok &= pass;
    }

    // 11. 工程存盘往返
    {
        let proj = out.join("selftest.sdfproj");
        io::save_project(&doc, &proj).unwrap();
        let back = io::open_project(&proj).unwrap();
        let pass =
            back.layers.len() == doc.layers.len() && back.layers[0].channels[0].frames.len() == 3;
        println!(
            "  存盘读回：{} 图层 {} 帧 {} ",
            back.layers.len(),
            back.layers[0].channels[0].frames.len(),
            if pass { "OK" } else { "!!" }
        );
        ok &= pass;
    }

    println!(
        "\n{}",
        if ok {
            "全部检查通过"
        } else {
            "!! 有检查未通过"
        }
    );
    std::process::exit(if ok { 0 } else { 1 });
}
