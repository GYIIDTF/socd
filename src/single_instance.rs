//! 单实例保护。
//!
//! 两个进程同时装 `WH_KEYBOARD_LL` 会让同一次按键被两条钩子链处理：按下被
//! 各吞一次、各注入一次，表现为按键重复或丢失。所以第二个实例必须让路。
//!
//! 实现用命名互斥量（`CreateMutexW`）。名字里带用户作用域，不同用户登录同一台
//! 机器时互不干扰。互斥量由内核维护，进程崩溃后自动释放，不会出现“上次没退
//! 干净就再也起不来”的情况。
//!
//! 第二个实例会读取第一个实例公布的界面地址并直接打开，用户体验上等同
//! “再点一次图标就把界面调出来”。
//!
//! 地址通过**命名共享内存**传递，不落盘：进程结束映射随之消失，`Data/` 目录里
//! 不会留下需要清理的临时文件。

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
    FILE_MAP_WRITE, PAGE_READWRITE,
};

const MUTEX_NAME_BASE: &str = "Local\\SOCD_SingleInstance_v1";
const SHARED_NAME_BASE: &str = "Local\\SOCD_UiUrl_v1";
/// 共享内存大小。装一个 `http://127.0.0.1:65535/` 绰绰有余。
const SHARED_BYTES: usize = 128;

const ERROR_ALREADY_EXISTS: u32 = 183;

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attrs: *const c_void, initial_owner: i32, name: *const u16) -> *mut c_void;
}

/// 单实例命名后缀。自动化测试并发跑多个实例时用 `SOCD_INSTANCE_SLOT`
/// 区分；正常运行时用空后缀，保持与旧版本的互斥量名字一致，
/// 避免升级后新旧两个版本同时装钩子。
fn slot() -> &'static str {
    static SLOT: OnceLock<String> = OnceLock::new();
    SLOT.get_or_init(|| {
        std::env::var("SOCD_INSTANCE_SLOT")
            .ok()
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
            .map(|s| format!("_{s}"))
            .unwrap_or_default()
    })
}

fn mutex_name() -> Vec<u16> {
    wide(&format!("{MUTEX_NAME_BASE}{}", slot()))
}

fn shared_name() -> Vec<u16> {
    wide(&format!("{SHARED_NAME_BASE}{}", slot()))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 持有互斥量句柄；进程退出时随进程一并释放，不需要显式关闭。
pub struct InstanceGuard {
    /// 仅用于持有所有权，不读取；Drop 时不关闭（见 acquire 的说明）。
    _handle: *mut c_void,
}

/// 尝试取得单实例所有权。返回 `None` 表示已有实例在运行。
///
/// 注意：句柄故意不关闭——`CreateMutexW` 的所有权绑定在进程上，提前关闭会
/// 让后来者误以为没有实例。进程结束时系统自动回收。
pub fn acquire() -> Option<InstanceGuard> {
    unsafe {
        let name = mutex_name();
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if handle.is_null() {
            // 创建失败（极少见）时宁可放行，也不要让用户完全无法启动
            return Some(InstanceGuard { _handle: std::ptr::null_mut() });
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return None;
        }
        Some(InstanceGuard { _handle: handle })
    }
}

/// 把当前界面地址写进共享内存，供后续实例读取。
///
/// 映射句柄故意泄漏：它必须活到进程退出，否则后来者打开时会看到空内容。
pub fn publish_url(url: &str) {
    let bytes = url.as_bytes();
    if bytes.len() + 1 > SHARED_BYTES {
        return;
    }

    unsafe {
        let name = shared_name();
        let handle: HANDLE = CreateFileMappingW(
            windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            SHARED_BYTES as u32,
            name.as_ptr(),
        );
        if handle.is_null() {
            return;
        }

        let view = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, SHARED_BYTES);
        if view.Value.is_null() {
            return;
        }

        let dst = std::slice::from_raw_parts_mut(view.Value as *mut u8, SHARED_BYTES);
        dst[..bytes.len()].copy_from_slice(bytes);
        dst[bytes.len()] = 0;

        UnmapViewOfFile(view);
        // handle 不关闭：句柄必须活到进程退出，否则后来者打开时会看到空内容
        let _ = handle;
    }
}

/// 读取已有实例公布的界面地址。
///
/// 带重试：第二个实例可能在第一个刚启动、还没发布地址时就开始运行，
/// 单次读取会落空。
pub fn existing_url() -> Option<String> {
    let name = shared_name();

    for _ in 0..30 {
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr());
            if !handle.is_null() {
                let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, SHARED_BYTES);
                if !view.Value.is_null() {
                    let raw = std::slice::from_raw_parts(view.Value as *const u8, SHARED_BYTES);
                    let end = raw.iter().position(|&b| b == 0).unwrap_or(0);
                    let url = String::from_utf8_lossy(&raw[..end]).trim().to_string();
                    UnmapViewOfFile(view);
                    CloseHandle(handle);
                    // 只在看着像本地地址时才用，避免把别处写的内容当命令
                    return url.starts_with("http://127.0.0.1:").then_some(url);
                }
                CloseHandle(handle);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    None
}
