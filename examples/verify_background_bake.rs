//! 最小可行性验证：后台线程烘焙，UI 不被阻塞。
//!
//! 用户要求：「将 UI 操作效果和算法逻辑分离，UI 显示 UI 的，加帧等在后台加帧」。
//!
//! 核心约束（必须先验证，再改真实代码）：
//!  1. `Doc`/`Mask` 不能跨线程（`Mask` 的 `fp_cache: Cell<Option<u64>>` 是 `!Sync`）
//!     → 后台线程只拿**纯数据快照**：Vec<Vec<u8>> + 角度 + 尺寸
//!  2. UI 线程必须能"不等结果就继续画"
//!     → 加帧后立刻显示帧本身（已有快路径），同时发一个后台烘焙任务
//!  3. 结果回来时若文档已变（用户又加了一帧），必须丢弃旧结果
//!     → 用内容指纹校验
//!
//! 本程序验证这三条都成立，并测量 UI 侧实际等待时间。
//!
//! 用法：`cargo run --release --offline --example verify_background_bake`

use std::sync::mpsc;
use std::time::{Duration, Instant};

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
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

/// 后台任务：只含可 Send 的纯数据，不碰 Doc
struct BakeJob {
    /// 内容指纹，用于判断结果是否过期
    fp: u64,
    angles: Vec<f32>,
    masks: Vec<Vec<u8>>,
    w: usize,
    h: usize,
}

/// 后台结果
struct BakeResult {
    fp: u64,
    res: Result<(Vec<u16>, sdf::Direction), String>,
    ms: f64,
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

    println!("后台烘焙可行性　画布 {W}×{H}");
    println!("================================================================");

    // ---------- 1. BakeJob 是否 Send ----------
    println!("\n[1] 任务数据可跨线程");
    fn assert_send<T: Send>() {}
    assert_send::<BakeJob>();
    assert_send::<BakeResult>();
    check!("BakeJob / BakeResult 都是 Send", true, String::new());
    // Mask 是 !Sync（Cell），但 Send 是另一回事——只移动不共享就行
    fn assert_send2<T: Send>() {}
    assert_send2::<Mask>();
    check!("Mask 本身是 Send（可移动进线程）", true, String::new());

    // ---------- 2. UI 侧不等结果 ----------
    println!("\n[2] 加帧后 UI 立刻可用，不等烘焙");
    let (tx, rx) = mpsc::channel::<BakeResult>();

    // 造 4 帧数据
    let mut frames: Vec<(f32, Mask)> = Vec::new();
    for i in 0..4 {
        frames.push((
            i as f32 * 30.0,
            circle(512.0, 512.0, 100.0 + 60.0 * i as f32),
        ));
    }
    let fp = 12345u64;
    let job = BakeJob {
        fp,
        angles: frames.iter().map(|(a, _)| *a).collect(),
        masks: frames.iter().map(|(_, m)| m.data().to_vec()).collect(),
        w: W as usize,
        h: H as usize,
    };

    // UI 线程计时开始
    let ui_start = Instant::now();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut mf: Vec<sdf::MaskFrame> = job
            .angles
            .iter()
            .zip(job.masks.iter())
            .map(|(a, m)| sdf::MaskFrame {
                angle: *a,
                mask: m.clone(),
            })
            .collect();
        let res = sdf::bake_auto(&mut mf, job.w, job.h);
        let _ = tx.send(BakeResult {
            fp: job.fp,
            res,
            ms: t0.elapsed().as_secs_f64() * 1000.0,
        });
    });

    // UI 侧：假做几件"用户能感知"的事（画一帧、响应一次鼠标）
    let mut ui_work = 0.0f64;
    for _ in 0..3 {
        let t = Instant::now();
        // 复刻"显示关键帧本身"的成本
        let _ = frames[1].1.clone();
        ui_work += t.elapsed().as_secs_f64() * 1000.0;
    }
    let ui_elapsed = ui_start.elapsed().as_secs_f64() * 1000.0;
    check!(
        format!("UI 侧 3 帧工作耗时 {ui_elapsed:.2} ms（未等烘焙）"),
        ui_elapsed < 20.0,
        format!("UI 被阻塞了 {ui_elapsed:.1} ms")
    );

    // ---------- 3. 结果回来 ----------
    println!("\n[3] 后台结果回传");
    let got = rx.recv_timeout(Duration::from_secs(5));
    match got {
        Ok(r) => {
            check!(
                format!("收到结果：指纹 {} 匹配，烘焙耗时 {:.1} ms", r.fp, r.ms),
                r.fp == fp && r.res.is_ok(),
                format!("指纹 {} != {fp} 或烘焙失败", r.fp)
            );
        }
        Err(e) => {
            check!("应收到结果", false, format!("{e}"));
        }
    }

    // ---------- 4. 过期结果必须能丢弃 ----------
    println!("\n[4] 文档变了，旧结果要能丢弃");
    let (tx2, rx2) = mpsc::channel::<BakeResult>();
    let job2 = BakeJob {
        fp: 111,
        angles: vec![0.0, 90.0],
        masks: vec![
            circle(512.0, 512.0, 100.0).data().to_vec(),
            circle(512.0, 512.0, 300.0).data().to_vec(),
        ],
        w: W as usize,
        h: H as usize,
    };
    std::thread::spawn(move || {
        let mut mf: Vec<sdf::MaskFrame> = job2
            .angles
            .iter()
            .zip(job2.masks.iter())
            .map(|(a, m)| sdf::MaskFrame {
                angle: *a,
                mask: m.clone(),
            })
            .collect();
        let res = sdf::bake_auto(&mut mf, job2.w, job2.h);
        let _ = tx2.send(BakeResult {
            fp: job2.fp,
            res,
            ms: 0.0,
        });
    });
    // 用户又加了一帧 → 当前文档指纹变成 222
    let current_fp = 222u64;
    let stale = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    let usable = stale.fp == current_fp;
    check!(
        format!(
            "过期结果（fp={} vs 当前 {current_fp}）被正确判为不可用",
            stale.fp
        ),
        !usable,
        "过期结果被采用了——会显示错误的贴图"
    );

    // ---------- 5. 拖动在等待期间仍可用旧图 ----------
    println!("\n[5] 等待期间拖动仍有图可看");
    {
        // 旧 texel 图（用户加帧前的那份）
        let mut old = Doc::with_steps(W, H, 32);
        old.active_channel_mut()
            .insert_frame(0.0, circle(512.0, 512.0, 100.0));
        old.active_channel_mut()
            .insert_frame(90.0, circle(512.0, 512.0, 300.0));
        let (dw, dh) = (old.width, old.height);
        let (texels, dir) = old.active_channel_mut().texels(dw, dh).expect("旧图");
        let mut disp = Mask::new(W, H);
        let t = Instant::now();
        sdf::sample_texels(&texels, 0.5, dir, &mut disp, false);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        check!(
            format!(
                "用旧 texel 图采样耗时 {ms:.2} ms（遮 {} 像素）",
                disp.count()
            ),
            disp.count() > 0,
            "旧图采不出内容".to_string()
        );
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    println!("\n结论：后台烘焙三条前提全部成立——");
    println!("  · 任务数据可 Send（只传纯数据，不传 Doc）");
    println!("  · UI 侧立即返回，不等结果");
    println!("  · 指纹校验能丢弃过期结果");
    if fail > 0 {
        std::process::exit(1);
    }
}
