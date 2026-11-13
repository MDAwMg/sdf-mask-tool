//! 应用设置：那些"会改、但不常改"的东西。
//!
//! 放置原则：会改动、但不常改的参数归此面板（放工具栏既占地方又容易误碰）。
//! 这类参数的特点是：调一次能用很久，放在工具栏上占地方、又容易被误碰。
//!
//! 存盘位置跟日志一致：优先程序同目录，写不开就退回临时目录。
//! 格式用 RON（工程里已经在用，不引新依赖）。
//! 读盘失败时**静默用默认值**——设置文件坏了不该拦住程序启动。

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// 新建工程的离散帧数
    pub default_steps: usize,
    /// 撤销步数上限。快照式撤销，每条约 8MB（1K 四通道两帧），别开太大。
    pub undo_limit: usize,
    /// 启动时的笔刷半径（画布像素）
    pub default_radius: f32,
    /// 新建工程的默认画布尺寸
    pub default_canvas_w: u32,
    pub default_canvas_h: u32,
    /// 新建关键帧时，用相邻两帧的插值遮罩做起点（而不是空白）
    pub interp_new_keyframe: bool,
    /// 在没有关键帧的位置落笔时，自动建一帧并在上面画。
    ///
    /// 和 `interp_new_keyframe` 是两件事：
    /// - 那个管"按 Ins 加帧时起点是插值还是空白"
    /// - 这个管"在没帧的地方能不能直接画"
    ///
    /// 关闭该开关时行为回到旧版：没有参数点就画不了，得先加参数点。
    pub auto_keyframe_on_paint: bool,
    /// 启动时自动开启违规检测（画布红叠加）
    pub show_violations: bool,

    // ------------------------------------------------ 3D 预览
    /// 左右分栏比例（画布占的比例，0.15..0.85）。
    /// 拖动过分隔条后记住比例，下次打开沿用。
    pub split_ratio: f32,
    /// 显示模式：0=仅画布 1=仅模型 2=同时。
    /// 存 u8 而不是直接存枚举，是为了让设置文件在枚举改名后仍能读。
    pub view_mode: u8,
    /// 3D 预览的线框开关
    pub model_wireframe: bool,
    /// 3D 预览的背面剔除开关
    pub model_backface_cull: bool,
    /// 启动时是否把模型 UV 线框叠到画布上。
    ///
    /// 默认关闭：UV 线框是"需要时才看"的辅助信息，常驻会盖住绘制内容。
    #[serde(default)]
    pub show_uv_on_canvas: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            default_steps: crate::model::doc::DEFAULT_STEPS,
            undo_limit: 20,
            default_radius: 24.0,
            default_canvas_w: 1024,
            default_canvas_h: 1024,
            interp_new_keyframe: true,
            // 默认开启：在没帧的地方落笔就直接画，不用先按 Ins。
            // 默认开启：符合 PS 的"随手就画"直觉。
            auto_keyframe_on_paint: true,
            show_violations: true,
            split_ratio: 0.5,
            // 默认「仅画布」。不加载模型时不显示 3D 面板，
            // 拖进模型或主动切换时才出现 3D 面板。
            view_mode: 0,
            model_wireframe: false,
            model_backface_cull: true,
            show_uv_on_canvas: false,
        }
    }
}

impl AppSettings {
    /// 夹到合法范围。读盘读到的旧值或手改的值都可能越界。
    pub fn sanitize(&mut self) {
        self.default_steps = crate::model::doc::clamp_steps(self.default_steps);
        self.undo_limit = self.undo_limit.clamp(1, 100);
        self.default_radius = self.default_radius.clamp(1.0, 512.0);
        self.default_canvas_w = self.default_canvas_w.clamp(16, 8192);
        self.default_canvas_h = self.default_canvas_h.clamp(16, 8192);
        self.split_ratio = self.split_ratio.clamp(0.15, 0.85);
        self.view_mode = self.view_mode.min(2);
    }
}

const FILE_NAME: &str = "sdf-mask-tool.cfg";

fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        out.push(dir.join(FILE_NAME));
    }
    out.push(std::env::temp_dir().join(FILE_NAME));
    out
}

/// 读设置。任何失败都退回默认值，不报错、不拦启动。
pub fn load() -> AppSettings {
    for p in candidates() {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        match ron::from_str::<AppSettings>(&text) {
            Ok(mut s) => {
                s.sanitize();
                log::info!("读取设置：{}", p.display());
                return s;
            }
            Err(e) => log::warn!("设置文件解析失败（用默认值）：{e}"),
        }
    }
    AppSettings::default()
}

/// 写设置。失败只记日志——设置存不下来不该影响正在做的事。
pub fn save(s: &AppSettings) {
    let text = match ron::ser::to_string_pretty(s, ron::ser::PrettyConfig::default()) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("序列化设置失败：{e}");
            return;
        }
    };
    for p in candidates() {
        if std::fs::write(&p, &text).is_ok() {
            return;
        }
    }
    log::warn!("设置写不进去（所有候选路径都失败）");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认值在合法范围() {
        let mut s = AppSettings::default();
        s.sanitize();
        assert_eq!(s, AppSettings::default(), "默认值本身就该合法");
    }

    #[test]
    fn 越界值被夹回来() {
        let mut s = AppSettings {
            default_steps: 0,
            undo_limit: 0,
            default_radius: 9999.0,
            default_canvas_w: 1,
            default_canvas_h: 99999,
            ..AppSettings::default()
        };
        s.sanitize();
        assert_eq!(s.default_steps, crate::model::doc::MIN_STEPS);
        assert_eq!(s.undo_limit, 1);
        assert_eq!(s.default_radius, 512.0);
        assert_eq!(s.default_canvas_w, 16);
        assert_eq!(s.default_canvas_h, 8192);
    }

    #[test]
    fn 序列化往返一致() {
        let a = AppSettings::default();
        let text = ron::ser::to_string_pretty(&a, ron::ser::PrettyConfig::default()).unwrap();
        let b: AppSettings = ron::from_str(&text).unwrap();
        assert_eq!(a.default_steps, b.default_steps);
        assert_eq!(a.undo_limit, b.undo_limit);
        assert!(b.interp_new_keyframe);
    }

    #[test]
    fn 分栏比例被夹在安全范围() {
        let mut s = AppSettings {
            split_ratio: 0.0,
            ..AppSettings::default()
        };
        s.sanitize();
        assert!((s.split_ratio - 0.15).abs() < 1e-6, "0 会让画布消失");

        s.split_ratio = 1.0;
        s.sanitize();
        assert!((s.split_ratio - 0.85).abs() < 1e-6, "1.0 会让模型消失");
    }

    #[test]
    fn 非法显示模式被夹回来() {
        let mut s = AppSettings {
            view_mode: 99,
            ..AppSettings::default()
        };
        s.sanitize();
        assert!(s.view_mode <= 2, "显示模式只有 0/1/2");
    }

    /// 旧版设置文件（没有新字段）必须能读，且新字段取默认值。
    /// 这直接决定"升级后旧设置文件能否正常读取"。
    #[test]
    fn 缺少新字段的旧设置文件仍能读() {
        let old = "(default_steps:32,undo_limit:15,default_radius:12.0,\
                   default_canvas_w:512,default_canvas_h:512,\
                   interp_new_keyframe:false,show_violations:false)";
        let s: AppSettings = ron::from_str(old).expect("旧文件必须能读");
        assert_eq!(s.default_steps, 32, "老值要保留");
        assert_eq!(s.split_ratio, 0.5, "新字段应取默认值");
        // 新增的 3D 字段默认「仅画布」——默认不显示 3D 模型
        assert_eq!(s.view_mode, 0, "旧文件没有 view_mode 时默认仅画布");
        assert!(s.model_backface_cull);
    }

    #[test]
    fn 默认设置是仅画布模式() {
        assert_eq!(AppSettings::default().view_mode, 0);
    }
}
