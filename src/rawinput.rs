//! 准星快捷键的原始输入监听。
//!
//! 用 Raw Input（`RIDEV_INPUTSINK`）而不是低级键盘钩子：
//!   * 不吞输入，不参与键盘钩子链——反作弊对 `WH_KEYBOARD_LL` 更敏感
//!   * 能同时拿到键盘和鼠标键
//!   * 只处理注册的那个设备类别，开销极小
//!
//! `RIDEV_INPUTSINK` 让窗口在后台也能收到 `WM_INPUT`，不需要焦点。
//!
//! 约束：`WM_INPUT` 只会投递到**注册该设备的窗口所属线程**的消息队列。
//! 准星窗口建在主线程，主线程跑 `GetMessageW` + `DispatchMessageW`，
//! 所以窗口过程能收到。

use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RIDEV_REMOVE, RID_INPUT,
};

// windows-sys 0.59 没有导出这些常量，按 Windows SDK 的定义写死。
// 引用自 winuser.h: 键盘标志用位 0/1，鼠标标记是 usButtonFlags 的位。
const RIM_TYPEMOUSE: u32 = 0;
const RIM_TYPEKEYBOARD: u32 = 1;
const RI_KEY_BREAK: u16 = 1;   // 松开
const RI_KEY_E0: u16 = 2;      // 扩展键前缀
const RI_MOUSE_LEFT_BUTTON_DOWN: u16 = 0x0001;
const RI_MOUSE_LEFT_BUTTON_UP: u16 = 0x0002;
const RI_MOUSE_RIGHT_BUTTON_DOWN: u16 = 0x0004;
const RI_MOUSE_RIGHT_BUTTON_UP: u16 = 0x0008;
const RI_MOUSE_MIDDLE_BUTTON_DOWN: u16 = 0x0010;
const RI_MOUSE_MIDDLE_BUTTON_UP: u16 = 0x0020;
const RI_MOUSE_BUTTON_4_DOWN: u16 = 0x0040;
const RI_MOUSE_BUTTON_4_UP: u16 = 0x0080;
const RI_MOUSE_BUTTON_5_DOWN: u16 = 0x0100;
const RI_MOUSE_BUTTON_5_UP: u16 = 0x0200;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_INPUT;

use crate::settings::{Hotkey, HotkeyKind};

/// 鼠标键编号，和 `Hotkey.code` 对应。
pub const MOUSE_LEFT: u16 = 0;
pub const MOUSE_RIGHT: u16 = 1;
pub const MOUSE_MIDDLE: u16 = 2;
pub const MOUSE_SIDE1: u16 = 3;
pub const MOUSE_SIDE2: u16 = 4;

/// 按键状态变化，交给主线程处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub kind: HotkeyKind,
    pub code: u16,
    pub down: bool,
    /// 哪个快捷键触发的
    pub slot: HotkeySlot,
}

/// 两个快捷键各管一摊：Enable 是总闸，Show 控制显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeySlot {
    /// 准星是否显示（临时，不写盘）
    Show,
    /// 准星功能是否启用（写盘）
    Enable,
}

/// 钩子状态的共享位：窗口过程把事件写进来，主线程取走。
/// 用 `Mutex<Vec>` 而不是队列库，事件极少（只在按开关键时）。
pub struct RawInputState {
    /// 最近一次注册是否成功，供界面诊断
    pub last_ok: AtomicBool,
    /// 收到的 WM_INPUT 总数（诊断用）
    pub input_count: std::sync::atomic::AtomicU64,
    /// 解析出的事件数（诊断用）
    pub event_count: std::sync::atomic::AtomicU64,
    /// 当前监听的两个快捷键。任一变化时重新注册设备。
    pub hotkey: std::sync::Mutex<Option<Hotkey>>,
    pub enable_hotkey: std::sync::Mutex<Option<Hotkey>>,
    /// 已到达的事件，由主线程 drain。
    pub events: std::sync::Mutex<Vec<KeyEvent>>,
    /// 是否已经注册过设备。
    pub registered_kb: AtomicBool,
    pub registered_mouse: AtomicBool,
    /// 上次的按键状态，用来过滤重复的 WM_INPUT。
    pub last_code: AtomicU16,
    pub last_down: AtomicBool,
    pub last_slot: std::sync::atomic::AtomicU8,
}

impl Default for RawInputState {
    fn default() -> Self {
        RawInputState {
            last_ok: AtomicBool::new(false),
            input_count: std::sync::atomic::AtomicU64::new(0),
            event_count: std::sync::atomic::AtomicU64::new(0),
            hotkey: std::sync::Mutex::new(None),
            enable_hotkey: std::sync::Mutex::new(None),
            events: std::sync::Mutex::new(Vec::new()),
            registered_kb: AtomicBool::new(false),
            registered_mouse: AtomicBool::new(false),
            last_code: AtomicU16::new(0),
            last_down: AtomicBool::new(false),
            last_slot: std::sync::atomic::AtomicU8::new(255),
        }
    }
}

/// 按当前快捷键重新注册输入设备。
///
/// 只注册需要的类别：快捷键是键盘就只收键盘，是鼠标就只收鼠标。
/// 切换类别时先移除旧的，避免同时收两类事件。
pub fn reconfigure(
    hwnd: HWND,
    state: &RawInputState,
    hotkey: Option<Hotkey>,
    enable_hotkey: Option<Hotkey>,
) {
    if let Ok(mut cur) = state.hotkey.lock() {
        *cur = hotkey;
    }
    if let Ok(mut cur) = state.enable_hotkey.lock() {
        *cur = enable_hotkey;
    }

    // 两个键可能一个键盘一个鼠标，哪类有需求就注册哪类
    let want_kb = [hotkey, enable_hotkey]
        .iter()
        .flatten()
        .any(|h| h.kind == HotkeyKind::Keyboard);
    let want_mouse = [hotkey, enable_hotkey]
        .iter()
        .flatten()
        .any(|h| h.kind == HotkeyKind::Mouse);

    let kb_on = state.registered_kb.swap(want_kb, Ordering::Relaxed);
    let mouse_on = state.registered_mouse.swap(want_mouse, Ordering::Relaxed);

    let mut ok = true;
    if kb_on != want_kb {
        ok &= set_device(hwnd, true, want_kb);
    }
    if mouse_on != want_mouse {
        ok &= set_device(hwnd, false, want_mouse);
    }
    state.last_ok.store(ok, Ordering::Relaxed);
    crate::logging::warn(&format!(
        "准星快捷键注册: hwnd={:?} kb={} mouse={} ok={}",
        hwnd, want_kb, want_mouse, ok
    ));
}

fn set_device(hwnd: HWND, keyboard: bool, enable: bool) -> bool {
    let usage_page = if keyboard { 0x01 } else { 0x01 };
    let usage = if keyboard { 0x06 } else { 0x02 }; // 键盘=6，鼠标=2

    let flags = if enable {
        0x00000100 // RIDEV_INPUTSINK：后台也能收到
    } else {
        RIDEV_REMOVE
    };

    let device = RAWINPUTDEVICE {
        usUsagePage: usage_page,
        usUsage: usage,
        dwFlags: flags,
        hwndTarget: if enable { hwnd } else { std::ptr::null_mut() },
    };

    unsafe {
        let ok = RegisterRawInputDevices(
            &device,
            1,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        );
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            crate::logging::warn(&format!(
                "RegisterRawInputDevices 失败 (kind={} enable={}): {err}",
                if keyboard { "kb" } else { "mouse" },
                enable
            ));
        }
        ok != 0
    }
}

/// 窗口过程里调用：解析 `WM_INPUT`，把匹配的事件推进队列。
pub fn handle_wm_input(lparam: LPARAM, state: &RawInputState) {
    state.input_count.fetch_add(1, Ordering::Relaxed);
    let show_key = state.hotkey.lock().ok().and_then(|h| *h);
    let enable_key = state.enable_hotkey.lock().ok().and_then(|h| *h);
    if show_key.is_none() && enable_key.is_none() {
        return;
    }

    // 两个键里找匹配的，带回槽位
    let matches = |kind: HotkeyKind, code: u16| -> Option<HotkeySlot> {
        if show_key.is_some_and(|k| k.kind == kind && k.code == code) {
            return Some(HotkeySlot::Show);
        }
        if enable_key.is_some_and(|k| k.kind == kind && k.code == code) {
            return Some(HotkeySlot::Enable);
        }
        None
    };

    let mut size: u32 = 0;
    unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            std::ptr::null_mut(),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
    }
    if size == 0 || size > 1024 {
        return;
    }

    let mut buf = [0u8; 1024];
    let read = unsafe {
        GetRawInputData(
            lparam as HRAWINPUT,
            RID_INPUT,
            buf.as_mut_ptr() as *mut _,
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if read == u32::MAX || read == 0 {
        return;
    }

    let raw = unsafe { &*(buf.as_ptr() as *const RAWINPUT) };
    let kind = unsafe { raw.header.dwType };

    let event = if kind == RIM_TYPEKEYBOARD {
        let kb = unsafe { &raw.data.keyboard };
        // 扫描码 + 扩展标记，和系统的扫描码约定一致
        let mut code = kb.MakeCode as u16;
        if kb.Flags & RI_KEY_E0 != 0 {
            code |= 0xE100;
        }
        let down = kb.Flags & RI_KEY_BREAK == 0;
        let Some(slot) = matches(HotkeyKind::Keyboard, code) else { return };
        KeyEvent { kind: HotkeyKind::Keyboard, code, down, slot }
    } else if kind == RIM_TYPEMOUSE {
        let mouse = unsafe { &raw.data.mouse };
        let low = unsafe { mouse.Anonymous.Anonymous.usButtonFlags };

        let (code, down) = if low & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
            (MOUSE_LEFT, true)
        } else if low & RI_MOUSE_LEFT_BUTTON_UP != 0 {
            (MOUSE_LEFT, false)
        } else if low & RI_MOUSE_RIGHT_BUTTON_DOWN != 0 {
            (MOUSE_RIGHT, true)
        } else if low & RI_MOUSE_RIGHT_BUTTON_UP != 0 {
            (MOUSE_RIGHT, false)
        } else if low & RI_MOUSE_MIDDLE_BUTTON_DOWN != 0 {
            (MOUSE_MIDDLE, true)
        } else if low & RI_MOUSE_MIDDLE_BUTTON_UP != 0 {
            (MOUSE_MIDDLE, false)
        } else if low & RI_MOUSE_BUTTON_4_DOWN != 0 {
            (MOUSE_SIDE1, true)
        } else if low & RI_MOUSE_BUTTON_4_UP != 0 {
            (MOUSE_SIDE1, false)
        } else if low & RI_MOUSE_BUTTON_5_DOWN != 0 {
            (MOUSE_SIDE2, true)
        } else if low & RI_MOUSE_BUTTON_5_UP != 0 {
            (MOUSE_SIDE2, false)
        } else {
            return;
        };

        let Some(slot) = matches(HotkeyKind::Mouse, code) else { return };
        KeyEvent { kind: HotkeyKind::Mouse, code, down, slot }
    } else {
        return;
    };

    // 有些设备会重复上报同一状态，过滤掉
    let same = state.last_code.load(Ordering::Relaxed) == event.code
        && state.last_down.load(Ordering::Relaxed) == event.down
        && state.last_slot.load(Ordering::Relaxed) == event.slot as u8;
    if same {
        return;
    }
    state.last_code.store(event.code, Ordering::Relaxed);
    state.last_down.store(event.down, Ordering::Relaxed);
    state.last_slot.store(event.slot as u8, Ordering::Relaxed);

    if let Ok(mut q) = state.events.lock() {
        if q.len() < 32 {
            q.push(event);
            state.event_count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 取走所有待处理事件。
pub fn drain(state: &RawInputState) -> Vec<KeyEvent> {
    state
        .events
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

pub fn wm_input() -> u32 {
    WM_INPUT
}
