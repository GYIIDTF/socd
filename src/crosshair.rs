//! 准星叠加层。
//!
//! 一个分层窗口（layered window）画在游戏画面上方：
//!   * `WS_EX_LAYERED` + `UpdateLayeredWindow` —— 支持逐像素 Alpha
//!   * `WS_EX_TRANSPARENT` —— 鼠标事件穿透，不挡操作
//!   * `WS_EX_NOACTIVATE` —— 不抢焦点，点它不会把游戏切到后台
//!   * `WS_EX_TOOLWINDOW` —— 不出现在 Alt+Tab 和任务栏
//!
//! 只在配置变化时重绘。准星是静态图形，逐帧刷新纯属浪费。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP,
    HDC, HGDIOBJ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, RegisterClassW, ShowWindow,
    UpdateLayeredWindow, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::settings::{Crosshair, CrosshairStyle};

const CLASS_NAME: &str = "SOCDCrosshairOverlay";

/// 声明进程为 per-monitor-v2 DPI 感知。
///
/// 不声明的话，系统会把 `GetCursorPos` / `GetClientRect` 的返回值
/// 按主屏缩放虚拟化——多显示器不同缩放比例时，准星位置会偏。
/// 必须在创建任何窗口之前调用。
pub fn enable_per_monitor_dpi() {
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        // Win10 1703+ 才有；失败说明系统更旧，保持默认即可
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// 当前显示器的缩放比例（1.0 = 96 DPI）。
///
/// `MonitorFromPoint` + `GetDpiForMonitor` 拿到的是该显示器自己的 DPI，
/// 不是主屏的——这是"鼠标在哪块屏就按哪块的缩放"的关键。
pub fn scale_for_monitor(point: POINT) -> f32 {
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::HiDpi::GetDpiForMonitor;
    use windows_sys::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI;

    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return 1.0;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return 1.0;
        }

        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) != 0 {
            return 1.0;
        }
        // 进程已是 per-monitor-v2 感知时这个值就是真实 DPI；
        // 无论如何都按它算比例，避免"主屏 100%、副屏 150%"时错位。
        dpi_x as f32 / 96.0
    }
}

/// 渲染出来的位图：BGRA + 预乘 Alpha，`UpdateLayeredWindow` 要这个格式。
struct Surface {
    hdc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    /// DIB 的像素区，由系统分配、随 bitmap 存活。直接写它就不用再拷贝。
    bits: *mut u32,
    pixels: Vec<u32>,
    width: i32,
    height: i32,
}

impl Surface {
    fn new(width: i32, height: i32) -> Option<Surface> {
        let width = width.max(1);
        let height = height.max(1);

        unsafe {
            let screen = windows_sys::Win32::Graphics::Gdi::GetDC(std::ptr::null_mut());
            let hdc = CreateCompatibleDC(screen);
            windows_sys::Win32::Graphics::Gdi::ReleaseDC(std::ptr::null_mut(), screen);
            if hdc.is_null() {
                return None;
            }

            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // 负数 = 自上而下，和我们的像素顺序一致
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(hdc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bitmap.is_null() || bits.is_null() {
                DeleteDC(hdc);
                return None;
            }
            let old = SelectObject(hdc, bitmap);

            Some(Surface {
                hdc,
                bitmap,
                old,
                bits: bits as *mut u32,
                pixels: vec![0; (width * height) as usize],
                width,
                height,
            })
        }
    }

    /// 把像素缓冲写回 DIB。
    ///
    /// `CreateDIBSection` 返回的 `bits` 指向系统分配的像素区，
    /// 直接在原地写——不需要再建临时位图再 BitBlt。
    fn flush_to_dc(&mut self) {
        if self.bits.is_null() {
            return;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.pixels.as_ptr(),
                self.bits,
                self.pixels.len(),
            );
        }
    }

    fn clear(&mut self) {
        self.pixels.iter_mut().for_each(|p| *p = 0);
    }

    /// 把一个颜色按 alpha 叠加到像素上（源覆盖目标）。
    ///
    /// 描边先画、本体后画，直接覆盖会让描边的半透明边缘被吃掉；
    /// 用标准 alpha 合成才叠得对。像素是预乘格式，合成公式相应简化。
    fn put(&mut self, x: i32, y: i32, color: [u8; 3], alpha: u8) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height || alpha == 0 {
            return;
        }
        let idx = (y * self.width + x) as usize;
        let dst = self.pixels[idx];

        let sa = alpha as u32;
        let inv = 255 - sa;

        let src_b = color[2] as u32 * sa / 255;
        let src_g = color[1] as u32 * sa / 255;
        let src_r = color[0] as u32 * sa / 255;
        let _ = &dst;

        // 预乘格式下：out = src + dst * (1 - src_a)
        let dst_a = (dst >> 24) & 0xFF;
        let dst_b = dst & 0xFF;
        let dst_g = (dst >> 8) & 0xFF;
        let dst_r = (dst >> 16) & 0xFF;

        let a = sa + dst_a * inv / 255;
        let b = src_b + dst_b * inv / 255;
        let g = src_g + dst_g * inv / 255;
        let r = src_r + dst_r * inv / 255;

        self.pixels[idx] = (a.min(255) << 24) | (r.min(255) << 16) | (g.min(255) << 8) | b.min(255);
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.old);
            DeleteObject(self.bitmap as HGDIOBJ);
            DeleteDC(self.hdc);
        }
    }
}

pub struct Overlay {
    hwnd: HWND,
    surface: Mutex<Option<Surface>>,
    visible: AtomicBool,
    /// Raw Input 快捷键监听
    pub raw: crate::rawinput::RawInputState,
}

/// 窗口过程需要拿到 Overlay 才能处理 WM_INPUT。
/// 用全局登记：一个进程只会有这一个准星窗口。
static OVERLAY_RAW: Mutex<Option<Arc<Overlay>>> = Mutex::new(None);

unsafe impl Send for Overlay {}
unsafe impl Sync for Overlay {}

impl Overlay {
    /// 建窗口。不显示，等 `apply` 决定。
    pub fn new() -> Option<Arc<Overlay>> {
        unsafe {
            let name: Vec<u16> = CLASS_NAME.encode_utf16().chain(std::iter::once(0)).collect();
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: std::ptr::null_mut(),
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: name.as_ptr(),
            };
            RegisterClassW(&wc);

            let ex_style = WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_NOACTIVATE
                | WS_EX_TOOLWINDOW
                | WS_EX_TOPMOST;

            let hwnd = CreateWindowExW(
                ex_style,
                name.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                0, 0, 1, 1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return None;
            }

            let overlay = Arc::new(Overlay {
                hwnd,
                surface: Mutex::new(None),
                visible: AtomicBool::new(false),
                raw: crate::rawinput::RawInputState::default(),
            });
            if let Ok(mut slot) = OVERLAY_RAW.lock() {
                *slot = Some(overlay.clone());
            }
            Some(overlay)
        }
    }

    /// 按当前配置重绘。`target` 是要覆盖的窗口区域，None 表示屏幕中心。
    pub fn apply(&self, cfg: &Crosshair, target: Option<(i32, i32, i32, i32)>) -> String {
        if !cfg.is_visible() || target.is_none() {
            self.hide();
            return format!("跳过：visible={} target={:?}", cfg.is_visible(), target);
        }
        let (left, top, right, bottom) = target.unwrap();
        let width = right - left;
        let height = bottom - top;
        if width <= 0 || height <= 0 {
            self.hide();
            return format!("跳过：尺寸 {width}x{height}");
        }

        // 画布只覆盖准星本身，不铺满整个窗口——省内存也省合成开销。
        //
        // 用奇数尺寸：中心像素是 canvas/2 的整数部分，奇数时四个方向
        // 到边缘的距离完全相等，画出来才是对称的。偶数会在某一侧多一格。
        let extent = (cfg.gap + cfg.size + cfg.outline + cfg.thickness + 2) as i32;
        let canvas: i32 = (extent * 2 + 1).max(9);

        let mut guard = self.surface.lock().unwrap();
        let need_new = match guard.as_ref() {
            Some(s) => s.width != canvas || s.height != canvas,
            None => true,
        };
        if need_new {
            *guard = Surface::new(canvas, canvas);
        }
        let Some(surface) = guard.as_mut() else {
            self.hide();
            return "跳过：Surface 创建失败".to_string();
        };

        draw(surface, cfg);

        // 准星放在窗口（或屏幕）正中，再加上用户设定的物理像素偏移。
        //
        // 偏移量按"物理像素"定义，但窗口坐标是逻辑像素（DPI 缩放后）。
        // 除以缩放比例才是屏幕上的真实位移——150% 缩放下偏移 100px
        // 应该是 100 物理像素，不是 100 逻辑像素（那会是 150 物理像素）。
        let center = POINT { x: left + width / 2, y: top + height / 2 };
        let scale = scale_for_monitor(center).max(0.5);
        let ox = (cfg.offset_x as f32 / scale).round() as i32;
        let oy = (cfg.offset_y as f32 / scale).round() as i32;

        let cx = center.x - canvas / 2 + ox;
        let cy = center.y - canvas / 2 + oy;

        let size = SIZE { cx: canvas, cy: canvas };
        let src = POINT { x: 0, y: 0 };
        let dst = POINT { x: cx, y: cy };

        unsafe {
            surface.flush_to_dc();
            // ULW_ALPHA 必须给 BLENDFUNCTION，传 null 会直接失败
            // （UpdateLayeredWindow 返回 0，GetLastError 是 ERROR_GEN_FAILURE）。
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255, // 逐像素 alpha 由 DIB 自己带
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let ok = UpdateLayeredWindow(
                self.hwnd,
                std::ptr::null_mut(),
                &dst,
                &size,
                surface.hdc,
                &src,
                0,
                &blend,
                ULW_ALPHA,
            );
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                return format!("UpdateLayeredWindow 失败: {err}");
            }
            if !self.visible.swap(true, Ordering::Relaxed) {
                ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        format!("绘制于 ({cx},{cy}) {canvas}x{canvas}")
    }

    pub fn hide(&self) {
        if self.visible.swap(false, Ordering::Relaxed) {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
    }

    /// 按新的快捷键重新注册 Raw Input 设备。
    pub fn set_hotkey(
        &self,
        hotkey: Option<crate::settings::Hotkey>,
        enable_hotkey: Option<crate::settings::Hotkey>,
    ) {
        crate::rawinput::reconfigure(self.hwnd, &self.raw, hotkey, enable_hotkey);
    }

    /// Raw Input 是否注册成功（诊断用）。
    pub fn raw_registered(&self) -> bool {
        self.raw.last_ok.load(Ordering::Relaxed)
    }

    /// 取走待处理的开关键事件。
    pub fn drain_hotkey_events(&self) -> Vec<crate::rawinput::KeyEvent> {
        crate::rawinput::drain(&self.raw)
    }

    pub fn is_window(&self) -> bool {
        unsafe { !self.hwnd.is_null() && IsWindow(self.hwnd) != 0 }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if msg == crate::rawinput::wm_input() {
        // 只在 Raw Input 线程自己的窗口过程里解析，避免锁竞争
        if let Ok(slot) = OVERLAY_RAW.lock() {
            if let Some(overlay) = slot.as_ref() {
                crate::rawinput::handle_wm_input(lparam, &overlay.raw);
            }
        }
        // 返回 0 是 WM_INPUT 的约定
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 把准星画进像素缓冲。
///
/// 用距离场 + 4× 超采样：每个像素取四个子采样点，统计落在图形内的比例，
/// 作为该像素的 alpha。这样圆环和斜边边缘是平滑的，不是锯齿。
///
/// 参数语义（和界面上的滑杆对应）：
///   * `gap`     中心到线条起点的距离
///   * `size`    线条总长度
///   * `thickness` 线宽
fn draw(surface: &mut Surface, cfg: &Crosshair) {
    surface.clear();
    let w = surface.width;
    let h = surface.height;
    let center_x = w as f32 / 2.0;
    let center_y = h as f32 / 2.0;

    let half_width = cfg.thickness.max(1) as f32 / 2.0;
    let radius = cfg.size.max(1) as f32 / 2.0;
    let gap = cfg.gap as f32 / 2.0;
    let dot_radius = (cfg.thickness.max(1) as f32 / 2.0).max(1.0);

    let base_alpha = cfg.opacity as u32 * 255 / 100;
    let outline = cfg.outline;

    // 描边：同一套形状放大一圈画深色，再画本体。
    // 纯白准星在雪地或亮天空里会糊成一片。
    for (pad, color, alpha_scale) in [
        (outline as f32, [0u8, 0u8, 0u8], 70u32),
        (0.0, cfg.color, 100u32),
    ] {
        let hw = half_width + pad;
        let r = radius + pad;
        let g = gap;
        let a_base = base_alpha * alpha_scale / 100;

        for y in 0..h {
            for x in 0..w {
                // 四个子采样点，各占 1/4 权重
                let mut hits = 0u32;
                for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                    let px = x as f32 + sx - center_x;
                    let py = y as f32 + sy - center_y;
                    let dx = px.abs();
                    let dy = py.abs();
                    let distance = (dx * dx + dy * dy).sqrt();

                    let inside = match cfg.style {
                        CrosshairStyle::Cross => {
                            // 横竖各一条，中心留空
                            (dx <= hw && dy >= g && dy <= g + r)
                                || (dy <= hw && dx >= g && dx <= g + r)
                        }
                        CrosshairStyle::Dot => distance <= dot_radius + pad,
                        CrosshairStyle::Ring => (distance - r).abs() <= hw,
                        CrosshairStyle::TShape => {
                            // 上、左、右三向，没有下
                            (dx <= hw && py <= -g && py >= -g - r)
                                || (dy <= hw && dx >= g && dx <= g + r)
                        }
                    };
                    if inside {
                        hits += 1;
                    }
                }
                if hits > 0 {
                    surface.put(x, y, color, (a_base * hits / 4) as u8);
                }
            }
        }
    }
}

/// 当前鼠标所在显示器的完整矩形（左, 上, 右, 下）。
pub fn monitor_under_cursor() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let RECT { left, top, right, bottom } = info.rcMonitor;
        Some((left, top, right, bottom))
    }
}

/// 前台窗口的客户区矩形（屏幕坐标）。准星只在它范围内生效。
pub fn foreground_client_rect() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetForegroundWindow};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut rc: RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &mut rc) == 0 {
            return None;
        }
        let mut tl = POINT { x: rc.left, y: rc.top };
        let mut br = POINT { x: rc.right, y: rc.bottom };
        if ClientToScreen(hwnd, &mut tl) == 0 || ClientToScreen(hwnd, &mut br) == 0 {
            return None;
        }
        Some((tl.x, tl.y, br.x, br.y))
    }
}

/// 准星开关的全局状态：总开关 + 当前应显示的配置。
#[derive(Default)]
pub struct CrosshairState {
    /// 用户按开关切的总状态
    pub armed: AtomicBool,
    /// 当前该显示的配置（由 App 在前台变化时更新）
    pub current: Mutex<Option<Crosshair>>,
}
