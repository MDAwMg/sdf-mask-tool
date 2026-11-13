//! 后台烘焙：把「算」和「显示」彻底分开。
//!
//! # 为什么要单独一个模块
//!
//! 烘焙 1K 八帧要 50–90ms。这是**算法成本**，不随人意志改变。
//! 唯一能做的是让烘焙不阻塞 UI。明确要求：
//!
//! > 将 UI 操作效果和算法逻辑分离，UI 显示 UI 的，加帧等在后台加帧
//!
//! # 设计约束（都已用 `examples/verify_background_bake.rs` 验证）
//!
//! 1. **`Doc` 不能跨线程**：`Mask` 的 `fp_cache: Cell<Option<u64>>` 使 `Mask` 为 `!Sync`。
//!    所以后台只接收纯数据快照 `Vec<Vec<u8>>`，不碰文档。
//!
//! 2. **结果可能过期**：连续加两次点，第一次的结果已被后续改动作废。
//!    用内容指纹校验——指纹对不上就丢弃，绝不显示错误的贴图。
//!
//! 3. **同一时刻只跑一个任务**：烘焙是幂等的，新任务直接顶掉旧的即可。
//!    排队没有意义（中间结果谁也不要），而且会积压。
//!
//! # 与 UI 的契约
//!
//! - `request()` 立刻返回，UI 继续用**已有的** texel 图渲染（不阻塞）
//! - `poll()` 每帧调一次，拿到结果就装上
//! - 结果未到时拖动仍流畅（旧图采样只要 0.27ms）

use std::sync::mpsc::{self, Receiver, TryRecvError};

use sdf_mask_tool::sdf;

/// 后台任务：只含可 Send 的纯数据。
struct Job {
    /// 发起时的内容指纹。回来时与当前比对，不一致就丢弃。
    fp: u64,
    frames: Vec<sdf::MaskFrame>,
    w: usize,
    h: usize,
}

/// 后台算完的结果。
pub struct BakedResult {
    pub fp: u64,
    pub res: Result<(Vec<u16>, sdf::Direction), String>,
    /// 实际计算耗时（毫秒），用于日志与自检
    pub ms: f64,
}

/// 排队等待派发的任务（同一时刻只跑一个烘焙）。
struct PendingJob {
    fp: u64,
    frames: Vec<sdf::MaskFrame>,
    w: usize,
    h: usize,
}

/// 后台烘焙器。持有当前任务的接收端。
pub struct BackgroundBaker {
    rx: Option<Receiver<BakedResult>>,
    /// 正在算的指纹（用于避免重复派发同一个任务）
    running_fp: Option<u64>,
    /// 有任务在跑时又来了新请求 → 只留最新的一份，等当前算完再派。
    ///
    /// 连续加 5 个点时，中间那 4 份结果无用，只需保留最后一份。
    pending: Option<PendingJob>,
    /// 累计完成的次数与总耗时，供状态栏显示
    pub done_count: u64,
    pub last_ms: f64,
    /// 被顶掉/合并掉的任务数（反映操作频率）
    pub superseded: u64,
}

impl Default for BackgroundBaker {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundBaker {
    pub fn new() -> Self {
        Self {
            rx: None,
            running_fp: None,
            pending: None,
            done_count: 0,
            last_ms: 0.0,
            superseded: 0,
        }
    }

    /// 是否有任务在跑
    pub fn is_busy(&self) -> bool {
        self.rx.is_some()
    }

    /// 这个指纹是否已经在算了。
    ///
    /// 调用方**必须先问这个再准备数据**：准备任务数据要拷全部遮罩
    /// （1K 八帧 8MB），而 `refresh_display` 是每帧都跑的。
    /// 先判断再拷贝，能省掉每帧 8MB 的内存带宽。
    pub fn is_running(&self, fp: u64) -> bool {
        self.running_fp == Some(fp)
    }

    /// 派发一次烘焙。**立即返回**，不等结果。
    ///
    /// # 为什么不做队列
    ///
    /// 烘焙是幂等的：新任务的结果完全覆盖旧任务。排队只会积压无用计算，
    /// 而且会让"最后加的那帧"排在很后面才烘完。所以这里**新任务直接顶掉旧的**：
    /// 旧线程仍存活时让其自行算完消亡（结果由指纹校验丢弃）。
    ///
    /// # 为什么要限制并发
    ///
    /// 连续快速加点时，每个点都会派发一次。若无限制，
    /// 8 个任务同时抢 24 个核里的 EDT，会把 UI 线程也挤掉——
    /// 实测拖动从 0.5ms 恶化到 26ms。所以同一时刻只允许一个烘焙任务。
    pub fn request(&mut self, fp: u64, frames: Vec<sdf::MaskFrame>, w: usize, h: usize) {
        // 同一个指纹已经在算 → 不重复派发
        if self.running_fp == Some(fp) {
            return;
        }
        // **已有任务在跑就先不派发**，记下待办，等当前任务结束后再派。
        // 这是"合并请求"：连续加 5 个点，只需烘最后一份。
        if self.rx.is_some() {
            self.pending = Some(PendingJob { fp, frames, w, h });
            self.superseded += 1;
            return;
        }
        self.spawn(fp, frames, w, h);
    }

    fn spawn(&mut self, fp: u64, frames: Vec<sdf::MaskFrame>, w: usize, h: usize) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let mut frames = frames;
            let res = sdf::bake_auto(&mut frames, w, h);
            let _ = tx.send(BakedResult {
                fp,
                res,
                ms: t0.elapsed().as_secs_f64() * 1000.0,
            });
        });
        self.rx = Some(rx);
        self.running_fp = Some(fp);
    }

    /// 每帧调一次：尝试取回结果。取到且指纹匹配才返回 `Some`。
    ///
    /// `current_fp` 是**现在**文档的指纹。若与结果的不一致，
    /// 说明等待期间文档又被改动，这个结果已经过期，直接丢弃。
    pub fn poll(&mut self, current_fp: u64) -> Option<BakedResult> {
        let rx = self.rx.as_ref()?;
        match rx.try_recv() {
            Ok(r) => {
                self.rx = None;
                self.running_fp = None;
                self.done_count += 1;
                self.last_ms = r.ms;

                // 先看有没有排队的任务要先派发（文档在这期间又被改动）
                let usable = r.fp == current_fp;
                if let Some(p) = self.pending.take() {
                    // 排队的任务才是最新的，派发该任务
                    self.spawn(p.fp, p.frames, p.w, p.h);
                    // 当前这份已过期（文档早已变了），丢掉
                    if usable {
                        self.superseded += 1;
                    }
                    return None;
                }
                if usable {
                    Some(r)
                } else {
                    self.superseded += 1;
                    None
                }
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // 线程崩了（比如 panic），清理状态避免永久卡在 busy
                self.rx = None;
                self.running_fp = None;
                None
            }
        }
    }

    /// 是否有排队的任务还没派发
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}
