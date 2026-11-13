// 让原生标题栏跟随应用的深色主题。
//
// 背景：egui 的 `sync_window_theme` 默认开启，会把 `ThemePreference::Dark` 转成
// `ViewportCommand::SetTheme(SystemTheme::Dark)` 发给 winit；但 Windows 上 winit
// 走的是 `SetWindowCompositionAttribute(WCA_USEDARKMODECOLORS)`——一个未公开 API。
// 微软官方文档对公开的 `DWMWA_USE_IMMERSIVE_DARK_MODE` 明确写着：
// "For compatibility reasons, all windows default to light mode regardless of the
// system setting."（未显式声明深色的窗口一律按浅色画）
// 因此标题栏是黑是白取决于操作系统版本、DWM 状态与调用时机，表现为"有时黑有时白"。
//
// 修法是补一次公开 API 调用：`DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`。
// 属性号在 Windows 11 Build 22000 起是 20，更早的 Windows 10 预览版用 19，
// 两个都试，谁成功认谁。

#[cfg(windows)]
pub fn apply_dark_title_bar(window: &impl raw_window_handle::HasWindowHandle) {
    use raw_window_handle::RawWindowHandle;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    // 标题栏深色在 Windows 11 Build 22000 起的属性号
    const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
    // Windows 10 1809–20H2 预览期使用过的旧属性号
    const DWMWA_USE_IMMERSIVE_DARK_MODE_LEGACY: u32 = 19;

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd: HWND = win32.hwnd.get() as HWND;

    let enabled: i32 = 1;
    // 两个属性号都试：不匹配的那个会返回 E_INVALIDARG，不影响已生效的那个
    for attr in [
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        DWMWA_USE_IMMERSIVE_DARK_MODE_LEGACY,
    ] {
        let hr = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attr,
                (&enabled as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            )
        };
        if hr == 0 {
            break;
        }
    }
}

#[cfg(not(windows))]
pub fn apply_dark_title_bar(_window: &impl raw_window_handle::HasWindowHandle) {}
