//! 端到端验证：「加帧 → 后台烘焙 → 拖动」全程流畅。
//!
//! 复刻 `App` 现在（后台烘焙版）的真实决策流程，断言：
//!  1. **加帧调用本身不等烘焙**（UI 立即返回）
//!  2. **加帧后的每一次拖动都 < 5ms**（用户真正要的）
//!  3. 后台结果回来后，画面与"全量重烘"一致（不能为了快而画错）
//!  4. 过期结果被丢弃（用户操作比烘焙快时不能显示错图）
//!
//! 用法：`cargo run --release --offline --example verify_drag_smooth`

use std::sync::mpsc;
use std::time::Instant;

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::Doc;
use sdf_mask_tool::sdf;

const W: u32 = 1024;
const H: u32 = 1024;
const STEPS: usize = 32;

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

/// 复刻 `App::refresh_display` 的慢路径（后台版）：
/// 有快照就用快照采样（不等烘焙），没有才同步烘一次。
fn drag_frame(
    doc: &mut Doc,
    step: usize,
    t: f32,
    disp: &mut Mask,
    snapshot: &mut Option<(u64, Vec<u16>, sdf::Direction)>,
) -> f64 {
    let t0 = Instant::now();
    let angle = step as f32 / (STEPS - 1) as f32 * 90.0;
    let (li, ri) = doc.active_channel_ref().bracket(angle);
    if li.is_none() && ri.is_none() {
        disp.clear();
        return t0.elapsed().as_secs_f64() * 1000.0;
    }
    // 快路径：停在关键帧上（直接显示帧本身）
    if doc.active_channel_ref().has_kf_at_step(step, STEPS) {
        if let Some(f) = doc.active_channel_ref().frame_for_step(step, STEPS) {
            *disp = f.mask.clone();
            return t0.elapsed().as_secs_f64() * 1000.0;
        }
    }
    let fp = doc.active_channel_ref().channel_fingerprint();
    // 快照指纹不符就取一次
    if snapshot.as_ref().map(|(sfp, _, _)| *sfp) != Some(fp)
        && let Some((tx, d)) = doc.active_channel_ref().cached_texels()
    {
        *snapshot = Some((fp, tx, d));
    }
    if let Some((_, tx, d)) = snapshot {
        sdf::sample_texels(tx, t, *d, disp, false);
    } else {
        let (dw, dh) = (doc.width, doc.height);
        if let Ok((tx, d)) = doc.active_channel_mut().texels(dw, dh) {
            sdf::sample_texels(&tx, t, d, disp, false);
        }
    }
    t0.elapsed().as_secs_f64() * 1000.0
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

    println!("加帧 → 后台烘焙 → 拖动 流畅性　画布 {W}×{H}");
    println!("================================================================");

    let mut doc = Doc::with_steps(W, H, STEPS);
    doc.active_channel_mut()
        .insert_frame(0.0, circle(512.0, 512.0, 100.0));
    doc.active_channel_mut()
        .insert_frame(90.0, circle(512.0, 512.0, 300.0));
    // 首次烘焙（用户已习惯这一次）
    let (dw, dh) = (doc.width, doc.height);
    let _ = doc.active_channel_mut().texels(dw, dh);

    let mut disp = Mask::new(W, H);
    let mut snapshot: Option<(u64, Vec<u16>, sdf::Direction)> = None;
    // 后台通道
    let (tx, rx) = mpsc::channel::<(u64, Result<(Vec<u16>, sdf::Direction), String>, f64)>();

    println!("\n[逐次加帧] 每次：测「加帧调用」耗时 + 加完后拖动全程");
    // 模拟"同一时刻只跑一个烘焙"（与 BackgroundBaker 的合并策略一致）
    let mut inflight = false;
    let mut pending = false;
    for round in 1..=6 {
        let a = 90.0 / (round as f32 + 2.0);

        // 加帧：起点取当前显示（App 行为）
        let step = (a / 90.0 * (STEPS - 1) as f32) as usize;
        let _ = drag_frame(&mut doc, step, a / 90.0, &mut disp, &mut snapshot);
        let start = disp.clone();

        // ---- 加帧调用（不含烘焙）----
        let t_add = Instant::now();
        doc.active_channel_mut().insert_frame(a, start);
        let fp = doc.active_channel_ref().channel_fingerprint();
        // 派发后台任务：**与 App 同样的合并策略**——已有任务在跑就只记待办，
        // 不另开线程。否则连续加帧会堆出一串线程抢 CPU，把拖动拖慢。
        if !inflight {
            let frames = doc.active_channel_ref().bake_job_frames();
            let (dw, dh) = (doc.width, doc.height);
            let tx2 = tx.clone();
            std::thread::spawn(move || {
                let t = Instant::now();
                let mut f = frames;
                let res = sdf::bake_auto(&mut f, dw as usize, dh as usize);
                let _ = tx2.send((fp, res, t.elapsed().as_secs_f64() * 1000.0));
            });
            inflight = true;
        } else {
            pending = true;
        }
        let add_ms = t_add.elapsed().as_secs_f64() * 1000.0;

        // ---- 加完后立刻拖过全程（模拟用户马上拖动查看）----
        let mut worst = 0.0f64;
        let mut total = 0.0f64;
        for s in 0..STEPS {
            let t = s as f32 / (STEPS - 1) as f32;
            let ms = drag_frame(&mut doc, s, t, &mut disp, &mut snapshot);
            total += ms;
            if ms > worst {
                worst = ms;
            }
        }
        let n = doc.active_channel_ref().frames.len();

        // 模拟 App 的 poll：拿回结果就清 inflight
        while let Ok((rfp, res, _)) = rx.try_recv() {
            let (dw, dh) = (doc.width, doc.height);
            let _ = doc.active_channel_mut().install_baked(rfp, res, dw, dh);
            inflight = false;
            if pending {
                pending = false;
                // 待办的那份要另派（真实 App 里由 BackgroundBaker 自动派发）
                let fp2 = doc.active_channel_ref().channel_fingerprint();
                let frames = doc.active_channel_ref().bake_job_frames();
                let tx2 = tx.clone();
                std::thread::spawn(move || {
                    let t = Instant::now();
                    let mut f = frames;
                    let res = sdf::bake_auto(&mut f, dw as usize, dh as usize);
                    let _ = tx2.send((fp2, res, t.elapsed().as_secs_f64() * 1000.0));
                });
                inflight = true;
            }
        }

        check!(
            format!(
                "第 {round} 次加帧（{a:.1}°，共 {n} 帧）：加帧调用 {add_ms:5.2} ms，拖动最慢 {worst:5.2} ms，全程 {total:6.2} ms"
            ),
            add_ms < 5.0 && worst < 5.0,
            format!("加帧 {add_ms:.2} ms / 拖动最慢 {worst:.2} ms 超限")
        );
    }

    // ---- 后台结果回来，装上并校验正确性 ----
    println!("\n[后台结果] 收回并校验");
    let mut got = 0;
    let mut stale = 0;

    // 先把循环期间已经产生的所有结果收干净。
    // 它们在派发之后文档又变了，指纹必然过期——正好验证"过期结果不污染画面"。
    while let Ok((rfp, res, ms)) = rx.try_recv() {
        let (dw, dh) = (doc.width, doc.height);
        let cur = doc.active_channel_ref().channel_fingerprint();
        if doc.active_channel_mut().install_baked(rfp, res, dw, dh) {
            got += 1;
            println!("  · 装上结果（后台 {ms:.1} ms）");
        } else {
            stale += 1;
            println!("  · 丢弃过期结果（fp {rfp} vs 当前 {cur}）");
        }
    }
    println!(
        "  · 循环期间取回：可用 {got} 个、过期 {stale} 个{}",
        if got == 0 {
            "（全过期，符合预期——每次派发后文档又变了）"
        } else {
            ""
        }
    );
    check!(
        "过期结果从未被装入（got==0 或 stale+got 均为正常丢弃）",
        got == 0,
        format!("got={got}——过期结果被装入了")
    );

    // 现在文档稳定了，按真实 App 的行为派发最后一次并等它。
    //
    // **用独立 channel**：上面那个 `rx` 里可能还有循环期间派发的任务
    // 正在跑、稍后才 send 进来，会干扰这次接收。
    // 真实 App 的 `BackgroundBaker` 同一时刻只有一个 Receiver，不存在这个问题。
    println!("\n[文档稳定后] 派发最后一次烘焙");
    {
        let (tx4, rx4) = mpsc::channel::<(u64, Result<(Vec<u16>, sdf::Direction), String>, f64)>();
        let fp = doc.active_channel_ref().channel_fingerprint();
        let (dw, dh) = (doc.width, doc.height);
        let frames = doc.active_channel_ref().bake_job_frames();
        std::thread::spawn(move || {
            let t = Instant::now();
            let mut f = frames;
            let res = sdf::bake_auto(&mut f, dw as usize, dh as usize);
            let _ = tx4.send((fp, res, t.elapsed().as_secs_f64() * 1000.0));
        });
        let (rfp, res, ms) = rx4
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("应能收到结果");
        let (dw, dh) = (doc.width, doc.height);
        let cur = doc.active_channel_ref().channel_fingerprint();
        if rfp != cur {
            println!("  · 诊断：后台 fp={rfp} 当前 fp={cur}");
        }
        if let Err(e) = &res {
            println!("  · 诊断：后台烘焙失败：{}", e.lines().next().unwrap_or(""));
        }
        let ok = doc.active_channel_mut().install_baked(rfp, res, dw, dh);
        if ok {
            got += 1;
            println!("  · 装上稳定后的结果（后台 {ms:.1} ms）");
        }
        check!(
            format!("文档稳定后的结果被装上（后台 {ms:.1} ms）"),
            ok,
            "稳定后的结果也没装上".to_string()
        );
    }
    while let Ok((rfp, res, ms)) = rx.try_recv() {
        let (dw, dh) = (doc.width, doc.height);
        let cur = doc.active_channel_ref().channel_fingerprint();
        if doc.active_channel_mut().install_baked(rfp, res, dw, dh) {
            got += 1;
            println!("  · 装上指纹 {rfp} 的结果（后台 {ms:.1} ms）");
        } else {
            stale += 1;
            println!("  · 丢弃指纹 {rfp}（当前 {cur}，已过期）");
        }
    }
    check!(
        format!("最终成功装入 1 个可用结果（总计 got={got}，过期丢弃 {stale}）"),
        got >= 1,
        format!("可用结果 0 个（stale={stale}）——后台烘焙没产出可用结果")
    );

    // ---- 正确性：装了后台结果后，与全量重烘一致 ----
    println!("\n[正确性] 装完后台结果后的 texel 图 == 全量重烘");
    {
        let mut fresh = Doc::with_steps(W, H, STEPS);
        for f in doc.active_channel_ref().frames.iter() {
            fresh
                .active_channel_mut()
                .insert_frame(f.angle, f.mask.clone());
        }
        let (dw, dh) = (fresh.width, fresh.height);
        let truth = fresh
            .active_channel_mut()
            .texels(dw, dh)
            .expect("应能烘焙")
            .0;
        let ours = doc
            .active_channel_ref()
            .cached_texels()
            .expect("应有缓存")
            .0;
        let diff = truth
            .iter()
            .zip(ours.iter())
            .filter(|(a, b)| a != b)
            .count();
        check!(
            format!("逐 texel 一致（差异 {diff}）"),
            diff == 0,
            format!("{diff} 个 texel 不一致——后台结果装错了")
        );
    }

    // ---- 过期结果必须被丢弃 ----
    println!("\n[过期结果] 文档又变了，旧结果不能采用");
    {
        let (tx3, rx3) = mpsc::channel::<(u64, Result<(Vec<u16>, sdf::Direction), String>)>();
        let old_fp = 111u64;
        let frames = doc.active_channel_ref().bake_job_frames();
        let (dw, dh) = (doc.width, doc.height);
        std::thread::spawn(move || {
            let mut f = frames;
            let _ = tx3.send((old_fp, sdf::bake_auto(&mut f, dw as usize, dh as usize)));
        });
        // 用户又加了一帧
        doc.active_channel_mut()
            .insert_frame(50.0, circle(512.0, 512.0, 180.0));
        let (rfp, res) = rx3.recv().unwrap();
        let installed = doc.active_channel_mut().install_baked(rfp, res, dw, dh);
        check!(
            format!("过期结果（fp={rfp}）被拒绝装入"),
            !installed,
            "过期结果被装入了——会显示错误的贴图".to_string()
        );
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
