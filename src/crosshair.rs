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
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetWindowPos, ShowWindow,
    UpdateLayeredWindow, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
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
    /// 上一次真正画进位图的配置、画布尺寸和屏幕位置。
    ///
    /// 前台轮询每 120ms 调一次 `apply`，但配置和形状通常几分钟不变。
    /// 没有这个记录的话，每一轮都会把几万像素重新算一遍再整个上传，
    /// 白白占着 CPU 和 GPU 带宽。
    drawn: Mutex<Option<(Crosshair, i32, i32, i32)>>,
    /// 上一次重新置顶的时间。游戏重新置顶是低频事件，没必要每 120ms
    /// 都调一次 `SetWindowPos`（它会让 DWM 重新做一次层级合成）。
    last_topmost: Mutex<Option<std::time::Instant>>,
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
                drawn: Mutex::new(None),
                last_topmost: Mutex::new(None),
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
        //
        // 中心点可能比四段线更靠里也可能更大，所以画布至少要有中心点
        // 那么大，否则会被裁掉一角。
        let shape_extent = (cfg.gap + cfg.size + cfg.outline + cfg.thickness + 2) as i32;
        let dot_extent = if cfg.center_dot {
            (cfg.center_dot_size + 2) as i32
        } else {
            0
        };
        let extent = shape_extent.max(dot_extent);
        let canvas: i32 = (extent * 2 + 1).max(9);

        // 准星放在窗口（或屏幕）正中，再加上用户设定的物理像素偏移。
        //
        // 偏移量直接当物理像素用，不做缩放换算。
        //
        // 进程声明了 per-monitor-v2 DPI 感知，`GetClientRect` /
        // `ClientToScreen` 拿到的已经是物理像素，偏移量也是按物理像素定义——
        // 两者单位一致，直接相加即可。
        //
        // 之前这里除以了显示器缩放比例（`offset / scale`），在 125% 缩放下
        // 设 100px 实际只移动 80px。实测确认后去掉。
        //
        // 像素对齐约定。设客户区在屏幕上是 [left, left+width)，几何中心
        // C = left + width/2。画布奇数边长 canvas = 2k+1，覆盖
        // [cx, cx+canvas)，视觉中心在 cx + k + 0.5。
        //
        // 要让准星和游戏内准星重合，得和游戏用同一个约定。DirectX 的
        // 屏幕空间把坐标 x 锚定到「像素中心 x+0.5」，即 C 落在列
        // floor(C) 的中心上：C=600.0 → 列 600（中心 600.5），
        // C=600.5 → 列 600（中心 600.5）。
        //
        // 两种宽度下都对上的唯一选择，是让画布中心 = floor(C) + 0.5：
        //   cx = floor(C) + 0.5 - k - 0.5 = floor(C) - k
        // 取整后正好是 floor(C) - canvas/2。
        //
        // 这里对 width（不是客户区右边界）取整，因为 width/2 在宽为
        // 奇数时会落成 .5，floor 后仍取那个像素——和游戏一致。
        let center_x = left as f64 + width as f64 / 2.0 + cfg.offset_x as f64;
        let center_y = top as f64 + height as f64 / 2.0 + cfg.offset_y as f64;
        let cx = center_x.floor() as i32 - canvas / 2;
        let cy = center_y.floor() as i32 - canvas / 2;

        // 上一轮画过什么。前台轮询每 120ms 调一次本函数，而配置和形状
        // 通常几分钟不变——没有这份记录的话，每一轮都要把几万像素重新
        // 算一遍再整个上传，白白占 CPU 和 D3D 合成带宽。
        let last = self.drawn.lock().ok().and_then(|s| *s);

        if self.visible.load(Ordering::Relaxed) {
            if let Some((drawn_cfg, drawn_canvas, drawn_x, drawn_y)) = last {
                if drawn_cfg == *cfg && drawn_canvas == canvas && (drawn_x, drawn_y) == (cx, cy) {
                    // 位置没变不代表层级没变：游戏切全屏时会把自己重新置顶，
                    // 压在准星上面。这里顺手把层级再抢回来。
                    self.reassert_topmost();
                    return format!("未变 ({cx},{cy}) {canvas}x{canvas}");
                }
            }
        }

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

        // 画布尺寸变了必须重画（旧位图已丢弃）；配置变了也要重画。
        // 只是窗口位置变了的话，位图内容仍然有效。
        let needs_paint = need_new
            || match last {
                Some((drawn_cfg, drawn_canvas, _, _)) => {
                    drawn_cfg != *cfg || drawn_canvas != canvas
                }
                None => true,
            };
        if needs_paint {
            draw(surface, cfg);
        }

        let size = SIZE { cx: canvas, cy: canvas };
        let src = POINT { x: 0, y: 0 };
        let dst = POINT { x: cx, y: cy };

        unsafe {
            if needs_paint {
                surface.flush_to_dc();
            }
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
                // 每次显示后都用完整参数重新置顶一次，而不是只动层级。
                //
                // 参考实现在创建窗口后立刻做的是：
                //   SetWindowPos(hwnd, HWND_TOPMOST, cx, cy, w, h, SWP_NOACTIVATE)
                // 位置和尺寸都明确给出来。只调 ulw 改位置、再单独摞层级，
                // 在某些合成阶段（尤其是刚从独占全屏切回）会留下一帧
                // 旧位置。这里保持和它一样的调用形态。
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    cx,
                    cy,
                    canvas,
                    canvas,
                    SWP_NOACTIVATE,
                );
                if let Ok(mut last) = self.last_topmost.lock() {
                    *last = Some(std::time::Instant::now());
                }
            }
            if let Ok(mut state) = self.drawn.lock() {
                *state = Some((*cfg, canvas, cx, cy));
            }
        }
        format!("绘制于 ({cx},{cy}) {canvas}x{canvas}")
    }

    pub fn hide(&self) {
        if self.visible.swap(false, Ordering::Relaxed) {
            unsafe { ShowWindow(self.hwnd, SW_HIDE) };
        }
        // 下次显示必须重新上传位图（窗口被隐藏过，内容不保证还在），
        // 所以清掉这份缓存。置顶节奏也一并重置：重新显示时要立刻抢层级。
        if let Ok(mut state) = self.drawn.lock() {
            *state = None;
        }
        if let Ok(mut last) = self.last_topmost.lock() {
            *last = None;
        }
    }

    /// 把窗口重新提到置顶层级的最上面。
    ///
    /// `WS_EX_TOPMOST` 只保证「在所有非置顶窗口之上」，不保证「在所有置顶
    /// 窗口之上」。游戏进入全屏时会自己调 `SetWindowPos(HWND_TOPMOST)`
    /// 把自己提到最前——同为置顶窗口，后设置的在上层，准星就被压到游戏
    /// 画面下面去了。这是「窗口化看得见、全屏看不见」最常见的原因。
    ///
    /// 在显示期间定期重新置顶把顺序抢回来。两秒一次足够：游戏切全屏是
    /// 低频事件，而每次调用都会让 DWM 重排一次层级，没必要更频繁。
    /// `SWP_NOACTIVATE` 保证不抢焦点。
    pub fn reassert_topmost(&self) {
        if !self.visible.load(Ordering::Relaxed) {
            return;
        }
        {
            let Ok(mut last) = self.last_topmost.lock() else {
                return;
            };
            let now = std::time::Instant::now();
            if let Some(prev) = *last {
                if now.duration_since(prev) < std::time::Duration::from_secs(2) {
                    return;
                }
            }
            *last = Some(now);
        }
        unsafe {
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
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
    // 描边有独立的透明度，不再写死 70%。
    let outline_alpha = cfg.outline_opacity as u32;

    // 描边：同一套形状放大一圈画深色，再画本体。
    // 纯白准星在雪地或亮天空里会糊成一片。
    for (pad, color, alpha_scale) in [
        (outline as f32, [0u8, 0u8, 0u8], outline_alpha),
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
                    if inside(cfg.style, px, py, hw, r, g, dot_radius + pad) {
                        hits += 1;
                    }
                }
                if hits > 0 {
                    surface.put(x, y, color, (a_base * hits / 4) as u8);
                }
            }
        }
    }

    // 中心点：独立于形状，叠加在中心。FPS 里常关掉四段线只留一个点。
    if cfg.center_dot {
        let r = (cfg.center_dot_size.max(1) as f32) / 2.0;
        for y in 0..h {
            for x in 0..w {
                let mut hits = 0u32;
                for (sx, sy) in [(0.25, 0.25), (0.75, 0.25), (0.25, 0.75), (0.75, 0.75)] {
                    let px = x as f32 + sx - center_x;
                    let py = y as f32 + sy - center_y;
                    if (px * px + py * py).sqrt() <= r {
                        hits += 1;
                    }
                }
                if hits > 0 {
                    surface.put(x, y, cfg.color, (base_alpha * hits / 4) as u8);
                }
            }
        }
    }
}

/// 形状的归属判断：点 (px, py) 是否落在准星上。
///
/// 坐标以画布中心为原点。抽成独立函数是为了能单测——形状画反了
/// （T 形曾经两次画错方向）光看代码很难发现，跑一遍断言立刻露馅。
fn inside(
    style: CrosshairStyle,
    px: f32,
    py: f32,
    hw: f32,
    r: f32,
    g: f32,
    dot_radius: f32,
) -> bool {
    match style {
        CrosshairStyle::Cross => {
            // 横竖各一条，中心留空
            (px.abs() <= hw && py.abs() >= g && py.abs() <= g + r)
                || (py.abs() <= hw && px.abs() >= g && px.abs() <= g + r)
        }
        CrosshairStyle::Dot => (px * px + py * py).sqrt() <= dot_radius,
        CrosshairStyle::Ring => ((px * px + py * py).sqrt() - r).abs() <= hw,
        CrosshairStyle::TShape => {
            // FPS 里的 T 形：去掉十字上方那一截，只留左右和下方。
            //
            // 上方留白是为了不挡住敌人头部——预瞄时会下意识把
            // 准星往上抬，更容易压到爆头线。中心仍留空隙，
            // 控枪时还能找到中心点。
            (px.abs() <= hw && py >= g && py <= g + r)
                || (py.abs() <= hw && px.abs() >= g && px.abs() <= g + r)
        }
    }
}

/// 包含指定点的显示器矩形（左, 上, 右, 下）。
pub fn monitor_containing(point: (i32, i32)) -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    unsafe {
        let pt = POINT { x: point.0, y: point.1 };
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

/// 当前前台窗口句柄，仅用于诊断。
pub fn foreground_hwnd() -> usize {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { GetForegroundWindow() as usize }
}

/// 前台窗口的客户区矩形（屏幕坐标）。准星只在它范围内生效。
///
/// 优先用 DWM 的可见边界（`DWMWA_EXTENDED_FRAME_BOUNDS`）。
///
/// 为什么不用 `GetClientRect`：窗口被「拉满窗口」功能拉到整屏后，
/// 为了让可见区铺满，窗口外框会伸到屏幕外一点；而 DWM 又从客户区
/// 里扣掉一圈不可见边框，于是 `GetClientRect` 给出 1906x1073、
/// 起点 (7,0)——中心比屏幕中心高 3.5 像素，准星跟着偏。
///
/// DWM 的可见边界就是玩家真正看到的画面区域，拉满后正好等于整块
/// 显示器，用它定位准星才是对的。
///
/// 反例：游戏用独占全屏时 DWM 给出的值可能不可靠，那种情况本来就
/// 显示不了准星，不影响。
pub fn foreground_client_rect() -> Option<(i32, i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetForegroundWindow};

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }

        // 先试 DWM 可见边界
        let mut vis: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut vis as *mut RECT as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        );
        if hr == 0 && vis.right > vis.left && vis.bottom > vis.top {
            return Some((vis.left, vis.top, vis.right, vis.bottom));
        }

        // DWM 不可用：退回客户区
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

#[cfg(test)]
mod tests {
    use super::*;

    // 常用几何：线宽 2 → hw=1，长度 10 → r=5，中心间距 4 → g=2
    const HW: f32 = 1.0;
    const R: f32 = 5.0;
    const G: f32 = 2.0;
    const DOT: f32 = 1.0;

    fn is(style: CrosshairStyle, px: f32, py: f32) -> bool {
        inside(style, px, py, HW, R, G, DOT)
    }

    #[test]
    fn cross_has_four_arms_and_empty_center() {
        // 中心区域留空，玩家才能看清目标点
        assert!(!is(CrosshairStyle::Cross, 0.0, 0.0), "中心必须留空");
        assert!(!is(CrosshairStyle::Cross, 0.0, 1.5), "间距内没有线");
        // 四个方向各有一段
        assert!(is(CrosshairStyle::Cross, 0.0, 5.0), "上臂");
        assert!(is(CrosshairStyle::Cross, 0.0, -5.0), "下臂");
        assert!(is(CrosshairStyle::Cross, 5.0, 0.0), "右臂");
        assert!(is(CrosshairStyle::Cross, -5.0, 0.0), "左臂");
        // 臂长到 gap+size 为止
        assert!(is(CrosshairStyle::Cross, 0.0, G + R - 0.1), "臂的末端仍在");
        assert!(!is(CrosshairStyle::Cross, 0.0, G + R + 0.5), "臂外没有线");
    }

    #[test]
    fn tshape_drops_only_the_top_arm() {
        // 这是用户纠正过两次的地方：T 形 = 去掉上方竖线，留左右和下方。
        assert!(!is(CrosshairStyle::TShape, 0.0, -5.0), "上方不能有竖线");
        assert!(is(CrosshairStyle::TShape, 0.0, 5.0), "下方必须保留");
        assert!(is(CrosshairStyle::TShape, 5.0, 0.0), "右臂");
        assert!(is(CrosshairStyle::TShape, -5.0, 0.0), "左臂");
        assert!(!is(CrosshairStyle::TShape, 0.0, 0.0), "中心留空");
    }

    #[test]
    fn dot_is_round_and_small() {
        assert!(is(CrosshairStyle::Dot, 0.0, 0.0), "中心是实心的");
        assert!(is(CrosshairStyle::Dot, 0.7, 0.7), "半径内");
        assert!(!is(CrosshairStyle::Dot, 5.0, 0.0), "半径外没有东西");
    }

    #[test]
    fn ring_is_hollow() {
        assert!(!is(CrosshairStyle::Ring, 0.0, 0.0), "圆环中心必须是空的");
        assert!(is(CrosshairStyle::Ring, R, 0.0), "环上的点属于圆环");
        assert!(is(CrosshairStyle::Ring, 0.0, -R), "四个方向都在环上");
        assert!(!is(CrosshairStyle::Ring, R + 3.0, 0.0), "环外没有东西");
    }

    #[test]
    fn draw_stays_inside_canvas() {
        // 画布尺寸按 gap+size+outline+thickness 算，图形不能超出——
        // 超出的部分会被裁掉，准星看起来就是缺了一截。
        let cfg = Crosshair {
            gap: 4,
            size: 10,
            outline: 2,
            thickness: 3,
            ..Crosshair::default()
        };
        let extent = (cfg.gap + cfg.size + cfg.outline + cfg.thickness + 2) as i32;
        let canvas = (extent * 2 + 1).max(9);
        let half = canvas as f32 / 2.0;

        let hw = cfg.thickness as f32 / 2.0 + cfg.outline as f32;
        let reach = cfg.gap as f32 / 2.0 + cfg.size as f32 / 2.0 + cfg.outline as f32;
        assert!(reach + hw <= half + 1.0, "图形半径 {reach}+{hw} 超出画布半宽 {half}");

        // 每个方向上的最远点都要落在画布内
        for (x, y) in [
            (0.0f32, -reach),
            (0.0, reach),
            (reach, 0.0),
            (-reach, 0.0),
        ] {
            assert!(x.abs() + hw <= half + 1.0 && y.abs() + hw <= half + 1.0);
        }
    }

    #[test]
    fn opposite_styles_do_not_overlap_in_center() {
        // 四种样式在中心都应该保持"能看见目标"：要么留空，要么是细点
        // 每种样式的中心命中结果固定下来，改绘制逻辑时不会悄悄变样
        let center = |s| inside(s, 0.0, 0.0, HW, R, G, DOT);
        assert!(!center(CrosshairStyle::Cross));
        assert!(!center(CrosshairStyle::Ring));
        assert!(!center(CrosshairStyle::TShape));
        assert!(center(CrosshairStyle::Dot));
    }

    /// 画布左上角的计算规则。和 `apply` 里保持一致，抽出来给测试用。
    fn canvas_origin(left: i32, width: i32, canvas: i32, offset: i32) -> i32 {
        (left as f64 + width as f64 / 2.0 + offset as f64).floor() as i32 - canvas / 2
    }

    #[test]
    fn crosshair_lands_on_directx_pixel_convention() {
        // 准星的视觉中心必须和游戏内准星落在同一像素。
        // DirectX 把坐标 x 锚定到像素中心 x+0.5，即几何中心 C 落在
        // 列 floor(C) 上。画布中心 = cx + canvas/2 + 0.5，
        // 这两个值必须一致，否则就是差一格。
        let canvas = 31;
        for (left, width) in [
            (0, 800),    // 偶数宽，中心 .0
            (0, 801),    // 奇数宽，中心 .5
            (200, 800),  // 非零起点
            (200, 801),
            (0, 1920),   // 常见分辨率
            (0, 1921),
            (-8, 1936),  // 跨屏窗口的负坐标
        ] {
            let cx = canvas_origin(left, width, canvas, 0);
            // 画布覆盖 [cx, cx+canvas)，画布中心（像素边界坐标）是
            // cx + canvas/2（整数除，canvas 为奇数时即中心像素的左边界），
            // 中心像素的像素中心就是 cx + canvas/2 + 0.5。
            let center_pixel = cx + canvas / 2;
            let drawn_center = center_pixel as f64 + 0.5;
            let real = left as f64 + width as f64 / 2.0;
            let expected = real.floor() as f64 + 0.5;
            assert!(
                (drawn_center - expected).abs() < 1e-9,
                "{width}px 宽：准星中心像素 {center_pixel}（中心 {drawn_center}），\
                 应落在列 {}（中心 {expected}，真实中心 {real}）",
                expected as i32
            );
        }
    }

    #[test]
    fn offset_shifts_by_exact_pixels() {
        let canvas = 31;
        let base = canvas_origin(100, 800, canvas, 0);
        for d in [1, 7, 100, -50, 120] {
            let moved = canvas_origin(100, 800, canvas, d);
            assert_eq!(moved - base, d, "偏移 {d} 应精确平移 {d} 像素");
        }
    }
}
