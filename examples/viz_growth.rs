//! 把两个方向的插值结果**画成图**，看清"生长/收缩"到底是什么。
//!
//! 用户的核心质疑：
//!   「哪里有从外向内生长/收缩的啊？只有从外部收缩/生长的。」
//!
//! 也就是说：形状应当从**边界朝一个方向**推进（比如阴影从画面外向内吃进来，
//! 或从某个边缘朝内生长），而不是**以形状中心为基准**均匀缩放（像圆从
//! 中心膨胀/收缩）。后者是"整体缩放"，不是阴影生长。
//!
//! 本程序把 t=0..1 的采样结果拼成一张图，直接看形状是怎么变的。
//!
//! 用法：`cargo run --release --offline --example viz_growth`

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::sdf;

const W: u32 = 256;
const H: u32 = 256;
const COLS: u32 = 11;

/// 左侧一个矩形（模拟"阴影从左边长进来"）
fn left_block() -> Mask {
    let mut m = Mask::new(W, H);
    for y in 60..200 {
        for x in 20..120 {
            m.set(x, y, 1);
        }
    }
    m
}

/// 更大的一块（模拟第二帧阴影扩到右边）
fn left_block_big() -> Mask {
    let mut m = Mask::new(W, H);
    for y in 40..220 {
        for x in 20..200 {
            m.set(x, y, 1);
        }
    }
    m
}

fn blank() -> Mask {
    Mask::new(W, H)
}

/// 把 11 个时刻的采样拼成一条横带，每个时刻下方画一条分隔线
fn strip(doc: &mut Doc, path: &str, title: &str) {
    let (dw, dh) = (doc.width, doc.height);
    let (texels, dir) = match doc.active_channel_mut().texels(dw, dh) {
        Ok(v) => v,
        Err(e) => {
            println!("  【{title}】烘焙失败：{}", e.lines().next().unwrap_or(""));
            return;
        }
    };
    let cell = W;
    let mut img = image::GrayImage::new(cell * COLS, H);
    let mut counts = Vec::new();
    for k in 0..COLS {
        let t = k as f32 / (COLS - 1) as f32;
        let mut m = Mask::new(dw, dh);
        sdf::sample_texels(&texels, t, dir, &mut m, false);
        counts.push(m.count());
        for y in 0..H {
            for x in 0..W {
                let v = if m.get(x, y) != 0 { 255u8 } else { 40 };
                img.put_pixel(k * cell + x, y, image::Luma([v]));
            }
        }
        // 网格线
        for y in 0..H {
            img.put_pixel(k * cell, y, image::Luma([120]));
        }
    }
    img.save(path).ok();
    println!("  【{title}】方向 = {dir:?}");
    println!("    各时刻像素数：{counts:?}");
    println!("    → 已存 {path}");
}

fn main() {
    println!("生长方向可视化　每格 {W}×{H}，从左到右 t=0→1");
    println!("================================================================");

    // 场景 1：第 1 帧有大块，第 2 帧空白（应当看到"阴影从边缘退去"）
    {
        let mut doc = Doc::with_steps(W, H, 64);
        doc.active_channel_mut().insert_frame(0.0, left_block_big());
        doc.active_channel_mut().insert_frame(90.0, blank());
        strip(
            &mut doc,
            "target/growth_shrink.png",
            "一帧有大块 → 一帧空白（收缩）",
        );
    }

    // 场景 2：第 1 帧空白，第 2 帧有大块（应当看到"阴影从边缘长进来"）
    {
        let mut doc = Doc::with_steps(W, H, 64);
        doc.active_channel_mut().insert_frame(0.0, blank());
        doc.active_channel_mut()
            .insert_frame(90.0, left_block_big());
        strip(
            &mut doc,
            "target/growth_grow.png",
            "空白 → 一帧有大块（生长）",
        );
    }

    // 场景 3：两块逐帧扩张（真实阴影移动：左边界不动，右边界推进）
    {
        let mut doc = Doc::with_steps(W, H, 64);
        doc.active_channel_mut().insert_frame(0.0, left_block());
        doc.active_channel_mut()
            .insert_frame(90.0, left_block_big());
        strip(
            &mut doc,
            "target/growth_edge.png",
            "小块 → 大块（真实阴影扩张，应只朝右侧推进）",
        );
    }

    // 场景 4：反向（大块 → 小块，阴影从右侧退去）
    {
        let mut doc = Doc::with_steps(W, H, 64);
        doc.active_channel_mut().insert_frame(0.0, left_block_big());
        doc.active_channel_mut().insert_frame(90.0, left_block());
        strip(
            &mut doc,
            "target/growth_edge_back.png",
            "大块 → 小块（阴影从右侧退去）",
        );
    }

    println!("\n================================================================");
    println!("看图判断：");
    println!("  · 如果形状**整体缩放**（矩形四边同时向内/外），那是错的——");
    println!("    真实阴影只会从**一个方向**推进。");
    println!("  · 如果只有一侧边界在移动，那就是对的。");
}
