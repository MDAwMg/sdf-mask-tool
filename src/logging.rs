//! 日志：同时输出到控制台、内存环形缓冲（给界面日志面板看）和磁盘文件。
//!
//! 为什么需要日志：这个工具的很多问题（绘制后不刷新、缓存没失效、
//! 底图纹理没重建、距离场算错）在界面上表现为"看起来没反应"，
//! 没有日志就只能靠猜。日志要能回答三个问题：发生了什么、
//! 在什么参数下发生、上一次为什么没生效。

use std::io::Write;
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// 内存里保留的日志条数
const RING_CAPACITY: usize = 2000;

#[derive(Clone, Debug)]
pub struct LogLine {
    pub level: Level,
    pub target: String,
    pub message: String,
}

static RING: Mutex<Vec<LogLine>> = Mutex::new(Vec::new());
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

struct AppLogger;

impl Log for AppLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = LogLine {
            level: record.level(),
            target: record.target().to_string(),
            message: format!("{}", record.args()),
        };

        // 控制台
        match record.level() {
            Level::Error => eprintln!("[E] {} — {}", line.target, line.message),
            Level::Warn => eprintln!("[W] {} — {}", line.target, line.message),
            Level::Info => println!("[I] {} — {}", line.target, line.message),
            _ => println!("[D] {} — {}", line.target, line.message),
        }

        // 文件
        if let Ok(mut guard) = FILE.lock()
            && let Some(f) = guard.as_mut()
        {
            let _ = writeln!(
                f,
                "{} [{}] {} — {}",
                now_stamp(),
                record.level(),
                line.target,
                line.message
            );
            let _ = f.flush();
        }

        // 环形缓冲
        if let Ok(mut ring) = RING.lock() {
            if ring.len() >= RING_CAPACITY {
                ring.remove(0);
            }
            ring.push(line);
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = FILE.lock()
            && let Some(f) = guard.as_mut()
        {
            let _ = f.flush();
        }
    }
}

fn now_stamp() -> String {
    // 不引入 chrono，用系统时间拼一个够用的时间戳
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    let ms = d.subsec_millis();
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// 初始化日志。`SDF_LOG=debug` 可提高级别；默认 info。
///
/// 日志文件写在程序同目录的 `sdf-mask-tool.log`，写不开就只走内存和控制台。
pub fn init() {
    let filter = match std::env::var("SDF_LOG").as_deref() {
        Ok("trace") => LevelFilter::Trace,
        Ok("debug") => LevelFilter::Debug,
        Ok("warn") => LevelFilter::Warn,
        Ok("error") => LevelFilter::Error,
        _ => LevelFilter::Info,
    };

    // 日志文件：优先放在可执行文件旁边，失败就退化到临时目录
    let candidates = [
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("sdf-mask-tool.log"))),
        Some(std::env::temp_dir().join("sdf-mask-tool.log")),
    ];
    for c in candidates.into_iter().flatten() {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&c)
        {
            // Rust 的 write! 对 String 天然输出 UTF-8，文件本身就是 UTF-8
            *FILE.lock().unwrap() = Some(f);
            break;
        }
    }

    let logger = Box::leak(Box::new(AppLogger));
    if log::set_logger(logger).is_ok() {
        log::set_max_level(filter);
    }

    log::info!("日志已启动（级别 {:?}）", filter);
    if let Ok(guard) = FILE.lock()
        && let Some(f) = guard.as_ref()
    {
        let _ = f.metadata();
    }
}

/// 取最近的日志（最新的在末尾）
pub fn snapshot() -> Vec<LogLine> {
    RING.lock().map(|r| r.clone()).unwrap_or_default()
}

pub fn clear() {
    if let Ok(mut r) = RING.lock() {
        r.clear();
    }
}

/// 日志面板
pub fn show_panel(ui: &mut egui::Ui) {
    use eframe::egui::{self, Color32, RichText};

    ui.horizontal(|ui| {
        ui.label(RichText::new("日志").strong());
        if ui.small_button("清空").clicked() {
            clear();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let n = snapshot().len();
            ui.label(
                RichText::new(format!("{n} 条"))
                    .size(11.0)
                    .color(Color32::from_gray(0x88)),
            );
        });
    });
    ui.separator();

    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .id_salt("log_scroll")
        .show(ui, |ui| {
            for line in snapshot() {
                let (color, tag) = match line.level {
                    log::Level::Error => (Color32::from_rgb(0xe0, 0x6c, 0x6c), "E"),
                    log::Level::Warn => (Color32::from_rgb(0xd8, 0xa0, 0x50), "W"),
                    log::Level::Info => (Color32::from_rgb(0x9c, 0xbe, 0x9c), "I"),
                    _ => (Color32::from_gray(0x88), "D"),
                };
                ui.label(
                    RichText::new(format!("{tag} {} — {}", line.target, line.message))
                        .size(11.0)
                        .color(color),
                );
            }
        });
}
