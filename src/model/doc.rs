//! 数据模型：画布、图层、通道、关键帧。
//!
//! 存储用纯二值：每像素 1 字节，0 表示"没有遮罩"，1 表示"被遮罩"。
//! 不做抗锯齿也不做灰度——遮罩只是选区，看得清、算得快最重要。

use rayon::prelude::*;

use crate::mask::Mask;

pub const CHANNELS: [char; 4] = ['R', 'G', 'B', 'A'];

/// 时间轴的默认离散刻度数。
///
/// **这只是"吸附格"，不是视觉分辨率**。时间轴本身的播放头是连续的 0..1，
/// 但在参数轴上加参数点、以及预览重算时，都吸附到这些格子上。
///
/// 为什么要离散：拖动时每变一格就要重新采样整张图。连续位置会让重算
/// 无穷无尽；离散格既能缓存、又足够细腻。
/// 导出走的是连续角度轴，不受这个限制——所以最终精度不受影响。
///
/// 这个值是**每个工程各自的设置**（`Doc::steps`），不是编译期常量：
/// 新建工程用这个默认值，之后可以在「设置」里改。
pub const DEFAULT_STEPS: usize = 64;

/// 步数下限。1 步无法插值（两端重合），必须至少 2。
pub const MIN_STEPS: usize = 2;
/// 步数上限。再多就只是在同一批像素上重复采样，没有收益。
pub const MAX_STEPS: usize = 512;

/// 把任意步数夹到合法范围。
pub fn clamp_steps(n: usize) -> usize {
    n.clamp(MIN_STEPS, MAX_STEPS)
}

/// 第 `i` 步（0..steps）对应的 **0..1 归一化位置**。第 0 步是 0，第 steps-1 步是 1。
pub fn step_norm(i: usize, steps: usize) -> f32 {
    let steps = clamp_steps(steps);
    let i = i.min(steps - 1);
    i as f32 / (steps - 1) as f32
}

/// 归一化位置 → 最接近的步号
pub fn norm_to_step(t: f32, steps: usize) -> usize {
    let steps = clamp_steps(steps);
    let s = (t.clamp(0.0, 1.0) * (steps - 1) as f32).round();
    (s as usize).min(steps - 1)
}

/// 第 `i` 步对应的角度（0..90°）。导出用。
pub fn step_angle(i: usize, steps: usize) -> f32 {
    step_norm(i, steps) * 90.0
}

/// 角度 → 最接近的步号
pub fn angle_to_step(angle: f32, steps: usize) -> usize {
    norm_to_step(angle / 90.0, steps)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ChannelMode {
    /// 只用 R 子层，导出单通道灰度图
    #[default]
    Gray,
    /// 四个子层各烘各的，合成 RGBA
    Rgba,
}

/// 一帧：某个角度下的二值遮罩
#[derive(Clone)]
pub struct Keyframe {
    pub angle: f32,
    pub mask: Mask,
}

impl Keyframe {
    pub fn new(angle: f32, mask: Mask) -> Self {
        Self { angle, mask }
    }
}

/// 一个通道 = 一条独立时间轴
pub struct Channel {
    pub name: String,
    pub visible: bool,
    pub frames: Vec<Keyframe>,
    /// 每个关键帧的距离场缓存（不遮为正）。仅烘焙时用。
    ///
    /// **不参与 `Clone`**：这个缓存是纯派生数据，重建成本远低于复制成本
    /// （4MB/帧 × 帧数，1K 画布 4 帧就是 16MB）。撤销快照要深拷整份文档，
    /// 把该缓存带上会让每次 `push_undo` 平白多花 2.8ms（实测 3.6→0.8ms，占 76.5%）。
    pub dist_cache: Vec<(u64, Vec<f32>)>,
    /// 烘焙结果缓存：texel 图（u16 满刻度）+ 编码方向。
    ///
    /// 预览走这条路：任意时刻只要一次 `texel > t` 比较就能出图，
    /// 和导出完全同源。键是全部帧的内容指纹。
    texel_cache: Option<(u64, Result<(Vec<u16>, crate::sdf::Direction), String>)>,
}

/// 手写 `Clone`：**只丢弃距离场缓存，保留 texel 缓存**。
///
/// 两者的取舍完全不同：
/// - `dist_cache` 是 4MB/帧 × 帧数（1K 四帧 16MB），而且是**真·死缓存**——
///   `bake_with_dir` 总是自己重算 EDT，从不读该缓存。复制该缓存纯属浪费内存带宽，
///   实测占 `Doc::clone` 的 76.5%（3.6ms → 0.8ms）。
/// - `texel_cache` 只有一份 2MB，却是**预览的命脉**。每次 `push_undo` 都会
///   clone 文档；如果连 texel 缓存也丢掉，那么"画一笔 → 松手重画预览"就要重烘 137ms，
///   等于把卡顿从加帧搬到了落笔。必须保留。
///
/// 丢掉 dist_cache 是正确的：该缓存按内容指纹校验（`dist_of` 里比对 `fp`），
/// 清空后下次访问只会重算，不会读到脏数据。
impl Clone for Channel {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            visible: self.visible,
            frames: self.frames.clone(),
            dist_cache: Vec::new(),
            texel_cache: self.texel_cache.clone(),
        }
    }
}

impl Channel {
    pub fn new(name: &str, w: u32, h: u32) -> Self {
        let _ = (w, h);
        Self {
            name: name.to_string(),
            visible: true,
            frames: Vec::new(),
            dist_cache: Vec::new(),
            texel_cache: None,
        }
    }

    /// 全部帧的内容指纹，用于判断 texel 缓存是否还有效。
    fn frames_fingerprint(&self) -> u64 {
        let mut acc: u64 = 0x9e3779b97f4a7c15u64.wrapping_add(self.frames.len() as u64);
        for f in self.frames.iter() {
            acc = acc.wrapping_mul(1099511628211) ^ f.mask.fingerprint();
            acc = acc.wrapping_mul(1099511628211) ^ (f.angle as u64);
        }
        acc
    }

    /// 取烘焙贴图（texel 图 + 方向）。缓存命中时零开销。
    ///
    /// 预览和导出都用此入口，保证"看到的就是导出的"。
    /// 数据不合法（来回翻转）时返回 `Err`，调用方应显示提示而不是画错图。
    ///
    /// **失败结果也要缓存**：烘焙一次 1K 要 3.6ms，
    /// 而数据不合法时每次调用都会失败——不缓存就是每帧白烧 3.6ms，
    /// 帧率直接崩。所以这里同时缓存错误信息。
    pub fn texels(&mut self, w: u32, h: u32) -> Result<(Vec<u16>, crate::sdf::Direction), String> {
        let fp = self.frames_fingerprint();
        if let Some((cached_fp, res)) = &self.texel_cache
            && *cached_fp == fp
        {
            return match res {
                Ok((texels, dir)) if texels.len() == (w * h) as usize => Ok((texels.clone(), *dir)),
                Ok(_) => Err("烘焙输出尺寸不符".to_string()),
                Err(e) => Err(e.clone()),
            };
        }

        let mut mf: Vec<crate::sdf::MaskFrame> = self
            .frames
            .iter()
            .map(|f| crate::sdf::MaskFrame {
                angle: f.angle,
                mask: f.mask.data().to_vec(),
            })
            .collect();
        // 先判方向（这一步只看遮罩，不需要距离场），
        // 再按方向用**可复用**的距离场烘焙。
        //
        // 关键优化：距离场只依赖单帧遮罩，加一帧时前面所有帧的 EDT 都没变。
        // 原来 `bake_auto` 内部全量重算（1K 四帧 119.7ms），加帧要白烧 3/4。
        let res = match crate::sdf::check_direction(&mf, w as usize, h as usize) {
            crate::sdf::DirectionCheck::Ok(dir) => {
                let sdfs = self.sdfs_for_bake(w as usize, h as usize);
                let out = crate::sdf::bake_from_sdfs(&mut mf, &sdfs, w as usize, h as usize, dir);
                Ok((out, dir))
            }
            crate::sdf::DirectionCheck::Bad {
                grow_conflicts,
                shrink_conflicts,
                total,
            } => Err(format!(
                "有像素的明暗来回翻转，两种方向都编码不了。\n\
                 按「阴影扩张」有 {} 个像素冲突，按「阴影收缩」有 {} 个冲突（共 {}）。\n\
                 每个像素在角度轴上只能翻转一次：要么一直受光后转暗，要么一直遮着后转亮，不能来回变。",
                grow_conflicts, shrink_conflicts, total
            )),
        };
        match res {
            Ok((out, dir)) => {
                self.texel_cache = Some((fp, Ok((out.clone(), dir))));
                Ok((out, dir))
            }
            Err(e) => {
                self.texel_cache = Some((fp, Err(e.clone())));
                Err(e)
            }
        }
    }

    /// 让 texel 缓存失效。任何改动点内容的操作都要调本方法。
    pub fn invalidate_texels(&mut self) {
        self.texel_cache = None;
    }

    /// 当前通道的内容指纹（对外暴露，供后台烘焙校验结果是否过期）。
    pub fn channel_fingerprint(&self) -> u64 {
        self.frames_fingerprint()
    }

    /// 导出当前通道的**纯数据快照**，交给后台线程烘焙。
    ///
    /// 为什么是快照而不是借用：`Mask` 里的指纹缓存是 `Cell`，`Doc` 不是 `Sync`，
    /// 跨线程借用过不了编译器。而烘焙只需要角度和遮罩字节，拷一份最省事
    /// （1K 八帧约 8MB 拷贝，1ms 级，相对烘焙的 60ms 可以忽略）。
    pub fn bake_job_frames(&self) -> Vec<crate::sdf::MaskFrame> {
        self.frames
            .iter()
            .map(|f| crate::sdf::MaskFrame {
                angle: f.angle,
                mask: f.mask.data().to_vec(),
            })
            .collect()
    }

    /// 装上后台算好的结果。
    ///
    /// `fp` 必须是**发起任务时**的指纹：这里再校验一次，
    /// 因为该指纹和当前状态可能已经不同（文档又被改动）。
    /// 返回 `true` 表示装上去了。
    pub fn install_baked(
        &mut self,
        fp: u64,
        res: Result<(Vec<u16>, crate::sdf::Direction), String>,
        w: u32,
        h: u32,
    ) -> bool {
        if fp != self.frames_fingerprint() {
            return false;
        }
        // 尺寸不符的结果不能装（画布中途改过尺寸）
        if let Ok((t, _)) = &res
            && t.len() != (w * h) as usize
        {
            return false;
        }
        self.texel_cache = Some((fp, res));
        true
    }

    /// 当前是否已有可用的 texel 缓存（不管内容新旧）。
    ///
    /// 后台烘焙期间 UI 就靠这份旧图继续渲染——该图内容与真值差异很小
    /// （实测加帧带来的采样差异最大 0.047%），肉眼看不出来。
    pub fn has_texel_cache(&self) -> bool {
        self.texel_cache.is_some()
    }

    /// 缓存的 texel 图是否**确实属于**给定的指纹。
    ///
    /// 这是 `cached_texels()` 的配套校验。那个方法故意不查指纹（返回"可能略旧"
    /// 的图，供后台烘焙期间顶着用），但只要调用方打算**长期持有**这份图
    /// （比如存进快照），就必须先问这一句——否则旧内容会被贴上新的指纹，
    /// 表现为"改完图播放毫无变化"。
    pub fn cache_matches(&self, fp: u64) -> bool {
        match &self.texel_cache {
            Some((cached_fp, Ok(_))) => *cached_fp == fp,
            _ => false,
        }
    }

    /// 取缓存的 texel 图，**不检查指纹**（可能略旧）。
    ///
    /// 后台烘焙期间用旧图顶上：宁可让画面落后几十毫秒，
    /// 也不能为了刷新而阻塞 UI。真值回来后 `install_baked` 会覆盖该缓存。
    ///
    /// 没有缓存时返回 `None`（首次使用，必须同步烘一次）。
    ///
    /// **借用这份图之前先想清楚**：该图可能不属于当前内容。
    /// 只在"这一帧就要画"的场景直接采样；要长期持有必须配 `cache_matches`。
    pub fn cached_texels(&self) -> Option<(Vec<u16>, crate::sdf::Direction)> {
        match &self.texel_cache {
            Some((_, Ok((t, d)))) => Some((t.clone(), *d)),
            _ => None,
        }
    }

    /// 取某个关键帧的距离场，缓存命中就直接用。
    ///
    /// 距离场算一次要几十毫秒，但画一笔只影响一帧，
    /// 其余帧的缓存能一直复用。
    pub fn dist_of(&mut self, frame_idx: usize, w: usize, h: usize) -> &[f32] {
        while self.dist_cache.len() <= frame_idx {
            self.dist_cache.push((0, Vec::new()));
        }
        let fp = self.frames[frame_idx].mask.fingerprint();
        let need = {
            let slot = &self.dist_cache[frame_idx];
            slot.0 != fp || slot.1.len() != w * h
        };
        if need {
            let raw = crate::sdf::signed_distance(self.frames[frame_idx].mask.data(), w, h, 1);
            // 极性翻转：不遮为正
            let dist: Vec<f32> = raw.into_iter().map(|v| -v).collect();
            self.dist_cache[frame_idx] = (fp, dist);
        }
        &self.dist_cache[frame_idx].1
    }

    /// 取**全部帧**的距离场，能复用的就复用。
    ///
    /// 这是烘焙路径的加速入口。关键事实：
    ///
    /// > 一个点的距离场只依赖该点自己的遮罩，与点集合无关。
    ///
    /// 所以加一帧时，前面所有帧的 EDT 都没变。原来的 `bake_with_dir`
    /// 每次都全量重算（1K 四帧 119.7ms），加一帧就要白烧 3/4。
    ///
    /// **按内容指纹匹配，不按下标**：插入关键帧会让后续帧下标整体位移，
    /// 按下标存会张冠李戴。这里用「指纹 → 距离场」的查找，位移后仍能命中。
    ///
    /// 返回的顺序与 `self.frames` 一致，供 `bake_from_sdfs` 使用。
    fn sdfs_for_bake(&mut self, w: usize, h: usize) -> Vec<Vec<f32>> {
        let n = self.frames.len();
        let need = (w * h) as usize;

        // 先把当前缓存的指纹建成查找表，避免 N² 扫描
        let mut by_fp: std::collections::HashMap<u64, usize> =
            std::collections::HashMap::with_capacity(self.dist_cache.len());
        for (i, (fp, data)) in self.dist_cache.iter().enumerate() {
            if *fp != 0 && data.len() == need {
                by_fp.insert(*fp, i);
            }
        }

        // 逐帧取：命中就 clone 出来（内存带宽换 EDT 的算力，划算）
        let mut hit: Vec<Option<Vec<f32>>> = Vec::with_capacity(n);
        let mut misses: Vec<usize> = Vec::new();
        for i in 0..n {
            let fp = self.frames[i].mask.fingerprint();
            match by_fp.get(&fp) {
                Some(&ci) => hit.push(Some(self.dist_cache[ci].1.clone())),
                None => {
                    hit.push(None);
                    misses.push(i);
                }
            }
        }

        // 只有没命中的帧才真的算 EDT，而且**并行**算。
        //
        // 注意：不能直接对 `self.frames` 并行——`Mask` 里的指纹缓存是
        // `Cell<Option<u64>>`，不是 `Sync`，跨线程借用编译器会拒绝。
        // 所以先把要算的遮罩切片收集出来（只是借用 &[u8]，本身就 Sync），
        // 再对这份引用并行。算完再回填。
        if !misses.is_empty() {
            let jobs: Vec<(usize, &[u8])> = misses
                .iter()
                .map(|&i| (i, self.frames[i].mask.data()))
                .collect();
            let computed: Vec<(usize, Vec<f32>)> = jobs
                .par_iter()
                .map(|&(i, mask)| {
                    let raw = crate::sdf::signed_distance(mask, w, h, 1);
                    let dist: Vec<f32> = raw.into_iter().map(|v| -v).collect();
                    (i, dist)
                })
                .collect();
            for (i, dist) in computed {
                hit[i] = Some(dist);
            }
        }

        // 收集结果并顺手把缓存整理成与 frames 对齐的形式。
        // 这样下次命中查找表更小、也自然清理了掉队条目。
        let out: Vec<Vec<f32>> = hit.into_iter().map(|v| v.unwrap_or_default()).collect();
        self.dist_cache = self
            .frames
            .iter()
            .zip(out.iter())
            .map(|(f, d)| (f.mask.fingerprint(), d.clone()))
            .collect();
        out
    }

    /// 让某一帧的缓存失效（画完一笔后调用）
    pub fn invalidate_dist(&mut self, frame_idx: usize) {
        if let Some(slot) = self.dist_cache.get_mut(frame_idx) {
            slot.0 = 0;
        }
    }

    /// 把所有缓存作废（增删帧、整体翻转后调用）
    pub fn clear_dist_cache(&mut self) {
        self.dist_cache.clear();
        self.texel_cache = None;
    }

    pub fn insert_frame(&mut self, angle: f32, mask: Mask) {
        let angle = angle.clamp(0.0, 90.0);
        match self
            .frames
            .binary_search_by(|f| f.angle.partial_cmp(&angle).unwrap())
        {
            Ok(i) => self.frames[i] = Keyframe::new(angle, mask),
            Err(i) => self.frames.insert(i, Keyframe::new(angle, mask)),
        }
        // 下标整体位移了，缓存作废
        self.clear_dist_cache();
    }

    pub fn remove_frame(&mut self, angle: f32) -> bool {
        if let Ok(i) = self
            .frames
            .binary_search_by(|f| f.angle.partial_cmp(&angle).unwrap())
        {
            self.frames.remove(i);
            self.clear_dist_cache();
            true
        } else {
            false
        }
    }

    pub fn has_frame_at(&self, angle: f32) -> bool {
        self.frames.iter().any(|f| (f.angle - angle).abs() < 0.01)
    }

    /// 这个吸附步上有没有关键帧。
    ///
    /// 时间轴 UI 统一按步号思考，避免到处做角度换算。
    /// `steps` 必须和文档的离散帧数一致，否则吸附格会和角度对不上。
    ///
    /// **关键帧的角度是连续的**（拖动不吸附），所以"某步有没有帧"是分桶问题：
    /// 角度落在这个步的桶里就算。两个帧挤进同一个桶时取角度最近的那个。
    pub fn has_kf_at_step(&self, step: usize, steps: usize) -> bool {
        self.index_at_step(step, steps).is_some()
    }

    /// 找某个吸附步上的关键帧下标（同桶多帧时取角度最近的那个）
    pub fn index_at_step(&self, step: usize, steps: usize) -> Option<usize> {
        let target = step_angle(step, steps);
        let mut best: Option<(usize, f32)> = None;
        for (i, f) in self.frames.iter().enumerate() {
            if angle_to_step(f.angle, steps) != step {
                continue;
            }
            let d = (f.angle - target).abs();
            if best.is_none() || d < best.unwrap().1 {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    }

    /// 取某个吸附步上的关键帧（引用）
    pub fn frame_for_step(&self, step: usize, steps: usize) -> Option<&Keyframe> {
        self.index_at_step(step, steps).map(|i| &self.frames[i])
    }

    /// 取某个吸附步上的关键帧（可变引用）
    pub fn frame_for_step_mut(&mut self, step: usize, steps: usize) -> Option<&mut Keyframe> {
        let i = self.index_at_step(step, steps)?;
        Some(&mut self.frames[i])
    }

    /// 按步号删掉关键帧。
    pub fn remove_frame_at_step(&mut self, step: usize, steps: usize) -> bool {
        match self.index_at_step(step, steps) {
            Some(i) => {
                self.frames.remove(i);
                self.clear_dist_cache();
                true
            }
            None => false,
        }
    }

    pub fn frame_at(&self, angle: f32) -> Option<&Keyframe> {
        self.frames.iter().find(|f| (f.angle - angle).abs() < 0.01)
    }

    pub fn frame_at_mut(&mut self, angle: f32) -> Option<&mut Keyframe> {
        self.frames
            .iter_mut()
            .find(|f| (f.angle - angle).abs() < 0.01)
    }

    /// 找出夹住 `angle` 的左右两帧下标。
    /// 正好落在某一帧上时返回 `(Some(i), None)`。
    pub fn bracket(&self, angle: f32) -> (Option<usize>, Option<usize>) {
        if self.frames.is_empty() {
            return (None, None);
        }
        if self.has_frame_at(angle) {
            let i = self
                .frames
                .iter()
                .position(|f| (f.angle - angle).abs() < 0.01)
                .unwrap();
            return (Some(i), None);
        }
        // 找第一个大于 angle 的帧
        let right = self.frames.iter().position(|f| f.angle > angle);
        match right {
            Some(r) => {
                if r == 0 {
                    // angle 比所有帧都小
                    (None, Some(0))
                } else {
                    (Some(r - 1), Some(r))
                }
            }
            None => (Some(self.frames.len() - 1), None),
        }
    }
}

/// 图层：通道的容器
#[derive(Clone)]
pub struct Layer {
    pub name: String,
    pub visible: bool,
    pub channel_mode: ChannelMode,
    pub channels: [Channel; 4],
}

impl Layer {
    pub fn new(name: &str, w: u32, h: u32) -> Self {
        Self {
            name: name.to_string(),
            visible: true,
            channel_mode: ChannelMode::default(),
            channels: [
                Channel::new("R", w, h),
                Channel::new("G", w, h),
                Channel::new("B", w, h),
                Channel::new("A", w, h),
            ],
        }
    }
}

/// 文档
#[derive(Clone)]
pub struct Doc {
    pub width: u32,
    pub height: u32,
    pub background: Option<image::RgbaImage>,
    pub background_visible: bool,
    pub layers: Vec<Layer>,
    pub active_layer: usize,
    pub active_channel: usize,
    /// 本工程的离散帧数（时间轴吸附格数）。可在设置里改。
    pub steps: usize,
}

impl Doc {
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_steps(width, height, DEFAULT_STEPS)
    }

    pub fn with_steps(width: u32, height: u32, steps: usize) -> Self {
        Self {
            width,
            height,
            background: None,
            background_visible: true,
            layers: vec![Layer::new("图层 1", width, height)],
            active_layer: 0,
            active_channel: 0,
            steps: clamp_steps(steps),
        }
    }

    pub fn add_layer(&mut self, name: &str) {
        self.layers.push(Layer::new(name, self.width, self.height));
        self.active_layer = self.layers.len() - 1;
        self.active_channel = 0;
    }

    pub fn remove_layer(&mut self, index: usize) {
        if self.layers.len() <= 1 || index >= self.layers.len() {
            return;
        }
        self.layers.remove(index);
        if self.active_layer >= self.layers.len() {
            self.active_layer = self.layers.len() - 1;
        }
    }

    /// 复制指定图层（内容和通道设置一起复制），插在原图层后面并选中新图层
    pub fn duplicate_layer(&mut self, index: usize) -> Option<usize> {
        if index >= self.layers.len() {
            return None;
        }
        let src = &self.layers[index];
        let mut dup = Layer::new(&format!("{} 拷贝", src.name), self.width, self.height);
        dup.visible = src.visible;
        dup.channel_mode = src.channel_mode;
        for ci in 0..4 {
            dup.channels[ci].name = src.channels[ci].name.clone();
            dup.channels[ci].visible = src.channels[ci].visible;
            dup.channels[ci].frames = src.channels[ci]
                .frames
                .iter()
                .map(|f| Keyframe::new(f.angle, f.mask.clone()))
                .collect();
        }
        self.layers.insert(index + 1, dup);
        self.active_layer = index + 1;
        Some(index + 1)
    }

    /// 清空指定图层的所有遮罩内容，但保留图层与关键帧结构
    pub fn clear_layer(&mut self, index: usize) {
        if let Some(l) = self.layers.get_mut(index) {
            for ch in l.channels.iter_mut() {
                for f in ch.frames.iter_mut() {
                    f.mask.clear();
                }
                ch.clear_dist_cache();
            }
        }
    }

    /// 上移 / 下移图层
    pub fn move_layer(&mut self, from: usize, to: usize) {
        if from >= self.layers.len() || to >= self.layers.len() || from == to {
            return;
        }
        let l = self.layers.remove(from);
        self.layers.insert(to, l);
        self.active_layer = to;
    }

    /// 把当前通道当前步的那一帧整体填入某个值（全选填充）
    pub fn fill_active_frame(&mut self, step: usize, steps: usize, value: u8) {
        let hit = {
            let ch = self.active_channel_mut();
            match ch.frame_for_step_mut(step, steps) {
                Some(f) => {
                    f.mask.fill(value);
                    true
                }
                None => false,
            }
        };
        if hit {
            self.active_channel_mut().clear_dist_cache();
        }
    }

    /// 把当前通道当前步的那一帧反相
    pub fn invert_active_frame(&mut self, step: usize, steps: usize) {
        let hit = {
            let ch = self.active_channel_mut();
            match ch.frame_for_step_mut(step, steps) {
                Some(f) => {
                    f.mask.invert();
                    true
                }
                None => false,
            }
        };
        if hit {
            self.active_channel_mut().clear_dist_cache();
        }
    }

    /// 反相整个图层：所有通道的所有帧都取补集。
    ///
    /// 和「反相本帧」的区别是作用范围——这里一次翻整层，
    /// 常用于"画的是受光区，但需要的是阴影区"这种整层反过来的情况。
    pub fn invert_layer(&mut self, index: usize) {
        if let Some(l) = self.layers.get_mut(index) {
            for ch in l.channels.iter_mut() {
                for f in ch.frames.iter_mut() {
                    f.mask.invert();
                }
                ch.clear_dist_cache();
            }
        }
    }

    /// 水平镜像整个图层：所有通道的所有帧左右翻转。
    ///
    /// 对称的脸只画一半就够了，另一半靠这个翻出来。
    pub fn mirror_layer(&mut self, index: usize) {
        if let Some(l) = self.layers.get_mut(index) {
            for ch in l.channels.iter_mut() {
                for f in ch.frames.iter_mut() {
                    f.mask.flip_h();
                }
                ch.clear_dist_cache();
            }
        }
    }

    /// 反转当前通道的时间轴：把关键帧按时间倒序重排。
    ///
    /// 角度映射成 `90 - 原角度`，于是原来在 0° 的帧跑到 90°，顺序整个颠倒。
    /// 好处是**不用重画**就能得到相反的变化方向：原本"阴影逐渐变大"的数据，
    /// 反过来就变成"阴影逐渐变小"，仍然是单调的，照样能导出。
    ///
    /// `frames` 是按角度升序存的，所以镜像后必须重新排序；
    /// 两个帧撞到同一角度时（步数很少时可能发生），保留后处理的那个。
    pub fn reverse_channel_timeline(&mut self) {
        let ch = self.active_channel_mut();
        if ch.frames.is_empty() {
            return;
        }
        for f in ch.frames.iter_mut() {
            f.angle = (90.0 - f.angle).clamp(0.0, 90.0);
        }
        let mut sorted: Vec<Keyframe> = Vec::with_capacity(ch.frames.len());
        for f in ch.frames.drain(..) {
            match sorted.binary_search_by(|e| e.angle.partial_cmp(&f.angle).unwrap()) {
                Ok(i) => sorted[i] = f,
                Err(i) => sorted.insert(i, f),
            }
        }
        ch.frames = sorted;
        ch.clear_dist_cache();
    }

    pub fn active(&self) -> &Layer {
        &self.layers[self.active_layer]
    }

    /// 把活动图层 / 活动通道夹回合法范围，并让绘制目标与导出目标一致。
    ///
    /// 两种会脱节的情形：
    ///
    /// 1. 删除图层或换文档后下标越界——直接索引会 panic。
    /// 2. 灰度图层只导出 R 子层（`io::export_layer_opts` 读 `channels[0]`），
    ///    若活动通道停在 G/B/A，画布上画的是那个子层，导出拿的却是 R，
    ///    表现是"画了但导出没有"。
    ///
    /// 所有可能改动图层集合或通道模式的地方都要调用本方法。
    pub fn normalize_active(&mut self) {
        if self.layers.is_empty() {
            self.layers
                .push(Layer::new("图层 1", self.width, self.height));
        }
        self.active_layer = self.active_layer.min(self.layers.len() - 1);
        self.active_channel = self.active_channel.min(CHANNELS.len() - 1);
        if self.layers[self.active_layer].channel_mode == ChannelMode::Gray {
            self.active_channel = 0;
        }
    }

    pub fn active_mut(&mut self) -> &mut Layer {
        &mut self.layers[self.active_layer]
    }

    pub fn active_channel_ref(&self) -> &Channel {
        &self.layers[self.active_layer].channels[self.active_channel]
    }

    pub fn active_channel_mut(&mut self) -> &mut Channel {
        &mut self.layers[self.active_layer].channels[self.active_channel]
    }

    /// 一张空白遮罩（全 0 = 全不遮）
    pub fn blank_mask(&self) -> Mask {
        Mask::new(self.width, self.height)
    }

    /// 新建一个默认两帧的通道：0° 和 90° 各一帧，都是空白。
    ///
    /// 两帧是最小可工作集合——少于两帧既看不到插值也导不出东西，
    /// 所以一上来就给两端各放一张。
    pub fn seed_default_frames(&mut self) {
        let (w, h) = (self.width, self.height);
        let steps = self.steps;
        let a0 = step_angle(0, steps);
        let a1 = step_angle(steps - 1, steps);
        let ch = self.active_channel_mut();
        if ch.frames.is_empty() {
            ch.insert_frame(a0, Mask::new(w, h));
            ch.insert_frame(a1, Mask::new(w, h));
        }
    }

    /// 在指定步号上确保有一帧
    pub fn ensure_frame(&mut self, step: usize, steps: usize) {
        if self.active_channel_ref().has_kf_at_step(step, steps) {
            return;
        }
        let m = self.blank_mask();
        let a = step_angle(step, steps);
        self.active_channel_mut().insert_frame(a, m);
    }

    /// 按当前关键帧的插值，造一张"这个角度上应该是怎样"的遮罩。
    ///
    /// 新增的参数点不从空白开始，而是**从自动插值的结果开始画**：
    /// 在原有两帧之间插一帧时，那一帧本来就有个"当前显示出来的样子"，
    /// 从那个样子出发修改比从零画起省事得多（画一笔修一块，
    /// 而不必把整片遮罩重画一遍）。
    ///
    /// 实现：找夹住 `angle` 的左右两帧，按角度比例做距离场插值——
    /// 和预览用的是同一套几何（`sdf::interpolate_masks`）。
    ///
    /// 三种退化情况：
    /// - 通道为空 → 空白
    /// - 只有一帧 → 直接用那一帧（没有可插的）
    /// - 落在所有帧之外 → 用最近那一帧
    pub fn interpolated_mask_at(&self, angle: f32) -> Mask {
        let ch = self.active_channel_ref();
        let (w, h) = (self.width, self.height);
        if ch.frames.is_empty() {
            return Mask::new(w, h);
        }
        if ch.frames.len() == 1 {
            return ch.frames[0].mask.clone();
        }

        let (li, ri) = ch.bracket(angle);
        match (li, ri) {
            // 正好落在某个参数点上：就是该点本身
            (Some(l), None) => ch.frames[l].mask.clone(),
            // 夹在两帧之间：按角度做插值
            (Some(l), Some(r)) => {
                let a0 = ch.frames[l].angle;
                let a1 = ch.frames[r].angle;
                let span = a1 - a0;
                let t = if span.abs() < 1e-6 {
                    0.0
                } else {
                    ((angle - a0) / span).clamp(0.0, 1.0)
                };
                let mut out = Mask::new(w, h);
                crate::sdf::interpolate_masks(&ch.frames[l].mask, &ch.frames[r].mask, t, &mut out);
                out
            }
            // 比所有帧都小：用第一帧
            (None, Some(r)) => ch.frames[r].mask.clone(),
            // 比所有帧都大：用最后一帧
            (None, None) => ch
                .frames
                .last()
                .map(|f| f.mask.clone())
                .unwrap_or_else(|| Mask::new(w, h)),
        }
    }

    /// 在指定**连续角度**加关键帧，遮罩内容二选一：
    /// - `interp = true`：用该角度处的插值结果做起点
    /// - `interp = false`：从空白开始
    ///
    /// 返回 `false` 表示那个角度已经有关键帧了。
    pub fn add_frame_at_angle(&mut self, angle: f32, interp: bool) -> bool {
        if self.angle_taken(angle, None) {
            return false;
        }
        let m = if interp {
            self.interpolated_mask_at(angle)
        } else {
            self.blank_mask()
        };
        self.active_channel_mut().insert_frame(angle, m);
        true
    }

    /// 把所有关键帧吸附到最近的步角。
    ///
    /// 关键帧本来就该落在离散步上：播放头只能停在这些位置，
    /// 落在步与步之间的帧永远"不是当前帧"，既画不了也删不掉。
    /// 打开旧工程或手工改过角度后调一次，把这个不变式修回来。
    ///
    /// # 撞格时的取舍
    ///
    /// 两个点可能吸到同一格。取消其中一个是**丢内容**：那一帧画过的遮罩
    /// 会连同对象一起消失，且没有任何提示。所以这里不覆盖，改为按顺序
    /// 占用该格之后最近的空格——内容全部保留，代价是位置被挤开一格。
    ///
    /// 只有该格往后已无处可放（整条轴都被占满）时才会真正丢弃，
    /// 那种情况下丢的是"比刻度数还多的参数点"，属于输入本身无法表达的配置。
    ///
    /// **只在文档结构真的变了的时候调**（读盘、改步数）。
    pub fn snap_frames(&mut self) {
        let steps = self.steps;
        for l in self.layers.iter_mut() {
            for ch in l.channels.iter_mut() {
                if ch.frames.is_empty() {
                    continue;
                }
                // 先按目标格排序，保证"先画的点占原格"这个顺序稳定
                let mut moved: Vec<Keyframe> = ch.frames.drain(..).collect();
                moved.sort_by(|a, b| {
                    angle_to_step(a.angle, steps)
                        .cmp(&angle_to_step(b.angle, steps))
                        .then_with(|| {
                            a.angle
                                .partial_cmp(&b.angle)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                });

                let mut used = vec![false; steps];
                let mut snapped: Vec<Keyframe> = Vec::with_capacity(moved.len());
                for f in moved {
                    let want = angle_to_step(f.angle, steps);
                    // 从目标格向后找最近的空格，找不到再向前找
                    let slot = (want..steps)
                        .find(|&s| !used[s])
                        .or_else(|| (0..want).rev().find(|&s| !used[s]));
                    let Some(slot) = slot else {
                        // 轴已满：保留内容，挤在末格上（position 相同则退化为覆盖）
                        snapped.push(Keyframe::new(step_angle(steps - 1, steps), f.mask));
                        continue;
                    };
                    used[slot] = true;
                    snapped.push(Keyframe::new(step_angle(slot, steps), f.mask));
                }
                snapped.sort_by(|a, b| {
                    a.angle
                        .partial_cmp(&b.angle)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                ch.frames = snapped;
                ch.clear_dist_cache();
            }
        }
    }

    // ------------------------------------------------ 关键帧导航与复制

    /// 当前步之前最近的关键帧角度
    pub fn prev_keyframe(&self, step: usize, steps: usize) -> Option<f32> {
        let target = step_angle(step, steps);
        self.active_channel_ref()
            .frames
            .iter()
            .filter(|f| f.angle < target - 0.01)
            .next_back()
            .map(|f| f.angle)
    }

    /// 当前步之后最近的关键帧角度
    pub fn next_keyframe(&self, step: usize, steps: usize) -> Option<f32> {
        let target = step_angle(step, steps);
        self.active_channel_ref()
            .frames
            .iter()
            .filter(|f| f.angle > target + 0.01)
            .next()
            .map(|f| f.angle)
    }

    /// 复制当前步的关键帧到相邻的空闲步，返回新的角度。
    ///
    /// 和 Blender 的 Shift+D 一致：复制品必须落在**另一格**上，
    /// 否则复制完还需手动拖走。优先往后找，末尾没空位就往前找。
    pub fn duplicate_keyframe(&mut self, step: usize, steps: usize) -> Option<f32> {
        let src = self
            .active_channel_ref()
            .frame_for_step(step, steps)?
            .mask
            .clone();

        let mut target: Option<f32> = None;
        for s in (step + 1)..steps {
            let a = step_angle(s, steps);
            if !self.active_channel_ref().has_kf_at_step(s, steps) {
                target = Some(a);
                break;
            }
        }
        if target.is_none() {
            for s in (0..step).rev() {
                let a = step_angle(s, steps);
                if !self.active_channel_ref().has_kf_at_step(s, steps) {
                    target = Some(a);
                    break;
                }
            }
        }
        let a = target?;
        self.active_channel_mut().insert_frame(a, src);
        Some(a)
    }

    // ------------------------------------------------ 通道搬运与画布尺寸

    /// 交换两个通道的内容（帧数据），通道自己的名字/可见性留在原位。
    ///
    /// 图层面板里把 R 子层拖到别的图层就是调这个——用「交换」而不是「移动」，
    /// 是因为交换天然可逆，目标位置原有的内容不会被静默丢掉。
    pub fn swap_channel(&mut self, a: (usize, usize), b: (usize, usize)) -> bool {
        if a == b || a.0 >= self.layers.len() || b.0 >= self.layers.len() {
            return false;
        }
        let frames_a = std::mem::take(&mut self.layers[a.0].channels[a.1].frames);
        let frames_b = std::mem::replace(&mut self.layers[b.0].channels[b.1].frames, frames_a);
        self.layers[a.0].channels[a.1].frames = frames_b;
        self.layers[a.0].channels[a.1].clear_dist_cache();
        self.layers[b.0].channels[b.1].clear_dist_cache();
        true
    }

    /// 改离散帧数。所有关键帧会被重新吸附到新步数的格子上。
    ///
    /// 增加步数时旧帧全部保留（吸附到更细的格子，位置会动一点点）；
    /// 减少步数时可能有两个帧吸到同一格，后面的覆盖前面的。
    /// 无论哪种，绘制过的遮罩内容都还在，只是位置被量化了。
    pub fn set_steps(&mut self, steps: usize) {
        let steps = clamp_steps(steps);
        if steps == self.steps {
            return;
        }
        self.steps = steps;
        self.snap_frames();
    }

    /// 把当前通道的第 `idx` 帧挪到 `new_angle`（**连续角度，不吸附**）。
    ///
    /// 拖动松手后不吸附到刻度格上，只保持视觉平滑。
    /// 挪完要重新排序：新角度可能插到别处，下标会变。
    /// 返回 `false` 表示目标角度已被另一帧占用（拒绝，避免静默覆盖）。
    pub fn move_frame_to_angle(&mut self, idx: usize, new_angle: f32) -> bool {
        let ch = self.active_channel_mut();
        if idx >= ch.frames.len() {
            return false;
        }
        let a = new_angle.clamp(0.0, 90.0);
        // 目标位置已被别的帧占用 → 拒绝。
        // 0.02° 是 64 帧下一格的约 1/70，足够精确又不会误判相邻帧。
        if ch
            .frames
            .iter()
            .enumerate()
            .any(|(i, f)| i != idx && (f.angle - a).abs() < 0.02)
        {
            return false;
        }
        let f = ch.frames.remove(idx);
        let nf = Keyframe::new(a, f.mask);
        match ch
            .frames
            .binary_search_by(|e| e.angle.partial_cmp(&a).unwrap())
        {
            Ok(i) => ch.frames[i] = nf,
            Err(i) => ch.frames.insert(i, nf),
        }
        ch.clear_dist_cache();
        true
    }

    /// 当前通道的目标角度上是否已有别的帧（拖动时判断"放不放得下"）
    pub fn angle_taken(&self, angle: f32, except_idx: Option<usize>) -> bool {
        let a = angle.clamp(0.0, 90.0);
        self.active_channel_ref()
            .frames
            .iter()
            .enumerate()
            .any(|(i, f)| Some(i) != except_idx && (f.angle - a).abs() < 0.02)
    }

    /// 改画布尺寸。遮罩按最近邻缩放，保持二值不被抹成灰；底图跟着重采样。
    pub fn resize(&mut self, w: u32, h: u32) {
        let w = w.clamp(16, 8192);
        let h = h.clamp(16, 8192);
        if w == self.width && h == self.height {
            return;
        }
        for l in self.layers.iter_mut() {
            for ch in l.channels.iter_mut() {
                for f in ch.frames.iter_mut() {
                    f.mask = resample_mask(&f.mask, w, h);
                }
                ch.clear_dist_cache();
            }
        }
        if let Some(bg) = &self.background {
            self.background = Some(image::imageops::resize(
                bg,
                w,
                h,
                image::imageops::FilterType::Lanczos3,
            ));
        }
        self.width = w;
        self.height = h;
    }
}

/// 最近邻重采样。遮罩只有 0/1，插值会凭空造出中间值，所以不放缩滤波器进去。
fn resample_mask(m: &Mask, w: u32, h: u32) -> Mask {
    let mut out = Mask::new(w, h);
    if m.width() == 0 || m.height() == 0 {
        return out;
    }
    for y in 0..h {
        let sy = ((y as u64 * m.height() as u64) / h as u64) as u32;
        let sy = sy.min(m.height() - 1);
        for x in 0..w {
            let sx = ((x as u64 * m.width() as u64) / w as u64) as u32;
            let sx = sx.min(m.width() - 1);
            out.set(x, y, m.get(sx, sy));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 新文档只有一个图层四个通道() {
        let d = Doc::new(64, 64);
        assert_eq!(d.layers.len(), 1);
        assert_eq!(d.layers[0].channels.len(), 4);
        assert_eq!(d.layers[0].channels[0].name, "R");
    }

    /// 锁住缓存的取舍契约：`dist_cache` 不进快照，`texel_cache` 要进。
    ///
    /// 两个方向都有人会"顺手优化"错：
    /// - 把 `dist_cache` 加回 `Clone` → 每次加帧/落笔多付 2.8ms 内存带宽
    /// - 把 `texel_cache` 从 `Clone` 去掉 → 每次快照后预览重烘 137ms，更糟
    #[test]
    fn 克隆只丢距离场缓存_保留texel缓存() {
        let mut d = Doc::new(16, 16);
        {
            let ch = &mut d.layers[0].channels[0];
            let mut m = Mask::new(16, 16);
            m.fill(0);
            m.set(8, 8, 1);
            ch.insert_frame(0.0, m.clone());
            let mut m2 = Mask::new(16, 16);
            m2.fill(1);
            ch.insert_frame(90.0, m2);
        }
        // 把两个缓存都填上
        let texels = d.layers[0].channels[0].texels(16, 16);
        assert!(texels.is_ok(), "合法数据应能烘焙");
        d.layers[0].channels[0].dist_of(0, 16, 16);
        assert!(
            !d.layers[0].channels[0].dist_cache.is_empty(),
            "前提：dist_cache 已填充"
        );
        assert!(
            d.layers[0].channels[0].texel_cache.is_some(),
            "前提：texel_cache 已填充"
        );

        let c = d.layers[0].channels[0].clone();
        assert!(
            c.dist_cache.is_empty(),
            "dist_cache 是死缓存（烘焙从不读它），不该进快照"
        );
        assert!(
            c.texel_cache.is_some(),
            "texel_cache 是预览命脉，丢了会让每次快照后重烘 137ms"
        );
        assert_eq!(c.frames.len(), 2, "帧数据必须完整复制");
    }

    /// `cached_texels()` 返回的是**可能过期**的图，`cache_matches()` 是该方法的
    /// 配套校验。两者必须分开，因为用途不同：
    ///
    /// - 渲染"这一帧"可以容忍过期（宁可画面落后几十毫秒，也不阻塞 UI）
    /// - 但**长期持有**这份图时必须校验，否则旧内容会被贴上新的指纹
    ///
    /// 这个 bug 真实发生过：新建工程后在 90° 画一笔，"从空帧长出来"
    /// 这个方向完全不工作、播放毫无变化。根因就是把 `cached_texels()` 的
    /// 旧图存进快照并贴上了当前指纹，从此再也不重烘。
    #[test]
    fn 旧缓存不能冒充当前内容() {
        let mut d = Doc::new(16, 16);
        {
            let ch = &mut d.layers[0].channels[0];
            ch.insert_frame(0.0, Mask::new(16, 16));
            ch.insert_frame(90.0, Mask::new(16, 16));
        }
        let fp_before = d.layers[0].channels[0].channel_fingerprint();
        assert!(d.layers[0].channels[0].texels(16, 16).is_ok(), "先烘一次");
        assert!(
            d.layers[0].channels[0].cache_matches(fp_before),
            "刚烘完，缓存应属于当前指纹"
        );

        // 改第 2 帧（模拟"在 90° 画一笔"）
        let mut m = Mask::new(16, 16);
        m.set(8, 8, 1);
        d.layers[0].channels[0].frames[1].mask = m;
        let fp_after = d.layers[0].channels[0].channel_fingerprint();
        assert_ne!(fp_before, fp_after, "改内容后指纹必须变");

        // 关键断言：此时旧缓存**不属于**当前指纹
        assert!(
            !d.layers[0].channels[0].cache_matches(fp_after),
            "旧缓存被误判为当前内容——画面会永远停在画之前"
        );
        // 但 cached_texels 仍然返回该旧图（供渲染临时顶上，不阻塞 UI）
        assert!(
            d.layers[0].channels[0].cached_texels().is_some(),
            "cached_texels 应仍可返回旧图供顶替"
        );
    }

    /// 重新烘焙后，缓存必须重新与当前指纹匹配。
    #[test]
    fn 重烘后缓存重新匹配() {
        let mut d = Doc::new(16, 16);
        {
            let ch = &mut d.layers[0].channels[0];
            ch.insert_frame(0.0, Mask::new(16, 16));
            ch.insert_frame(90.0, Mask::new(16, 16));
        }
        let _ = d.layers[0].channels[0].texels(16, 16);

        let mut m = Mask::new(16, 16);
        m.set(8, 8, 1);
        d.layers[0].channels[0].frames[1].mask = m;
        let fp = d.layers[0].channels[0].channel_fingerprint();
        assert!(!d.layers[0].channels[0].cache_matches(fp), "改完应不匹配");

        assert!(d.layers[0].channels[0].texels(16, 16).is_ok());
        assert!(
            d.layers[0].channels[0].cache_matches(fp),
            "重烘后应重新匹配"
        );
    }

    /// 克隆出来的文档，texel 缓存必须仍然可用（指纹能对上，不重算）。
    #[test]
    fn 克隆后烘焙缓存仍命中() {
        let mut d = Doc::new(16, 16);
        {
            let ch = &mut d.layers[0].channels[0];
            ch.insert_frame(0.0, Mask::new(16, 16));
            let mut m = Mask::new(16, 16);
            m.fill(1);
            ch.insert_frame(90.0, m);
        }
        let a = d.layers[0].channels[0].texels(16, 16);
        assert!(a.is_ok());
        let mut c = d.clone();
        let b = c.layers[0].channels[0].texels(16, 16);
        assert!(b.is_ok());
        assert_eq!(a.unwrap().0, b.unwrap().0, "克隆后烘焙结果应一致");
    }

    #[test]
    fn 关键帧自动按角度排序() {
        let mut d = Doc::new(8, 8);
        let ch = &mut d.layers[0].channels[0];
        ch.insert_frame(45.0, Mask::new(8, 8));
        ch.insert_frame(0.0, Mask::new(8, 8));
        ch.insert_frame(90.0, Mask::new(8, 8));
        let a: Vec<f32> = ch.frames.iter().map(|f| f.angle).collect();
        assert_eq!(a, vec![0.0, 45.0, 90.0]);
    }

    #[test]
    fn 同角度覆盖() {
        let mut d = Doc::new(8, 8);
        let ch = &mut d.layers[0].channels[0];
        ch.insert_frame(30.0, Mask::new(8, 8));
        let mut m = Mask::new(8, 8);
        m.set(0, 0, 1);
        ch.insert_frame(30.0, m);
        assert_eq!(ch.frames.len(), 1);
        assert_eq!(ch.frames[0].mask.get(0, 0), 1);
    }

    #[test]
    fn 空白遮罩全为零() {
        let d = Doc::new(16, 16);
        let m = d.blank_mask();
        assert!(m.data().iter().all(|&v| v == 0));
    }

    #[test]
    fn 区间查找_落在关键帧上() {
        let mut d = Doc::new(4, 4);
        let ch = &mut d.layers[0].channels[0];
        ch.insert_frame(0.0, Mask::new(4, 4));
        ch.insert_frame(45.0, Mask::new(4, 4));
        ch.insert_frame(90.0, Mask::new(4, 4));
        assert_eq!(ch.bracket(45.0), (Some(1), None));
    }

    #[test]
    fn 区间查找_两帧之间() {
        let mut d = Doc::new(4, 4);
        let ch = &mut d.layers[0].channels[0];
        ch.insert_frame(0.0, Mask::new(4, 4));
        ch.insert_frame(45.0, Mask::new(4, 4));
        ch.insert_frame(90.0, Mask::new(4, 4));
        assert_eq!(ch.bracket(20.0), (Some(0), Some(1)));
        assert_eq!(ch.bracket(60.0), (Some(1), Some(2)));
    }

    #[test]
    fn 区间查找_超出两端() {
        let mut d = Doc::new(4, 4);
        let ch = &mut d.layers[0].channels[0];
        ch.insert_frame(30.0, Mask::new(4, 4));
        ch.insert_frame(60.0, Mask::new(4, 4));
        // 比最小的小：没有左帧
        assert_eq!(ch.bracket(10.0), (None, Some(0)));
        // 比最大的大：没有右帧
        assert_eq!(ch.bracket(80.0), (Some(1), None));
    }

    #[test]
    fn 区间查找_空通道() {
        let d = Doc::new(4, 4);
        assert_eq!(d.layers[0].channels[0].bracket(45.0), (None, None));
    }

    #[test]
    fn 不能删最后一个图层() {
        let mut d = Doc::new(8, 8);
        d.remove_layer(0);
        assert_eq!(d.layers.len(), 1);
    }

    // ------------------------------------------------ 本轮新增：整层操作

    /// 造一个"左半遮"的遮罩
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
    fn 反相图层所有帧都翻过来() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 4));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(8, 8, 6));
        d.invert_layer(0);

        let f0 = d.active_channel_ref().frame_at(0.0).unwrap();
        let f90 = d.active_channel_ref().frame_at(90.0).unwrap();
        assert_eq!(f0.mask.get(0, 0), 0, "原来被遮的应变成受光");
        assert_eq!(f0.mask.get(7, 0), 1, "原来受光的应变成被遮");
        assert_eq!(f0.mask.count(), 64 - 4 * 8);
        assert_eq!(f90.mask.count(), 64 - 6 * 8);
    }

    #[test]
    fn 反相两次回到原样() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 3));
        let before = d.active_channel_ref().frames[0].mask.count();
        d.invert_layer(0);
        d.invert_layer(0);
        assert_eq!(d.active_channel_ref().frames[0].mask.count(), before);
    }

    #[test]
    fn 镜像图层左右互换() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 4));
        d.mirror_layer(0);
        let f = d.active_channel_ref().frame_at(0.0).unwrap();
        assert_eq!(f.mask.get(0, 0), 0, "左边缘应变空");
        assert_eq!(f.mask.get(7, 0), 1, "右边缘应变实");
        assert_eq!(f.mask.count(), 4 * 8, "总像素数不变");
    }

    #[test]
    fn 镜像两次回到原样() {
        let mut d = Doc::new(9, 9); // 奇数宽度最容易暴露对称轴错误
        d.active_channel_mut().insert_frame(0.0, left_half(9, 9, 4));
        let orig = d.active_channel_ref().frames[0].mask.data().to_vec();
        d.mirror_layer(0);
        d.mirror_layer(0);
        assert_eq!(d.active_channel_ref().frames[0].mask.data(), &orig[..]);
    }

    #[test]
    fn 镜像后所有帧都翻() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 2));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(8, 8, 5));
        d.mirror_layer(0);
        // 第 90° 帧原来占 05 列，镜像后应占 3..8 列
        let f = d.active_channel_ref().frame_at(90.0).unwrap();
        assert_eq!(f.mask.get(0, 0), 0);
        assert_eq!(f.mask.get(7, 0), 1);
        assert_eq!(f.mask.count(), 5 * 8);
    }

    // ------------------------------------------------ 本轮新增：反转时间轴

    #[test]
    fn 反转时间轴后角度镜像() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 2));
        d.active_channel_mut()
            .insert_frame(30.0, left_half(8, 8, 4));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(8, 8, 6));

        d.reverse_channel_timeline();

        let angles: Vec<f32> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| f.angle)
            .collect();
        assert_eq!(angles, vec![0.0, 60.0, 90.0], "角度应镜像并重新升序");

        // 原来 0° 的窄遮罩现在跑到 90°，原来的宽遮罩跑到 0°
        let f0 = d.active_channel_ref().frame_at(0.0).unwrap();
        assert_eq!(f0.mask.count(), 6 * 8, "0° 现在是最宽的");
        let f90 = d.active_channel_ref().frame_at(90.0).unwrap();
        assert_eq!(f90.mask.count(), 2 * 8, "90° 现在是最窄的");
    }

    #[test]
    fn 反转两次回到原样() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 2));
        d.active_channel_mut()
            .insert_frame(45.0, left_half(8, 8, 5));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(8, 8, 7));
        let before: Vec<(f32, usize)> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| (f.angle, f.mask.count()))
            .collect();

        d.reverse_channel_timeline();
        d.reverse_channel_timeline();

        let after: Vec<(f32, usize)> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| (f.angle, f.mask.count()))
            .collect();
        assert_eq!(before, after, "反转两次应还原");
    }

    #[test]
    fn 反转让扩张数据变成收缩() {
        use crate::model::nesting::{NestDir, detect};
        let mut d = Doc::new(16, 8);
        d.active_channel_mut()
            .insert_frame(0.0, left_half(16, 8, 3));
        d.active_channel_mut()
            .insert_frame(45.0, left_half(16, 8, 7));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(16, 8, 12));

        let refs: Vec<&Mask> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| &f.mask)
            .collect();
        let grows = detect(&refs, NestDir::Grows, 16, 8);
        assert!(grows.is_clean(), "原始数据是合法扩张");

        d.reverse_channel_timeline();
        let refs2: Vec<&Mask> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| &f.mask)
            .collect();
        let shrinks = detect(&refs2, NestDir::Shrinks, 16, 8);
        assert!(shrinks.is_clean(), "反转后应变成合法收缩");
    }

    // ------------------------------------------------ 本轮新增：可配步数

    #[test]
    fn 默认步数是64() {
        let d = Doc::new(8, 8);
        assert_eq!(d.steps, DEFAULT_STEPS);
        assert_eq!(DEFAULT_STEPS, 64);
    }

    #[test]
    fn 步数换算覆盖两端() {
        for steps in [2usize, 32, 64, 128] {
            assert_eq!(step_norm(0, steps), 0.0);
            assert!((step_norm(steps - 1, steps) - 1.0).abs() < 1e-6);
            assert_eq!(norm_to_step(0.0, steps), 0);
            assert_eq!(norm_to_step(1.0, steps), steps - 1);
            assert!((step_angle(steps - 1, steps) - 90.0).abs() < 1e-4);
        }
    }

    #[test]
    fn 改步数后所有帧仍落在步角上() {
        let mut d = Doc::with_steps(8, 8, 32);
        d.active_channel_mut().insert_frame(0.0, Mask::new(8, 8));
        d.active_channel_mut()
            .insert_frame(step_angle(16, 32), Mask::new(8, 8));
        d.active_channel_mut().insert_frame(90.0, Mask::new(8, 8));

        d.set_steps(64);
        assert_eq!(d.steps, 64);
        for f in d.active_channel_ref().frames.iter() {
            let s = angle_to_step(f.angle, 64);
            assert!(
                (f.angle - step_angle(s, 64)).abs() < 1e-3,
                "{} 应落在 64 步的格子上",
                f.angle
            );
        }
    }

    #[test]
    fn 减少步数时撞格会合并而不是留重复() {
        let mut d = Doc::with_steps(8, 8, 128);
        // 128 步里第 60 和 61 步在 64 步下会撞到一起
        d.active_channel_mut()
            .insert_frame(step_angle(60, 128), Mask::new(8, 8));
        d.active_channel_mut()
            .insert_frame(step_angle(61, 128), Mask::new(8, 8));
        d.set_steps(64);
        // 撞格的帧应合并
        let mut angles: Vec<f32> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| f.angle)
            .collect();
        angles.sort_by(|a, b| a.partial_cmp(b).unwrap());
        angles.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
        assert_eq!(
            angles.len(),
            d.active_channel_ref().frames.len(),
            "不该留下同角度的重复帧"
        );
    }

    #[test]
    fn 步数被夹在合法范围() {
        assert_eq!(clamp_steps(0), MIN_STEPS, "0 步无法插值");
        assert_eq!(clamp_steps(1), MIN_STEPS);
        assert_eq!(clamp_steps(99999), MAX_STEPS);
        let mut d = Doc::new(8, 8);
        d.set_steps(1);
        assert_eq!(d.steps, MIN_STEPS);
    }

    // ------------------------------------------------ 本轮新增：新帧从插值开始

    #[test]
    // ------------------------------------------------ 空帧插值（实测缺陷）

    /// **回归测试：从空白帧插值时，遮罩必须真的长出来。**
    ///
    /// 已修复的缺陷：加参数点后插值不生效。
    /// 根因：空帧的距离场是一个常数哨兵值（7.6e8），
    /// 线性插值时哨兵值压倒真实距离，导致几乎整段时间都是空的。
    ///
    /// 决定性证据：实测 64×64 图、空帧 + 有内容帧、t=0.44 处仍是 0 像素。
    ///
    /// 修法：空帧不参与距离场插值，改为"按 t 收缩有内容帧"。
    #[test]
    fn 从空白帧插值时遮罩逐渐长出() {
        let (w, h) = (64u32, 64u32);
        let mut d = Doc::new(w, h);
        // 0° 空白，90° 中心一块实心方块
        let empty = Mask::new(w, h);
        let mut solid = Mask::new(w, h);
        for y in 20..44 {
            for x in 20..44 {
                solid.set(x, y, 1);
            }
        }
        let full = solid.count();
        d.active_channel_mut().insert_frame(0.0, empty);
        d.active_channel_mut().insert_frame(90.0, solid);

        // 逐段检查：越靠近 90° 遮得越多
        let mut prev = 0usize;
        for (angle, label) in [
            (10.0f32, "10°"),
            (30.0, "30°"),
            (45.0, "45°"),
            (60.0, "60°"),
            (80.0, "80°"),
            (89.0, "89°"),
        ] {
            let c = d.interpolated_mask_at(angle).count();
            assert!(
                c >= prev,
                "{label} 遮 {c} 像素，比更早的角度（{prev}）还少——单调性被破坏"
            );
            assert!(c <= full, "{label} 遮 {c} 像素，超过了完整帧的 {full}");
            prev = c;
        }

        // 关键断言：中途必须有实质内容，不能一直空着
        let mid = d.interpolated_mask_at(45.0).count();
        assert!(
            mid > full / 10,
            "45° 只遮 {mid} 像素（完整帧 {full}）——空帧插值仍然失效，这正是空帧插值失效的表现"
        );
        // 接近 90° 时应基本完整
        let late = d.interpolated_mask_at(88.0).count();
        assert!(late > full / 2, "88° 只遮 {late} / {full}——应该接近完整");
    }

    /// 反向：从有内容帧过渡到空白帧，应逐渐消失。
    #[test]
    fn 过渡到空白帧时遮罩逐渐消失() {
        let (w, h) = (64u32, 64u32);
        let mut d = Doc::new(w, h);
        let mut solid = Mask::new(w, h);
        for y in 20..44 {
            for x in 20..44 {
                solid.set(x, y, 1);
            }
        }
        d.active_channel_mut().insert_frame(0.0, solid);
        d.active_channel_mut().insert_frame(90.0, Mask::new(w, h));

        let mut prev = d.interpolated_mask_at(5.0).count();
        for angle in [20.0f32, 40.0, 60.0, 85.0] {
            let c = d.interpolated_mask_at(angle).count();
            assert!(
                c <= prev,
                "{angle}° 遮 {c}，比更早的 {prev} 还多——应逐渐消失"
            );
            prev = c;
        }
        assert!(prev < 100, "85° 应该几乎空，实际遮 {prev} 像素");
    }

    /// 两帧都空白时插值仍是空白（不能凭空造出内容）。
    #[test]
    fn 两帧都空白时插值仍为空() {
        let mut d = Doc::new(32, 32);
        d.active_channel_mut().insert_frame(0.0, Mask::new(32, 32));
        d.active_channel_mut().insert_frame(90.0, Mask::new(32, 32));
        for a in [10.0f32, 45.0, 80.0] {
            assert_eq!(
                d.interpolated_mask_at(a).count(),
                0,
                "{a}° 两帧都空，插值不该有内容"
            );
        }
    }

    fn 新帧的插值起点介于两端之间() {
        let mut d = Doc::new(32, 32);
        d.active_channel_mut()
            .insert_frame(0.0, left_half(32, 32, 4));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(32, 32, 28));

        let mid = d.interpolated_mask_at(45.0);
        let c = mid.count();
        assert!(
            c > 4 * 32 && c < 28 * 32,
            "45° 的插值应介于 4 列和 28 列之间，实际 {c}"
        );
    }

    #[test]
    fn 落在关键帧上时插值就是那一帧() {
        let mut d = Doc::new(16, 16);
        d.active_channel_mut()
            .insert_frame(0.0, left_half(16, 16, 3));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(16, 16, 9));

        let at0 = d.interpolated_mask_at(0.0);
        assert_eq!(at0.count(), 3 * 16, "0° 应正好是那一帧本身");
        let at90 = d.interpolated_mask_at(90.0);
        assert_eq!(at90.count(), 9 * 16);
    }

    #[test]
    fn 空通道插值是空白不崩溃() {
        let d = Doc::new(8, 8);
        assert_eq!(d.interpolated_mask_at(45.0).count(), 0);
    }

    #[test]
    fn 单帧通道插值就用那一帧() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut()
            .insert_frame(30.0, left_half(8, 8, 5));
        assert_eq!(d.interpolated_mask_at(10.0).count(), 5 * 8);
        assert_eq!(d.interpolated_mask_at(80.0).count(), 5 * 8);
    }

    #[test]
    fn 加新帧默认用插值起点() {
        let mut d = Doc::new(32, 32);
        d.active_channel_mut()
            .insert_frame(0.0, left_half(32, 32, 4));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(32, 32, 28));

        assert!(d.add_frame_at_angle(45.0, true), "应能加入");
        let f = d.active_channel_ref().frame_at(45.0).unwrap();
        let c = f.mask.count();
        assert!(
            c > 4 * 32 && c < 28 * 32,
            "新帧应从插值开始，不该是空白，实际 {c}"
        );
    }

    #[test]
    fn 关掉插值时新帧从空白开始() {
        let mut d = Doc::new(32, 32);
        d.active_channel_mut()
            .insert_frame(0.0, left_half(32, 32, 4));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(32, 32, 28));

        assert!(d.add_frame_at_angle(45.0, false));
        let f = d.active_channel_ref().frame_at(45.0).unwrap();
        assert_eq!(f.mask.count(), 0, "关掉插值就该是空白");
    }

    #[test]
    fn 已经有帧的位置加不进去() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut()
            .insert_frame(45.0, left_half(8, 8, 2));
        assert!(!d.add_frame_at_angle(45.0, true), "同角度应拒绝");
        assert_eq!(d.active_channel_ref().frames.len(), 1);
    }

    #[test]
    fn 新帧角度是精确的连续值() {
        let mut d = Doc::new(8, 8);
        d.active_channel_mut().insert_frame(0.0, left_half(8, 8, 2));
        d.active_channel_mut()
            .insert_frame(90.0, left_half(8, 8, 6));

        // 新点精确落在当前位置（仅视觉），不对齐刻度
        let want = 33.7_f32;
        assert!(d.add_frame_at_angle(want, true));
        let angles: Vec<f32> = d
            .active_channel_ref()
            .frames
            .iter()
            .map(|f| f.angle)
            .collect();
        assert!(
            angles.iter().any(|a| (a - want).abs() < 1e-4),
            "新帧角度应精确为 {want}°，实际 {angles:?}"
        );
    }

    // ------------------------------------------------ 违规数据的缓存

    /// 违规数据烘焙必然失败。**失败也必须缓存**，
    /// 否则预览每帧都重试一次烘焙（1K 下 3.6ms），帧率直接崩。
    #[test]
    fn 烘焙失败的结果也被缓存() {
        let mut d = Doc::new(64, 64);
        // 左半 → 右半：既不包含也不被包含，两个方向都编码不了
        let mut a = Mask::new(64, 64);
        for y in 0..64 {
            for x in 0..32 {
                a.set(x, y, 1);
            }
        }
        let mut b = Mask::new(64, 64);
        for y in 0..64 {
            for x in 32..64 {
                b.set(x, y, 1);
            }
        }
        d.active_channel_mut().insert_frame(0.0, a);
        d.active_channel_mut().insert_frame(90.0, b);

        // 首次：真算，失败
        let first = d.active_channel_mut().texels(64, 64);
        assert!(first.is_err(), "来回翻转的数据必须烘焙失败");

        // 第二次：必须命中失败缓存，返回同样的错误
        let second = d.active_channel_mut().texels(64, 64);
        assert!(second.is_err(), "仍然应该是错误");
        assert_eq!(
            first.unwrap_err(),
            second.unwrap_err(),
            "缓存返回的错误信息应一致"
        );

        // 内容一改，缓存必须失效（否则修好了仍然报错）
        d.active_channel_mut().frames[1].mask.clear();
        let after = d.active_channel_mut().texels(64, 64);
        assert!(after.is_ok(), "把冲突帧清空后应能烘焙成功，说明缓存已失效");
    }
}
