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

    fn put(&mut self, x: i32, y: i32, color: [u8; 3], alpha: u8) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height || alpha == 0 {
            return;
        }
        // 预乘 alpha：BGRA 每个分量都乘以 a/255
        let a = alpha as u32;
        let b = (color[2] as u32 * a / 255) as u32;
        let g = (color[1] as u32 * a / 255) as u32;
        let r = (color[0] as u32 * a / 255) as u32;
        let px = (a << 24) | (r << 16) | (g << 8) | b;
        self.pixels[(y * self.width + x) as usize] = px;
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
}

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

            Some(Arc::new(Overlay {
                hwnd,
                surface: Mutex::new(None),
                visible: AtomicBool::new(false),
            }))
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

        // 准星放在窗口（或屏幕）正中
        let cx = left + width / 2 - canvas / 2;
        let cy = top + height / 2 - canvas / 2;

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
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 把准星画进像素缓冲。
fn draw(surface: &mut Surface, cfg: &Crosshair) {
    surface.clear();
    let w = surface.width;
    let h = surface.height;
    let center = (w / 2, h / 2);
    let alpha = (cfg.opacity as u32 * 255 / 100) as u8;
    let t = cfg.thickness.max(1) as i32;
    let gap = cfg.gap as i32;
    let size = cfg.size.max(1) as i32;

    let mut put = |surface: &mut Surface, x: i32, y: i32, color: [u8; 3], a: u8| {
        surface.put(x, y, color, a);
    };

    // 描边：先画一圈更大的深色，再画本体。
    // 纯白准星在雪地或亮天空里会糊成一片。
    let outline = cfg.outline as i32;
    let passes: &[(i32, [u8; 3])] = if outline > 0 {
        &[(outline, [0, 0, 0]), (0, cfg.color)]
    } else {
        &[(0, cfg.color)]
    };

    for &(pad, color) in passes {
        let a = if color == [0, 0, 0] {
            // 描边稍微透明一点，免得显得死黑
            (alpha as u32 * 70 / 100) as u8
        } else {
            alpha
        };
        let th = t + pad;
        let g = gap;
        let s = size + pad;

        match cfg.style {
            CrosshairStyle::Cross => {
                // 四条线：上下左右各一段，中心留空。
                // 笔画以中心线为轴左右各占 th/2，偶数宽度时向下取整，
                // 保证四个方向完全对称。
                let lo = -(th / 2);
                let hi = lo + th;
                for i in 0..s {
                    for k in lo..hi {
                        let off = g + i;
                        put(surface, center.0 + off, center.1 + k, color, a);
                        put(surface, center.0 - off, center.1 + k, color, a);
                        put(surface, center.0 + k, center.1 + off, color, a);
                        put(surface, center.0 + k, center.1 - off, color, a);
                    }
                }
            }
            CrosshairStyle::Dot => {
                let r = (t + pad).max(1);
                for dy in -r..=r {
                    for dx in -r..=r {
                        if dx * dx + dy * dy <= r * r {
                            put(surface, center.0 + dx, center.1 + dy, color, a);
                        }
                    }
                }
            }
            CrosshairStyle::Ring => {
                let r = s.max(2);
                let band = th.max(1);
                for dy in -r..=r {
                    for dx in -r..=r {
                        let d2 = dx * dx + dy * dy;
                        let outer = r * r;
                        let inner = (r - band) * (r - band);
                        if d2 <= outer && d2 >= inner {
                            put(surface, center.0 + dx, center.1 + dy, color, a);
                        }
                    }
                }
            }
            CrosshairStyle::TShape => {
                // 上、左、右三段，没有下
                let lo = -(th / 2);
                let hi = lo + th;
                for i in 0..s {
                    for k in lo..hi {
                        let off = g + i;
                        put(surface, center.0 + k, center.1 - off, color, a);
                        put(surface, center.0 + off, center.1 + k, color, a);
                        put(surface, center.0 - off, center.1 + k, color, a);
                    }
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
