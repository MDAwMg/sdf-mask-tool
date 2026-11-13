//! 3D 预览：模型加载与软件光栅。
//!
//! 定位是**纯预览**：不修改模型、不做骨骼变形、不做材质。
//! 目的只有一个——看清"这块遮罩在模型上落在哪"。
//!
//! 性能是本模块的第一约束（预览只需可辨识，优化优先于画质）：
//! - 加载时把所有数据摊平成"每三角 3 个"的紧密数组，渲染循环里零间接寻址；
//! - 光栅器自己做背面剔除、视锥裁剪、微小三角剔除；
//! - 模块本身**不含任何每帧状态**，渲染与否完全由调用方的脏标记决定。

pub mod fbx;
pub mod mesh;
pub mod obj;
pub mod raster;

pub use mesh::{LoadError, Mesh, MeshStats};

use std::path::Path;

/// 一次能接受的三角面上限。
///
/// 超过这个数就不是"预览"了，是在拿 UI 线程赌运气。
/// 参照：10 万面与 7.4 万面的样本模型都远低于此；
/// 而 216 万面的地形网格必须拒绝，
/// 否则加载当场冻住。
pub const MAX_TRIS: usize = 1_200_000;

/// 超过这个面数就提示"可能卡顿"，但仍然允许加载。
pub const WARN_TRIS: usize = 300_000;

/// 支持的模型扩展名（小写，不含点）。
pub const SUPPORTED_EXTS: [&str; 2] = ["obj", "fbx"];

/// 这个扩展名是不是模型文件。
pub fn is_model_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .map(|s| SUPPORTED_EXTS.contains(&s.as_str()))
        .unwrap_or(false)
}

/// 按扩展名分派解析。
pub fn load_mesh(path: &Path) -> Result<Mesh, LoadError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "obj" => obj::load(path),
        "fbx" => fbx::load(path),
        other => Err(LoadError::Format(format!(
            "不支持的模型格式：.{other}（只支持 .obj / .fbx）"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn 识别模型扩展名() {
        assert!(is_model_path(&PathBuf::from("a.obj")));
        assert!(is_model_path(&PathBuf::from("a.OBJ")));
        assert!(is_model_path(&PathBuf::from("a.Fbx")));
        assert!(!is_model_path(&PathBuf::from("a.png")));
        assert!(!is_model_path(&PathBuf::from("a")));
    }

    #[test]
    fn 不支持的格式给出明确错误() {
        let e = load_mesh(&PathBuf::from("x.stl")).unwrap_err();
        let s = e.to_string();
        assert!(s.contains("stl"), "错误信息应指出具体格式：{s}");
        assert!(s.contains("obj"), "错误信息应给出可用格式：{s}");
    }
}
