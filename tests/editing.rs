//! 本轮新增：关键帧导航/复制、通道交换、画布改尺寸、黑白笔刷。

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::{DEFAULT_STEPS, Doc, angle_to_step, step_angle};
use sdf_mask_tool::paint;

/// 测试统一用默认步数
const TS: usize = DEFAULT_STEPS;

fn doc_two_frames() -> Doc {
    let mut d = Doc::new(32, 32);
    d.seed_default_frames();
    d
}

/// 造一张左侧 cols 列被遮的遮罩
fn left_cols(n: u32, cols: u32) -> Mask {
    let mut m = Mask::new(n, n);
    for y in 0..n {
        for x in 0..cols.min(n) {
            m.set(x, y, 1);
        }
    }
    m
}

// ---------------------------------------------------------------- 关键帧导航

#[test]
fn 上一个下一个关键帧能跳对() {
    let mut d = Doc::new(8, 8);
    let a0 = step_angle(0, TS);
    let a1 = step_angle(10, TS);
    let a2 = step_angle(DEFAULT_STEPS - 1, TS);
    d.active_channel_mut().insert_frame(a0, Mask::new(8, 8));
    d.active_channel_mut().insert_frame(a1, Mask::new(8, 8));
    d.active_channel_mut().insert_frame(a2, Mask::new(8, 8));

    assert_eq!(d.prev_keyframe(angle_to_step(a1, TS), TS), Some(a0));
    assert_eq!(d.next_keyframe(angle_to_step(a1, TS), TS), Some(a2));
    // 两端没有更远的帧
    assert_eq!(d.prev_keyframe(angle_to_step(a0, TS), TS), None);
    assert_eq!(d.next_keyframe(angle_to_step(a2, TS), TS), None);
}

#[test]
fn 非步角的帧会被吸附回步角() {
    let mut d = Doc::new(8, 8);
    // 模拟旧工程/手工写入：帧落在步与步之间。
    // 注意 30° 在默认 64 步下**恰好是步角**（21 × 90/63），
    // 所以这里必须取一个真正偏离的值，否则测不到吸附。
    let off = 30.5_f32;
    d.active_channel_mut().insert_frame(off, Mask::new(8, 8));
    d.active_channel_mut().insert_frame(0.0, Mask::new(8, 8));

    let expect = step_angle(angle_to_step(off, TS), TS);
    assert!(
        !d.active_channel_ref().has_frame_at(expect),
        "吸附前 {off}° 不等于任何步角，不该命中 {expect}"
    );

    d.snap_frames();

    for f in d.active_channel_ref().frames.iter() {
        let s = angle_to_step(f.angle, TS);
        assert!(
            (f.angle - step_angle(s, TS)).abs() < 1e-4,
            "{}° 应被吸附到步角",
            f.angle
        );
    }
    assert!(
        d.active_channel_ref().has_frame_at(expect),
        "应落到离 {off}° 最近的步角 {expect}"
    );
}

/// 两个点吸进同一格时，**两帧内容都必须留下**。
///
/// 早期实现撞格时用后者覆盖前者，等于静默丢掉一整帧画过的遮罩；
/// 保存再打开就会少一个参数点，且没有任何提示。现在改为把后一个点
/// 挤到相邻空格上，内容一个不少。
#[test]
fn 吸附撞格时两帧内容都保留() {
    let mut d = Doc::new(8, 8);
    let a = step_angle(5, TS);
    // 两个角度会吸到同一格，但内容不同（4 列 vs 12 列）
    d.active_channel_mut()
        .insert_frame(a - 0.4, left_cols(8, 4));
    d.active_channel_mut()
        .insert_frame(a + 0.4, left_cols(8, 12));
    d.snap_frames();

    let frames = &d.active_channel_ref().frames;
    assert_eq!(frames.len(), 2, "撞格不能丢帧，应挤到相邻空格");
    // 不带进带出的 assert 需要 frames 借用结束，先取出内容
    let mut counts: Vec<usize> = frames.iter().map(|f| f.mask.count()).collect();
    counts.sort_unstable();
    // 4 列 × 8 行 = 32；12 列被画布宽度截到 8 列 × 8 行 = 64
    assert_eq!(counts, vec![32, 64], "两帧的遮罩内容都必须还在");

    // 落点必须彼此不同，且都在刻度上
    let a0 = frames[0].angle;
    let a1 = frames[1].angle;
    assert!((a0 - a1).abs() > 0.01, "两个点不能落在同一位置");
    for f in frames {
        let s = angle_to_step(f.angle, TS);
        assert!(
            (f.angle - step_angle(s, TS)).abs() < 1e-3,
            "吸附后必须落在刻度上，实际 {}",
            f.angle
        );
    }
}

#[test]
fn 关键帧导航跳过插值区() {
    let mut d = Doc::new(8, 8);
    let a0 = step_angle(0, TS);
    let a1 = step_angle(20, TS);
    d.active_channel_mut().insert_frame(a0, Mask::new(8, 8));
    d.active_channel_mut().insert_frame(a1, Mask::new(8, 8));
    // 从第 0 步找下一个，应直接跳到第 20 步，而不是中间的某个步
    assert_eq!(d.next_keyframe(angle_to_step(a0, TS), TS), Some(a1));
}

// ---------------------------------------------------------------- 复制关键帧

#[test]
fn 复制关键帧落到相邻空位并带内容() {
    let mut d = doc_two_frames();
    let mut m = left_cols(32, 10);
    d.active_channel_mut().insert_frame(0.0, m.clone());
    let before = d.active_channel_ref().frames.len();

    let na = d.duplicate_keyframe(0, TS).expect("应复制成功");
    assert!(
        (na - step_angle(1, TS)).abs() < 1e-4,
        "应落在紧跟的下一格，实际 {na}"
    );
    assert_eq!(d.active_channel_ref().frames.len(), before + 1);
    // 内容一致
    let dup = d.active_channel_ref().frame_at(na).unwrap();
    assert_eq!(dup.mask.count(), m.count());
}

#[test]
fn 复制关键帧不覆盖已有帧() {
    let mut d = Doc::new(8, 8);
    // 把前三个步都占满
    for s in 0..3 {
        d.active_channel_mut()
            .insert_frame(step_angle(s, TS), Mask::new(8, 8));
    }
    let na = d.duplicate_keyframe(0, TS).unwrap();
    assert!(
        (na - step_angle(3, TS)).abs() < 1e-4,
        "应跳过已占用的格子，实际 {na}"
    );
    assert_eq!(d.active_channel_ref().frames.len(), 4, "不该覆盖掉已有帧");
}

#[test]
fn 末尾满位时往前找空位() {
    let mut d = Doc::new(8, 8);
    // 占满最后几格，让「往后找」失败
    for s in (DEFAULT_STEPS - 3)..DEFAULT_STEPS {
        d.active_channel_mut()
            .insert_frame(step_angle(s, TS), Mask::new(8, 8));
    }
    let last = step_angle(DEFAULT_STEPS - 1, TS);
    let na = d
        .duplicate_keyframe(angle_to_step(last, TS), TS)
        .expect("应往前找到空位");
    assert!(na < last, "应插到前面，实际 {na}");
}

#[test]
fn 非关键帧处不能复制() {
    let mut d = doc_two_frames();
    assert_eq!(d.duplicate_keyframe(5, TS), None);
}

// ---------------------------------------------------------------- 通道交换

#[test]
fn 交换通道内容互换() {
    let mut d = Doc::new(8, 8);
    d.add_layer("第二层");
    // 层0 R 放 3 帧，层1 G 放 1 帧
    for s in 0..3 {
        d.layers[0].channels[0].insert_frame(step_angle(s, TS), Mask::new(8, 8));
    }
    d.layers[1].channels[1].insert_frame(0.0, left_cols(8, 4));

    assert!(d.swap_channel((0, 0), (1, 1)));
    assert_eq!(d.layers[0].channels[0].frames.len(), 1, "应换成 G 的内容");
    assert_eq!(d.layers[1].channels[1].frames.len(), 3, "应换成 R 的内容");
    assert_eq!(d.layers[0].channels[0].frames[0].mask.count(), 32);
}

#[test]
fn 交换是自反的() {
    let mut d = Doc::new(8, 8);
    d.add_layer("B");
    d.layers[0].channels[0].insert_frame(0.0, left_cols(8, 2));
    d.layers[1].channels[2].insert_frame(0.0, left_cols(8, 6));
    let a0 = d.layers[0].channels[0].frames[0].mask.count();
    let b0 = d.layers[1].channels[2].frames[0].mask.count();

    d.swap_channel((0, 0), (1, 2));
    d.swap_channel((0, 0), (1, 2));

    assert_eq!(d.layers[0].channels[0].frames[0].mask.count(), a0);
    assert_eq!(d.layers[1].channels[2].frames[0].mask.count(), b0);
}

#[test]
fn 交换自己或越界返回假() {
    let mut d = Doc::new(8, 8);
    assert!(!d.swap_channel((0, 0), (0, 0)), "同位置应拒绝");
    assert!(!d.swap_channel((0, 0), (9, 0)), "越界应拒绝");
}

#[test]
fn 交换后距离场缓存被清掉() {
    let mut d = Doc::new(8, 8);
    d.add_layer("B");
    d.layers[0].channels[0].insert_frame(0.0, left_cols(8, 2));
    let _ = d.layers[0].channels[0].dist_of(0, 8, 8);
    assert!(!d.layers[0].channels[0].dist_cache.is_empty());

    d.swap_channel((0, 0), (1, 1));
    assert!(
        d.layers[0].channels[0].dist_cache.is_empty(),
        "内容换了，缓存必须失效"
    );
}

// ---------------------------------------------------------------- 画布尺寸

#[test]
fn 改尺寸后所有帧跟着缩放() {
    let mut d = doc_two_frames();
    d.active_channel_mut().insert_frame(0.0, left_cols(32, 16));
    d.resize(64, 64);
    assert_eq!(d.width, 64);
    assert_eq!(d.height, 64);
    let f = d.active_channel_ref().frame_at(0.0).unwrap();
    assert_eq!(f.mask.width(), 64);
    assert_eq!(f.mask.height(), 64);
    // 原来是左半遮，缩放后仍是左半
    assert_eq!(f.mask.get(0, 32), 1);
    assert_eq!(f.mask.get(63, 32), 0);
}

#[test]
fn 改尺寸保持二值不被抹灰() {
    let mut d = doc_two_frames();
    d.active_channel_mut().insert_frame(0.0, left_cols(32, 16));
    d.resize(17, 17); // 非整数倍缩放，最容易出中间值
    let f = d.active_channel_ref().frame_at(0.0).unwrap();
    assert!(
        f.mask.data().iter().all(|&v| v == 0 || v == 1),
        "必须仍然只有 0 和 1"
    );
}

#[test]
fn 改尺寸会把底图重采样() {
    let mut d = doc_two_frames();
    d.background = Some(image::RgbaImage::from_pixel(
        32,
        32,
        image::Rgba([9, 9, 9, 255]),
    ));
    d.resize(16, 48);
    let bg = d.background.as_ref().unwrap();
    assert_eq!((bg.width(), bg.height()), (16, 48));
}

#[test]
fn 尺寸被夹在合理范围() {
    let mut d = Doc::new(32, 32);
    d.resize(1, 1);
    assert!(d.width >= 16 && d.height >= 16, "太小应被抬到下限");
    d.resize(99999, 99999);
    assert!(d.width <= 8192 && d.height <= 8192, "太大应被压到上限");
}

#[test]
fn 改尺寸后缓存失效() {
    let mut d = doc_two_frames();
    d.active_channel_mut().insert_frame(0.0, left_cols(32, 8));
    let _ = d.active_channel_mut().dist_of(0, 32, 32);
    d.resize(64, 64);
    assert!(
        d.active_channel_ref().dist_cache.is_empty(),
        "尺寸变了距离场必须重算"
    );
}

// ---------------------------------------------------------------- 黑白笔刷

#[test]
fn 黑笔写入1白笔写入0() {
    let mut m = Mask::new(16, 16);
    paint::stamp(&mut m, 8.0, 8.0, 3.0, false);
    assert_eq!(m.get(8, 8), 1, "黑笔应盖上遮罩");

    paint::stamp(&mut m, 8.0, 8.0, 3.0, true);
    assert_eq!(m.get(8, 8), 0, "白笔应擦掉遮罩");
}

#[test]
fn 白笔只擦笔刷范围() {
    let mut m = Mask::new(32, 32);
    m.fill(1);
    paint::stamp(&mut m, 16.0, 16.0, 4.0, true);
    assert_eq!(m.get(16, 16), 0, "笔心应被擦掉");
    assert_eq!(m.get(0, 0), 1, "范围外不受影响");
}

#[test]
fn 笔刷半径决定覆盖范围() {
    let mut small = Mask::new(64, 64);
    paint::stamp(&mut small, 32.0, 32.0, 4.0, false);
    let mut big = Mask::new(64, 64);
    paint::stamp(&mut big, 32.0, 32.0, 16.0, false);
    assert!(
        big.count() > small.count() * 10,
        "半径 4 倍的笔刷面积应大约 16 倍: {} vs {}",
        small.count(),
        big.count()
    );
}

#[test]
fn 填充走指定值() {
    let mut m = Mask::new(8, 8);
    paint::flood_fill(&mut m, 0, 0, 1);
    assert_eq!(m.count(), 64, "黑填充应染满");
    paint::flood_fill(&mut m, 0, 0, 0);
    assert_eq!(m.count(), 0, "白填充应清空");
}

// ---------------------------------------------------------------- 角度换算

#[test]
fn 角度与步号往返一致() {
    for s in 0..DEFAULT_STEPS {
        assert_eq!(angle_to_step(step_angle(s, TS), TS), s, "步 {s} 往返不一致");
    }
}

// ---------------------------------------------------------------- 刻度对齐

/// 界面落点对齐刻度后，任意拖拽终点都必然落在刻度上。
///
/// 这条不变式是"参数点不会互相覆盖"的前提：两个点若都能落在刻度之间，
/// 就可能吸进同一格并在读盘时被合并。
#[test]
fn 刻度吸附后的位置彼此可分() {
    use sdf_mask_tool::model::doc::{norm_to_step, step_norm};

    // 任意连续位置吸附后，必然是某个刻度的精确位置
    for k in 0..=1000 {
        let t = k as f32 / 1000.0;
        let s = norm_to_step(t, TS);
        let snapped = step_norm(s, TS);
        // 吸附结果反过来再算一次步号必须还是同一个步
        assert_eq!(norm_to_step(snapped, TS), s, "t={t} 吸附后步号不稳定");
        // 吸附后的位置必须精确等于该步的位置（不是近似）
        assert!(
            (snapped - step_norm(s, TS)).abs() < 1e-6,
            "t={t} 吸附结果不在刻度上"
        );
    }
}

/// 整条轴上相邻刻度的角度间隔严格大于 `angle_taken` 的判定容差。
///
/// 若两者量级接近，把一个点拖到相邻刻度时会被判成"目标已被占用"，
/// 表现为"明明有空位却拖不过去"。
#[test]
fn 相邻刻度的间隔远大于占用容差() {
    let step_gap = 90.0 / (TS - 1) as f32;
    let tolerance = 0.02_f32;
    assert!(
        step_gap > tolerance * 10.0,
        "刻度间隔 {step_gap:.4}° 与占用容差 {tolerance}° 太接近，会误判相邻刻度为已占用"
    );
}
