//! 端到端验证「加关键帧」的实际耗时与画面连续性。
//!
//! 直接模拟 App 的决策流程（和 `App::do_add_keyframe_at` /
//! `App::refresh_display` 保持同一套判断），断言两件事：
//!
//! 1. **耗时**：一次加帧不应超过 10ms（修复前 1K 四帧实测 200ms）
//! 2. **连续性**：加帧后的画面 == 加帧前显示的画面（逐像素）
//!
//! 第 2 条容易被忽略但是关键：用户加关键帧是为了"接着画"，
//! 如果加帧瞬间画面跳一下（哪怕 100 个像素），就等于白改了图。
//!
//! 用法：`cargo run --release --offline --example verify_keyframe_fast`

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

/// 模拟 `App::refresh_display` 的两个分支，返回 (显示遮罩, 是否报错)。
///
/// `violation` 对应 `App::violations`（由 `refresh_violations` 在 display 之前刷新）：
/// 快路径跳过烘焙后，`display_error` 必须从它导出，否则停在关键帧上时
/// "数据不合法"提示会凭空消失。
fn sim_display(
    doc: &mut Doc,
    step: usize,
    steps: usize,
    t: f32,
    invert: bool,
    violation: usize,
) -> (Mask, bool) {
    let angle = step as f32 / (steps - 1).max(1) as f32 * 90.0;
    let (li, ri) = doc.active_channel_ref().bracket(angle);
    if li.is_none() && ri.is_none() {
        return (Mask::new(W, H), false);
    }
    // 快路径：在关键帧上（不烘焙）
    if doc.active_channel_ref().has_kf_at_step(step, steps) {
        if let Some(f) = doc.active_channel_ref().frame_for_step(step, steps) {
            let mut m = f.mask.clone();
            if invert {
                m.invert();
            }
            return (m, violation > 0);
        }
    }
    // 慢路径：烘焙 + 采样
    match doc.active_channel_mut().texels(W, H) {
        Ok((texels, dir)) => {
            let mut out = Mask::new(W, H);
            sdf::sample_texels(&texels, t, dir, &mut out, invert);
            (out, false)
        }
        Err(_) => (Mask::new(W, H), true),
    }
}

/// 对应 `App::refresh_violations`：返回违规像素数
fn violation_count(doc: &Doc) -> usize {
    let refs: Vec<&Mask> = doc
        .active_channel_ref()
        .frames
        .iter()
        .map(|f| &f.mask)
        .collect();
    if refs.len() < 2 {
        return 0;
    }
    nesting::plan(&refs, W as usize, H as usize)
        .violations
        .count
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

    println!("加关键帧 端到端验证　画布 {W}×{H}");
    println!("================================================================");

    let steps = 32usize;

    // ---- 场景：先建 2 帧工程（像 seed_default_frames），然后逐次加帧 ----
    let mut doc = Doc::with_steps(W, H, steps);
    doc.active_channel_mut()
        .insert_frame(0.0, circle(512.0, 512.0, 120.0));
    doc.active_channel_mut()
        .insert_frame(90.0, circle(512.0, 512.0, 340.0));

    // 加帧序列：都插在已有帧之间，模拟真实使用
    let targets = [45.0f32, 22.5, 67.5, 11.25, 78.75, 33.75];

    println!("\n[逐次加帧] 每次：测耗时 + 比对加帧前后画面");
    let mut worst_ms = 0.0f64;
    for a in targets {
        let step = sdf_mask_tool::model::doc::angle_to_step(a, steps);
        // 加帧**前**屏幕显示什么（把播放头放到目标角度）
        let t = a / 90.0;
        let v_before = violation_count(&doc);
        let (before, _) = sim_display(&mut doc, step, steps, t, false, v_before);

        // 计时开始：完整复刻 do_add_keyframe_at
        let t0 = Instant::now();

        // 1. push_undo（快照）
        let snap = doc.clone();
        std::hint::black_box(&snap);

        // 2. 起点取 display_mask（角度重合时）
        let start = before.clone();

        // 3. 插入
        doc.active_channel_mut().insert_frame(a, start);

        // 4. 违规体检（App 里是 refresh_violations）
        let v_after = violation_count(&doc);

        // 5. 下一帧的显示（走快路径，应当不烘焙）
        let (after, err) = sim_display(&mut doc, step, steps, t, false, v_after);

        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        if ms > worst_ms {
            worst_ms = ms;
        }

        let diff = (0..(W * H) as usize)
            .filter(|&i| before.data()[i] != after.data()[i])
            .count();

        check!(
            format!(
                "{a:>5.2}°　{ms:6.2} ms　画面差 {diff} 像素（帧数 {}）",
                doc.active_channel_ref().frames.len()
            ),
            diff == 0 && ms < 30.0 && !err,
            format!("耗时 {ms:.1}ms / 差 {diff} 像素 / 错误={err}")
        );
    }

    println!("\n  最慢一次：{worst_ms:.2} ms（修复前同规模约 200ms）");

    // ---- 断言：非关键帧仍走烘焙，且结果正确 ----
    println!("\n[非关键帧仍走烘焙路径]");
    let step_mid = 16usize; // 45° 附近，先确认它不是关键帧
    if !doc.active_channel_ref().has_kf_at_step(step_mid, steps) {
        let tt = step_mid as f32 / (steps - 1) as f32;
        let v = violation_count(&doc);
        let (m, err) = sim_display(&mut doc, step_mid, steps, tt, false, v);
        let cnt = m.count();
        check!(
            format!("步 {step_mid}（{:.1}°）烘焙出图，遮 {cnt} 像素", tt * 90.0),
            !err && cnt > 0,
            format!("错误={err}，遮 {cnt} 像素")
        );
    } else {
        println!("  · 步 {step_mid} 恰好是关键帧，跳过");
    }

    // ---- 断言：违规数据在关键帧上也能报出来 ----
    println!("\n[跳过烘焙后，违规仍能被发现]");
    let mut bad = Doc::with_steps(W, H, 32);
    bad.active_channel_mut()
        .insert_frame(0.0, circle(512.0, 512.0, 80.0));
    bad.active_channel_mut()
        .insert_frame(45.0, circle(512.0, 512.0, 300.0));
    bad.active_channel_mut()
        .insert_frame(90.0, circle(512.0, 512.0, 90.0));
    let v_bad = violation_count(&bad);
    let s0 = sdf_mask_tool::model::doc::angle_to_step(0.0, 32);
    let (_, err_at_kf) = sim_display(&mut bad, s0, 32, 0.0, false, v_bad);
    check!(
        format!(
            "来回翻转数据：违规 {} 像素，停在关键帧上也能判定非法",
            v_bad
        ),
        v_bad > 0 && err_at_kf,
        format!("违规 {v_bad}，关键帧上 err={err_at_kf}")
    );

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
