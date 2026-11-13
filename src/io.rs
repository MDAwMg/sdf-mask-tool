//! 导入底图、导出 SDF 贴图、工程存盘。

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use image::{GrayImage, RgbaImage};
use serde::{Deserialize, Serialize};

use crate::mask::Mask;
use crate::model::Doc;
use crate::model::doc::{ChannelMode, Keyframe, Layer};
use crate::sdf;

// ---------------------------------------------------------------- 导出

// ---------------------------------------------------------------- 导出

/// 烘焙产物：u16 texel 图 + 编码方向。
///
/// 内部一律用 u16（满刻度 65535）。8bit 输出是最后一步截断，
/// 所以 8bit 和 16bit 走**同一条精度路径**，不会出现"两套算法"的隐患。
///
/// 方向必须跟着贴图一起走：采样规则依赖方向，丢了方向会把遮罩还原反。
#[derive(Clone, Debug)]
pub struct Baked {
    pub texels: Vec<u16>,
    pub width: u32,
    pub height: u32,
    pub direction: sdf::Direction,
}

impl Baked {
    /// 按归一化时间采样成二值图（与引擎同一条规则）。
    ///
    /// 输出遵循照片习惯：**被遮 = 黑(0)，受光 = 白(255)**。
    /// `Mask` 里 1 表示被遮，`to_gray` 会把 1 映射成 255，
    /// 那是编辑器内部叠加用的约定，和这里的"黑白图"相反，所以要取反。
    pub fn restore_at(&self, t: f32) -> GrayImage {
        let mut mask = Mask::new(self.width, self.height);
        sdf::sample_texels(&self.texels, t, self.direction, &mut mask, false);
        let mut img = mask.to_gray();
        for p in img.pixels_mut() {
            p.0[0] = 255 - p.0[0];
        }
        img
    }

    /// 8bit 灰度图（满刻度 252，避开 255 的"全程受光"语义）
    pub fn to_gray8(&self) -> GrayImage {
        sdf::texels_to_gray8(&self.texels, self.width, self.height)
    }

    /// 16bit 灰度图（满刻度 65535，无损）
    pub fn to_gray16(&self) -> image::ImageBuffer<image::Luma<u16>, Vec<u16>> {
        sdf::texels_to_gray16(&self.texels, self.width, self.height)
    }

    /// 按位深取 8bit 图，供普通导出路径使用
    pub fn image(&self) -> GrayImage {
        self.to_gray8()
    }
}

/// 把通道烘焙成 SDF 贴图（自动判定方向）。
pub fn bake_channel(ch: &crate::model::doc::Channel, w: u32, h: u32) -> Result<Baked, String> {
    let mut frames: Vec<sdf::MaskFrame> = ch
        .frames
        .iter()
        .map(|f| sdf::MaskFrame {
            angle: f.angle,
            mask: f.mask.data().to_vec(),
        })
        .collect();
    let (texels, direction) = sdf::bake_auto(&mut frames, w as usize, h as usize)
        .map_err(|e| format!("通道 {}：{e}", ch.name))?;
    if texels.len() != (w * h) as usize {
        return Err("烘焙输出尺寸不符".to_string());
    }
    Ok(Baked {
        texels,
        width: w,
        height: h,
        direction,
    })
}

/// 按角度还原成二值图（用于预览校验）。角度范围 0..90。
pub fn restore_at(baked: &Baked, angle: f32) -> GrayImage {
    baked.restore_at(angle.clamp(0.0, 90.0) / 90.0)
}

/// 输出位深
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BitDepth {
    /// 8bit 单通道灰度（满刻度 252）
    #[default]
    Bit8,
    /// 16bit 单通道灰度（满刻度 65535）
    Bit16,
}

impl BitDepth {
    pub fn label(self) -> &'static str {
        match self {
            BitDepth::Bit8 => "8 bit",
            BitDepth::Bit16 => "16 bit",
        }
    }
}

/// 导出选项（导出窗口里的设置）
#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub bit_depth: BitDepth,
    /// 反向：把最终遮罩取反
    pub invert: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            bit_depth: BitDepth::Bit8,
            invert: false,
        }
    }
}

/// PNG 元数据块的键名：编码方向。
///
/// 贴图数值本身无法区分方向，着色器必须知道该用 `texel > t` 还是 `texel < t`。
/// 把方向写进 PNG 的 tEXt 块，引擎侧与美术侧都能直接从文件读出，
/// 不需要口头约定，也不会因为换个画法就悄悄还原反。
pub const PNG_KEY_DIRECTION: &str = "sdf-direction";

/// PNG 元数据块的键名：是否反向导出。
///
/// 「反向」把 texel 翻成 `FULL - v`，文件的数值语义随之改变。
/// 不记录这一位，引擎无法判断该不该再翻一次。
pub const PNG_KEY_INVERT: &str = "sdf-invert";

/// PNG 元数据块的键名：贴图语义版本，便于日后改编码时兼容。
pub const PNG_KEY_VERSION: &str = "sdf-version";

/// PNG 元数据块的键名：通道模式（gray / rgba）。
///
/// 单通道和四通道的数值含义相同，但引擎侧的采样轴不同
/// （灰度读 .r，RGBA 四个通道各是一条独立的参数轴）。记下来省得猜。
pub const PNG_KEY_CHANNEL_MODE: &str = "sdf-channels";

/// 当前贴图语义版本。
const TEX_SEMANTICS_VERSION: &str = "1";

/// 按选项把烘焙结果写成 PNG。
///
/// **反向不是对像素值取反**：那样会把"全程受光=255"翻成 0，
/// 而 0 恰恰是"0° 就遮"的专用值，语义会撞车。
/// 正确做法是把 texel 本身翻过来（`FULL - v`），
/// 再按原方向语义重新编码 —— 相当于把整条时间轴的明暗反过来。
///
/// 方向与反向标志都会写进 PNG 的 tEXt 块（见 `PNG_KEY_*`），
/// 否则同一份数值在两种方向下的含义相反，引擎无从判断。
pub fn write_baked(baked: &Baked, opts: &ExportOptions, path: &Path) -> Result<(), String> {
    let flipped: Vec<u16> = if opts.invert {
        baked
            .texels
            .iter()
            .map(|&v| (sdf::TEXEL_FULL as u16).saturating_sub(v))
            .collect()
    } else {
        baked.texels.clone()
    };

    let mut png: Vec<u8> = Vec::new();
    match opts.bit_depth {
        BitDepth::Bit8 => {
            let data: Vec<u8> = flipped.iter().map(|&v| sdf::texel_to_u8(v)).collect();
            let img = GrayImage::from_raw(baked.width, baked.height, data)
                .ok_or_else(|| "像素缓冲区与画布尺寸不匹配".to_string())?;
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| format!("编码 PNG 失败: {e}"))?;
        }
        BitDepth::Bit16 => {
            let img: image::ImageBuffer<image::Luma<u16>, Vec<u16>> =
                image::ImageBuffer::from_raw(baked.width, baked.height, flipped)
                    .ok_or_else(|| "像素缓冲区与画布尺寸不匹配".to_string())?;
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| format!("编码 PNG 失败: {e}"))?;
        }
    }

    let tags = [
        (PNG_KEY_DIRECTION, baked.direction.tag()),
        (PNG_KEY_INVERT, if opts.invert { "1" } else { "0" }),
        (PNG_KEY_VERSION, TEX_SEMANTICS_VERSION),
        (PNG_KEY_CHANNEL_MODE, "gray"),
    ];
    let png = png_with_text_chunks(&png, &tags)?;
    std::fs::write(path, &png).map_err(|e| format!("写文件失败: {e}"))?;
    Ok(())
}

/// 往已编码好的 PNG 里插入 tEXt 块。
///
/// 只改动 IHDR 之后的块序列：签名 + IHDR 原样保留，tEXt 插在 IHDR 之后，
/// 其余块（IDAT/IEND）顺序不动。PNG 规范允许 tEXt 出现在 IHDR 与 IEND 之间
/// 的任意位置，放在最前面可以让解析方不用读完像素就能拿到语义标记。
fn png_with_text_chunks(png: &[u8], tags: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() < SIG.len() + 8 || png[..8] != SIG {
        return Err("内部错误：PNG 签名不符".to_string());
    }

    let mut out = Vec::with_capacity(png.len() + tags.len() * 32);
    out.extend_from_slice(&SIG);

    // 复制 IHDR（长度 13 的块，固定 25 字节：4 长度 + 4 类型 + 13 数据 + 4 CRC）
    let ihdr_len = read_be_u32(&png[8..12]) as usize;
    let ihdr_end = 8 + 4 + 4 + ihdr_len + 4;
    if png.len() < ihdr_end {
        return Err("内部错误：PNG 的 IHDR 不完整".to_string());
    }
    out.extend_from_slice(&png[8..ihdr_end]);

    for (key, value) in tags {
        let mut data = Vec::with_capacity(key.len() + 1 + value.len());
        data.extend_from_slice(key.as_bytes());
        data.push(0); // 关键字与文本之间用 NUL 分隔
        data.extend_from_slice(value.as_bytes());

        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut typed = b"tEXt".to_vec();
        typed.extend_from_slice(&data);
        out.extend_from_slice(&typed);
        out.extend_from_slice(&crc32(&typed).to_be_bytes());
    }

    out.extend_from_slice(&png[ihdr_end..]);
    Ok(out)
}

fn read_be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// CRC-32（PNG 规范要求的多项式，与 zip 用的是同一个）。
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// 导出单个图层。
///
/// - `Gray`：只用 R 通道，导出单通道灰度
/// - `Rgba`：四个通道各烘一张，合成 RGBA
pub fn export_layer_opts(
    doc: &Doc,
    layer: usize,
    path: &Path,
    opts: &ExportOptions,
) -> Result<(), String> {
    let l = doc.layers.get(layer).ok_or("图层不存在")?;
    let (w, h) = (doc.width, doc.height);

    // 少于两帧没有插值可言，导出的图没有意义
    let usable = l.channels.iter().any(|c| c.frames.len() >= 2);
    if !usable {
        return Err("至少需要两帧才能导出".to_string());
    }

    match l.channel_mode {
        ChannelMode::Gray => {
            let baked = bake_channel(&l.channels[0], w, h)?;
            write_baked(&baked, opts, path)?;
        }
        ChannelMode::Rgba => {
            // RGBA 只能 8bit：PNG 的 16bit 是单通道或 RGBA16，
            // 这里简化处理，RGBA 固定 8bit 并在界面上说明。
            let r = bake_channel(&l.channels[0], w, h)?;
            let g = bake_channel(&l.channels[1], w, h)?;
            let b = bake_channel(&l.channels[2], w, h)?;
            let a = bake_channel(&l.channels[3], w, h)?;

            // 四个通道各自独立判方向，可能不一致。而 PNG 只有一份像素、
            // 也只能记一个方向标记——两个通道用相反的比较式，引擎无法同时正确。
            //
            // 不静默产出一张注定读错的图：直接拒绝，要求先把画法统一。
            let dirs = [r.direction, g.direction, b.direction, a.direction];
            if dirs.windows(2).any(|w| w[0] != w[1]) {
                let names = ["R", "G", "B", "A"];
                let detail: Vec<String> = names
                    .iter()
                    .zip(dirs.iter())
                    .map(|(n, d)| format!("{n}={}", d.label()))
                    .collect();
                return Err(format!(
                    "四个通道的明暗变化方向不一致（{}）。\n\
                     RGBA 导出成一张图，只能按一个方向解读；\n\
                     请让各通道的阴影都朝同一方向推进（都扩张或都收缩）后再导出。",
                    detail.join("，")
                ));
            }

            let iter = |bk: &Baked| -> Vec<u8> {
                let src: Vec<u16> = if opts.invert {
                    bk.texels
                        .iter()
                        .map(|&v| (sdf::TEXEL_FULL as u16).saturating_sub(v))
                        .collect()
                } else {
                    bk.texels.clone()
                };
                src.iter().map(|&v| sdf::texel_to_u8(v)).collect()
            };
            let (rv, gv, bv, av) = (iter(&r), iter(&g), iter(&b), iter(&a));

            let mut img = RgbaImage::new(w, h);
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) as usize;
                    img.put_pixel(x, y, image::Rgba([rv[i], gv[i], bv[i], av[i]]));
                }
            }
            let mut png: Vec<u8> = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| format!("编码 PNG 失败: {e}"))?;
            let tags = [
                (PNG_KEY_DIRECTION, r.direction.tag()),
                (PNG_KEY_INVERT, if opts.invert { "1" } else { "0" }),
                (PNG_KEY_VERSION, TEX_SEMANTICS_VERSION),
                (PNG_KEY_CHANNEL_MODE, "rgba"),
            ];
            let png = png_with_text_chunks(&png, &tags)?;
            std::fs::write(path, &png).map_err(|e| format!("写文件失败: {e}"))?;
        }
    }
    Ok(())
}

/// 导出单个图层（默认选项）。
pub fn export_layer(doc: &Doc, layer: usize, path: &Path) -> Result<(), String> {
    export_layer_opts(doc, layer, path, &ExportOptions::default())
}

/// 导出全部可见图层，文件名取图层名。返回写出的文件数。
pub fn export_all(doc: &Doc, dir: &Path) -> Result<usize, String> {
    export_all_opts(doc, dir, &ExportOptions::default())
}

/// 按选项导出全部可见图层，文件名取图层名。返回写出的文件数。
///
/// 图层名可能撞名：`sanitize_name` 会把 `/ : * ? " < > |` 全换成 `_`，
/// 于是 "a/b" 与 "a:b" 都变成 `a_b.png`，后写的静默覆盖前一个，
/// 而返回的文件数仍然加一——看起来像两层都导出了。
/// 这里对重名追加序号，保证每个图层各自落一个文件。
pub fn export_all_opts(doc: &Doc, dir: &Path, opts: &ExportOptions) -> Result<usize, String> {
    let mut n = 0;
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, l) in doc.layers.iter().enumerate() {
        if !l.visible {
            continue;
        }
        if !l.channels.iter().any(|c| c.frames.len() >= 2) {
            continue;
        }
        let safe = sanitize_name(&l.name);
        let stem = unique_name(&safe, &mut used);
        export_layer_opts(doc, i, &dir.join(format!("{stem}.png")), opts)?;
        n += 1;
    }
    if n == 0 {
        return Err("没有可导出的图层（每层至少需要两帧）".into());
    }
    Ok(n)
}

/// 在已占用的文件名集合里给 `base` 找一个不冲突的名字。
fn unique_name(base: &str, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    for k in 2..=usize::MAX {
        let cand = format!("{base}_{k}");
        if used.insert(cand.clone()) {
            return cand;
        }
    }
    unreachable!("序号是无限的，不可能全部占满")
}

fn sanitize_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if r#"\/:*?"<>|"#.contains(c) { '_' } else { c })
        .collect();
    if cleaned.trim().is_empty() {
        "layer".to_string()
    } else {
        cleaned
    }
}

// ---------------------------------------------------------------- 底图

pub fn load_background(path: &Path, w: u32, h: u32) -> Result<RgbaImage, String> {
    let img = image::open(path).map_err(|e| format!("读图失败: {e}"))?;
    let rgba = img.to_rgba8();
    if rgba.width() == w && rgba.height() == h {
        Ok(rgba)
    } else {
        Ok(image::imageops::resize(
            &rgba,
            w,
            h,
            image::imageops::FilterType::Lanczos3,
        ))
    }
}

// ---------------------------------------------------------------- 工程存盘

#[derive(Serialize, Deserialize)]
struct ProjectMeta {
    version: u32,
    width: u32,
    height: u32,
    background: Option<String>,
    background_visible: bool,
    layers: Vec<LayerMeta>,
    /// 离散帧数。旧工程没有这个字段，用 `serde(default)` 补上默认值。
    #[serde(default = "default_steps")]
    steps: usize,
}

fn default_steps() -> usize {
    crate::model::doc::DEFAULT_STEPS
}

#[derive(Serialize, Deserialize)]
struct LayerMeta {
    name: String,
    visible: bool,
    gray_mode: bool,
    channels: Vec<ChannelMeta>,
}

#[derive(Serialize, Deserialize)]
struct ChannelMeta {
    name: String,
    visible: bool,
    frames: Vec<FrameMeta>,
}

#[derive(Serialize, Deserialize)]
struct FrameMeta {
    angle: f32,
    entry: String,
}

const META_ENTRY: &str = "project.ron";
const BG_ENTRY: &str = "background.png";

/// 工程文件扩展名（不含点）。
///
/// 工程是 ZIP 容器，内容与导出的 PNG 贴图无法靠魔数区分，
/// 因此后缀是「双击打开」和「拖入窗口」唯一的识别依据。
/// 全项目只在这里定义一次，其余位置一律引用，避免改名时漏改。
pub const PROJECT_EXT: &str = "sdfmask";

/// 旧版本的工程扩展名。打开时仍接受，保证已存的工程能继续读。
pub const PROJECT_EXT_LEGACY: &str = "sdfproj";

/// 判断路径是否是工程文件。
///
/// 扩展名比较忽略大小写：Windows 上用户可能存成 `.SDFMASK`，
/// 而 `Path::extension` 保留原始大小写。
pub fn is_project_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| e == PROJECT_EXT || e == PROJECT_EXT_LEGACY)
}

pub fn save_project(doc: &Doc, path: &Path) -> Result<(), String> {
    // **先写临时文件，成功后再原子替换目标**。
    //
    // 直接 `File::create(path)` 会立刻把已有工程截断成 0 字节。之后任何一步
    // 失败（磁盘满、编码错误、中途断电）都会留下半截 zip，原工程已经没了。
    // 临时文件 + rename 保证要么是完整的新工程，要么是完整的旧工程。
    let tmp = temp_sibling(path);
    // 上次异常退出可能留下同名临时文件
    let _ = std::fs::remove_file(&tmp);

    let result = write_project_to(doc, &tmp);
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // rename 覆盖是原子的（Windows 上 `std::fs::rename` 会替换已存在的目标）
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("替换工程文件失败: {e}"));
    }
    Ok(())
}

/// 与目标同目录的临时文件名。
///
/// 必须同目录：跨卷 rename 会退化成"复制+删除"，失去原子性。
fn temp_sibling(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn write_project_to(doc: &Doc, path: &Path) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("建文件失败: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut layer_metas = Vec::new();
    for (li, l) in doc.layers.iter().enumerate() {
        let mut ch_metas = Vec::new();
        for (ci, ch) in l.channels.iter().enumerate() {
            let mut frames = Vec::new();
            for (fi, f) in ch.frames.iter().enumerate() {
                let name = format!("mask_{li}_{ci}_{fi}.png");
                let mut buf = std::io::Cursor::new(Vec::new());
                f.mask
                    .to_gray()
                    .write_to(&mut buf, image::ImageFormat::Png)
                    .map_err(|e| format!("编码 PNG 失败: {e}"))?;
                zip.start_file(&name, opts)
                    .map_err(|e| format!("写 zip 失败: {e}"))?;
                zip.write_all(buf.get_ref())
                    .map_err(|e| format!("写 zip 失败: {e}"))?;
                frames.push(FrameMeta {
                    angle: f.angle,
                    entry: name,
                });
            }
            ch_metas.push(ChannelMeta {
                name: ch.name.clone(),
                visible: ch.visible,
                frames,
            });
        }
        layer_metas.push(LayerMeta {
            name: l.name.clone(),
            visible: l.visible,
            gray_mode: l.channel_mode == ChannelMode::Gray,
            channels: ch_metas,
        });
    }

    let bg_entry = if let Some(bg) = &doc.background {
        let mut buf = std::io::Cursor::new(Vec::new());
        bg.write_to(&mut buf, image::ImageFormat::Png)
            .map_err(|e| format!("编码底图失败: {e}"))?;
        zip.start_file(BG_ENTRY, opts)
            .map_err(|e| format!("写 zip 失败: {e}"))?;
        zip.write_all(buf.get_ref())
            .map_err(|e| format!("写 zip 失败: {e}"))?;
        Some(BG_ENTRY.to_string())
    } else {
        None
    };

    let meta = ProjectMeta {
        version: 3,
        width: doc.width,
        height: doc.height,
        background: bg_entry,
        background_visible: doc.background_visible,
        layers: layer_metas,
        steps: doc.steps,
    };

    let ron = ron::ser::to_string_pretty(&meta, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("序列化失败: {e}"))?;
    zip.start_file(META_ENTRY, opts)
        .map_err(|e| format!("写 zip 失败: {e}"))?;
    zip.write_all(ron.as_bytes())
        .map_err(|e| format!("写 zip 失败: {e}"))?;
    let mut writer = zip.finish().map_err(|e| format!("收尾失败: {e}"))?;
    // 落盘再 rename：否则断电可能得到"文件名已换、内容还没写完"的状态。
    //
    // 必须用写句柄同步。对一个新的**只读**句柄调 `sync_all` 在 Windows 上
    // 会被拒（os error 5），因为刷新缓冲区需要写权限。
    writer
        .sync_all()
        .map_err(|e| format!("刷新磁盘缓存失败: {e}"))?;
    Ok(())
}

pub fn open_project(path: &Path) -> Result<Doc, String> {
    let file = File::open(path).map_err(|e| format!("打开失败: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("不是有效的工程文件: {e}"))?;

    let meta: ProjectMeta = {
        let mut f = zip
            .by_name(META_ENTRY)
            .map_err(|_| "工程文件缺少 project.ron".to_string())?;
        let mut s = String::new();
        f.read_to_string(&mut s)
            .map_err(|e| format!("读元数据失败: {e}"))?;
        ron::from_str(&s).map_err(|e| format!("解析元数据失败: {e}"))?
    };

    // 画布尺寸直接进 `Mask::new` 与烘焙的 `w*h` 乘法。畸形工程给个 10 万
    // 会一次性申请几十 GB，或在 `w*h` 处整数溢出。
    //
    // 上限与 `Doc::resize` 一致（8192）；下限放宽到 1——`resize` 的 16 是
    // 交互输入的下限，而内部构造的小画布（测试、程序化生成）是合法状态。
    if meta.width == 0 || meta.height == 0 || meta.width > 8192 || meta.height > 8192 {
        return Err(format!(
            "工程画布尺寸 {}×{} 超出允许范围（1–8192），文件可能已损坏",
            meta.width, meta.height
        ));
    }

    let mut doc = Doc::with_steps(meta.width, meta.height, meta.steps);
    doc.background_visible = meta.background_visible;
    doc.layers.clear();

    for lm in &meta.layers {
        let mut layer = Layer::new(&lm.name, doc.width, doc.height);
        layer.visible = lm.visible;
        layer.channel_mode = if lm.gray_mode {
            ChannelMode::Gray
        } else {
            ChannelMode::Rgba
        };
        for (ci, cm) in lm.channels.iter().enumerate().take(4) {
            layer.channels[ci].name = cm.name.clone();
            layer.channels[ci].visible = cm.visible;
            for fm in &cm.frames {
                // 角度会进 `partial_cmp().unwrap()` 与排序比较器。
                // NaN 在那里是 panic，在烘焙里是静默变成"0° 就遮"。
                // 两种表现都不该由损坏文件触发，直接拒绝。
                if !fm.angle.is_finite() || !(0.0..=90.0).contains(&fm.angle) {
                    return Err(format!(
                        "工程里的参数点位置 {} 非法（应为 0–90 的有限数）",
                        fm.angle
                    ));
                }
                let bytes = {
                    let mut f = zip
                        .by_name(&fm.entry)
                        .map_err(|_| format!("缺少 {}", fm.entry))?;
                    let mut v = Vec::new();
                    f.read_to_end(&mut v)
                        .map_err(|e| format!("读 {} 失败: {e}", fm.entry))?;
                    v
                };
                let img = image::load_from_memory(&bytes)
                    .map_err(|e| format!("解码 {} 失败: {e}", fm.entry))?
                    .to_luma8();
                // 尺寸不符的遮罩会让 EDT 越界读（release 下 panic）——
                // 内部只有 debug_assert，挡不住这种输入。
                if (img.width(), img.height()) != (doc.width, doc.height) {
                    return Err(format!(
                        "{} 的尺寸 {}×{} 与画布 {}×{} 不符",
                        fm.entry,
                        img.width(),
                        img.height(),
                        doc.width,
                        doc.height
                    ));
                }
                layer.channels[ci]
                    .frames
                    .push(Keyframe::new(fm.angle, Mask::from_gray(&img)));
            }
        }
        doc.layers.push(layer);
    }

    if doc.layers.is_empty() {
        doc.layers.push(Layer::new("图层 1", doc.width, doc.height));
    }

    if let Some(entry) = &meta.background {
        let bytes = {
            let mut f = zip.by_name(entry).map_err(|_| "缺少底图".to_string())?;
            let mut v = Vec::new();
            f.read_to_end(&mut v)
                .map_err(|e| format!("读底图失败: {e}"))?;
            v
        };
        doc.background = image::load_from_memory(&bytes)
            .map_err(|e| format!("解码底图失败: {e}"))
            .map(|i| i.to_rgba8())
            .ok();
    }

    doc.active_layer = 0;
    doc.active_channel = 0;
    // 关键帧必须落在 32 个离散步上，否则播放头停不到那里，
    // 那一帧就既画不了也删不掉。旧工程可能存着任意角度，这里统一吸附。
    doc.snap_frames();
    Ok(doc)
}

/// 水平翻转一张遮罩
pub fn flip_mask(m: &Mask) -> Mask {
    let (w, h) = (m.width(), m.height());
    let mut out = Mask::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set(w - 1 - x, y, m.get(x, y));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::doc::{DEFAULT_STEPS, angle_to_step, step_angle};

    /// 测试统一用默认步数
    const TS: usize = DEFAULT_STEPS;

    fn test_dir() -> std::path::PathBuf {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-out");
        std::fs::create_dir_all(&p).ok();
        p
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        test_dir().join(name)
    }

    /// 造一个两帧的最小文档
    fn doc_with_two_frames() -> Doc {
        let mut d = Doc::new(8, 8);
        d.layers[0].channels[0].insert_frame(0.0, Mask::new(8, 8));
        let mut m = Mask::new(8, 8);
        for y in 0..8 {
            for x in 0..4 {
                m.set(x, y, 1);
            }
        }
        d.layers[0].channels[0].insert_frame(90.0, m);
        d
    }

    #[test]
    fn 灰度层导出得到单通道图() {
        let mut d = doc_with_two_frames();
        d.layers[0].channel_mode = ChannelMode::Gray;
        let p = tmp("sdf_gray.png");
        export_layer(&d, 0, &p).unwrap();
        assert_eq!(image::open(&p).unwrap().color(), image::ColorType::L8);
    }

    #[test]
    fn rgba层导出四通道() {
        let mut d = doc_with_two_frames();
        d.layers[0].channel_mode = ChannelMode::Rgba;
        let p = tmp("sdf_rgba.png");
        export_layer(&d, 0, &p).unwrap();
        assert_eq!(image::open(&p).unwrap().color(), image::ColorType::Rgba8);
    }

    #[test]
    fn 少于两帧拒绝导出() {
        let mut d = Doc::new(8, 8);
        d.layers[0].channels[0].insert_frame(0.0, Mask::new(8, 8));
        let p = tmp("sdf_one.png");
        let r = export_layer(&d, 0, &p);
        assert!(r.is_err(), "只有一帧应该拒绝导出");
        assert!(r.unwrap_err().contains("两帧"));
    }

    #[test]
    fn 存盘读盘往返一致() {
        let mut d = doc_with_two_frames();
        d.layers[0].name = "测试层".into();
        let p = tmp(&format!("sdf_roundtrip.{PROJECT_EXT}"));
        save_project(&d, &p).unwrap();
        let back = open_project(&p).unwrap();
        assert_eq!(back.width, 8);
        assert_eq!(back.layers.len(), 1);
        assert_eq!(back.layers[0].name, "测试层");
        assert_eq!(back.layers[0].channels[0].frames.len(), 2);
        // 二值保持不变
        assert_eq!(back.layers[0].channels[0].frames[1].mask.get(0, 0), 1);
        assert_eq!(back.layers[0].channels[0].frames[1].mask.get(7, 0), 0);
    }

    /// 扩展名识别：新后缀、旧后缀都要认出是工程，且忽略大小写。
    ///
    /// 扩展名是「双击打开」与「拖入窗口」唯一的识别依据——工程是 ZIP 容器，
    /// 内容层面和导出的 PNG 贴图无法靠魔数区分。判断漏一种后缀，
    /// 文件会被当成底图去加载，报错信息还指向图片解码。
    #[test]
    fn 工程扩展名识别忽略大小写并兼容旧后缀() {
        use std::path::PathBuf;
        let yes = [
            format!("a.{PROJECT_EXT}"),
            format!("a.{}", PROJECT_EXT.to_uppercase()),
            format!("a.{PROJECT_EXT_LEGACY}"),
            format!("a.{}", PROJECT_EXT_LEGACY.to_uppercase()),
            // 资源管理器可能存成首字母大写
            format!("a.{}", {
                let mut c = PROJECT_EXT.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }),
        ];
        for name in yes {
            assert!(
                is_project_path(&PathBuf::from(&name)),
                "{name} 应被识别为工程文件"
            );
        }

        let no = [
            "a.png".to_string(),
            "a.jpg".to_string(),
            // 只是前缀相同，不能误判
            format!("a.{PROJECT_EXT}.png"),
            format!("a.{PROJECT_EXT}ing"),
            "a".to_string(),
            "a.".to_string(),
        ];
        for name in no {
            assert!(
                !is_project_path(&PathBuf::from(&name)),
                "{name} 不应被识别为工程文件"
            );
        }
    }

    /// 改后缀后旧工程仍要能打开：容器内部结构没有变。
    #[test]
    fn 旧后缀的工程仍可打开() {
        let d = doc_with_two_frames();
        let p = tmp(&format!("sdf_legacy.{PROJECT_EXT_LEGACY}"));
        save_project(&d, &p).unwrap();
        assert!(is_project_path(&p), "旧后缀必须仍被识别");
        let back = open_project(&p).unwrap();
        assert_eq!(back.layers[0].channels[0].frames.len(), 2);
    }

    #[test]
    fn 存盘保留多图层多通道() {
        let mut d = doc_with_two_frames();
        d.add_layer("第二层");
        d.layers[1].channels[2].insert_frame(0.0, Mask::new(8, 8));
        d.layers[1].channels[2].insert_frame(step_angle(16, TS), Mask::new(8, 8));

        let p = tmp(&format!("sdf_multi.{PROJECT_EXT}"));
        save_project(&d, &p).unwrap();
        let back = open_project(&p).unwrap();
        assert_eq!(back.layers.len(), 2);
        assert_eq!(back.layers[0].channels[0].frames.len(), 2);
        assert_eq!(back.layers[1].channels[2].frames.len(), 2);
        assert!((back.layers[1].channels[2].frames[1].angle - step_angle(16, TS)).abs() < 1e-3);
    }

    /// 旧工程可能存着不在步角上的帧（那时候还没强制吸附）。
    /// 读盘后必须把角度与步号对齐，否则播放头停不过去，那些点永远编辑不到。
    #[test]
    fn 读盘把非步角的帧吸附回步角() {
        let mut d = doc_with_two_frames();
        // 45° 不在任何步上：31 段分 90°，每段约 2.903°
        d.layers[0].channels[0].insert_frame(45.0, Mask::new(8, 8));
        let p = tmp(&format!("sdf_snap.{PROJECT_EXT}"));
        save_project(&d, &p).unwrap();

        let back = open_project(&p).unwrap();
        let expected = step_angle(angle_to_step(45.0, TS), TS);
        assert!(
            back.layers[0].channels[0].has_frame_at(expected),
            "45° 应被吸附到最近的步角 {expected}"
        );
        for f in back.layers[0].channels[0].frames.iter() {
            let s = angle_to_step(f.angle, TS);
            assert!(
                (f.angle - step_angle(s, TS)).abs() < 1e-3,
                "所有帧都应落在步角上，{} 不在",
                f.angle
            );
        }
    }

    #[test]
    fn 底图尺寸不符时缩放到画布() {
        let p = tmp("sdf_bg_src.png");
        RgbaImage::from_pixel(100, 50, image::Rgba([10, 20, 30, 255]))
            .save(&p)
            .unwrap();
        let bg = load_background(&p, 64, 64).unwrap();
        assert_eq!(bg.width(), 64);
        assert_eq!(bg.height(), 64);
    }

    #[test]
    fn 水平翻转左右互换() {
        let mut m = Mask::new(4, 4);
        m.set(0, 0, 1);
        let f = flip_mask(&m);
        assert_eq!(f.get(3, 0), 1);
        assert_eq!(f.get(0, 0), 0);
    }

    #[test]
    fn 导出全部跳过单帧图层() {
        let mut d = doc_with_two_frames();
        d.add_layer("单帧层");
        d.layers[1].channels[0].insert_frame(0.0, Mask::new(8, 8));
        let dir = test_dir().join("sdf_export_all");
        std::fs::create_dir_all(&dir).unwrap();
        let n = export_all(&d, &dir).unwrap();
        assert_eq!(n, 1, "单帧图层应被跳过");
    }

    // ------------------------------------------------ 方向校验

    /// 阴影随角度增加 → 可编码，不该告警
    // ------------------------------------------------ 方向判定

    /// 阴影随角度扩张 → 能编码，导出不该报错
    #[test]
    fn 阴影随角度扩张时可导出() {
        let mut d = Doc::new(8, 8);
        d.layers[0].channels[0].insert_frame(0.0, Mask::new(8, 8));
        let mut m = Mask::new(8, 8);
        for y in 0..8 {
            for x in 0..4 {
                m.set(x, y, 1);
            }
        }
        d.layers[0].channels[0].insert_frame(90.0, m);
        let p = tmp("sdf_grow_ok.png");
        assert!(export_layer(&d, 0, &p).is_ok(), "阴影扩张应能导出");
    }

    /// 阴影随角度收缩 → 试另一个方向能通过，导出也成功（方向自动翻转）
    #[test]
    fn 阴影随角度收缩时用反向编码导出() {
        let mut d = Doc::new(8, 8);
        let mut big = Mask::new(8, 8);
        for y in 0..8 {
            for x in 0..6 {
                big.set(x, y, 1);
            }
        }
        d.layers[0].channels[0].insert_frame(0.0, big);
        let mut small = Mask::new(8, 8);
        for y in 0..8 {
            for x in 0..2 {
                small.set(x, y, 1);
            }
        }
        d.layers[0].channels[0].insert_frame(90.0, small);

        let p = tmp("sdf_shrink_ok.png");
        assert!(
            export_layer(&d, 0, &p).is_ok(),
            "收缩方向应自动改用反向编码，不该报错"
        );

        // 且还原两端要和关键帧一致
        let img = bake_channel(&d.layers[0].channels[0], 8, 8).unwrap();
        let r0 = restore_at(&img, 0.0);
        let r90 = restore_at(&img, 90.0);
        assert_eq!(
            r0.get_pixel(5, 4).0[0],
            0,
            "0° 时 x=5 应被遮（原帧是宽阴影）"
        );
        assert_eq!(r90.get_pixel(5, 4).0[0], 255, "90° 时 x=5 应受光");
    }

    /// 来回翻转的数据两个方向都不行 → 必须拒绝导出
    #[test]
    fn 来回翻转时拒绝导出() {
        let mut d = Doc::new(8, 8);
        let wide = {
            let mut m = Mask::new(8, 8);
            for y in 0..8 {
                for x in 0..6 {
                    m.set(x, y, 1);
                }
            }
            m
        };
        let narrow = {
            let mut m = Mask::new(8, 8);
            for y in 0..8 {
                for x in 0..2 {
                    m.set(x, y, 1);
                }
            }
            m
        };
        // 宽 → 窄 → 宽 → 窄：来回翻
        d.layers[0].channels[0].insert_frame(0.0, wide.clone());
        d.layers[0].channels[0].insert_frame(22.5, narrow.clone());
        d.layers[0].channels[0].insert_frame(45.0, wide);
        d.layers[0].channels[0].insert_frame(90.0, narrow);

        let p = tmp("sdf_back_forth.png");
        let r = export_layer(&d, 0, &p);
        assert!(r.is_err(), "来回翻转必须拒绝导出");
        let msg = r.unwrap_err();
        assert!(
            msg.contains("来回翻转") || msg.contains("冲突"),
            "错误信息应说明来回翻转，实际: {msg}"
        );
    }

    #[test]
    fn 图层名非法字符被替换() {
        let s = sanitize_name("a/b:c*?");
        assert!(!s.contains('/'));
        assert!(!s.contains('*'));
    }

    #[test]
    fn 存盘保留离散帧数() {
        let mut d = doc_with_two_frames();
        d.set_steps(128);
        let p = tmp(&format!("sdf_steps.{PROJECT_EXT}"));
        save_project(&d, &p).unwrap();
        let back = open_project(&p).unwrap();
        assert_eq!(back.steps, 128, "离散帧数是工程设置，必须跟着存盘");
    }

    /// 旧工程的 project.ron 里没有 steps 字段，读盘不能报错，要用默认值。
    #[test]
    fn 旧工程缺steps字段时用默认值() {
        let mut d = doc_with_two_frames();
        let p = tmp(&format!("sdf_old.{PROJECT_EXT}"));
        save_project(&d, &p).unwrap();

        // 手工把 steps 那行从 ron 里删掉，模拟 version 2 的老工程
        let mut meta_text = String::new();
        {
            use std::io::Read as _;
            let file = File::open(&p).unwrap();
            let mut zip = zip::ZipArchive::new(file).unwrap();
            let mut f = zip.by_name(META_ENTRY).unwrap();
            f.read_to_string(&mut meta_text).unwrap();
        }
        let stripped: String = meta_text
            .lines()
            .filter(|l| !l.trim_start().starts_with("steps:"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!stripped.contains("steps:"), "应已删掉 steps 行");

        // 重新打一个只改了元数据的工程
        let p2 = tmp(&format!("sdf_old2.{PROJECT_EXT}"));
        {
            let file = File::create(&p2).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            let orig = File::open(&p).unwrap();
            let mut src = zip::ZipArchive::new(orig).unwrap();
            let names: Vec<String> = src.file_names().map(|n| n.unwrap().to_string()).collect();
            for (i, name) in names.iter().enumerate() {
                let mut f = src.by_index(i).unwrap();
                let mut buf = Vec::new();
                use std::io::Read as _;
                f.read_to_end(&mut buf).unwrap();
                drop(f);
                let content: Vec<u8> = if name == META_ENTRY {
                    stripped.as_bytes().to_vec()
                } else {
                    buf
                };
                zip.start_file(name, opts).unwrap();
                zip.write_all(&content).unwrap();
            }
            zip.finish().unwrap();
        }

        let back = open_project(&p2).expect("缺 steps 字段不该导致读盘失败");
        assert_eq!(back.steps, crate::model::doc::DEFAULT_STEPS);
        let _ = &mut d;
    }

    #[test]
    fn 烘焙出的图能还原两端() {
        let d = doc_with_two_frames();
        let sdf_img = bake_channel(&d.layers[0].channels[0], 8, 8).unwrap();
        // 0°：全不遮 → 全白
        let r0 = restore_at(&sdf_img, 0.0);
        assert!(r0.iter().filter(|&&v| v > 127).count() > 60);
        // 90°：左半被遮
        let r90 = restore_at(&sdf_img, 90.0);
        assert_eq!(r90.get_pixel(1, 4).0[0], 0, "左侧应被遮");
        assert_eq!(r90.get_pixel(7, 4).0[0], 255, "右侧应不遮");
    }

    // ------------------------------------------------ 导出契约（本轮新增）

    /// 导出文件必须带方向与反向标记。
    ///
    /// 没有这两个字段，同一份数值在两种方向下的含义相反，引擎无从判断——
    /// 表现为"导出成功但整张贴图明暗颠倒"。
    #[test]
    fn 导出文件带方向元数据() {
        let d = doc_with_two_frames();
        let p = tmp("sdf_meta_dir.png");
        let baked = bake_channel(&d.layers[0].channels[0], 8, 8).unwrap();
        write_baked(&baked, &ExportOptions::default(), &p).unwrap();

        let bytes = std::fs::read(&p).unwrap();
        let tags = read_text_chunks(&bytes);
        assert!(
            tags.iter()
                .any(|(k, v)| k == PNG_KEY_DIRECTION && v == baked.direction.tag()),
            "必须写入 sdf-direction，实际 {tags:?}"
        );
        assert!(
            tags.iter().any(|(k, v)| k == PNG_KEY_INVERT && v == "0"),
            "必须写入 sdf-invert=0，实际 {tags:?}"
        );
        assert!(
            tags.iter().any(|(k, _)| k == PNG_KEY_VERSION),
            "必须写入语义版本，实际 {tags:?}"
        );
        // 加了元数据块之后 PNG 仍必须能被标准解码器读出
        let img = image::open(&p).expect("带 tEXt 的 PNG 必须仍可解码");
        assert_eq!((img.width(), img.height()), (8, 8));
    }

    /// 反向导出要把 `invert` 记成 1，否则引擎会按未反向解读。
    #[test]
    fn 反向导出记下标志() {
        let d = doc_with_two_frames();
        let p = tmp("sdf_meta_inv.png");
        let baked = bake_channel(&d.layers[0].channels[0], 8, 8).unwrap();
        let opts = ExportOptions {
            bit_depth: BitDepth::Bit8,
            invert: true,
        };
        write_baked(&baked, &opts, &p).unwrap();
        let tags = read_text_chunks(&std::fs::read(&p).unwrap());
        assert!(
            tags.iter().any(|(k, v)| k == PNG_KEY_INVERT && v == "1"),
            "反向导出必须记 sdf-invert=1，实际 {tags:?}"
        );
    }

    /// 图层名撞名时不能静默覆盖。
    ///
    /// `sanitize_name` 把 `/` 和 `:` 都换成 `_`，于是 "a/b" 与 "a:b"
    /// 会落到同一个文件名上。早期实现后写的覆盖前一个，返回的文件数
    /// 却仍然加一——看起来像两层都导出了。
    #[test]
    fn 导出撞名不覆盖() {
        let dir = test_dir().join("sdf_dup_name");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut d = Doc::new(8, 8);
        let mut m = Mask::new(8, 8);
        m.set(1, 1, 1);
        d.layers[0].name = "a/b".to_string();
        d.layers[0].channels[0].insert_frame(0.0, Mask::new(8, 8));
        d.layers[0].channels[0].insert_frame(90.0, m);
        let mut l2 = d.layers[0].clone();
        l2.name = "a:b".to_string();
        d.layers.push(l2);

        let n = export_all(&d, &dir).unwrap();
        let files: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".png"))
            .collect();
        assert_eq!(n, 2, "两层都应计入");
        assert_eq!(
            files.len(),
            2,
            "两个图层必须各落一个文件，实际只有 {files:?}"
        );
    }

    /// 打开尺寸非法的工程要返回 Err，不能 panic 也不能尝试分配巨量内存。
    #[test]
    fn 拒绝畸形画布尺寸() {
        // 用最小 zip 手写一份 meta，绕过 save_project 自己构造
        let p = test_dir().join(format!("sdf_bad_size.{PROJECT_EXT}"));
        {
            let file = File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file(META_ENTRY, opts).unwrap();
            let bad = "ProjectMeta(version:3,width:100000,height:100000,\
                       background:None,background_visible:true,layers:[],\
                       steps:64)";
            zip.write_all(bad.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        match open_project(&p) {
            Ok(_) => panic!("超大尺寸必须被拒绝"),
            Err(e) => assert!(e.contains("超出允许范围"), "错误信息应说明原因，实际：{e}"),
        }
    }

    /// 打开含 NaN 角度的工程要返回 Err。
    ///
    /// NaN 会进 `partial_cmp().unwrap()`（排序处 panic）与烘焙路径
    /// （静默变成"0° 就遮"）。两种表现都不该由损坏文件触发。
    #[test]
    fn 拒绝非法参数点角度() {
        let p = test_dir().join(format!("sdf_bad_angle.{PROJECT_EXT}"));
        {
            let file = File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            // 先放一张合法 PNG 供条目引用
            let mut buf = std::io::Cursor::new(Vec::new());
            Mask::new(8, 8)
                .to_gray()
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            zip.start_file("mask_0_0_0.png", opts).unwrap();
            zip.write_all(buf.get_ref()).unwrap();
            zip.start_file(META_ENTRY, opts).unwrap();
            let bad = "ProjectMeta(version:3,width:8,height:8,\
                       background:None,background_visible:true,\
                       layers:[LayerMeta(name:\"L\",visible:true,gray_mode:true,\
                       channels:[ChannelMeta(name:\"R\",visible:true,\
                       frames:[FrameMeta(angle:NaN,entry:\"mask_0_0_0.png\")])])],\
                       steps:64)";
            zip.write_all(bad.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        match open_project(&p) {
            Ok(_) => panic!("NaN 角度必须被拒绝"),
            Err(e) => assert!(e.contains("非法"), "错误信息应说明原因，实际：{e}"),
        }
    }

    /// 打开尺寸与画布不符的遮罩要返回 Err（否则 EDT 会越界读）。
    #[test]
    fn 拒绝尺寸不符的遮罩() {
        let p = test_dir().join(format!("sdf_bad_mask.png.{PROJECT_EXT}"));
        {
            let file = File::create(&p).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            let mut buf = std::io::Cursor::new(Vec::new());
            // 16×16 的遮罩，但 meta 声明画布 8×8
            Mask::new(16, 16)
                .to_gray()
                .write_to(&mut buf, image::ImageFormat::Png)
                .unwrap();
            zip.start_file("mask_0_0_0.png", opts).unwrap();
            zip.write_all(buf.get_ref()).unwrap();
            zip.start_file(META_ENTRY, opts).unwrap();
            let bad = "ProjectMeta(version:3,width:8,height:8,\
                       background:None,background_visible:true,\
                       layers:[LayerMeta(name:\"L\",visible:true,gray_mode:true,\
                       channels:[ChannelMeta(name:\"R\",visible:true,\
                       frames:[FrameMeta(angle:0.0,entry:\"mask_0_0_0.png\")])])],\
                       steps:64)";
            zip.write_all(bad.as_bytes()).unwrap();
            zip.finish().unwrap();
        }
        match open_project(&p) {
            Ok(_) => panic!("尺寸不符的遮罩必须被拒绝"),
            Err(e) => assert!(e.contains("不符"), "错误信息应说明原因，实际：{e}"),
        }
    }

    /// RGBA 四通道方向不一致时必须拒绝导出。
    ///
    /// 四个通道共用一个 PNG 文件与一份方向标记，方向相反就无法同时正确解读。
    #[test]
    fn 通道方向不一致时拒绝导出() {
        let mut d = Doc::new(16, 16);
        d.layers[0].channel_mode = ChannelMode::Rgba;
        // R：扩张（小 → 大）
        let small = {
            let mut m = Mask::new(16, 16);
            for y in 6..10 {
                for x in 6..10 {
                    m.set(x, y, 1);
                }
            }
            m
        };
        let big = {
            let mut m = Mask::new(16, 16);
            for y in 2..14 {
                for x in 2..14 {
                    m.set(x, y, 1);
                }
            }
            m
        };
        d.layers[0].channels[0].insert_frame(0.0, small.clone());
        d.layers[0].channels[0].insert_frame(90.0, big.clone());
        // G：收缩（大 → 小）
        d.layers[0].channels[1].insert_frame(0.0, big);
        d.layers[0].channels[1].insert_frame(90.0, small);

        let p = test_dir().join("sdf_rgba_mixed.png");
        match export_layer_opts(&d, 0, &p, &ExportOptions::default()) {
            Ok(()) => panic!("方向不一致必须拒绝导出，否则引擎无法正确解读"),
            Err(e) => assert!(e.contains("方向不一致"), "错误信息应说明原因，实际：{e}"),
        }
    }

    /// 从 PNG 里读出所有 tEXt 块，测试用。
    fn read_text_chunks(png: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut i = 8usize;
        while i + 8 <= png.len() {
            let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
            let ty = &png[i + 4..i + 8];
            if ty == b"tEXt" && i + 8 + len <= png.len() {
                let data = &png[i + 8..i + 8 + len];
                if let Some(pos) = data.iter().position(|&b| b == 0) {
                    let k = String::from_utf8_lossy(&data[..pos]).to_string();
                    let v = String::from_utf8_lossy(&data[pos + 1..]).to_string();
                    out.push((k, v));
                }
            }
            if ty == b"IEND" {
                break;
            }
            i += 12 + len;
        }
        out
    }
}
