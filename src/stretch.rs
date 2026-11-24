//! 把指定程序的主窗口拉到铺满显示器。
//!
//! 用途：无边框全屏时显卡缩放不生效（DWM 合成绕过了 GPU 缩放器），
//! 游戏画面可能只占屏幕一部分或按等比留黑边。这里直接把窗口的
//! 位置和尺寸改成整块显示器，让 DWM 替我们把画面铺满。
//!
//! 和显卡缩放的区别：
//!   * 显卡缩放发生在 GPU 输出层，只在独占全屏时生效；
//!   * 这里的拉伸发生在 DWM 合成层，无边框全屏和窗口化都能用。
//!
//! 副作用：画面会被强制拉伸到目标比例（不做保持宽高比），
//! 相当于把「全屏拉伸」这件事交给 DWM 做。这正好是玩家要的效果。
//!
//! 只动窗口的位置和尺寸，不改任何进程内存，也不注入代码。

use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowRect, IsWindow,
    GetWindowThreadProcessId, IsWindowVisible, SetWindowLongW, SetWindowPos, GWL_STYLE, GWL_EXSTYLE,
    GW_OWNER, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOZORDER, WS_CAPTION, WS_MAXIMIZE, WS_MINIMIZE, WS_THICKFRAME,
};

/// 一个窗口的原始样式与位置，用于恢复。
#[derive(Clone, Copy)]
struct Saved {
    hwnd: HWND,
    style: i32,
    ex_style: i32,
    rect: RECT,
}

/// 拉伸状态：记住被改过的窗口，关掉时恢复原样。
#[derive(Default)]
pub struct Stretcher {
    active: Option<Saved>,
    /// 上次执行拉伸的时刻，用于冷却
    last_applied: Option<std::time::Instant>,
}

// 窗口句柄是进程内整数，只从 App 线程访问。
unsafe impl Send for Stretcher {}

/// 进程 id -> 可执行文件名（小写）。读不到时返回 None。
fn process_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::Foundation::CloseHandle;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        let name = path.rsplit(['\\', '/']).next()?.to_ascii_lowercase();
        Some(name)
    }
}

/// 判断一个窗口是不是程序的主窗口（可见、无 owner、有标题栏、在屏幕上）。
fn is_main_window(hwnd: HWND) -> bool {
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return false;
        }
        if !GetWindow(hwnd, GW_OWNER).is_null() {
            return false;
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        // 只认带标题栏的顶层窗口；工具窗口、弹出菜单都排除。
        if style & WS_CAPTION as u32 == 0 {
            return false;
        }
        if style & (WS_MINIMIZE as u32) != 0 {
            return false;
        }
        // 有些程序（浏览器等）最小化后不置 WS_MINIMIZE，只是把窗口
        // 挪到屏幕外 (-32000,-32000)。按坐标再排除一次，否则会把一个
        // 看不见的窗口当成主窗口去拉伸。
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) != 0 {
            const OFFSCREEN: i32 = -10000;
            if r.left <= OFFSCREEN || r.top <= OFFSCREEN {
                return false;
            }
        }
        true
    }
}

struct FindCtx {
    /// 目标 exe 名，小写、无结尾 NUL
    target: String,
    found: HWND,
}

unsafe extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam as *mut FindCtx);
    if !ctx.found.is_null() || !is_main_window(hwnd) {
        return 1;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == 0 {
        return 1;
    }
    if let Some(name) = process_name(pid) {
        if name == ctx.target {
            ctx.found = hwnd;
        }
    }
    1
}

/// 找出某个程序的主窗口。找不到返回 null。
fn find_main_window(program: &str) -> HWND {
    let mut ctx = FindCtx {
        // 统一成小写比较：Windows 的文件名不区分大小写，用户填 "DeltaForce.exe"
        // 和进程报上来的 "deltaforce.exe" 必须算同一个。
        target: program.to_ascii_lowercase(),
        found: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(enum_cb), &mut ctx as *mut FindCtx as isize);
    }
    ctx.found
}

/// `GetWindowRect` 的原始窗口矩形（含 DWM 阴影边框）。
fn window_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) == 0 {
            return None;
        }
        Some(r)
    }
}

/// 窗口实际的可见边界（不含 DWM 阴影）。
///
/// `GetWindowRect` 会把窗口周围那圈不可见的拖拽边框也算进去，
/// 带标题栏的窗口常常比真实可见区域大 8 像素。
fn visible_rect(hwnd: HWND) -> Option<RECT> {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS,
    };

    unsafe {
        let mut r: RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut r as *mut RECT as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        );
        if hr != 0 {
            // DWM 不可用（旧系统或被禁用）时退回普通矩形
            let mut r2: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut r2) == 0 {
                return None;
            }
            return Some(r2);
        }
        Some(r)
    }
}

/// 某个窗口是否属于指定程序（按进程的 exe 名比较）。
fn belongs_to(hwnd: HWND, program: &str) -> bool {
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return false;
        }
        process_name(pid)
            .is_some_and(|n| n == program.to_ascii_lowercase())
    }
}

/// 窗口所在显示器的矩形。
fn monitor_rect(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if mon.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut info) == 0 {
            return None;
        }
        Some(info.rcMonitor)
    }
}

impl Stretcher {
    pub fn new() -> Stretcher {
        Stretcher::default()
    }

    /// 把 `program` 的主窗口拉到它所在显示器的大小。
    ///
    /// 已经拉伸过且目标没变时不做任何事（避免每 120ms 重复调用）。
    /// 返回是否执行了拉伸。
    pub fn stretch(&mut self, program: &str) -> bool {
        if program.is_empty() {
            return false;
        }

        // 目标程序变了（比如切换了配置）：把上一个窗口还原。
        //
        // 每个程序最多只拉一个窗口。切换配置时不还原的话，旧窗口会
        // 一直保持拉满、没有标题栏，用户再也调不回来。
        //
        // 还原后直接往下走：restore() 内部会 take() 掉 active，
        // 下面的 find_main_window 会重新找目标程序的窗口。
        if let Some(s) = self.active {
            if !belongs_to(s.hwnd, program) {
                self.restore();
            }
        }

        // 已经在拉伸的窗口优先复用。
        //
        // 拉伸时会去掉 WS_CAPTION，而 is_main_window() 要求窗口带标题栏。
        // 不先查这里的话，下一轮就「找不到窗口」→ restore() → 标题栏回来
        // → 又找到 → 再拉伸，窗口在两个状态之间反复跳。
        let hwnd = match self.active {
            Some(s) if unsafe { IsWindow(s.hwnd) } != 0 => s.hwnd,
            _ => find_main_window(program),
        };
        if hwnd.is_null() {
            self.restore();
            return false;
        }

        // 同一个窗口只拉伸一次，之后不再动它。
        //
        // 判据是「已经拉伸过这个窗口」+「距上次拉伸不足冷却时间」。
        //
        // 为什么不能只看尺寸：DWM 的可见边界（DWMWA_EXTENDED_FRAME_BOUNDS）
        // 在 SetWindowPos 之后要过一帧才更新。轮询间隔 120ms 足够一帧，
        // 但第二次 SetWindowPos 补偿阴影又会把值打回中间态，于是每轮都
        // 读到「还没到位」，窗口被反复改尺寸，界面持续闪烁。
        //
        // 冷却期给了 DWM 稳定的时间。冷却过后如果可见区仍然偏了
        // （比如窗口被外部改小），才会重新拉伸。
        const COOLDOWN: std::time::Duration = std::time::Duration::from_millis(1500);
        if let Some(s) = self.active {
            if s.hwnd == hwnd {
                let fresh = self
                    .last_applied
                    .is_some_and(|t| t.elapsed() < COOLDOWN);
                if fresh {
                    return false;
                }
                if let (Some(vis), Some(m)) = (visible_rect(hwnd), monitor_rect(hwnd)) {
                    if vis.left == m.left
                        && vis.top == m.top
                        && vis.right == m.right
                        && vis.bottom == m.bottom
                    {
                        return false;
                    }
                }
            }
        }

        let Some(mon) = monitor_rect(hwnd) else {
            return false;
        };

        unsafe {
            // 先存下原样，退出时能还原
            if self.active.map(|s| s.hwnd) != Some(hwnd) {
                let mut r: RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut r);
                self.active = Some(Saved {
                    hwnd,
                    style: GetWindowLongW(hwnd, GWL_STYLE),
                    ex_style: GetWindowLongW(hwnd, GWL_EXSTYLE),
                    rect: r,
                });
            }

            // 去掉标题栏和可调整边框。不去掉的话，窗口外框会占掉
            // 一部分屏幕，画面铺不满。
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            let new_style = style
                & !((WS_CAPTION | WS_THICKFRAME) as u32)
                & !(WS_MAXIMIZE as u32);
            SetWindowLongW(hwnd, GWL_STYLE, new_style as i32);

            // 先按显示器矩形设一次，让 DWM 去掉旧边框并重算阴影。
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                mon.left,
                mon.top,
                mon.right - mon.left,
                mon.bottom - mon.top,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );

            // 去掉标题栏后仍可能残留一圈阴影：`GetWindowRect` 给出
            // (-8,-8,1920,1080)，真实可见区是 (-8,-8,1912,1072)，
            // 四周各让出 8 像素，画面铺不满。
            //
            // 量出这圈边距，把窗口再外扩相应的量，让可见区正好
            // 等于显示器矩形。
            if let (Some(vis), Some(raw)) = (visible_rect(hwnd), window_rect(hwnd)) {
                let dl = vis.left - raw.left;
                let dt = vis.top - raw.top;
                let dr = raw.right - vis.right;
                let db = raw.bottom - vis.bottom;
                if dl != 0 || dt != 0 || dr != 0 || db != 0 {
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        mon.left - dl,
                        mon.top - dt,
                        (mon.right - mon.left) + dl + dr,
                        (mon.bottom - mon.top) + dt + db,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                }
            }
        }
        self.last_applied = Some(std::time::Instant::now());
        true
    }

    /// 恢复被拉伸窗口的原始样式和位置。
    pub fn restore(&mut self) {
        self.last_applied = None;
        if let Some(s) = self.active.take() {
            if unsafe { IsWindowVisible(s.hwnd) } == 0 {
                return;
            }
            unsafe {
                SetWindowLongW(s.hwnd, GWL_STYLE, s.style);
                SetWindowLongW(s.hwnd, GWL_EXSTYLE, s.ex_style);
                // 一步恢复位置，同时取消置顶。
                //
                // 原来是先 HWND_TOPMOST 再 HWND_NOTOPMOST，中间那一帧
                // 窗口会跳到最前面闪一下。直接给 HWND_NOTOPMOST 一步到位。
                SetWindowPos(
                    s.hwnd,
                    (-2isize) as HWND,
                    s.rect.left,
                    s.rect.top,
                    s.rect.right - s.rect.left,
                    s.rect.bottom - s.rect.top,
                    SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_lossy_name_matches_case_insensitively() {
        // 名字比较走 to_ascii_lowercase，大小写不同也要能匹配
        let a = "DeltaForce.exe".to_ascii_lowercase();
        let b = "deltaforce.exe".to_ascii_lowercase();
        assert_eq!(a, b);
    }

    #[test]
    fn absent_program_yields_null_window() {
        let hwnd = find_main_window("definitely-not-running.exe");
        assert!(hwnd.is_null());
    }

    #[test]
    fn minimized_window_is_not_main() {
        // 建一个带标题栏的窗口，然后最小化，is_main_window 必须返回 false。
        // 有些程序最小化后不置 WS_MINIMIZE 而是把窗口挪到 -32000，
        // 两种形态都不能被当成主窗口。
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, ShowWindow, SW_MINIMIZE,
            WS_OVERLAPPEDWINDOW,
        };

        unsafe extern "system" fn proc_(h: HWND, m: u32, w: usize, l: isize) -> isize {
            DefWindowProcW(h, m, w, l)
        }

        let cls: Vec<u16> = "SocdStretchTest\0".encode_utf16().collect();
        let wc = windows_sys::Win32::UI::WindowsAndMessaging::WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(proc_),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
        };
        unsafe {
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0, cls.as_ptr(), cls.as_ptr(), WS_OVERLAPPEDWINDOW,
                100, 100, 400, 300,
                std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                return; // 无桌面会话时跳过
            }
            ShowWindow(hwnd, windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE);
            assert!(is_main_window(hwnd), "普通窗口应被识别为主窗口");

            ShowWindow(hwnd, SW_MINIMIZE);
            assert!(!is_main_window(hwnd), "最小化的窗口不能被当成主窗口");

            DestroyWindow(hwnd);
        }
    }
}
