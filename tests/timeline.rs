//! 拖动时间轴的性能与正确性。

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::{DEFAULT_STEPS, Doc, angle_to_step, step_angle};
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

#[test]
fn 默认离散帧数是64且两端是090() {
    assert_eq!(DEFAULT_STEPS, 64, "默认离散帧数");
    let steps = DEFAULT_STEPS;
    assert_eq!(step_angle(0, steps), 0.0);
    assert!((step_angle(steps - 1, steps) - 90.0).abs() < 1e-4);
    // 任何合法步数下两端都得是 0° 和 90°
    for n in [2usize, 32, 64, 128, 512] {
        assert_eq!(step_angle(0, n), 0.0, "{n} 帧的左端应 0°");
        assert!(
            (step_angle(n - 1, n) - 90.0).abs() < 1e-3,
            "{n} 帧的右端应 90°"
        );
    }
}

#[test]
fn 默认文档就是两帧且一左一右() {
    let mut doc = Doc::new(64, 64);
    doc.seed_default_frames();
    let ch = doc.active_channel_ref();
    assert_eq!(ch.frames.len(), 2, "应有 0° 和 90° 两帧");
    assert!((ch.frames[0].angle - 0.0).abs() < 1e-4);
    assert!((ch.frames[1].angle - 90.0).abs() < 1e-4);
    // 两帧都是空白（数值全 0）
    assert!(ch.frames[0].mask.data().iter().all(|&v| v == 0));
    assert!(ch.frames[1].mask.data().iter().all(|&v| v == 0));
}

#[test]
fn 距离场缓存命中后不重算() {
    let mut doc = Doc::new(64, 64);
    doc.seed_default_frames();
    // 给两帧都画东西，否则距离场是哨兵值
    let mut a = Mask::new(64, 64);
    a.set(10, 10, 1);
    doc.active_channel_mut().insert_frame(0.0, a);
    let mut b = Mask::new(64, 64);
    for y in 0..64 {
        for x in 0..20 {
            b.set(x, y, 1);
        }
    }
    doc.active_channel_mut().insert_frame(90.0, b);

    // 第一次取：算
    let d0 = doc.active_channel_mut().dist_of(0, 64, 64).to_vec();
    // 第二次取：应命中缓存（内容不变，结果相同）
    let d1 = doc.active_channel_mut().dist_of(0, 64, 64).to_vec();
    assert_eq!(d0, d1);

    // 改内容后必须失效
    doc.active_channel_mut().frames[0].mask.set(30, 30, 1);
    let d2 = doc.active_channel_mut().dist_of(0, 64, 64).to_vec();
    assert_ne!(d0, d2, "内容变了距离场要重算");
}

#[test]
fn 拖动整条时间轴的总耗时可接受() {
    let n = 512u32;
    let mut doc = Doc::new(n, n);
    doc.active_channel_mut().insert_frame(0.0, Mask::new(n, n));
    doc.active_channel_mut()
        .insert_frame(90.0, left_half(n, n, n / 2));

    // 预热点两个端点的距离场
    let da = doc
        .active_channel_mut()
        .dist_of(0, n as usize, n as usize)
        .to_vec();
    let db = doc
        .active_channel_mut()
        .dist_of(1, n as usize, n as usize)
        .to_vec();

    let mut out = Mask::new(n, n);
    let t0 = std::time::Instant::now();
    for s in 0..DEFAULT_STEPS {
        let angle = step_angle(s, TS);
        let (li, ri) = doc.active_channel_ref().bracket(angle);
        if let (Some(l), Some(r)) = (li, ri) {
            let a = doc.active_channel_ref().frames[l].angle;
            let b = doc.active_channel_ref().frames[r].angle;
            let t = ((angle - a) / (b - a)).clamp(0.0, 1.0);
            sdf::interpolate_from_dist(&da, &db, t, &mut out);
        }
    }
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    // DEFAULT_STEPS 帧 × 512² 的纯混合，应该远低于一帧 EDT 的耗时
    assert!(ms < 200.0, "拖完整条轴应很快，实际 {ms:.1}ms");
    println!("{} 帧全扫一遍：{ms:.1}ms", DEFAULT_STEPS);
}

// ---------------------------------------------------------------- 连续拖动（不吸附）

/// 用户明确要求：拖动关键帧松手后**不要**吸附到离散格，只保持视觉平滑。
#[test]
fn 拖动关键帧保持连续角度不吸附() {
    let mut d = Doc::with_steps(16, 16, 64);
    d.active_channel_mut()
        .insert_frame(0.0, left_half(16, 16, 4));
    d.active_channel_mut()
        .insert_frame(90.0, left_half(16, 16, 12));

    // 拖到 30.7°——这个值明显不在任何步角上
    let target = 30.7_f32;
    assert!(d.move_frame_to_angle(0, target), "应能移动");
    let angles: Vec<f32> = d
        .active_channel_ref()
        .frames
        .iter()
        .map(|f| f.angle)
        .collect();
    assert!(
        angles.iter().any(|a| (a - target).abs() < 1e-4),
        "角度应精确保持 {target}°，实际 {angles:?}"
    );
    // 确认它确实不是步角（否则这条测试测不到东西）
    assert!(
        (target - step_angle(angle_to_step(target, 64), 64)).abs() > 0.1,
        "测试值必须是真正的非步角"
    );
}

#[test]
fn 拖动后关键帧仍按角度排序() {
    let mut d = Doc::with_steps(16, 16, 64);
    d.active_channel_mut()
        .insert_frame(0.0, left_half(16, 16, 2));
    d.active_channel_mut()
        .insert_frame(45.0, left_half(16, 16, 6));
    d.active_channel_mut()
        .insert_frame(90.0, left_half(16, 16, 10));

    // 把 0° 那帧拖到 60°（越过 45°）
    assert!(d.move_frame_to_angle(0, 60.0));
    let angles: Vec<f32> = d
        .active_channel_ref()
        .frames
        .iter()
        .map(|f| f.angle)
        .collect();
    assert_eq!(angles, vec![45.0, 60.0, 90.0], "挪过去后必须重新排序");
}

#[test]
fn 拖到已被占用的位置会被拒绝() {
    let mut d = Doc::with_steps(16, 16, 64);
    d.active_channel_mut()
        .insert_frame(0.0, left_half(16, 16, 2));
    d.active_channel_mut()
        .insert_frame(30.0, left_half(16, 16, 6));

    assert!(
        !d.move_frame_to_angle(0, 30.0),
        "目标位置已有帧，应拒绝而不是静默覆盖"
    );
    assert_eq!(d.active_channel_ref().frames.len(), 2, "不该丢帧");
    assert!(d.angle_taken(30.0, None));
    assert!(
        !d.angle_taken(30.0, Some(1)),
        "排除自己之后就不算占用（同帧在原地不动）"
    );
}

/// 完成一次开放中心停止后，像素层面的采样必须仍然可用。
#[test]
fn 连续角度下的分桶查找仍然命中() {
    let mut d = Doc::with_steps(16, 16, 64);
    d.active_channel_mut()
        .insert_frame(0.0, left_half(16, 16, 4));
    d.move_frame_to_angle(0, 30.7);

    // 30.7° 落在第 21 格（90/63 × 21 ≈ 30.0，边界取整后仍在附近）
    let s = angle_to_step(30.7, 64);
    assert!(
        d.active_channel_ref().has_kf_at_step(s, 64),
        "连续角度必须能被分桶找到，否则画布会拒绝落笔"
    );
    assert!(d.active_channel_ref().frame_for_step(s, 64).is_some());
}

#[test]
fn 中间步的遮罩介于两端之间() {
    let n = 64u32;
    let mut doc = Doc::new(n, n);
    doc.active_channel_mut()
        .insert_frame(0.0, left_half(n, n, 8));
    doc.active_channel_mut()
        .insert_frame(90.0, left_half(n, n, 40));

    let da = doc
        .active_channel_mut()
        .dist_of(0, n as usize, n as usize)
        .to_vec();
    let db = doc
        .active_channel_mut()
        .dist_of(1, n as usize, n as usize)
        .to_vec();

    let mut mid = Mask::new(n, n);
    sdf::interpolate_from_dist(&da, &db, 0.5, &mut mid);

    let c0 = left_half(n, n, 8).count();
    let c1 = left_half(n, n, 40).count();
    let cm = mid.count();
    assert!(cm > c0 && cm < c1, "中间态应在两端之间: {c0} < {cm} < {c1}");
}
