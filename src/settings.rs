//! Settings and mapping data model.
//!
//! File format is intentionally compatible with the original Jai implementation's
//! `settings.socd`, so existing configs keep working.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SETTINGS_VERSION: u32 = 1;

/// Windows scan codes. Extended keys (arrows, numpad) carry the `0xE100` marker
/// that the original used to disambiguate from the colliding non-extended codes.
pub type ScanCode = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MappingMode {
    Remap,
    Opposite,
    OppositeNoRepress,
    Neutral,
}

impl MappingMode {
    pub fn as_settings_str(self) -> &'static str {
        match self {
            MappingMode::Remap => "REMAP",
            MappingMode::Opposite => "OPPOSITE",
            MappingMode::OppositeNoRepress => "OPPOSITE_NO_REPRESS",
            MappingMode::Neutral => "NEUTRAL",
        }
    }

    pub fn from_settings_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "REMAP" => Some(MappingMode::Remap),
            "OPPOSITE" => Some(MappingMode::Opposite),
            "OPPOSITE_NO_REPRESS" => Some(MappingMode::OppositeNoRepress),
            "NEUTRAL" => Some(MappingMode::Neutral),
            _ => None,
        }
    }

    /// Human label shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            MappingMode::Remap => "重映射",
            MappingMode::Opposite => "对向·后按生效",
            MappingMode::OppositeNoRepress => "对向·不重按",
            MappingMode::Neutral => "对向·相互抵消",
        }
    }

    /// One-line explanation shown under the mode picker.
    pub fn hint(self) -> &'static str {
        match self {
            MappingMode::Remap => "按下左键时改为输出右键，原按键被吞掉。",
            MappingMode::Opposite => "两个相反方向同时按住时，后按的那个生效；松开后自动回到先按的方向。",
            MappingMode::OppositeNoRepress => "两个相反方向同时按住时，后按的那个生效；松开后不回到先按的方向。",
            MappingMode::Neutral => "两个相反方向同时按住时互相抵消，两个都不输出。",
        }
    }

    pub fn all() -> [MappingMode; 4] {
        [
            MappingMode::Remap,
            MappingMode::Opposite,
            MappingMode::OppositeNoRepress,
            MappingMode::Neutral,
        ]
    }
}

impl fmt::Display for MappingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_settings_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mapping {
    pub source: ScanCode,
    pub destination: ScanCode,
    pub mode: MappingMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub program: String,
    pub mappings: Vec<Mapping>,
}

impl Profile {
    pub fn new(program: impl Into<String>) -> Self {
        Profile {
            program: program.into(),
            mappings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub profiles: Vec<Profile>,
    /// Index into `profiles` of the profile selected in the UI.
    pub active_profile_index: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            profiles: Vec::new(),
            active_profile_index: 0,
        }
    }
}

impl Settings {
    /// Parses the original `settings.socd` text format.
    pub fn parse(text: &str) -> Result<Settings, String> {
        let mut settings = Settings::default();

        let mut version_ok = false;
        for line in text.lines() {
            if line.trim_start().starts_with('[') {
                let inner = line
                    .trim()
                    .trim_start_matches('[')
                    .split(']')
                    .next()
                    .unwrap_or("");
                match inner.trim().parse::<u32>() {
                    Ok(v) if v == SETTINGS_VERSION => version_ok = true,
                    Ok(v) => return Err(format!("unexpected settings version {v}")),
                    Err(_) => {}
                }
                break;
            }
        }
        if !version_ok {
            return Err("missing settings version header".into());
        }

        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }

            // 顶层 `key: value` 行（含旧版本遗留的 close_to_tray / runtime_debug）
            // 一律跳过；只有 PROFILE_START 之后的内容才进入配置模型
            if line.contains(':') {
                continue;
            }

            if line != "PROFILE_START" {
                continue;
            }

            let mut profile = Profile::new("");
            let mut is_active = false;
            for line in lines.by_ref() {
                let line = line.trim();
                if line == "PROFILE_END" {
                    break;
                }
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim();
                match key.trim().to_ascii_lowercase().as_str() {
                    "active" => is_active = value == "true",
                    "program" => profile.program = value.to_string(),
                    // 旧版本遗留字段：Windows 专用程序，忽略
                    // 旧版本遗留字段：Windows 专用程序，忽略
                    "platform" => {}
                    "mapping" => {
                        // `MODE source_code destination_code`
                        let parts: Vec<&str> = value.split_whitespace().collect();
                        if parts.len() == 3 {
                            if let (Some(mode), Ok(source), Ok(destination)) = (
                                MappingMode::from_settings_str(parts[0]),
                                parts[1].parse::<ScanCode>(),
                                parts[2].parse::<ScanCode>(),
                            ) {
                                profile.mappings.push(Mapping {
                                    source,
                                    destination,
                                    mode,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }

            if is_active {
                settings.active_profile_index = settings.profiles.len();
            }
            settings.profiles.push(profile);
        }

        if settings.profiles.is_empty() {
            settings.profiles.push(Profile::new(""));
        }
        if settings.active_profile_index >= settings.profiles.len() {
            settings.active_profile_index = 0;
        }

        Ok(settings)
    }

    pub fn serialize(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "[{}] # Config version, do not delete this!!! Actually, you're not supposed to modify this by hand at all! You will regret manually editing this file!\n\n",
            SETTINGS_VERSION
        ));
        for (i, profile) in self.profiles.iter().enumerate() {
            out.push_str("PROFILE_START\n");
            if i == self.active_profile_index {
                out.push_str("active: true\n");
            }
            out.push_str(&format!("program: {}\n", profile.program));
            for m in &profile.mappings {
                out.push_str(&format!(
                    "mapping: {} {} {}\n",
                    m.mode.as_settings_str(),
                    m.source,
                    m.destination
                ));
            }
            out.push_str("PROFILE_END\n");
        }

        out
    }

    pub fn load(path: &Path) -> Result<Settings, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
        Settings::parse(&text)
    }

    /// 原子写入：先写同目录的临时文件，再改名覆盖。
    ///
    /// 直接 `fs::write` 会在写到一半崩溃时留下截断的配置，下次启动解析失败，
    /// 用户的全部映射就丢了。改名在同一文件系统内是原子操作。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = self.serialize();

        let tmp = path.with_extension("socd.tmp");
        fs::write(&tmp, &text).map_err(|e| format!("写入 {tmp:?} 失败：{e}"))?;

        // Windows 上 rename 不能覆盖已存在的目标，先删再改名。
        // 中间窗口极短，且此时临时文件已完整，最坏情况是配置回到上一次状态。
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        fs::rename(&tmp, path).map_err(|e| format!("替换 {path:?} 失败：{e}"))
    }
}

/// Settings live next to the executable, like the original.
/// 配置文件位置：数据目录下的 `settings.socd`。
///
/// 数据目录默认在 exe 旁边的 `SOCD` 文件夹（见 `paths` 模块），不可写时
/// 退回用户目录。首次调用会把旧版本的配置搬过来。
pub fn settings_path() -> PathBuf {
    if let Some(dir) = crate::paths::data_dir() {
        let path = dir.join("settings.socd");
        crate::paths::migrate_legacy(&path);
        return path;
    }

    // 连用户目录都拿不到时退到当前工作目录，至少让程序能跑
    PathBuf::from("settings.socd")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut s = Settings::default();
        let mut p = Profile::new("Hollow Knight.exe");
        p.mappings.push(Mapping {
            source: 0x11,
            destination: 0x1F,
            mode: MappingMode::Opposite,
        });
        p.mappings.push(Mapping {
            source: 0x19,
            destination: 0x1F,
            mode: MappingMode::Remap,
        });
        s.profiles.push(p);
        s.profiles.push(Profile::new("celeste.exe"));
        s.active_profile_index = 1;

        let text = s.serialize();
        let parsed = Settings::parse(&text).expect("parse");

        assert_eq!(parsed.profiles.len(), 2);
        assert_eq!(parsed.profiles[0].program, "Hollow Knight.exe");
        assert_eq!(parsed.profiles[0].mappings.len(), 2);
        assert_eq!(parsed.profiles[1].program, "celeste.exe");
        assert_eq!(parsed.active_profile_index, 1);
        assert_eq!(parsed.profiles[0].mappings[0].mode, MappingMode::Opposite);
    }

    #[test]
    fn parses_legacy_header() {
        let text = "[1] # Config version, do not delete this!!!\n\nclose_to_tray: false\nruntime_debug: false\n";
        let s = Settings::parse(text).expect("parse");
        assert_eq!(s.profiles.len(), 1);
        assert_eq!(s.profiles[0].program, "");
    }

    #[test]
    fn rejects_wrong_version() {
        assert!(Settings::parse("[99] # header\n").is_err());
    }
}
