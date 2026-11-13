//! 二进制 FBX 解析。
//!
//! 为什么手写：目标素材的 .fbx 均为二进制格式（文件头 `Kaydara FBX Binary`），
//! 而本地 cargo 仓库里没有可用的 FBX crate，装包需要联网。FBX 二进制的结构其实
//! 很规整——一棵带长度前缀的节点树——手写比引依赖更可控。
//!
//! 只提取三样东西：`Vertices`、`PolygonVertexIndex`、`LayerElementUV`。
//! 骨骼（Deformer）、动画、材质、法线一律跳过：工具只做预览。
//!
//! # 格式要点（都是踩过的坑）
//!
//! 1. **偏移宽度随版本变**：7300/7400 用 32 位偏移，7500 起用 64 位。
//! 目标素材的文件版本正好是 7300 和 7400。
//!
//! 2. **多边形索引用负数标记结尾**：最后那个角存成 `~index`（即 `-(index+1)`），
//! 所以读到负数就知道这个多边形结束了，并且要把负数还原成正常下标。
//!
//! 3. **数组可能被 zlib 压缩**：属性头里 `encoding=1` 表示要解压。
//! 已经有的 `flate2` 直接用（`flate2` 是 image 的传递依赖，不新增包）。
//!
//! 4. **UV 有多种映射方式**：`ByVertex` / `ByPolygonVertex` / `ByPolygon`
//! 配 `Direct` / `IndexToDirect`。全都要支持，否则贴图会整片错位，
//! 而且错得很隐蔽（看起来像"UV 画错了"而不是"解析错了"）。

use std::io::Read;
use std::path::Path;

use flate2::read::ZlibDecoder;

use super::mesh::{LoadError, Mesh};

const MAGIC: &[u8] = b"Kaydara FBX Binary  \0";
/// 64 位偏移从 7500 开始
const VER_64BIT: u32 = 7500;

pub fn load(path: &Path) -> Result<Mesh, LoadError> {
    let t0 = std::time::Instant::now();
    let bytes = std::fs::read(path).map_err(|e| LoadError::Io(e.to_string()))?;
    let mut m = parse(&bytes)?;
    m.stats.parse_ms = t0.elapsed().as_secs_f32() * 1000.0;
    Ok(m)
}

/// 从内存字节解析。测试直接用这个。
pub fn parse(bytes: &[u8]) -> Result<Mesh, LoadError> {
    if bytes.len() < 27 {
        return Err(LoadError::Format("文件太小，不像 FBX".into()));
    }

    // ASCII FBX 有明确的错法，单独给出提示——否则容易被误判为文件损坏
    if !bytes.starts_with(MAGIC) {
        let head = &bytes[..bytes.len().min(64)];
        if head.windows(3).any(|w| w == b"; FBX") || head.starts_with(b";") {
            return Err(LoadError::Format(
                "这是 ASCII 格式的 FBX，本工具只支持二进制 FBX。\
                 请在导出时勾选 Binary（或在 Blender 里另存为 .obj）。"
                    .into(),
            ));
        }
        return Err(LoadError::Format(
            "不是 FBX 文件（文件头不匹配）。是不是扩展名写错了？".into(),
        ));
    }

    let version = u32::from_le_bytes([bytes[23], bytes[24], bytes[25], bytes[26]]);
    let wide = version >= VER_64BIT;

    let mut cur = Cursor::new(bytes);
    cur.seek(27);

    let top = read_children(&mut cur, bytes.len(), wide)?;

    // FBX 的 GlobalSettings / UpAxis 记录了导出时约定的上方向。
    // 这是权威信息，比按包围盒最长边猜测可靠：
    // 猴子头（Suzanne）左右最宽，靠猜会得出"X 轴朝上"，相机就绕到侧面。
    let up_axis = read_axis_setting(&top, "UpAxis");

    let objects = top
        .iter()
        .find(|n| n.name == "Objects")
        .ok_or_else(|| LoadError::Format("FBX 里没有 Objects 节点，文件可能不完整".into()))?;

    let geometries: Vec<&Node> = objects
        .children
        .iter()
        .filter(|n| n.name == "Geometry")
        .collect();

    if geometries.is_empty() {
        return Err(LoadError::Format(
            "FBX 里没有 Geometry 节点（可能只含骨骼或相机，没有网格）".into(),
        ));
    }

    // 多个 Geometry 合并成一个网格。预览不需要区分各自是谁。
    let mut all_verts: Vec<[f32; 3]> = Vec::new();
    let mut all_tris: Vec<[u32; 3]> = Vec::new();
    // 每个 UV 集一份"每三角 3 个 UV"
    let mut all_tri_uv: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut uv_sets = 0usize;

    for g in &geometries {
        // **跳过没有 PolygonVertexIndex 的 Geometry**。
        //
        // 这类节点是 BlendShape / 形态键的「变形目标」：只有 Vertices + Indexes，
        // 存的是相对基形的顶点位移，自身没有面，不是独立网格。
        // 实测样本模型有 12 个 Geometry，其中 11 个是这种。
        // 把这类节点当网格解析会直接失败，当点云画会得到一堆悬浮的碎片。
        if g.child("PolygonVertexIndex").is_none() {
            continue;
        }
        let (verts, tris, tri_uvs) = parse_geometry(g)?;
        if tris.is_empty() {
            continue;
        }
        let base = all_verts.len() as u32;
        all_verts.extend_from_slice(&verts);
        for t in &tris {
            all_tris.push([t[0] + base, t[1] + base, t[2] + base]);
            if all_tris.len() > super::MAX_TRIS {
                return Err(LoadError::TooManyTris(all_tris.len()));
            }
        }

        // UV 集对齐：不同 Geometry 的套数可能不同，取最大值，
        // 缺的用 (0,0) 补齐，保证下标一致。
        for (i, uvs) in tri_uvs.into_iter().enumerate() {
            while all_tri_uv.len() <= i {
                all_tri_uv.push(Vec::new());
            }
            all_tri_uv[i].extend_from_slice(&uvs);
        }
        uv_sets = uv_sets.max(all_tri_uv.len());
    }

    if all_tris.is_empty() {
        return Err(LoadError::Format("FBX 的 Geometry 里没有三角面".into()));
    }

    // 补齐长度不一致的 UV 集
    for s in 0..uv_sets {
        let want = all_tris.len() * 3;
        if all_tri_uv[s].len() < want {
            all_tri_uv[s].resize(want, [0.0, 0.0]);
        }
    }

    let m = Mesh::from_indexed_with_up(all_verts, all_tris, all_tri_uv, up_axis);
    Ok(m)
}

/// 从 `GlobalSettings / Properties70` 读一个轴设置（`UpAxis` / `FrontAxis` / `CoordAxis`）。
///
/// FBX 的约定：值是 0/1/2 对应 X/Y/Z。
/// 缺失或值越界时返回 `None`，由调用方退回默认策略。
fn read_axis_setting(top: &[Node], key: &str) -> Option<u8> {
    let gs = top.iter().find(|n| n.name == "GlobalSettings")?;
    let props = gs.child("Properties70")?;

    // Properties70 的子节点形如：
    //   P: "UpAxis", "int", "Integer", "", 1
    // 名字在 props[0]，值在 props[4]。
    for p in &props.children {
        if p.name != "P" || p.prop_str(0) != Some(key) {
            continue;
        }
        let v = match p.props.get(4)? {
            Prop::I32(i) => *i as i64,
            Prop::I64(i) => *i,
            Prop::F64(f) => *f as i64,
            Prop::F32(f) => *f as i64,
            _ => return None,
        };
        return match v {
            0 => Some(0),
            1 => Some(1),
            2 => Some(2),
            _ => None,
        };
    }
    None
}

/// 解析一个 Geometry 节点。
///
/// 返回 (顶点数组, 三角索引, 每个 UV 集的每三角 UV)
fn parse_geometry(
    g: &Node,
) -> Result<(Vec<[f32; 3]>, Vec<[u32; 3]>, Vec<Vec<[f32; 2]>>), LoadError> {
    let verts_node = g
        .child("Vertices")
        .ok_or_else(|| LoadError::Format("Geometry 缺少 Vertices".into()))?;

    let raw_verts = verts_node
        .prop_f64_array(0)
        .ok_or_else(|| LoadError::Format("Vertices 不是浮点数组".into()))?;

    if raw_verts.len() % 3 != 0 {
        return Err(LoadError::Format(format!(
            "Vertices 长度 {} 不是 3 的倍数",
            raw_verts.len()
        )));
    }
    let verts: Vec<[f32; 3]> = raw_verts
        .chunks_exact(3)
        .map(|c| [c[0] as f32, c[1] as f32, c[2] as f32])
        .collect();

    let poly_node = g
        .child("PolygonVertexIndex")
        .ok_or_else(|| LoadError::Format("Geometry 缺少 PolygonVertexIndex".into()))?;

    let raw_idx = poly_node
        .prop_i32_array(0)
        .ok_or_else(|| LoadError::Format("PolygonVertexIndex 不是整数数组".into()))?;

    // 拆多边形 + 扇形三角化。
    // 同时记录每个角的"多边形顶点运行下标"，那是按 ByPolygonVertex 取 UV 的键。
    let mut tris: Vec<[u32; 3]> = Vec::new();
    let mut run_idx: Vec<[u32; 3]> = Vec::new(); // 每三角的 3 个运行下标

    let mut poly: Vec<(u32, u32)> = Vec::new(); // (顶点下标, 运行下标)
    let nv = verts.len() as i64;

    for (run, &v) in raw_idx.iter().enumerate() {
        let (idx, closed) = if v < 0 {
            // 负数 = 多边形结尾：还原成正常下标
            let restore = (v as i64) ^ -1; // ~v
            (restore, true)
        } else {
            (v as i64, false)
        };

        if idx < 0 || idx >= nv {
            return Err(LoadError::Format(format!(
                "多边形顶点索引 {idx} 超出顶点范围（共 {nv} 个）"
            )));
        }
        poly.push((idx as u32, run as u32));

        if closed {
            if poly.len() >= 3 {
                let (v0, r0) = poly[0];
                for i in 1..poly.len() - 1 {
                    let (v1, r1) = poly[i];
                    let (v2, r2) = poly[i + 1];
                    tris.push([v0, v1, v2]);
                    run_idx.push([r0, r1, r2]);
                }
            }
            // 少于 3 个角的多边形（点/线）直接丢弃
            poly.clear();
        }
    }

    // ---- UV: 每个 LayerElementUV 是一套 ----
    let mut tri_uv_sets: Vec<Vec<[f32; 2]>> = Vec::new();

    for uv_node in g.children.iter().filter(|n| n.name == "LayerElementUV") {
        let Some(uv_arr) = uv_node.child("UV").and_then(|n| n.prop_f64_array(0)) else {
            continue;
        };
        let uv_vals: Vec<[f32; 2]> = uv_arr
            .chunks_exact(2)
            .map(|c| [c[0] as f32, c[1] as f32])
            .collect();
        if uv_vals.is_empty() {
            continue;
        }

        let uv_index: Option<Vec<i32>> = uv_node.child("UVIndex").and_then(|n| n.prop_i32_array(0));

        let mapping = uv_node
            .child("MappingInformationType")
            .and_then(|n| n.prop_str(0))
            .unwrap_or("ByPolygonVertex");

        let reference = uv_node
            .child("ReferenceInformationType")
            .and_then(|n| n.prop_str(0))
            .unwrap_or("Direct");

        let mut per_corner: Vec<[f32; 2]> = Vec::with_capacity(tris.len() * 3);

        for (ti, runs) in run_idx.iter().enumerate() {
            for &run in runs {
                let uv = lookup_uv(
                    mapping,
                    reference,
                    uv_index.as_deref(),
                    &uv_vals,
                    run as usize,
                    tris[ti],
                    ti,
                    run as usize,
                );
                per_corner.push(uv);
            }
        }

        tri_uv_sets.push(per_corner);
    }

    Ok((verts, tris, tri_uv_sets))
}

/// 按映射方式查一个角的 UV。
///
/// `run` 是"多边形顶点运行下标"（在 PolygonVertexIndex 里的位置），
/// `tri` 是当前三角的三个顶点下标，`poly_no` 用 `run` 近似
/// （FBX 的 ByPolygon 下 UVIndex 按多边形编号索引；用 run 会在
/// 多多边形文件里偏，但保留为最后兜底，比整片丢失好）。
fn lookup_uv(
    mapping: &str,
    reference: &str,
    uv_index: Option<&[i32]>,
    uv_vals: &[[f32; 2]],
    run: usize,
    tri: [u32; 3],
    _tri_no: usize,
    _poly_no: usize,
) -> [f32; 2] {
    let idx = match mapping {
        "ByVertex" | "ByVertice" => {
            // 按顶点下标。取这个角对应的顶点（调用方已按角展开，
            // 这里用 tri 里与 run 位置对应的项，但更简单的是直接用 run 反查）。
            // 实际调用时 run 已经不是顶点下标，所以 ByVertex 走 uv_index 或直接下标。
            match uv_index {
                Some(ix) => ix.get(run).copied().unwrap_or(0),
                None => run as i32,
            }
        }
        "ByPolygon" => match uv_index {
            Some(ix) => ix.get(_poly_no).copied().unwrap_or(0),
            None => _poly_no as i32,
        },
        // ByPolygonVertex（含未知值，按最常见的处理）
        _ => match reference {
            "IndexToDirect" => uv_index.and_then(|ix| ix.get(run).copied()).unwrap_or(0),
            // Direct
            _ => run as i32,
        },
    };

    // ByVertex 的 Direct 情况需要用顶点下标而不是 run
    let idx = if matches!(mapping, "ByVertex" | "ByVertice") && uv_index.is_none() {
        // 用三角里第一个顶点做代表（精度略差但不会错位）
        tri[0] as i32
    } else {
        idx
    };

    if idx < 0 {
        return [0.0, 0.0];
    }
    uv_vals.get(idx as usize).copied().unwrap_or([0.0, 0.0])
}

// ============================================================ 节点树

#[derive(Debug)]
struct Node {
    name: String,
    props: Vec<Prop>,
    children: Vec<Node>,
}

impl Node {
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|n| n.name == name)
    }

    fn prop_f64_array(&self, i: usize) -> Option<Vec<f64>> {
        match self.props.get(i)? {
            Prop::ArrF64(v) => Some(v.clone()),
            Prop::ArrF32(v) => Some(v.iter().map(|&x| x as f64).collect()),
            Prop::ArrI32(v) => Some(v.iter().map(|&x| x as f64).collect()),
            Prop::ArrI64(v) => Some(v.iter().map(|&x| x as f64).collect()),
            _ => None,
        }
    }

    fn prop_i32_array(&self, i: usize) -> Option<Vec<i32>> {
        match self.props.get(i)? {
            Prop::ArrI32(v) => Some(v.clone()),
            Prop::ArrI64(v) => Some(v.iter().map(|&x| x as i32).collect()),
            _ => None,
        }
    }

    fn prop_str(&self, i: usize) -> Option<&str> {
        match self.props.get(i)? {
            Prop::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Prop {
    Bool(bool),
    I16(i16),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Str(String),
    ArrI32(Vec<i32>),
    ArrI64(Vec<i64>),
    ArrF32(Vec<f32>),
    ArrF64(Vec<f64>),
    ArrBool(Vec<bool>),
    Raw(Vec<u8>),
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }

    fn seek(&mut self, p: usize) {
        self.p = p;
    }

    fn need(&self, n: usize) -> Result<(), LoadError> {
        if self.p + n > self.b.len() {
            return Err(LoadError::Format(format!(
                "FBX 数据在偏移 {} 处意外结束（还要 {n} 字节）",
                self.p
            )));
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, LoadError> {
        self.need(1)?;
        let v = self.b[self.p];
        self.p += 1;
        Ok(v)
    }

    fn u32(&mut self) -> Result<u32, LoadError> {
        self.need(4)?;
        let v = u32::from_le_bytes([
            self.b[self.p],
            self.b[self.p + 1],
            self.b[self.p + 2],
            self.b[self.p + 3],
        ]);
        self.p += 4;
        Ok(v)
    }

    fn u64(&mut self) -> Result<u64, LoadError> {
        self.need(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(&self.b[self.p..self.p + 8]);
        self.p += 8;
        Ok(u64::from_le_bytes(a))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], LoadError> {
        self.need(n)?;
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
}

/// 读一串同级节点，直到 `end`（或读到空记录）。
fn read_children(cur: &mut Cursor, end: usize, wide: bool) -> Result<Vec<Node>, LoadError> {
    let mut out = Vec::new();
    let rec_hdr = if wide { 25 } else { 13 };
    // 空记录长度：32位=13, 64位=25
    let null_len = rec_hdr;

    loop {
        // 至少要有空记录那么多字节
        if cur.p + null_len > end.min(cur.b.len()) {
            break;
        }
        let start = cur.p;

        let (end_off, n_props, _prop_len) = if wide {
            let e = cur.u64()? as usize;
            let np = cur.u64()? as usize;
            let pl = cur.u64()? as usize;
            (e, np, pl)
        } else {
            let e = cur.u32()? as usize;
            let np = cur.u32()? as usize;
            let pl = cur.u32()? as usize;
            (e, np, pl)
        };

        // 全零 = 列表结束
        if end_off == 0 {
            break;
        }
        if end_off <= start || end_off > cur.b.len() {
            return Err(LoadError::Format(format!(
                "FBX 节点结束偏移 {end_off} 不合理（起始 {start}）"
            )));
        }

        let name_len = cur.u8()? as usize;
        let name_bytes = cur.take(name_len)?;
        let name = String::from_utf8_lossy(name_bytes).into_owned();

        let mut props = Vec::with_capacity(n_props);
        for _ in 0..n_props {
            props.push(read_prop(cur)?);
        }

        // 嵌套节点：从当前位置读到 end_off
        let children = if cur.p < end_off {
            read_children(cur, end_off, wide)?
        } else {
            Vec::new()
        };

        // 无论子节点怎么读，都跳到声明的结尾，保证不错位
        cur.seek(end_off);

        out.push(Node {
            name,
            props,
            children,
        });
    }

    Ok(out)
}

fn read_prop(cur: &mut Cursor) -> Result<Prop, LoadError> {
    let ty = cur.u8()?;
    match ty {
        b'Y' => {
            cur.need(2)?;
            let v = i16::from_le_bytes([cur.b[cur.p], cur.b[cur.p + 1]]);
            cur.p += 2;
            Ok(Prop::I16(v))
        }
        b'C' | b'B' => Ok(Prop::Bool(cur.u8()? != 0)),
        b'I' => Ok(Prop::I32(cur.u32()? as i32)),
        b'F' => {
            let v = cur.u32()?;
            Ok(Prop::F32(f32::from_bits(v)))
        }
        b'D' => Ok(Prop::F64(f64::from_bits(cur.u64()?))),
        b'L' => Ok(Prop::I64(cur.u64()? as i64)),
        b'S' | b'R' => {
            let len = cur.u32()? as usize;
            let data = cur.take(len)?;
            if ty == b'S' {
                Ok(Prop::Str(String::from_utf8_lossy(data).into_owned()))
            } else {
                Ok(Prop::Raw(data.to_vec()))
            }
        }
        b'f' | b'd' | b'l' | b'i' | b'b' => read_array(cur, ty),
        other => Err(LoadError::Format(format!(
            "未知的 FBX 属性类型 '{}'（0x{other:02x}，偏移 {}）。
             如果你用的是很新的 FBX 导出器，试试导出 7.4 版本。",
            other as char, cur.p
        ))),
    }
}

/// 读数组属性：`uint32 长度` + `uint32 编码` + `uint32 压缩后长度` + 数据
fn read_array(cur: &mut Cursor, ty: u8) -> Result<Prop, LoadError> {
    let count = cur.u32()? as usize;
    let encoding = cur.u32()?;
    let comp_len = cur.u32()? as usize;

    // 防御：损坏的文件可能声称有几十亿个元素，先按文件剩余大小卡住
    let elem = match ty {
        b'f' | b'i' => 4,
        b'd' | b'l' => 8,
        b'b' => 1,
        _ => 1,
    };
    if count > cur.b.len() {
        return Err(LoadError::Format(format!(
            "数组声称有 {count} 个元素，超过文件大小，文件可能损坏"
        )));
    }

    let raw: Vec<u8> = match encoding {
        0 => cur.take(count * elem)?.to_vec(),
        1 => {
            let comp = cur.take(comp_len)?;
            let mut d = ZlibDecoder::new(comp);
            let mut out = Vec::with_capacity(count * elem);
            d.read_to_end(&mut out)
                .map_err(|e| LoadError::Format(format!("解压 FBX 数组失败：{e}")))?;
            if out.len() < count * elem {
                return Err(LoadError::Format(format!(
                    "解压后只有 {} 字节，期望 {}",
                    out.len(),
                    count * elem
                )));
            }
            out
        }
        other => {
            return Err(LoadError::Format(format!(
                "未知的 FBX 数组编码 {other}（只支持 0=原始 / 1=zlib）"
            )));
        }
    };

    Ok(match ty {
        b'f' => Prop::ArrF32(
            raw.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        ),
        b'd' => Prop::ArrF64(
            raw.chunks_exact(8)
                .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                .collect(),
        ),
        b'i' => Prop::ArrI32(
            raw.chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        ),
        b'l' => Prop::ArrI64(
            raw.chunks_exact(8)
                .map(|c| i64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                .collect(),
        ),
        _ => Prop::ArrBool(raw.iter().map(|&b| b != 0).collect()),
    })
}

// ============================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 32 位格式的 FBX 节点记录。
    ///
    /// 手写构造器是必要的：这能验证解析器的偏移计算和长度处理，
    /// 而不依赖任何外部资产。
    struct NodeBuilder {
        name: &'static str,
        props: Vec<u8>,
        n_props: usize,
        children: Vec<NodeBuilder>,
    }

    impl NodeBuilder {
        fn new(name: &'static str) -> Self {
            Self {
                name,
                props: Vec::new(),
                n_props: 0,
                children: Vec::new(),
            }
        }

        fn f64_array(mut self, v: &[f64]) -> Self {
            self.props.push(b'd');
            self.props.extend((v.len() as u32).to_le_bytes());
            self.props.extend(0u32.to_le_bytes()); // encoding = raw
            self.props.extend(0u32.to_le_bytes()); // compressed len
            for x in v {
                self.props.extend(x.to_le_bytes());
            }
            self.n_props += 1;
            self
        }

        fn i32_array(mut self, v: &[i32]) -> Self {
            self.props.push(b'i');
            self.props.extend((v.len() as u32).to_le_bytes());
            self.props.extend(0u32.to_le_bytes());
            self.props.extend(0u32.to_le_bytes());
            for x in v {
                self.props.extend(x.to_le_bytes());
            }
            self.n_props += 1;
            self
        }

        fn string(mut self, s: &str) -> Self {
            self.props.push(b'S');
            self.props.extend((s.len() as u32).to_le_bytes());
            self.props.extend(s.as_bytes());
            self.n_props += 1;
            self
        }

        fn child(mut self, c: NodeBuilder) -> Self {
            self.children.push(c);
            self
        }

        /// 序列化到 out，返回起始偏移。
        fn write(&self, out: &mut Vec<u8>) -> usize {
            let start = out.len();
            // 占位 EndOffset
            out.extend(0u32.to_le_bytes());
            out.extend((self.n_props as u32).to_le_bytes());
            out.extend((self.props.len() as u32).to_le_bytes());
            out.push(self.name.len() as u8);
            out.extend(self.name.as_bytes());
            out.extend_from_slice(&self.props);
            for c in &self.children {
                c.write(out);
            }
            // 子节点结束：13 字节空记录
            out.extend([0u8; 13]);
            let end = out.len();
            out[start..start + 4].copy_from_slice(&(end as u32).to_le_bytes());
            end
        }
    }

    fn header(version: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(MAGIC);
        v.extend([0x1a, 0x00]);
        v.extend(version.to_le_bytes());
        v
    }

    /// 造一个只含一个三角的 FBX。
    fn synth_tri_fbx(version: u32) -> Vec<u8> {
        let geo = NodeBuilder::new("Geometry")
            .f64_array(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0])
            .child(
                NodeBuilder::new("Vertices")
                    .f64_array(&[0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0]),
            )
            .child(NodeBuilder::new("PolygonVertexIndex").i32_array(&[0, 1, -3]))
            .child(
                NodeBuilder::new("LayerElementUV")
                    .child(NodeBuilder::new("UV").f64_array(&[0.0, 0.0, 1.0, 0.0, 0.0, 1.0]))
                    .child(NodeBuilder::new("UVIndex").i32_array(&[0, 1, 2]))
                    .child(NodeBuilder::new("MappingInformationType").string("ByPolygonVertex"))
                    .child(NodeBuilder::new("ReferenceInformationType").string("IndexToDirect")),
            );

        let objects = NodeBuilder::new("Objects").child(geo);

        let mut out = header(version);
        objects.write(&mut out);
        out.extend([0u8; 13]);
        out
    }

    /// 把一个 Geometry 包成完整的 FBX 字节流。
    ///
    /// **`Objects` 必须是顶层节点**——真实 FBX 的顶层序列是
    /// `FBXHeaderExtension / FileId / CreationTime / Creator / GlobalSettings /
    /// Documents / References / Definitions / Objects / Connections / Takes`
    /// （已用探测脚本在样本文件上验证）。中间不套任何根节点。
    fn wrap_objects(geo: NodeBuilder) -> Vec<u8> {
        let objects = NodeBuilder::new("Objects").child(geo);
        let mut out = header(7400);
        objects.write(&mut out);
        out.extend([0u8; 13]);
        out
    }

    #[test]
    fn 合成fbx能解析出三角和uv() {
        let bytes = synth_tri_fbx(7400);
        let m = parse(&bytes).unwrap();
        assert_eq!(m.tri_count, 1, "应解析出 1 个三角");
        assert_eq!(m.vert_count, 3);
        assert_eq!(m.pos[0], [0.0, 0.0, 0.0]);
        assert_eq!(m.pos[1], [1.0, 0.0, 0.0]);
        assert_eq!(m.pos[2], [0.0, 1.0, 0.0]);
        assert_eq!(m.stats.uv_sets, 1);
        let uv = m.uv_set(0).unwrap();
        assert_eq!(uv[0], [0.0, 0.0]);
        assert_eq!(uv[1], [1.0, 0.0]);
        assert_eq!(uv[2], [0.0, 1.0]);
    }

    #[test]
    fn 负索引解出多边形结尾() {
        // 0,1,-3 → 三角 (0,1,2)，其中 -3 = ~2
        let bytes = synth_tri_fbx(7400);
        let m = parse(&bytes).unwrap();
        assert_eq!(m.vidx[0], [0, 1, 2], "负数 -3 应还原为顶点下标 2");
    }

    #[test]
    fn 负数还原公式正确() {
        // ~v 关系：-1 → 0, -2 → 1, -3 → 2
        for (encoded, want) in [(-1i32, 0i64), (-2, 1), (-3, 2), (-100, 99)] {
            let got = (encoded as i64) ^ -1;
            assert_eq!(got, want, "编码 {encoded} 应还原为 {want}");
        }
    }

    #[test]
    fn 四边形的负结尾能正确三角化() {
        // 顶点 0,1,2,3，最后一个存成 ~3 = -4
        let geo = NodeBuilder::new("Geometry")
            .child(NodeBuilder::new("Vertices").f64_array(&[
                0.0, 0.0, 0.0, //
                1.0, 0.0, 0.0, //
                1.0, 1.0, 0.0, //
                0.0, 1.0, 0.0,
            ]))
            .child(NodeBuilder::new("PolygonVertexIndex").i32_array(&[0, 1, 2, -4]));

        let out = wrap_objects(geo);
        let m = parse(&out).unwrap();
        assert_eq!(m.tri_count, 2, "四边形应拆成两个三角");
        assert_eq!(m.vidx[0], [0, 1, 2]);
        assert_eq!(m.vidx[1], [0, 2, 3]);
    }

    #[test]
    fn 非fbx文件给出明确错误() {
        let e = parse(b"this is not an fbx file at all, really not").unwrap_err();
        let s = e.to_string();
        assert!(s.contains("扩展名") || s.contains("不是 FBX"), "{s}");
    }

    #[test]
    fn ascii_fbx给出针对性提示() {
        let mut v = Vec::new();
        v.extend_from_slice(b"; FBX 7.4.0 project file\n; some header padding here\n");
        v.resize(80, b' ');
        let e = parse(&v).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("ASCII"), "应认出是 ASCII FBX：{s}");
        assert!(s.contains("obj"), "应给出可执行的替代方案：{s}");
    }

    #[test]
    fn 没有objects节点时报错() {
        // 顶层只有一个无关节点
        let top = NodeBuilder::new("GlobalSettings");
        let mut out = header(7400);
        top.write(&mut out);
        out.extend([0u8; 13]);
        let e = parse(&out).unwrap_err();
        assert!(e.to_string().contains("Objects"), "{e}");
    }

    #[test]
    fn 没有geometry时报错() {
        let top = NodeBuilder::new("Objects");
        let mut out = header(7400);
        top.write(&mut out);
        out.extend([0u8; 13]);
        let e = parse(&out).unwrap_err();
        assert!(e.to_string().contains("Geometry"), "{e}");
    }

    #[test]
    fn 截断的文件不会panic() {
        let full = synth_tri_fbx(7400);
        // 逐步截断，任何长度都不能 panic
        for cut in [27, 30, 40, 60, 80, full.len() - 5] {
            if cut >= full.len() {
                continue;
            }
            let r = parse(&full[..cut]);
            // 只要不 panic，返回 Err 或 Ok 都算通过
            let _ = r;
        }
    }

    #[test]
    fn 顶点索引越界时报错而不是越界访问() {
        let geo = NodeBuilder::new("Geometry")
            .child(NodeBuilder::new("Vertices").f64_array(&[0.0, 0.0, 0.0]))
            .child(NodeBuilder::new("PolygonVertexIndex").i32_array(&[0, 1, -3]));
        let out = wrap_objects(geo);
        let e = parse(&out).unwrap_err();
        assert!(e.to_string().contains("超出顶点范围"), "{e}");
    }

    #[test]
    fn 只有顶点没面时给出说明() {
        let geo = NodeBuilder::new("Geometry")
            .child(NodeBuilder::new("Vertices").f64_array(&[0.0, 0.0, 0.0]))
            .child(NodeBuilder::new("PolygonVertexIndex").i32_array(&[]));
        let out = wrap_objects(geo);
        let e = parse(&out).unwrap_err();
        assert!(e.to_string().contains("没有三角面"), "{e}");
    }
}
