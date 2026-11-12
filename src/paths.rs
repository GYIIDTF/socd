//! 程序数据目录。
//!
//! 配置放在 exe 旁边的 `Data` 文件夹里，这样整个程序是自包含的：拷贝、备份、
//! 换机器都只涉及一个目录。
//!
//! 代价是程序必须放在可写位置。`Program Files` 这类受保护目录下写不进去，
//! 此时会退回 `%LOCALAPPDATA%\Data`，并且界面上的保存错误会如实提示用户。

use std::path::PathBuf;

/// exe 同目录下的数据文件夹名。
const DATA_DIR_NAME: &str = "Data";

/// 旧版本用的目录名，用于一次性迁移。
const LEGACY_DIR_NAMES: &[&str] = &["Hitboxer", "SOCD"];

/// 数据目录。优先 exe 旁边的 `Data`；不可写时退回 `%LOCALAPPDATA%\Data`。
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = beside_exe() {
        if ensure_writable(&dir) {
            return Some(dir);
        }
    }
    fallback_dir()
}

fn beside_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.join(DATA_DIR_NAME))
}

fn fallback_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)?;
    let dir = base.join(DATA_DIR_NAME);
    ensure_writable(&dir).then_some(dir)
}

/// 目录存在且能写入。新建目录也算可写。
fn ensure_writable(dir: &PathBuf) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    // 真正试探一次写入：只检查权限位在 Windows 上不可靠
    let probe = dir.join(".write-test");
    match std::fs::write(&probe, b"") {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 把旧版本留在别处的配置搬进新目录。
///
/// 只在新目录还没有配置文件时执行，且失败不影响启动。
pub fn migrate_legacy(new_file: &std::path::Path) {
    if new_file.exists() {
        return;
    }

    // 1) 上一版把配置放在 exe 旁的 SOCD\ 子目录
    // 2) 更早的版本直接放在 exe 同目录
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        for candidate in [exe_dir.join("SOCD").join("settings.socd"), exe_dir.join("settings.socd")] {
            if candidate.exists() {
                let _ = std::fs::copy(&candidate, new_file);
                return;
            }
        }
    }

    // 2) 再早的版本放在 %LOCALAPPDATA%\<旧名>\
    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        for name in LEGACY_DIR_NAMES {
            let old = base.join(name).join("settings.socd");
            if old.exists() {
                let _ = std::fs::copy(&old, new_file);
                return;
            }
        }
    }
}

/// 旧版本在数据目录里留下的界面地址文件。
///
/// 现在地址走命名共享内存（见 `single_instance`），不再落盘；这里保留一个
/// 清理入口，启动时删掉旧版本残留，`Data/` 里就只剩配置文件。
pub fn remove_legacy_url_file() {
    if let Some(dir) = data_dir() {
        let _ = std::fs::remove_file(dir.join("ui-url.txt"));
    }
}

