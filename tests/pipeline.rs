//! 端到端验证：画遮罩 → 烘焙 SDF → 按时间采样还原。
//!
//! 不依赖 GUI，直接跑数据管线。
//!
//! 注意烘焙产物是 `Baked`（贴图 + 编码方向）。方向必须跟着贴图走：
//! 采样规则依赖它，丢了方向会把遮罩还原反。

use sdf_mask_tool::io;
use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::Doc;
use sdf_mask_tool::sdf;

fn blank(w: u32, h: u32) -> Mask {
    Mask::new(w, h)
}

/// 左侧 cols 列被遮
fn left_half(w: u32, h: u32, cols: u32) -> Mask {
    let mut m = Mask::new(w, h);
    for y in 0..h {
        for x in 0..cols.min(w) {
            m.set(x, y, 1);
        }
    }
    m
}

fn full(w: u32, h: u32) -> Mask {
    let mut m = Mask::new(w, h);
    m.fill(1);
    m
}

#[test]
fn 两帧烧出的图能还原两端形状() {
    let mut doc = Doc::new(64, 64);
    // 0°：全不遮；90°：左侧 24 列被遮  →  阴影扩张
    doc.active_channel_mut().insert_frame(0.0, blank(64, 64));
    doc.active_channel_mut()
        .insert_frame(90.0, left_half(64, 64, 24));

    let baked = io::bake_channel(doc.active_channel_ref(), 64, 64).unwrap();
    assert_eq!(baked.direction, sdf::Direction::ShadowGrows);

    // 0° 还原：全不遮（全白）
    let r0 = baked.restore_at(0.0);
    let lit = r0.pixels().filter(|p| p.0[0] > 127).count();
    assert!(lit > 64 * 64 * 95 / 100, "0° 应几乎全不遮，实际 {lit}");

    // 90° 还原：左暗右亮
    let r90 = baked.restore_at(1.0);
    assert_eq!(r90.get_pixel(1, 32).0[0], 0, "左侧应被遮");
    assert_eq!(r90.get_pixel(60, 32).0[0], 255, "右侧应不遮");
}

#[test]
fn 边界扫过的地方临界角单调() {
    let mut doc = Doc::new(64, 8);
    doc.active_channel_mut().insert_frame(0.0, blank(64, 8));
    doc.active_channel_mut()
        .insert_frame(90.0, left_half(64, 8, 32));

    let baked = io::bake_channel(doc.active_channel_ref(), 64, 8).unwrap();
    let sdf = baked.to_gray8();

    let a36 = sdf.get_pixel(36, 4).0[0] as i32;
    let a34 = sdf.get_pixel(34, 4).0[0] as i32;
    let a32 = sdf.get_pixel(32, 4).0[0] as i32;
    assert!(
        a36 >= a34 && a34 >= a32,
        "越靠近边界越早被遮（texel 越小）: {a36} {a34} {a32}"
    );
    assert_eq!(a36, 255, "远离边界的像素全程不遮");
}

/// 通道之间互不干扰。
///
/// R 给一段有几何意义的渐变（0° 全亮 → 45° 半遮 → 90° 全遮），
/// G 全程空白。R 必须有过渡带、G 必须全是 255，两者互不影响。
///
/// 注意别用"0° 空白 → 90° 全遮"验证：那两帧都是哨兵（整幅无边界），
/// 没有任何几何信息，只能判定"阴影在这个区间内出现"，
/// 无法区分它在这区间里的哪一刻出现。
#[test]
fn 通道之间互不干扰() {
    let mut doc = Doc::new(32, 32);

    // R 通道：中间加一帧部分遮，才有真实几何可比
    doc.active_channel = 0;
    doc.active_channel_mut().insert_frame(0.0, blank(32, 32));
    doc.active_channel_mut()
        .insert_frame(45.0, left_half(32, 32, 16));
    doc.active_channel_mut().insert_frame(90.0, full(32, 32));

    // G 通道：两帧全空 → 全程受光
    doc.active_channel = 1;
    doc.active_channel_mut().insert_frame(0.0, blank(32, 32));
    doc.active_channel_mut().insert_frame(90.0, blank(32, 32));

    let sdf_r = io::bake_channel(&doc.layers[0].channels[0], 32, 32)
        .unwrap()
        .to_gray8();
    let sdf_g = io::bake_channel(&doc.layers[0].channels[1], 32, 32)
        .unwrap()
        .to_gray8();

    // G 全程不遮，所有 texel 都该是 255
    assert!(
        sdf_g.pixels().all(|p| p.0[0] == 255),
        "G 全程空白，应全部是 255"
    );
    // R 必须产生过渡带（不是清一色 0 或 255）
    let mid = sdf_r
        .pixels()
        .filter(|p| p.0[0] > 2 && p.0[0] < 253)
        .count();
    assert!(mid > 100, "R 应产生过渡带，实际只有 {mid} 个过渡像素");
    // 且 R 与 G 明显不同，证明两个通道各算各的
    let diff = sdf_r
        .pixels()
        .zip(sdf_g.pixels())
        .filter(|(a, b)| a.0[0] != b.0[0])
        .count();
    assert!(diff > 100, "R 与 G 应不同，实际只有 {diff} 个像素不同");
}

#[test]
fn 单帧全不遮输出全白() {
    let mut doc = Doc::new(16, 16);
    doc.active_channel_mut().insert_frame(45.0, blank(16, 16));
    let baked = io::bake_channel(doc.active_channel_ref(), 16, 16).unwrap();
    assert!(baked.to_gray8().pixels().all(|p| p.0[0] == 255));
}

#[test]
fn 单帧全遮输出全黑() {
    let mut doc = Doc::new(16, 16);
    doc.active_channel_mut().insert_frame(45.0, full(16, 16));
    let baked = io::bake_channel(doc.active_channel_ref(), 16, 16).unwrap();
    assert!(baked.to_gray8().pixels().all(|p| p.0[0] == 0));
}

#[test]
fn 关键帧乱序不影响结果() {
    let mut a = Doc::new(32, 8);
    a.active_channel_mut().insert_frame(0.0, blank(32, 8));
    a.active_channel_mut()
        .insert_frame(90.0, left_half(32, 8, 16));
    let sdf_a = io::bake_channel(a.active_channel_ref(), 32, 8).unwrap();

    let mut b = Doc::new(32, 8);
    b.active_channel_mut()
        .insert_frame(90.0, left_half(32, 8, 16));
    b.active_channel_mut().insert_frame(0.0, blank(32, 8));
    let sdf_b = io::bake_channel(b.active_channel_ref(), 32, 8).unwrap();

    assert_eq!(
        sdf_a.to_gray8().as_raw(),
        sdf_b.to_gray8().as_raw(),
        "插入顺序不该影响结果"
    );
}

#[test]
fn 空通道烘焙成全零不崩溃() {
    let doc = Doc::new(16, 16);
    let baked = io::bake_channel(doc.active_channel_ref(), 16, 16).unwrap();
    assert_eq!(baked.width, 16);
    assert!(baked.to_gray8().pixels().all(|p| p.0[0] == 0));
}

#[test]
fn 四帧序列两端还原得准() {
    let mut doc = Doc::new(32, 16);
    doc.active_channel_mut().insert_frame(0.0, blank(32, 16));
    doc.active_channel_mut()
        .insert_frame(30.0, left_half(32, 16, 8));
    doc.active_channel_mut()
        .insert_frame(60.0, left_half(32, 16, 20));
    doc.active_channel_mut()
        .insert_frame(90.0, left_half(32, 16, 28));

    let baked = io::bake_channel(doc.active_channel_ref(), 32, 16).unwrap();

    let r0 = baked.restore_at(0.0);
    assert_eq!(r0.get_pixel(16, 8).0[0], 255, "0° 中部应不遮");

    let r90 = baked.restore_at(1.0);
    assert_eq!(r90.get_pixel(1, 8).0[0], 0, "90° 最左应被遮");
    assert_eq!(r90.get_pixel(31, 8).0[0], 255, "90° 最右应不遮");

    // 边界随角度单调右移
    let boundary = |t: f32| {
        let img = baked.restore_at(t);
        (0..32)
            .find(|&x| img.get_pixel(x, 8).0[0] > 127)
            .unwrap_or(32)
    };
    assert!(
        boundary(0.0) <= boundary(1.0),
        "边界应随角度推进：{} → {}",
        boundary(0.0),
        boundary(1.0)
    );
}

/// 阴影收缩的数据：自动改用反向编码，两端还原仍正确。
#[test]
fn 阴影收缩也能正确导出() {
    let mut doc = Doc::new(32, 16);
    doc.active_channel_mut()
        .insert_frame(0.0, left_half(32, 16, 24));
    doc.active_channel_mut()
        .insert_frame(90.0, left_half(32, 16, 8));

    let baked = io::bake_channel(doc.active_channel_ref(), 32, 16).unwrap();
    assert_eq!(
        baked.direction,
        sdf::Direction::ShadowShrinks,
        "阴影收缩应自动改用反向编码"
    );

    let r0 = baked.restore_at(0.0);
    assert_eq!(r0.get_pixel(20, 8).0[0], 0, "0° 时 x=20 应被遮（宽阴影）");
    let r90 = baked.restore_at(1.0);
    assert_eq!(
        r90.get_pixel(20, 8).0[0],
        255,
        "90° 时 x=20 应受光（窄阴影）"
    );
    assert_eq!(r90.get_pixel(2, 8).0[0], 0, "90° 时 x=2 仍被遮");
}

#[test]
fn 插值出的中间遮罩位于两帧之间() {
    let a = left_half(32, 32, 8);
    let b = left_half(32, 32, 24);
    let mut out = Mask::new(32, 32);
    sdf::interpolate_masks(&a, &b, 0.5, &mut out);

    assert_eq!(out.get(4, 16), 1, "左侧应被遮");
    assert_eq!(out.get(28, 16), 0, "右侧不遮");
    assert_eq!(out.get(14, 16), 1, "14 应被遮");
    assert_eq!(out.get(18, 16), 0, "18 不该被遮");
}

#[test]
fn 插值两端正好等于原帧() {
    let a = left_half(32, 32, 8);
    let b = left_half(32, 32, 24);
    let mut out = Mask::new(32, 32);

    sdf::interpolate_masks(&a, &b, 0.0, &mut out);
    for y in 0..32 {
        for x in 0..32 {
            assert_eq!(out.get(x, y), a.get(x, y), "t=0 应等于 a");
        }
    }

    sdf::interpolate_masks(&a, &b, 1.0, &mut out);
    for y in 0..32 {
        for x in 0..32 {
            assert_eq!(out.get(x, y), b.get(x, y), "t=1 应等于 b");
        }
    }
}
