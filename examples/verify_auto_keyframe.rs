//! 验证「在没有关键帧的地方绘制时自动插帧」是否可行、是否安全。
//!
//! 用户要求：在没有帧的地方用画笔/形状绘制时，自动插帧然后绘画。
//!
//! 需要验证四件事：
//!  1. 自动插帧后的起点内容 == 屏幕上正显示的内容（用户看到什么就画在什么上）
//!  2. 在插值起点上继续画，数据仍可能合法（嵌套不被破坏）
//!  3. 画超出范围时能被检测出来（不能悄悄产生错误数据）
//!  4. 自动插帧的成本（必须 < 一帧预算，否则画笔会顿）
//!
//! 用法：`cargo run --release --offline --example verify_auto_keyframe`

use std::time::Instant;

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::model::nesting;
use sdf_mask_tool::sdf;

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

    println!("自动插帧绘制可行性　画布 {W}×{H}");
    println!("================================================================");

    let steps = 32usize;

    // ---------- 1. 起点 == 屏幕内容 ----------
    println!("\n[1] 自动插帧的起点必须等于屏幕上正显示的内容");
    {
        let mut doc = Doc::with_steps(W, H, steps);
        doc.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        doc.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));

        // 播放头停在 45°（这格没有关键帧）
        let step = sdf_mask_tool::model::doc::angle_to_step(45.0, steps);
        let has_kf = doc.active_channel_ref().has_kf_at_step(step, steps);
        check!("45° 那格确实没有关键帧（前提）", !has_kf, "那里已有关键帧");

        // 屏幕显示的内容（texel 采样路径）
        let (dw, dh) = (doc.width, doc.height);
        let (texels, dir) = doc.active_channel_mut().texels(dw, dh).expect("应能烘焙");
        let t = 0.5;
        let mut on_screen = Mask::new(W, H);
        sdf::sample_texels(&texels, t, dir, &mut on_screen, false);

        // 自动插帧：把屏幕内容作为新帧
        let a = 45.0;
        doc.active_channel_mut().insert_frame(a, on_screen.clone());

        let inserted = doc.active_channel_ref().frame_at(a).expect("应能取到");
        let diff = (0..(W * H) as usize)
            .filter(|&i| inserted.mask.data()[i] != on_screen.data()[i])
            .count();
        check!(
            format!("新帧内容 == 插帧前的屏幕内容（差异 {diff} 像素）"),
            diff == 0,
            format!("{diff} 像素不同——用户会看到画面跳变")
        );
    }

    // ---------- 2. 在插值起点上继续画，数据是否仍合法 ----------
    println!("\n[2] 在插值起点上继续画，嵌套是否被破坏");
    {
        let mut doc = Doc::with_steps(W, H, steps);
        doc.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        doc.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let (dw, dh) = (doc.width, doc.height);
        let (texels, dir) = doc.active_channel_mut().texels(dw, dh).unwrap();
        let mut mid = Mask::new(W, H);
        sdf::sample_texels(&texels, 0.5, dir, &mut mid, false);
        doc.active_channel_mut().insert_frame(45.0, mid.clone());

        // 在 45° 帧上"擦掉"一小块（用户想减点阴影）
        let mut erased = mid.clone();
        for y in 480..540u32 {
            for x in 480..540u32 {
                erased.set(x, y, 0);
            }
        }
        doc.active_channel_ref(); // 保持借用干净
        let mut d2 = doc.clone();
        if let Some(f) = d2.active_channel_mut().frame_at_mut(45.0) {
            f.mask = erased;
        }
        let refs: Vec<&Mask> = d2
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| &f.mask)
            .collect();
        let plan = nesting::plan(&refs, W as usize, H as usize);
        check!(
            format!(
                "在中间帧擦一块后：违规 {} 像素（扩张方向）",
                plan.violations.count
            ),
            plan.violations.count > 0,
            "擦掉内容却不算违规——检测漏了".to_string()
        );
        println!("    说明：在中间帧做**减法**会产生违规（扩张方向要求后一帧更大），");
        println!("    这是正确的检测行为，用户可以用「自动修正」或改用收缩方向。");
    }

    // ---------- 3. 加内容是否合法 ----------
    println!("\n[3] 在中间帧**加**内容是否合法");
    {
        let mut doc = Doc::with_steps(W, H, steps);
        doc.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        doc.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let (dw, dh) = (doc.width, doc.height);
        let (texels, dir) = doc.active_channel_mut().texels(dw, dh).unwrap();
        let mut mid = Mask::new(W, H);
        sdf::sample_texels(&texels, 0.5, dir, &mut mid, false);

        // 在 45° 帧上加点东西（不超出 90° 帧的范围）
        let mut added = mid.clone();
        for y in 470..490u32 {
            for x in 470..490u32 {
                added.set(x, y, 1);
            }
        }
        doc.active_channel_mut().insert_frame(45.0, added);

        let refs: Vec<&Mask> = doc
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| &f.mask)
            .collect();
        let plan = nesting::plan(&refs, W as usize, H as usize);
        // 我加的那块在 90° 帧的范围内 → 仍满足"后一帧更大" → 合法。
        // 这是**期望**行为：自动插帧后随手加内容不该被判违规。
        check!(
            format!("加一小块后违规 {} 像素（应为 0）", plan.violations.count),
            plan.violations.count == 0,
            format!(
                "违规 {} 像素——在范围内加内容不该破坏嵌套",
                plan.violations.count
            )
        );
        println!("    说明：只要新加的内容不超出后一帧的范围，扩张方向仍然合法。");
    }

    // ---------- 4. 自动插帧的成本 ----------
    println!("\n[4] 自动插帧的成本（必须在画笔落下前完成）");
    {
        let mut doc = Doc::with_steps(W, H, steps);
        doc.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        doc.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let (dw, dh) = (doc.width, doc.height);
        let _ = doc.active_channel_mut().texels(dw, dh);

        // 复刻：取屏幕内容（已有）+ 插入 + 压快照
        let mut disp = Mask::new(W, H);
        let (texels, dir) = doc.active_channel_mut().texels(dw, dh).unwrap();
        sdf::sample_texels(&texels, 0.5, dir, &mut disp, false);

        let t0 = Instant::now();
        let _snap = doc.clone(); // 撤销快照
        doc.active_channel_mut().insert_frame(45.0, disp);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        check!(
            format!("自动插帧 + 撤销快照耗时 {ms:.2} ms"),
            ms < 30.0,
            format!("{ms:.1} ms 太慢，画笔会顿")
        );
    }

    // ---------- 5. 通道为空时的行为 ----------
    println!("\n[5] 通道一张帧都没有时绘制");
    {
        let mut doc = Doc::with_steps(W, H, steps);
        // 什么都不加
        let step = sdf_mask_tool::model::doc::angle_to_step(45.0, steps);
        let has_kf = doc.active_channel_ref().has_kf_at_step(step, steps);
        let empty = doc.active_channel_ref().frames.is_empty();
        check!(
            format!("空通道：帧数 0={empty}，该格有关键帧={has_kf}"),
            empty && !has_kf,
            "前提不成立".to_string()
        );
        // 应当建空白帧（没有相邻帧可插值）
        let blank = doc.blank_mask();
        doc.active_channel_mut().insert_frame(45.0, blank);
        let n = doc.active_channel_ref().frames.len();
        check!(
            format!("空通道绘制 → 建 1 个空白帧（现在 {n} 帧）"),
            n == 1,
            format!("建了 {n} 帧")
        );
        println!("    注意：只有 1 帧时无法导出（至少要 2 帧），需要提示用户。");
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
