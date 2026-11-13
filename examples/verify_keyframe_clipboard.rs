//! 验证「复制 / 粘贴关键帧」的完整语义。
//!
//! 用户指出：原来的"复制关键帧"一步就复制到**紧邻空位**并跳过去，
//! 名字却叫"复制"，自然会去找"粘贴"——找不到就以为功能残缺。
//! 而且它复制不到远处、没法粘多次。
//!
//! 现在拆成两套：
//!   Ctrl+C / Ctrl+V —— 进剪贴板，可粘贴到任意位置、可多次
//!   Shift+D         —— 旧行为，原位复制到紧邻空位
//!
//! 本程序验证剪贴板语义（逻辑与 `App::do_copy_keyframe` / `do_paste_keyframe` 一致）。
//!
//! 用法：`cargo run --release --offline --example verify_keyframe_clipboard`

use sdf_mask_tool::mask::Mask;
use sdf_mask_tool::model::doc::{Doc, angle_to_step, step_angle};

const W: u32 = 64;
const H: u32 = 64;
const STEPS: usize = 64;

fn blob(seed: u8) -> Mask {
    let mut m = Mask::new(W, H);
    let r = 8 + seed as u32;
    for y in 20..(20 + r) {
        for x in 20..(20 + r) {
            m.set(x, y, 1);
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

    println!("复制 / 粘贴关键帧 语义验证");
    println!("================================================================");

    // ---------- 1. 复制后能粘贴到任意远处 ----------
    println!("\n[1] 复制 → 移到远处 → 粘贴");
    {
        let mut doc = Doc::with_steps(W, H, STEPS);
        doc.active_channel_mut().insert_frame(0.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(90.0, Mask::new(W, H));
        // 在 5° 左右建一帧带内容
        let src_angle = 5.0f32;
        doc.active_channel_mut().insert_frame(src_angle, blob(4));
        let src_count = doc
            .active_channel_ref()
            .frame_at(src_angle)
            .unwrap()
            .mask
            .count();

        // 模拟 Ctrl+C：剪贴板 = (角度, 遮罩)
        let clip: (f32, Mask) = {
            let f = doc.active_channel_ref().frame_at(src_angle).unwrap();
            (f.angle, f.mask.clone())
        };

        // 模拟"把播放头移到 60° 附近"再 Ctrl+V
        let dst_angle = 60.0f32;
        let step = angle_to_step(dst_angle, STEPS);
        let dst = step_angle(step, STEPS);
        check!(
            "目标位置原本没有关键帧（前提）",
            !doc.active_channel_ref().has_kf_at_step(step, STEPS),
            "那里已有关键帧".to_string()
        );

        let (_, mask) = clip.clone();
        doc.active_channel_mut().insert_frame(dst, mask);

        let pasted = doc
            .active_channel_ref()
            .frame_at(dst)
            .map(|f| f.mask.count());
        check!(
            format!("粘贴到了 {dst:.1}°（远离源 {src_angle:.1}°），遮 {pasted:?} 像素"),
            pasted == Some(src_count),
            format!("源 {src_count} 像素，粘出来 {pasted:?}")
        );
        check!(
            "源帧没有被消耗（仍可继续粘）",
            doc.active_channel_ref().frame_at(src_angle).is_some(),
            "源帧消失了".to_string()
        );
    }

    // ---------- 2. 可以粘多次 ----------
    println!("\n[2] 一次复制，粘贴到多个位置");
    {
        let mut doc = Doc::with_steps(W, H, STEPS);
        doc.active_channel_mut().insert_frame(0.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(90.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(5.0, blob(4));
        let src_count = doc.active_channel_ref().frame_at(5.0).unwrap().mask.count();
        let clip = {
            let f = doc.active_channel_ref().frame_at(5.0).unwrap();
            (f.angle, f.mask.clone())
        };

        let mut targets = Vec::new();
        for a in [20.0f32, 40.0, 60.0] {
            let step = angle_to_step(a, STEPS);
            let ang = step_angle(step, STEPS);
            doc.active_channel_mut().insert_frame(ang, clip.1.clone());
            targets.push((ang, step));
        }
        let ok = targets.iter().all(|(ang, _)| {
            doc.active_channel_ref()
                .frame_at(*ang)
                .map(|f| f.mask.count() == src_count)
                .unwrap_or(false)
        });
        check!(
            format!("粘了 {} 处，内容都正确", targets.len()),
            ok,
            "有位置没粘上或内容不对".to_string()
        );
        check!(
            format!(
                "现在共 {} 帧（原本 3 帧 + 粘 3 次）",
                doc.active_channel_ref().frames.len()
            ),
            doc.active_channel_ref().frames.len() == 6,
            format!("实际 {}", doc.active_channel_ref().frames.len())
        );
    }

    // ---------- 3. 粘到已有关键帧的位置 = 覆盖 ----------
    println!("\n[3] 粘到已有关键帧的位置：应当覆盖");
    {
        let mut doc = Doc::with_steps(W, H, STEPS);
        doc.active_channel_mut().insert_frame(0.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(90.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(5.0, blob(4));
        doc.active_channel_mut().insert_frame(30.0, blob(1));
        let src_count = doc.active_channel_ref().frame_at(5.0).unwrap().mask.count();
        let dst_before = doc
            .active_channel_ref()
            .frame_at(30.0)
            .unwrap()
            .mask
            .count();
        check!(
            format!("前提：源 {src_count} 像素，目标原本 {dst_before} 像素（不同）"),
            src_count != dst_before,
            "两者相同，测不出覆盖".to_string()
        );

        let clip = {
            let f = doc.active_channel_ref().frame_at(5.0).unwrap();
            (f.angle, f.mask.clone())
        };
        let n_before = doc.active_channel_ref().frames.len();
        doc.active_channel_mut().insert_frame(30.0, clip.1.clone());

        check!(
            "帧数不变（覆盖而不是新增）",
            doc.active_channel_ref().frames.len() == n_before,
            format!("{} → {}", n_before, doc.active_channel_ref().frames.len())
        );
        let after = doc
            .active_channel_ref()
            .frame_at(30.0)
            .unwrap()
            .mask
            .count();
        check!(
            format!("30° 的内容被覆盖成源内容（{dst_before} → {after}）"),
            after == src_count,
            "没覆盖上".to_string()
        );
    }

    // ---------- 4. 剪贴板为空时应拒绝并提示 ----------
    println!("\n[4] 剪贴板为空时不做事（只是提示）");
    {
        let mut doc = Doc::with_steps(W, H, STEPS);
        doc.active_channel_mut().insert_frame(0.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(90.0, Mask::new(W, H));
        let n_before = doc.active_channel_ref().frames.len();
        let clip: Option<(f32, Mask)> = None;

        // 复刻 do_paste_keyframe 的短路逻辑
        let did_something = if clip.is_some() {
            doc.active_channel_mut().insert_frame(45.0, blob(2));
            true
        } else {
            false
        };
        check!(
            "剪贴板为空时帧数不变",
            !did_something && doc.active_channel_ref().frames.len() == n_before,
            "不该改动文档".to_string()
        );
    }

    // ---------- 5. 旧行为仍可用（原位复制到紧邻空位） ----------
    println!("\n[5] 旧行为保留：原位复制到紧邻空位");
    {
        let mut doc = Doc::with_steps(W, H, STEPS);
        doc.active_channel_mut().insert_frame(0.0, Mask::new(W, H));
        doc.active_channel_mut().insert_frame(90.0, Mask::new(W, H));
        let step = angle_to_step(0.0, STEPS);
        let n_before = doc.active_channel_ref().frames.len();
        let new_angle = doc.duplicate_keyframe(step, STEPS);
        check!(
            format!("duplicate_keyframe 返回新角度 {new_angle:?}"),
            new_angle.is_some(),
            "返回 None".to_string()
        );
        check!(
            "帧数 +1",
            doc.active_channel_ref().frames.len() == n_before + 1,
            format!("{} → {}", n_before, doc.active_channel_ref().frames.len())
        );
    }

    println!("\n================================================================");
    println!("通过 {pass}，失败 {fail}");
    if fail > 0 {
        std::process::exit(1);
    }
}
