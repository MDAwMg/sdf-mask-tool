//! Wavefront OBJ 解析。
//!
//! 只关心几何和 UV：`v` / `vt` / `f`。材质、法线、分组一律忽略
//! （工具只做预览，不处理材质）。
//!
//! 面支持三种写法，这是 OBJ 最容易写错的地方：
//! - `f v`              只要顶点
//! - `f v/vt`           顶点 + UV
//! - `f v/vt/vn`        顶点 + UV + 法线
//! 以及负索引（`-1` = 最后一个顶点）。虽然实测样本全是正索引，
//! 但 Blender/Maya 导出的文件里负索引很常见，不支持就会静默出错。
//!
//! 多边形（四边及以上）用扇形三角化：`(0, i, i+1)`。
//! 对凸多边形正确，对凹多边形会出错——但 OBJ 里凹多边形本来就不是
//! 合法用法（导出器都先三角化），不为该情况增加复杂度。

use std::io::{BufRead, BufReader};
use std::path::Path;

use super::mesh::{LoadError, Mesh};

/// 解析过程中的原始缓冲。
struct Raw {
    verts: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    /// 每三角 3 个顶点下标
    tris: Vec<[u32; 3]>,
    /// 每三角 3 个 UV 下标（没有 UV 时为空）
    tri_uv_idx: Vec<[u32; 3]>,
    has_uv: bool,
}

pub fn load(path: &Path) -> Result<Mesh, LoadError> {
    let t0 = std::time::Instant::now();

    let f = std::fs::File::open(path).map_err(|e| LoadError::Io(e.to_string()))?;
    let mut r = BufReader::with_capacity(1 << 20, f);

    let mut raw = Raw {
        verts: Vec::new(),
        uvs: Vec::new(),
        tris: Vec::new(),
        tri_uv_idx: Vec::new(),
        has_uv: false,
    };

    // 逐行读。OBJ 是文本，用 BufRead::read_line 而不是 lines()，
    // 因为 lines() 遇到非 UTF-8 会直接报错，而 OBJ 注释里可能有不规范字节。
    let mut line = String::with_capacity(256);
    let mut lineno = 0usize;
    loop {
        line.clear();
        let n = r
            .read_line(&mut line)
            .map_err(|e| LoadError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        lineno += 1;
        let s = line.trim();

        // 跳过空行、注释、以及所有不认识的指令
        if s.is_empty() || s.starts_with('#') {
            continue;
        }

        let mut it = s.split_whitespace();
        let Some(tag) = it.next() else { continue };

        match tag {
            "v" => {
                let (x, y, z) = parse3(&mut it, lineno, "v")?;
                raw.verts.push([x, y, z]);
            }
            "vt" => {
                // vt 可以只有 u，也可能带 w。只取前两个。
                let u = parse1(&mut it, lineno, "vt")?;
                let v = it.next().and_then(|t| t.parse::<f32>().ok()).unwrap_or(0.0);
                raw.uvs.push([u, v]);
                raw.has_uv = true;
            }
            "f" => {
                parse_face(&mut it, &mut raw, lineno)?;
                if raw.tris.len() > super::MAX_TRIS {
                    return Err(LoadError::TooManyTris(raw.tris.len()));
                }
            }
            // vn / o / g / s / usemtl / mtllib / l / p 等一律忽略
            _ => {}
        }
    }

    if raw.tris.is_empty() {
        return Err(LoadError::Format(
            "文件里没有可用的面（f 指令）。是不是只导出了顶点？".into(),
        ));
    }
    if raw.verts.is_empty() {
        return Err(LoadError::Format("文件里没有顶点（v 指令）".into()));
    }

    // UV 摊平：把"每三角 3 个 UV 下标"变成"每三角 3 个 UV 值"。
    // 没写 vt 的面补 (0,0)——这样渲染时会采样到纹理角上而不是崩。
    let mut uv_sets: Vec<Vec<[f32; 2]>> = Vec::new();
    if raw.has_uv && !raw.uvs.is_empty() {
        let mut tri_uv: Vec<[f32; 2]> = Vec::with_capacity(raw.tris.len() * 3);
        for t in &raw.tri_uv_idx {
            for &i in t {
                tri_uv.push(raw.uvs.get(i as usize).copied().unwrap_or([0.0, 0.0]));
            }
        }
        uv_sets.push(tri_uv);
    }

    let mut m = Mesh::from_indexed(raw.verts, raw.tris, uv_sets);
    m.stats.parse_ms = t0.elapsed().as_secs_f32() * 1000.0;
    Ok(m)
}

/// 解析一条 `f` 指令，按角数决定三角化方式。
fn parse_face<'a, I: Iterator<Item = &'a str>>(
    it: &mut I,
    raw: &mut Raw,
    lineno: usize,
) -> Result<(), LoadError> {
    // 先把这一行的所有角收集起来（最多几百个，小分配可接受）
    let mut corners: Vec<(u32, u32)> = Vec::with_capacity(8); // (v, vt)
    for tok in it {
        let (vi, vti) = parse_corner(tok, raw.verts.len(), raw.uvs.len(), lineno)?;
        corners.push((vi, vti));
    }

    if corners.len() < 3 {
        return Err(LoadError::Format(format!(
            "第 {lineno} 行的面只有 {} 个顶点，无法构成三角面",
            corners.len()
        )));
    }

    // 扇形三角化
    let (v0, t0) = corners[0];
    for i in 1..corners.len() - 1 {
        let (v1, t1) = corners[i];
        let (v2, t2) = corners[i + 1];
        raw.tris.push([v0, v1, v2]);
        raw.tri_uv_idx.push([t0, t1, t2]);
    }
    Ok(())
}

/// 解析一个面角 `v` / `v/vt` / `v/vt/vn`，返回 (顶点下标, UV 下标)。
///
/// 负索引按 OBJ 规范解释：`-1` 是"最后一个"。
fn parse_corner(
    tok: &str,
    n_verts: usize,
    n_uvs: usize,
    lineno: usize,
) -> Result<(u32, u32), LoadError> {
    let mut parts = tok.split('/');

    let vs = parts.next().unwrap_or("");
    let vi = resolve_index(vs, n_verts, lineno)?;

    // 第二部分可能是 UV，也可能空（`v//vn`）
    let uv_part = parts.next().unwrap_or("");
    let vti = if uv_part.is_empty() {
        0
    } else {
        resolve_index(uv_part, n_uvs, lineno)?
    };

    // 第三部分（法线）忽略
    Ok((vi, vti))
}

/// 把 OBJ 索引字符串（1 基，可为负）转成 0 基下标，并校验范围。
fn resolve_index(s: &str, count: usize, lineno: usize) -> Result<u32, LoadError> {
    let n: i64 = s
        .parse()
        .map_err(|_| LoadError::Format(format!("第 {lineno} 行的索引 '{s}' 不是整数")))?;

    let idx = if n > 0 {
        n - 1
    } else if n < 0 {
        // 负索引：-1 = 最后一个
        count as i64 + n
    } else {
        return Err(LoadError::Format(format!(
            "第 {lineno} 行出现索引 0（OBJ 索引从 1 开始）"
        )));
    };

    if idx < 0 || idx >= count as i64 {
        return Err(LoadError::Format(format!(
            "第 {lineno} 行的索引 {n} 超出范围（当前有 {count} 个）"
        )));
    }
    Ok(idx as u32)
}

fn parse1<'a, I: Iterator<Item = &'a str>>(
    it: &mut I,
    lineno: usize,
    tag: &str,
) -> Result<f32, LoadError> {
    it.next()
        .and_then(|t| t.parse::<f32>().ok())
        .ok_or_else(|| LoadError::Format(format!("第 {lineno} 行的 {tag} 缺少数值")))
}

fn parse3<'a, I: Iterator<Item = &'a str>>(
    it: &mut I,
    lineno: usize,
    tag: &str,
) -> Result<(f32, f32, f32), LoadError> {
    let x = parse1(it, lineno, tag)?;
    let y = parse1(it, lineno, tag)?;
    let z = parse1(it, lineno, tag)?;
    Ok((x, y, z))
}

// ============================================================ 测试

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 写一个临时 obj，返回路径。
    ///
    /// **不用 `std::env::temp_dir()`**：本机沙箱下写系统临时目录会被拒绝
    /// （`Os { code: 5, PermissionDenied }`）。`target/` 是 cargo 自己的
    /// 输出目录，一定可写，且跟着 `cargo clean` 一起清理。
    fn tmp_obj(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("obj_test_tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn 最小obj_单三角() {
        let p = tmp_obj("tri.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n");
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 1);
        assert_eq!(m.vert_count, 3);
        assert_eq!(m.stats.uv_sets, 0, "没有 vt 就不该造 UV 集");
        assert_eq!(m.pos[0], [0.0, 0.0, 0.0]);
        assert_eq!(m.pos[2], [0.0, 1.0, 0.0]);
    }

    #[test]
    fn 带uv的obj() {
        let p = tmp_obj(
            "uv.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nf 1/1 2/2 3/3\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 1);
        assert_eq!(m.stats.uv_sets, 1);
        let uv = m.uv_set(0).unwrap();
        assert_eq!(uv[0], [0.0, 0.0]);
        assert_eq!(uv[1], [1.0, 0.0]);
        assert_eq!(uv[2], [0.0, 1.0]);
    }
    #[test]
    fn 带法线的vtn格式() {
        let p = tmp_obj(
            "vtn.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0 0\nvt 1 0\nvt 0 1\nvn 0 0 1\nf 1/1/1 2/2/1 3/3/1\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 1);
        assert_eq!(m.stats.uv_sets, 1, "v/vt/vn 格式的 UV 不能丢");
        assert_eq!(m.uv_set(0).unwrap()[1], [1.0, 0.0]);
    }

    #[test]
    fn 双斜线格式_只有顶点和法线() {
        let p = tmp_obj(
            "v_vn.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nvn 0 0 1\nf 1//1 2//1 3//1\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 1, "v//vn 也必须能解析");
        assert_eq!(m.stats.uv_sets, 0, "空 UV 位不该造出 UV 集");
    }

    #[test]
    fn 四边形被扇形三角化() {
        let p = tmp_obj(
            "quad.obj",
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3 4\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 2, "四边形应拆成 2 个三角");
        assert_eq!(m.vidx[0], [0, 1, 2]);
        assert_eq!(m.vidx[1], [0, 2, 3]);
    }

    #[test]
    fn 五边形拆成三个三角() {
        let p = tmp_obj(
            "pent.obj",
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0.5 2 0\nv 0 1 0\nf 1 2 3 4 5\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 3);
        assert_eq!(m.vidx[2], [0, 3, 4]);
    }

    #[test]
    fn 负索引按倒数解释() {
        // -1 = 最后一个顶点
        let p = tmp_obj("neg.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf -3 -2 -1\n");
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 1);
        assert_eq!(m.vidx[0], [0, 1, 2], "负索引应解析成 0,1,2");
    }

    #[test]
    fn 越界索引给出带行号的错误() {
        let p = tmp_obj("oob.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 99\n");
        let e = load(&p).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("4"), "错误应指出行号：{s}");
        assert!(s.contains("超出范围"), "{s}");
    }

    #[test]
    fn 索引0被判为非法() {
        let p = tmp_obj("zero.obj", "v 0 0 0\nv 1 0 0\nv 0 1 0\nf 0 1 2\n");
        let e = load(&p).unwrap_err();
        assert!(e.to_string().contains("从 1 开始"), "{e}");
    }

    #[test]
    fn 没有面时报错而不是静默返回空() {
        let p = tmp_obj("noface.obj", "v 0 0 0\nv 1 0 0\n");
        let e = load(&p).unwrap_err();
        assert!(e.to_string().contains("没有可用的面"), "{e}");
    }

    #[test]
    fn 注释空行和未知指令被忽略() {
        let p = tmp_obj(
            "mixed.obj",
            "# Blender 导出\no Cube\nmtllib x.mtl\ng grp\nusemtl mat\nv 0 0 0\nv 1 0 0\nv 0 1 0\ns off\nf 1 2 3\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 1);
    }

    #[test]
    fn 混合三角和四边形() {
        let p = tmp_obj(
            "mix.obj",
            "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nf 1 2 3\nf 1 2 3 4\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.tri_count, 3, "1 个三角 + 1 个四边(2 三角) = 3");
    }

    #[test]
    fn 只有一个分量的vt也能读() {
        let p = tmp_obj(
            "vt1.obj",
            "v 0 0 0\nv 1 0 0\nv 0 1 0\nvt 0.5\nvt 0.9\nvt 0.1\nf 1/1 2/2 3/3\n",
        );
        let m = load(&p).unwrap();
        assert_eq!(m.stats.uv_sets, 1);
        let uv = m.uv_set(0).unwrap();
        assert_eq!(uv[0], [0.5, 0.0], "缺的 v 分量应补 0");
    }
}
