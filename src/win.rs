//! Win32 platform layer: keyboard hook, injection, focus detection, key names.
//!
//! The hook callback runs on an arbitrary thread and must be fast, so it only
//! reads a shared snapshot and pushes actions onto a queue that the UI thread
//! drains. It never allocates or locks anything contended.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::ProcessStatus::K32EnumProcesses;
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId,
    SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use crate::socd::{self, BindTable, Inject, KeyState, Pass};

const MAX_PATH: usize = 260;
/// Windows reports extended keys with a bare scan code that collides with the
/// numpad. We tag them so the two never alias, same trick as the original.
const EXTENDED_TAG: u32 = 0xE100;
const NUMLOCK_SCAN: u32 = 69;

/// A key event observed by the hook, forwarded to the UI as a debug feed.
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub scan: u32,
    pub down: bool,
    pub is_repeat: bool,
    pub swallowed: bool,
}

pub struct HookState {
    /// Grouped so a profile switch swaps table+keys atomically with respect to
    /// the hook thread. Contention is near zero: the hook only takes it when
    /// the state actually changes.
    pub machine: Mutex<Machine>,
    /// Set by the focus watcher; when false the hook passes everything through.
    pub enabled: AtomicBool,
    /// Bumped whenever `table` is replaced, so the hook knows to resync.
    pub generation: AtomicU64,
    /// Debug feed of recent events, drained by the UI.
    pub events: Mutex<Vec<KeyEvent>>,
    pub last_event_scan: AtomicU32,
    pub total_events: AtomicU64,
    /// Test-only: process injected events too, so an automated test can drive
    /// the real hook callback without a physical keyboard. Off in normal runs.
    pub accept_injected: AtomicBool,
}

impl HookState {
    pub fn new() -> Self {
        HookState {
            machine: Mutex::new(Machine::default()),
            enabled: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
            last_event_scan: AtomicU32::new(0),
            total_events: AtomicU64::new(0),
            accept_injected: AtomicBool::new(false),
        }
    }
}

#[derive(Default)]
pub struct Machine {
    pub table: BindTable,
    pub keys: KeyState,
}

/// Owns the installed hook. Dropping it uninstalls.
pub struct HookGuard {
    handle: isize,
}

unsafe impl Send for HookGuard {}

impl Drop for HookGuard {
    fn drop(&mut self) {
        if self.handle != 0 {
            unsafe { UnhookWindowsHookEx(self.handle as _) };
        }
    }
}

static STATE: Mutex<Option<Arc<HookState>>> = Mutex::new(None);

/// Installs the global low-level keyboard hook.
pub fn install(state: Arc<HookState>) -> Result<HookGuard, String> {
    *STATE.lock().unwrap() = Some(state);

    unsafe {
        let module: HINSTANCE = GetModuleHandleW(std::ptr::null());
        let handle = SetWindowsHookExW(WH_KEYBOARD_LL as i32, Some(hook_proc), module, 0);
        if handle.is_null() {
            return Err("SetWindowsHookExW failed".into());
        }
        Ok(HookGuard { handle: handle as isize })
    }
}

unsafe extern "system" fn hook_proc(n_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if n_code != HC_ACTION as i32 {
        return CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam);
    }

    let Some(state) = current_state() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam) };
    };

    let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };

    // Our own injections come back through this hook; let them pass untouched.
    // (Unless a test asked us to accept them, which is how the hook path gets
    // exercised without a physical keyboard.)
    if kb.flags & LLKHF_INJECTED != 0 && !state.accept_injected.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam) };
    }

    // NumLock chatter around numpad keys carries no user intent.
    if kb.scanCode == NUMLOCK_SCAN {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam) };
    }

    let is_down = wparam as u32 == WM_KEYDOWN as u32 || wparam as u32 == WM_SYSKEYDOWN as u32;

    let mut scan = kb.scanCode;
    if kb.flags & LLKHF_EXTENDED != 0 {
        scan |= EXTENDED_TAG;
    }

    if !state.enabled.load(Ordering::Relaxed) {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam) };
    }

    // If the UI thread is mid-swap we skip this event rather than block input.
    let (outcome, is_repeat) = match state.machine.try_lock() {
        Ok(mut m) => {
            let repeat = m.keys.is_down(scan) && is_down;
            let Machine { table, keys } = &mut *m;
            let out = socd::handle(table, keys, scan, is_down, repeat);
            // 在同一次加锁里登记注入状态：钩子关闭时要靠它补发 up。
            for a in &out.actions {
                match a {
                    Inject::Down(c) => keys.note_down(*c),
                    Inject::Up(c) => keys.note_up(*c),
                }
            }
            (out, repeat)
        }
        Err(TryLockError::WouldBlock) => {
            return unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam) };
        }
        Err(TryLockError::Poisoned(p)) => {
            let mut m = p.into_inner();
            let repeat = m.keys.is_down(scan) && is_down;
            let Machine { table, keys } = &mut *m;
            let out = socd::handle(table, keys, scan, is_down, repeat);
            for a in &out.actions {
                match a {
                    Inject::Down(c) => keys.note_down(*c),
                    Inject::Up(c) => keys.note_up(*c),
                }
            }
            (out, repeat)
        }
    };

    for action in &outcome.actions {
        let (code, flags_up) = match action {
            Inject::Down(c) => (*c, false),
            Inject::Up(c) => (*c, true),
        };
        inject_scancode(code, flags_up);
    }

    let swallowed = outcome.pass == Pass::Swallow;

    // Debug feed. Bounded so a stalled UI can never grow memory without limit.
    if let Ok(mut ev) = state.events.try_lock() {
        if ev.len() < 256 {
            ev.push(KeyEvent {
                scan,
                down: is_down,
                is_repeat,
                swallowed,
            });
        }
    }
    state.last_event_scan.store(scan, Ordering::Relaxed);
    state.total_events.fetch_add(1, Ordering::Relaxed);

    if swallowed {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), n_code, wparam, lparam) }
    }
}

/// 补发所有"已注入按下、尚未松开"的目标键。
///
/// 钩子即将被禁用时必须调用：禁用后物理键的 keyup 不再经过我们，
/// 被注入的键就没有机会收到 up，会在系统里一直保持按下。
pub fn release_injected(state: &HookState) {
    let mut m = state.machine.lock().unwrap();
    for code in m.keys.take_pending() {
        inject_scancode(code, true);
    }
}

fn current_state() -> Option<Arc<HookState>> {
    // try_lock: a contended lock means the UI is mid-swap, so skip this event
    // rather than stall the input pipeline.
    STATE.try_lock().ok().and_then(|g| g.clone())
}

fn inject_scancode(scan: u32, key_up: bool) {
    let extended = scan & EXTENDED_TAG != 0;
    let code = (scan & 0xFF) as u16;

    let mut flags = KEYEVENTF_SCANCODE;
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }

    unsafe {
        let mut input: INPUT = std::mem::zeroed();
        input.r#type = INPUT_KEYBOARD;
        input.Anonymous.ki = KEYBDINPUT {
            wVk: 0,
            wScan: code,
            dwFlags: flags,
            time: 0,
            dwExtraInfo: 0,
        };
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Converts a browser virtual-key code (`KeyboardEvent.keyCode`) into the
/// scan code this program uses everywhere else.
///
/// The page cannot know scan codes, and `MapVirtualKey` alone cannot tell an
/// arrow key from the numpad clone, so extended keys are handled explicitly.
pub fn vk_to_scan(vk: u32) -> Option<u32> {
    const EXTENDED_VKS: &[u32] = &[
        0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, // pgup pgdn end home arrows? (see below)
        0x2D, 0x2E, // insert delete
        0x90, 0x2C, 0x13, 0x91, // numlock printscreen pause scrolllock
        0x6F, 0x0D, 0xA3, 0xA5, 0x5B, 0x5C, // numpad / enter rctrl ralt lwin rwin
    ];
    // 0x25-0x28 are the arrow keys and are extended.
    let _ = EXTENDED_VKS;

    let extended = matches!(
        vk,
        0x21..=0x28 | 0x2D..=0x2E | 0x90 | 0x2C | 0x13 | 0x91 | 0x6F | 0xA3 | 0xA5 | 0x5B | 0x5C
    );

    let scan = unsafe {
        windows_sys::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)
    };
    if scan == 0 {
        return None;
    }
    Some(if extended { scan | EXTENDED_TAG } else { scan })
}

const MAPVK_VK_TO_VSC: u32 = 0;

/// Reads the timestamp field out of a `KBDLLHOOKSTRUCT`; kept for parity with
/// the debug view's ordering needs.
pub fn key_name(scan: u32) -> String {
    let extended = scan & EXTENDED_TAG != 0;
    let lparam = (scan & 0xFF) << 16 | if extended { 1 << 24 } else { 0 };
    let mut buf = [0u16; 128];

    let len = unsafe { GetKeyNameTextW(lparam as i32, buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return format!("0x{scan:X}");
    }
    let raw = String::from_utf16_lossy(&buf[..len as usize]);
    localize_key_name(&raw)
}

/// Windows hands back English key names for the non-printing keys. Letter and
/// digit keys already read fine, so only the names in this table are swapped.
fn localize_key_name(raw: &str) -> String {
    match raw {
        "Space" => "空格".into(),
        "Enter" => "回车".into(),
        "Esc" => "Esc".into(),
        "Escape" => "Esc".into(),
        "Tab" => "Tab".into(),
        "Backspace" => "退格".into(),
        "Caps Lock" => "大写锁定".into(),
        "Num Lock" => "数字锁定".into(),
        "Scroll Lock" => "滚动锁定".into(),
        "Print Screen" => "截屏".into(),
        "Pause" => "暂停".into(),
        "Insert" => "插入".into(),
        "Delete" => "删除".into(),
        "Home" => "Home".into(),
        "End" => "End".into(),
        "Page Up" => "上翻页".into(),
        "Page Down" => "下翻页".into(),
        "Left" => "←".into(),
        "Right" => "→".into(),
        "Up" => "↑".into(),
        "Down" => "↓".into(),
        "Left Shift" => "左 Shift".into(),
        "Right Shift" => "右 Shift".into(),
        "Left Ctrl" => "左 Ctrl".into(),
        "Right Ctrl" => "右 Ctrl".into(),
        "Left Alt" => "左 Alt".into(),
        "Right Alt" => "右 Alt".into(),
        "Ctrl" => "Ctrl".into(),
        "Shift" => "Shift".into(),
        "Alt" => "Alt".into(),
        "Win" => "Win".into(),
        "Applications" => "菜单键".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::localize_key_name;

    #[test]
    fn letters_and_digits_pass_through() {
        assert_eq!(localize_key_name("W"), "W");
        assert_eq!(localize_key_name("F5"), "F5");
        assert_eq!(localize_key_name("1"), "1");
    }

    #[test]
    fn named_keys_are_translated() {
        assert_eq!(localize_key_name("Space"), "空格");
        assert_eq!(localize_key_name("Enter"), "回车");
        assert_eq!(localize_key_name("Left"), "←");
        assert_eq!(localize_key_name("Up"), "↑");
        assert_eq!(localize_key_name("Page Down"), "下翻页");
    }

    #[test]
    fn unknown_names_survive_unchanged() {
        assert_eq!(localize_key_name("Some Vendor Key"), "Some Vendor Key");
    }
}

/// Returns the basename of the executable owning the foreground window, or
/// `None` when nothing is focused (or the process is not readable).
/// 最近一次查询的前台窗口及其程序名。
///
/// 调用方每 120ms 轮询一次，而 `OpenProcess` + `QueryFullProcessImageNameW`
/// 是两次系统调用。窗口没变时没必要重查，缓存一次即可省掉绝大部分开销。
static LAST_FG: Mutex<Option<(usize, Option<String>)>> = Mutex::new(None);

pub fn foreground_program() -> Option<String> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }

        let key = hwnd as usize;
        // 命中缓存就直接返回，连锁都是快速的
        if let Ok(cache) = LAST_FG.lock() {
            if let Some((cached_hwnd, ref name)) = *cache {
                if cached_hwnd == key {
                    return name.clone();
                }
            }
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            // Windows reports the idle process for a brief moment while a window
            // minimizes or is destroyed.
            return None;
        }

        let name = process_exe_name(pid);

        if let Ok(mut cache) = LAST_FG.lock() {
            *cache = Some((key, name.clone()));
        }
        name
    }
}

fn process_exe_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        if handle.is_null() {
            return None;
        }

        let mut buf = [0u16; MAX_PATH];
        let mut size = MAX_PATH as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut size);
        windows_sys::Win32::Foundation::CloseHandle(handle);

        if ok == 0 || size == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        Some(
            path.rsplit(['\\', '/'])
                .next()
                .unwrap_or(&path)
                .to_string(),
        )
    }
}

/// Enumerates running executables, for the "add profile" picker.
pub fn running_programs() -> Vec<String> {
    let mut pids = vec![0u32; 4096];
    let mut needed: u32 = 0;

    unsafe {
        let ok = K32EnumProcesses(
            pids.as_mut_ptr(),
            (pids.len() * std::mem::size_of::<u32>()) as u32,
            &mut needed,
        );
        if ok == 0 {
            return Vec::new();
        }
        pids.truncate(needed as usize / std::mem::size_of::<u32>());
    }

    let mut names: Vec<String> = pids
        .into_iter()
        .filter(|&pid| pid != 0)
        .filter_map(process_exe_name)
        .collect();

    names.sort_by_key(|n| n.to_ascii_lowercase());
    names.dedup();
    names
}
