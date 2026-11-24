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

/// 准星样式。形状是固定几种，颜色和尺寸可调。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CrosshairStyle {
    /// 四段短线，中心留空
    Cross,
    /// 单点
    Dot,
    /// 空心圆环
    Ring,
    /// T 形：上左右三向，无下
    TShape,
}

impl CrosshairStyle {
    pub fn as_settings_str(self) -> &'static str {
        match self {
            CrosshairStyle::Cross => "CROSS",
            CrosshairStyle::Dot => "DOT",
            CrosshairStyle::Ring => "RING",
            CrosshairStyle::TShape => "T_SHAPE",
        }
    }

    pub fn from_settings_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "CROSS" => Some(CrosshairStyle::Cross),
            "DOT" => Some(CrosshairStyle::Dot),
            "RING" => Some(CrosshairStyle::Ring),
            // 兼容早期写错的 T 形拼写
            "TSHAPE" | "T_SHAPE" => Some(CrosshairStyle::TShape),
            _ => None,
        }
    }
}

/// 快捷键来源：键盘还是鼠标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HotkeyKind {
    Keyboard,
    Mouse,
}

impl HotkeyKind {
    pub fn as_settings_str(self) -> &'static str {
        match self {
            HotkeyKind::Keyboard => "KEYBOARD",
            HotkeyKind::Mouse => "MOUSE",
        }
    }
    pub fn from_settings_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "KEYBOARD" => Some(HotkeyKind::Keyboard),
            "MOUSE" => Some(HotkeyKind::Mouse),
            _ => None,
        }
    }
}

/// 一个快捷键。键盘存扫描码，鼠标存按键位（0=左 1=右 2=中 3/4=侧键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    pub kind: HotkeyKind,
    pub code: u16,
}

/// 快捷键的触发方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HotkeyMode {
    /// 按一下切换显示/隐藏
    Toggle,
    /// 按住时隐藏，松开恢复
    HoldToHide,
    /// 按住时显示，松开隐藏
    HoldToShow,
}

impl HotkeyMode {
    pub fn as_settings_str(self) -> &'static str {
        match self {
            HotkeyMode::Toggle => "TOGGLE",
            HotkeyMode::HoldToHide => "HOLD_TO_HIDE",
            HotkeyMode::HoldToShow => "HOLD_TO_SHOW",
        }
    }
    pub fn from_settings_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "TOGGLE" => Some(HotkeyMode::Toggle),
            "HOLD_TO_HIDE" => Some(HotkeyMode::HoldToHide),
            "HOLD_TO_SHOW" => Some(HotkeyMode::HoldToShow),
            // 旧配置里可能还有 ALWAYS（已合并到 Toggle），读成 Toggle
            "ALWAYS" => Some(HotkeyMode::Toggle),
            _ => None,
        }
    }
}

/// 准星设置。挂在 profile 上，跟着前台程序自动切换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crosshair {
    pub enabled: bool,
    pub style: CrosshairStyle,
    /// RGB，各 0-255
    pub color: [u8; 3],
    /// 线宽，像素
    pub thickness: u32,
    /// 中心到第一段的距离，像素
    pub gap: u32,
    /// 每段长度，像素（圆环模式下是半径）
    pub size: u32,
    /// 不透明度 0-100
    pub opacity: u8,
    /// 描边宽度，0 表示不加描边。深色背景上纯色准星容易看不见。
    pub outline: u32,
    /// 描边不透明度 0-100。独立于主体，方便在亮背景上把描边压淡。
    pub outline_opacity: u8,
    /// 中心点。和主体分开控制——FPS 里常关掉主体只留一个点。
    pub center_dot: bool,
    /// 中心点直径，像素
    pub center_dot_size: u32,
    /// 相对显示器中心的偏移，物理像素。正 X 向右，正 Y 向下。
    pub offset_x: i32,
    pub offset_y: i32,
}

impl Default for Crosshair {
    fn default() -> Self {
        Crosshair {
            enabled: false,
            style: CrosshairStyle::Cross,
            // 默认白色。预设色板里的第一种，和界面一致。
            // 觉得白在亮背景糊的话，可以在界面上换成绿/黄/橙。
            color: [255, 255, 255],
            thickness: 2,
            gap: 4,
            size: 10,
            opacity: 100,
            outline: 1,
            outline_opacity: 70,
            center_dot: false,
            center_dot_size: 2,
            offset_x: 0,
            offset_y: 0,
        }
    }
}

impl Crosshair {
    pub fn is_visible(&self) -> bool {
        self.enabled && self.opacity > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub program: String,
    pub mappings: Vec<Mapping>,
    pub crosshair: Crosshair,
    /// 该程序在前台时，把它的窗口拉满整个显示器。
    ///
    /// 用于无边框全屏下显卡缩放失效的情况：由 DWM 替我们做拉伸。
    pub stretch_window: bool,
}

impl Profile {
    pub fn new(program: impl Into<String>) -> Self {
        Profile {
            program: program.into(),
            mappings: Vec::new(),
            stretch_window: false,
            crosshair: Crosshair::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub profiles: Vec<Profile>,
    /// Index into `profiles` of the profile selected in the UI.
    pub active_profile_index: usize,
    /// 准星快捷键。全局设置——它控制的是"怎么开关"，不是"显示什么"，
    /// 所以不跟 profile 走。
    pub hotkey: Option<Hotkey>,
    /// 切换"启用"的快捷键。和 hotkey（显示开关）是两个独立的键。
    pub enable_hotkey: Option<Hotkey>,
    pub hotkey_mode: HotkeyMode,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            profiles: Vec::new(),
            active_profile_index: 0,
            hotkey: None,
            enable_hotkey: None,
            hotkey_mode: HotkeyMode::Toggle,
        }
    }
}

impl Settings {
    /// Parses the original `settings.socd` text format.
    pub fn parse(text: &str) -> Result<Settings, String> {
        let mut settings = Settings::default();
        let mut hotkey: Option<Hotkey> = None;
        let mut enable_hotkey: Option<Hotkey> = None;
        let mut hotkey_mode = HotkeyMode::Toggle;

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

            // 顶层 `key: value` 行。准星快捷键在这里解析，
            // 其余（含旧版本遗留的 close_to_tray / runtime_debug）跳过。
            if let Some((k, v)) = line.split_once(':') {
                let v = v.trim();
                match k.trim().to_ascii_lowercase().as_str() {
                    "hotkey" => {
                        let parts: Vec<&str> = v.split_whitespace().collect();
                        if parts.len() == 2 {
                            if let (Some(kind), Ok(code)) = (
                                HotkeyKind::from_settings_str(parts[0]),
                                parts[1].parse::<u16>(),
                            ) {
                                hotkey = Some(Hotkey { kind, code });
                            }
                        }
                    }
                    "enable_hotkey" => {
                        let parts: Vec<&str> = v.split_whitespace().collect();
                        if parts.len() == 2 {
                            if let (Some(kind), Ok(code)) = (
                                HotkeyKind::from_settings_str(parts[0]),
                                parts[1].parse::<u16>(),
                            ) {
                                enable_hotkey = Some(Hotkey { kind, code });
                            }
                        }
                    }
                    "hotkey_mode" => {
                        if let Some(m) = HotkeyMode::from_settings_str(v) {
                            hotkey_mode = m;
                        }
                    }
                    _ => {}
                }
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
                    "stretch_window" => profile.stretch_window = value == "true",
                    // 旧版本遗留字段：Windows 专用程序，忽略
                    "platform" => {}
                    // 准星：一个字段一行，逐项覆盖默认值。
                    // 旧配置没有这些行，保留默认值即可。
                    "crosshair" => {
                        profile.crosshair.enabled = value == "true";
                    }
                    "crosshair_style" => {
                        if let Some(v) = CrosshairStyle::from_settings_str(value) {
                            profile.crosshair.style = v;
                        }
                    }
                    "crosshair_color" => {
                        let parts: Vec<u8> = value
                            .split_whitespace()
                            .filter_map(|x| x.parse().ok())
                            .collect();
                        if parts.len() == 3 {
                            profile.crosshair.color = [parts[0], parts[1], parts[2]];
                        }
                    }
                    "crosshair_thickness" => {
                        if let Ok(v) = value.parse::<u32>() {
                            profile.crosshair.thickness = v.clamp(1, 20);
                        }
                    }
                    "crosshair_gap" => {
                        if let Ok(v) = value.parse::<u32>() {
                            profile.crosshair.gap = v.clamp(0, 100);
                        }
                    }
                    "crosshair_size" => {
                        if let Ok(v) = value.parse::<u32>() {
                            profile.crosshair.size = v.clamp(1, 100);
                        }
                    }
                    "crosshair_opacity" => {
                        if let Ok(v) = value.parse::<u8>() {
                            profile.crosshair.opacity = v.min(100);
                        }
                    }
                    "crosshair_outline" => {
                        if let Ok(v) = value.parse::<u32>() {
                            profile.crosshair.outline = v.min(10);
                        }
                    }
                    "crosshair_outline_opacity" => {
                        if let Ok(v) = value.parse::<u8>() {
                            profile.crosshair.outline_opacity = v.min(100);
                        }
                    }
                    "crosshair_center_dot" => {
                        profile.crosshair.center_dot = value == "true";
                    }
                    "crosshair_center_dot_size" => {
                        if let Ok(v) = value.parse::<u32>() {
                            profile.crosshair.center_dot_size = v.clamp(1, 20);
                        }
                    }
                    "crosshair_offset_x" => {
                        if let Ok(v) = value.parse::<i32>() {
                            profile.crosshair.offset_x = v.clamp(-4000, 4000);
                        }
                    }
                    "crosshair_offset_y" => {
                        if let Ok(v) = value.parse::<i32>() {
                            profile.crosshair.offset_y = v.clamp(-4000, 4000);
                        }
                    }
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

        settings.hotkey = hotkey;
        settings.enable_hotkey = enable_hotkey;
        settings.hotkey_mode = hotkey_mode;

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

        // 全局准星快捷键。放在所有 profile 之前——它不属于任何配置，
        // 控制的是"怎么开关"而不是"显示什么"。
        if let Some(hk) = self.hotkey {
            out.push_str(&format!(
                "hotkey: {} {}\n",
                hk.kind.as_settings_str(),
                hk.code
            ));
        }
        if let Some(hk) = self.enable_hotkey {
            out.push_str(&format!(
                "enable_hotkey: {} {}\n",
                hk.kind.as_settings_str(),
                hk.code
            ));
        }
        if self.hotkey_mode != HotkeyMode::Toggle {
            out.push_str(&format!(
                "hotkey_mode: {}\n",
                self.hotkey_mode.as_settings_str()
            ));
        }

        for (i, profile) in self.profiles.iter().enumerate() {
            out.push_str("PROFILE_START\n");
            if i == self.active_profile_index {
                out.push_str("active: true\n");
            }
            out.push_str(&format!("program: {}\n", profile.program));
            if profile.stretch_window {
                out.push_str("stretch_window: true\n");
            }
            for m in &profile.mappings {
                // 草稿只存在于编辑中的界面，不该落盘：它没有真实按键，
                // 写进文件只会留下 `mapping: OPPOSITE 0 0` 这种脏行。
                if m.source == 0 || m.destination == 0 {
                    continue;
                }
                out.push_str(&format!(
                    "mapping: {} {} {}\n",
                    m.mode.as_settings_str(),
                    m.source,
                    m.destination
                ));
            }
            {
                let c = &profile.crosshair;
                let d = Crosshair::default();
                // 只写和默认值不同的项。准星是后加的功能，旧配置里没有
                // 这些行，解析时保留默认值即可。
                if c.enabled != d.enabled {
                    out.push_str(&format!("crosshair: {}\n", c.enabled));
                }
                if c.style != d.style {
                    out.push_str(&format!("crosshair_style: {}\n", c.style.as_settings_str()));
                }
                if c.color != d.color {
                    out.push_str(&format!(
                        "crosshair_color: {} {} {}\n",
                        c.color[0], c.color[1], c.color[2]
                    ));
                }
                if c.thickness != d.thickness {
                    out.push_str(&format!("crosshair_thickness: {}\n", c.thickness));
                }
                if c.gap != d.gap {
                    out.push_str(&format!("crosshair_gap: {}\n", c.gap));
                }
                if c.size != d.size {
                    out.push_str(&format!("crosshair_size: {}\n", c.size));
                }
                if c.opacity != d.opacity {
                    out.push_str(&format!("crosshair_opacity: {}\n", c.opacity));
                }
                if c.outline != d.outline {
                    out.push_str(&format!("crosshair_outline: {}\n", c.outline));
                }
                if c.outline_opacity != d.outline_opacity {
                    out.push_str(&format!(
                        "crosshair_outline_opacity: {}\n",
                        c.outline_opacity
                    ));
                }
                if c.center_dot != d.center_dot {
                    out.push_str(&format!("crosshair_center_dot: {}\n", c.center_dot));
                }
                if c.center_dot_size != d.center_dot_size {
                    out.push_str(&format!(
                        "crosshair_center_dot_size: {}\n",
                        c.center_dot_size
                    ));
                }
                if c.offset_x != d.offset_x {
                    out.push_str(&format!("crosshair_offset_x: {}\n", c.offset_x));
                }
                if c.offset_y != d.offset_y {
                    out.push_str(&format!("crosshair_offset_y: {}\n", c.offset_y));
                }
            }
            out.push_str("PROFILE_END\n");
        }

        out
    }

    pub fn load(path: &Path) -> Result<Settings, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
        Settings::parse(&text)
    }

    /// 原子写入：先写同目录的临时文件，再原子替换。
    ///
    /// 直接 `fs::write` 会在写到一半崩溃时留下截断的配置，下次启动解析失败，
    /// 用户的全部映射就丢了。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = self.serialize();

        let tmp = path.with_extension("socd.tmp");
        fs::write(&tmp, &text).map_err(|e| format!("写入 {tmp:?} 失败：{e}"))?;

        replace_file(&tmp, path).map_err(|e| format!("替换 {path:?} 失败：{e}"))
    }
}

/// 用 `src` 原子替换 `dst`。
///
/// Windows 上 `fs::rename` 不能覆盖已存在的目标，常见写法是"先删再改名"，
/// 但那两步之间崩溃或断电就会把配置弄丢——用户看到的是映射全没了。
/// `MoveFileExW` 带 `MOVEFILE_REPLACE_EXISTING` 一步完成替换，没有这个窗口。
#[cfg(windows)]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;

    let wide = |p: &Path| -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    };

    let ok = unsafe { MoveFileExW(wide(src).as_ptr(), wide(dst).as_ptr(), MOVEFILE_REPLACE_EXISTING) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::rename(src, dst)
}

/// 配置文件位置：数据目录下的 `settings.socd`。
///
/// 数据目录默认在 exe 旁边的 `Data` 文件夹（见 `paths` 模块），不可写时
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
    fn hotkey_roundtrip() {
        let mut s = Settings::default();
        s.hotkey = Some(Hotkey { kind: HotkeyKind::Mouse, code: 3 });
        s.hotkey_mode = HotkeyMode::HoldToHide;
        let text = s.serialize();
        let parsed = Settings::parse(&text).expect("parse");
        assert_eq!(parsed.hotkey, Some(Hotkey { kind: HotkeyKind::Mouse, code: 3 }));
        assert_eq!(parsed.hotkey_mode, HotkeyMode::HoldToHide);
    }

    #[test]
    fn crosshair_offset_roundtrip() {
        let mut s = Settings::default();
        let mut p = Profile::new("game.exe");
        p.crosshair.enabled = true;
        p.crosshair.offset_x = -120;
        p.crosshair.offset_y = 45;
        s.profiles.push(p);
        let text = s.serialize();
        let parsed = Settings::parse(&text).expect("parse");
        assert_eq!(parsed.profiles[0].crosshair.offset_x, -120);
        assert_eq!(parsed.profiles[0].crosshair.offset_y, 45);
    }

    #[test]
    fn old_config_without_crosshair_still_loads() {
        // 准星是后加的功能，旧配置里没有相关行
        let text = "[1] # header

PROFILE_START
program: a.exe
mapping: OPPOSITE 30 32
PROFILE_END
";
        let s = Settings::parse(text).expect("parse");
        assert_eq!(s.profiles[0].crosshair, Crosshair::default());
        assert_eq!(s.hotkey, None);
        assert_eq!(s.hotkey_mode, HotkeyMode::Toggle);
    }

    #[test]
    fn rejects_wrong_version() {
        assert!(Settings::parse("[99] # header\n").is_err());
    }
}
