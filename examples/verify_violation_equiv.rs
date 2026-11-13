//! 验证 `nesting` 违规检测与 `bake` 方向判定是同一个条件。
//!
//! 用途：跳过烘焙后，`display_error`（"数据不合法，无插值"）不能再靠烘焙失败来设置，
//! 改为从 `refresh_violations` 的结果导出。这个替换成立的前提是：
//!
//!   `plan(...).violations.count == 0`  ⟺  `bake_auto(...)` 成功
//!
//! 若不成立，就会出现"缓存说合法、烘焙说非法"（或反之）的分裂。
//!
//! 用法：`cargo run --release --offline --example verify_violation_equiv`

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::nesting;
use sdf_mask_tool::sdf;

fn circle(w: u32, h: u32, cx: f32, cy: f32, r: f32) -> Mask {
    let mut m = Mask::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            if dx * dx + dy * dy <= r * r {
                m.set(x, y, 1);
            }
        }
    }
    m
}

fn rect(w: u32, h: u32, x0: i32, y0: i32, x1: i32, y1: i32) -> Mask {
    let mut m = Mask::new(w, h);
    for y in y0.max(0)..y1.min(h as i32) {
        for x in x0.max(0)..x1.min(w as i32) {
            m.set(x as u32, y as u32, 1);
        }
    }
    m
}

fn main() {
    const W: u32 = 128;
    const H: u32 = 128;
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

    println!("违规检测 vs 烘焙方向判定 等价性　画布 {W}×{H}");
    println!("================================================================");

    // 各种数据：合法的、扩张的、收缩的、来回翻转的、平移的、单帧、空帧混合
    let cases: Vec<(&str, Vec<(f32, Mask)>)> = vec![
        (
            "合法-扩张圆",
            vec![
                (0.0, circle(W, H, 64.0, 64.0, 10.0)),
                (30.0, circle(W, H, 64.0, 64.0, 30.0)),
                (60.0, circle(W, H, 64.0, 64.0, 50.0)),
                (90.0, circle(W, H, 64.0, 64.0, 62.0)),
            ],
        ),
        (
            "合法-收缩圆",
            vec![
                (0.0, circle(W, H, 64.0, 64.0, 60.0)),
                (45.0, circle(W, H, 64.0, 64.0, 35.0)),
                (90.0, circle(W, H, 64.0, 64.0, 8.0)),
            ],
        ),
        (
            "非法-来回翻转",
            vec![
                (0.0, circle(W, H, 64.0, 64.0, 10.0)),
                (30.0, circle(W, H, 64.0, 64.0, 50.0)),
                (60.0, circle(W, H, 64.0, 64.0, 12.0)),
                (90.0, circle(W, H, 64.0, 64.0, 55.0)),
            ],
        ),
        (
            "非法-平移",
            vec![
                (0.0, rect(W, H, 5, 5, 45, 45)),
                (45.0, rect(W, H, 80, 80, 120, 120)),
                (90.0, rect(W, H, 5, 5, 45, 45)),
            ],
        ),
        ("单帧", vec![(0.0, circle(W, H, 64.0, 64.0, 20.0))]),
        (
            "空帧+有内容",
            vec![
                (0.0, Mask::new(W, H)),
                (45.0, circle(W, H, 64.0, 64.0, 30.0)),
                (90.0, circle(W, H, 64.0, 64.0, 55.0)),
            ],
        ),
        (
            "全满帧",
            vec![
                (0.0, {
                    let mut m = Mask::new(W, H);
                    m.fill(1);
                    m
                }),
                (90.0, circle(W, H, 64.0, 64.0, 30.0)),
            ],
        ),
        (
            "非法-扩张后又缩回",
            vec![
                (0.0, circle(W, H, 64.0, 64.0, 20.0)),
                (30.0, circle(W, H, 64.0, 64.0, 45.0)),
                (60.0, circle(W, H, 64.0, 64.0, 46.0)),
                (90.0, circle(W, H, 64.0, 64.0, 15.0)),
            ],
        ),
    ];

    for (name, frames) in cases {
        let refs: Vec<&Mask> = frames.iter().map(|(_, m)| m).collect();
        let plan = nesting::plan(&refs, W as usize, H as usize);
        let nesting_ok = plan.violations.count == 0;

        let mut mf: Vec<sdf::MaskFrame> = frames
            .iter()
            .map(|(a, m)| sdf::MaskFrame {
                angle: *a,
                mask: m.data().to_vec(),
            })
            .collect();
        let bake_ok = sdf::bake_auto(&mut mf, W as usize, H as usize).is_ok();

        check!(
            format!("{name}：违规检测={} 烘焙={}", nesting_ok, bake_ok),
            nesting_ok == bake_ok,
            format!(
                "分裂！违规检测说{}，烘焙说{}（违规像素 {}）",
                if nesting_ok { "合法" } else { "非法" },
                if bake_ok { "合法" } else { "非法" },
                plan.violations.count
            )
        );
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        println!("\n⚠ 等价关系不成立，不能用 violations 替代烘焙失败来设 display_error。");
        std::process::exit(1);
    }
    println!("等价关系成立：violations.count==0 ⟺ bake_auto 成功");
}
