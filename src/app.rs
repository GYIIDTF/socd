//! Application controller: owns settings, applies profiles, serves the WebView UI.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::settings::{settings_path, Mapping, MappingMode, Profile, Settings, ScanCode};
use crate::win::{self, HookState};

/* ---------- wire types shared with the frontend ---------- */

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingView {
    pub source: ScanCode,
    pub destination: ScanCode,
    pub source_name: String,
    pub destination_name: String,
    pub mode: String,
    pub mode_label: String,
    /// 草稿：还没有指定按键，不参与映射，也不参与查重
    pub draft: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    pub index: usize,
    pub program: String,
    pub mappings: Vec<MappingView>,
    pub crosshair: crate::settings::Crosshair,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub profiles: Vec<ProfileView>,
    pub active_index: usize,
    /// Program name of the profile currently armed against the foreground app.
    pub armed_program: Option<String>,
    pub hooked: bool,
    pub settings_path: String,
    /// 配置读取失败的用户可见说明，正常时为 null
    pub load_error: Option<String>,
    pub modes: Vec<ModeOption>,
    /// 准星叠加层是否可用（窗口创建成功）
    pub crosshair_available: bool,
    /// 准星总开关
    pub crosshair_armed: bool,
    /// 当前快捷键，null 表示未设置
    pub hotkey: Option<crate::settings::Hotkey>,
    /// 快捷键触发模式
    pub hotkey_mode: crate::settings::HotkeyMode,
    /// 快捷键当前是否按住（按住类模式下由它决定显示）
    pub hotkey_held: bool,
    /// 快捷键的显示名，由后端统一生成，避免前后端两套命名
    pub hotkey_name: Option<String>,
    /// Raw Input 是否注册成功
    pub hotkey_registered: bool,
    pub raw_input_count: u64,
    pub raw_event_count: u64,
    pub crosshair_last: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeOption {
    pub value: String,
    pub label: String,
    pub hint: String,
}

/// 鼠标键的中文名。编号约定见 `rawinput` 模块。
fn mouse_key_name(code: u16) -> &'static str {
    match code {
        0 => "鼠标左键",
        1 => "鼠标右键",
        2 => "鼠标中键",
        3 => "鼠标侧键 1",
        4 => "鼠标侧键 2",
        _ => "鼠标键",
    }
}

fn mode_options() -> Vec<ModeOption> {
    MappingMode::all()
        .iter()
        .map(|m| ModeOption {
            value: m.as_settings_str().to_string(),
            label: m.label().to_string(),
            hint: m.hint().to_string(),
        })
        .collect()
}

/// 草稿映射的标记值：源键为 0（Windows 没有对应的物理键）。
pub const DRAFT_SCAN: ScanCode = 0;

/// 这类模式会在目标键上补一条反向映射，因此占用目标键的槽位。
/// REMAP 是单向的，不占。
fn is_pairing_mode(mode: MappingMode) -> bool {
    matches!(
        mode,
        MappingMode::Opposite | MappingMode::OppositeNoRepress | MappingMode::Neutral
    )
}

fn mapping_view(m: &Mapping) -> MappingView {
    // 两侧都设好才算一条可用的映射；只填了一侧时仍按草稿呈现
    let has_source = m.source != DRAFT_SCAN;
    let has_destination = m.destination != DRAFT_SCAN;

    MappingView {
        source: m.source,
        destination: m.destination,
        source_name: if has_source { win::key_name(m.source) } else { String::new() },
        destination_name: if has_destination {
            win::key_name(m.destination)
        } else {
            String::new()
        },
        mode: m.mode.as_settings_str().to_string(),
        mode_label: m.mode.label().to_string(),
        draft: !(has_source && has_destination),
    }
}

/* ---------- controller ---------- */

pub struct App {
    pub settings: Settings,
    pub path: PathBuf,
    pub hook: Arc<HookState>,
    pub armed: Option<String>,
    pub guard: Option<win::HookGuard>,
    /// 本次启动是否生成了全新配置。首次运行需要提示用户去托盘找界面，
    /// 否则静默启动看起来像什么都没发生。
    pub first_run: bool,
    /// 配置文件存在但读不出来时的说明。非空表示"当前界面上是空配置，
    /// 继续操作会覆盖原文件"，必须让用户看见。
    pub load_error: Option<String>,
    /// 准星叠加层。创建失败时为 None，功能降级但不影响按键映射。
    pub overlay: Option<Arc<crate::crosshair::Overlay>>,
    /// 准星总开关。关掉后任何配置的准星都不显示。
    ///
    /// Toggle 模式下由快捷键翻转；按住类模式下由"当前是否按住"派生。
    pub crosshair_armed: bool,
    /// 快捷键当前是否按住。按住类模式靠它决定显示与否。
    pub hotkey_held: bool,
    /// 最近一次准星渲染的结果，用于界面/诊断
    pub crosshair_last: String,
}

impl App {
    pub fn load() -> App {
        let path = settings_path();
        // 首次运行没有配置文件是正常状态，不是错误：用默认值启动，并把这份
        // 默认配置写盘，下次启动就有文件了。
        let first_run = !path.exists();
        let mut load_error = None;
        let settings = match Settings::load(&path) {
            Ok(s) => s,
            Err(e) => {
                if !first_run {
                    crate::logging::warn(&format!("读取配置失败（{e}），使用默认值"));
                    // 文件存在但读不出来（截断、版本不符、带 BOM、被占用）。
                    // 接下来任何一次保存都会用默认内容覆盖它，用户此前的
                    // 全部映射就没了。先留一份备份，并让界面提示这件事。
                    let backup = path.with_extension("socd.bak");
                    let _ = std::fs::copy(&path, &backup);
                    load_error = Some(format!(
                        "配置文件读取失败，已备份为 {}。当前显示的是空配置，\
                         继续操作会覆盖原文件。",
                        backup
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "settings.socd.bak".into())
                    ));
                }
                Settings::default()
            }
        };

        let mut app = App {
            settings,
            path,
            hook: Arc::new(HookState::new()),
            armed: None,
            guard: None,
            first_run,
            load_error,
            overlay: crate::crosshair::Overlay::new(),
            crosshair_armed: true,
            hotkey_held: false,
            crosshair_last: String::new(),
        };

        if app.settings.profiles.is_empty() {
            app.settings.profiles.push(Profile::new(""));
        }

        // 按已保存的配置注册准星快捷键。不注册的话重启后快捷键失效，
        // 用户得重新设一次——而他不会想到要这么做。
        if let (Some(overlay), Some(hk)) = (app.overlay.as_ref(), app.settings.hotkey) {
            overlay.set_hotkey(Some(hk));
        }

        match win::install(app.hook.clone()) {
            Ok(g) => app.guard = Some(g),
            Err(e) => crate::logging::warn(&format!("键盘钩子安装失败：{e}")),
        }

        app.reapply();

        if first_run {
            let _ = app.save();
        }

        app
    }

    /// Pushes the selected profile into the hook and arms it.
    pub fn reapply(&mut self) {
        let idx = self.settings.active_profile_index.min(self.settings.profiles.len() - 1);
        let profile = &self.settings.profiles[idx];

        {
            // 草稿没有按键，不能进查找表，否则会把扫描码 0 当成真实按键
            let effective: Vec<Mapping> = profile
                .mappings
                .iter()
                .filter(|m| m.source != DRAFT_SCAN && m.destination != DRAFT_SCAN)
                .copied()
                .collect();
            let mut m = self.hook.machine.lock().unwrap();
            // 换查找表前先把还按着的目标键松开，再清状态。
            // 顺序不能反：reset 会清掉计数，之后就没法知道该补发哪些 up 了。
            for code in m.keys.take_pending() {
                win::inject_scancode(code, true);
            }
            m.table.install(&effective);
            m.keys.reset();
        }
        self.hook.generation.fetch_add(1, Ordering::Relaxed);

        // 快捷键控制的"总开关"跟着当前配置的启用状态走。
        // 两个状态不同步的话，用户切到另一个配置后按快捷键，
        // 第一次按下看不出变化（净值相同），会以为快捷键坏了。
        self.crosshair_armed = self
            .settings
            .profiles
            .get(idx)
            .map(|p| p.crosshair.enabled)
            .unwrap_or(false);

        // 立刻按当前前台判断是否生效，而不是先假定"启用"等下一次轮询纠正。
        // 否则选中某个程序的配置时，界面会短暂显示"对 X 生效中"——
        // 即便 X 根本不在前台。
        self.poll_focus();
    }

    /// Called on a timer: enables the hook only while the armed program is focused.
    pub fn poll_focus(&mut self) {
        // 快捷键事件来自准星窗口的 Raw Input 监听
        self.poll_hotkey();

        if self.settings.profiles.is_empty() {
            return;
        }
        let idx = self.settings.active_profile_index.min(self.settings.profiles.len() - 1);
        let program = &self.settings.profiles[idx].program;

        let should_enable = if program.is_empty() {
            // 空程序名表示全局配置，始终生效
            true
        } else {
            match win::foreground_program() {
                Some(focused) => focused.eq_ignore_ascii_case(program),
                None => false,
            }
        };

        let was = self.hook.enabled.swap(should_enable, Ordering::Relaxed);
        if was != should_enable {
            // 跨越启停边界：先把还按着的目标键补发 up，再清空状态。
            //
            // 只 reset 是不够的——被注入按下的 Ctrl/方向键不会因为钩子关闭
            // 而自动松开，物理键的 keyup 也不再经过我们，于是那个键会一直
            // 卡在按下状态，直到用户重新按一次。
            win::release_injected(&self.hook);
            self.hook.machine.lock().unwrap().keys.reset();
        }
        // armed 表示"当前实际生效的程序名"，无论启停是否变化都要同步：
        // 切换配置时 was == should_enable（都未启用），但 armed 必须跟着
        // 新配置清空，否则界面会显示上一个配置的名字。
        //
        // 全局配置没有程序名，armed 保持 None——界面上"对所有程序生效"
        // 是靠配置本身表达的，不需要再报一个名字。
        self.armed = if should_enable && !program.is_empty() {
            Some(program.clone())
        } else {
            None
        };

        self.refresh_crosshair(should_enable);
    }

    /// 处理准星快捷键事件，按模式更新显示状态。
    fn poll_hotkey(&mut self) {
        let Some(overlay) = self.overlay.clone() else { return };
        let events = overlay.drain_hotkey_events();
        if events.is_empty() {
            return;
        }

        // 快捷键控制的就是"启用"这个状态本身——不是另一个独立开关。
        // 之前 crosshair_armed 和跨配置的 enabled 是两套状态，用户看到
        // 界面开着却不显示、或者反过来，很困惑。
        //
        // 按住类模式需要记住"按住前的值"，松开时恢复；否则 HoldToShow
        // 松开后会把用户原本开着的准星关掉。
        let mode = self.settings.hotkey_mode;
        let mut changed = false;
        for ev in events {
            self.hotkey_held = ev.down;
            let want = match mode {
                crate::settings::HotkeyMode::Toggle => {
                    if ev.down {
                        Some(!self.crosshair_armed)
                    } else {
                        None
                    }
                }
                crate::settings::HotkeyMode::HoldToHide => Some(!ev.down),
                crate::settings::HotkeyMode::HoldToShow => Some(ev.down),
            };
            if let Some(v) = want {
                if v != self.crosshair_armed {
                    self.crosshair_armed = v;
                    changed = true;
                }
            }
        }

        // 把状态同步进当前配置，界面上的"启用"开关跟着变
        if changed {
            let idx = self
                .settings
                .active_profile_index
                .min(self.settings.profiles.len().saturating_sub(1));
            if let Some(p) = self.settings.profiles.get_mut(idx) {
                p.crosshair.enabled = self.crosshair_armed;
            }
            let _ = self.save();
        }
        self.refresh_now();
    }

    /// 按当前状态决定准星是否显示、显示成什么样。
    ///
    /// 三个条件同时满足才显示：
    ///   1. 总开关打开（开关键可切）
    ///   2. 当前配置的准星 enabled
    ///   3. 该配置对应的程序在前台（全局配置则始终满足）
    ///
    /// 位置取"鼠标所在显示器"与"前台窗口客户区"的交集——准星只在游戏
    /// 窗口范围内出现，不会飘到桌面或别的窗口上。
    fn refresh_crosshair(&mut self, program_focused: bool) {
        let Some(overlay) = self.overlay.clone() else {
            return;
        };

        let idx = self
            .settings
            .active_profile_index
            .min(self.settings.profiles.len().saturating_sub(1));
        let Some(profile) = self.settings.profiles.get(idx) else {
            return;
        };

        let cfg = profile.crosshair;
        // 全局配置没有 program，任何时候都算"匹配"
        let matches = profile.program.is_empty() || program_focused;

        if !self.crosshair_armed || !cfg.is_visible() || !matches {
            overlay.hide();
            self.crosshair_last = format!(
                "隐藏：armed={} visible={} matches={}",
                self.crosshair_armed,
                cfg.is_visible(),
                matches
            );
            return;
        }

        let Some(client) = crate::crosshair::foreground_client_rect() else {
            overlay.hide();
            self.crosshair_last = "隐藏：拿不到前台窗口客户区".into();
            return;
        };
        let Some(monitor) = crate::crosshair::monitor_under_cursor() else {
            overlay.hide();
            self.crosshair_last = "隐藏：拿不到鼠标所在显示器".into();
            return;
        };

        // 交集：窗口可能横跨两块屏，准星画在两者都覆盖的区域中心
        let left = client.0.max(monitor.0);
        let top = client.1.max(monitor.1);
        let right = client.2.min(monitor.2);
        let bottom = client.3.min(monitor.3);
        if right <= left || bottom <= top {
            // 窗口完全不在鼠标那块屏上
            overlay.hide();
            self.crosshair_last = format!(
                "隐藏：窗口({},{},{},{}) 与显示器({},{},{},{}) 无交集",
                client.0, client.1, client.2, client.3,
                monitor.0, monitor.1, monitor.2, monitor.3
            );
            return;
        }

        self.crosshair_last = overlay.apply(&cfg, Some((left, top, right, bottom)));
    }

    /// 立即按当前前台状态刷新一次准星。配置改动后调用。
    fn refresh_now(&mut self) {
        let idx = self
            .settings
            .active_profile_index
            .min(self.settings.profiles.len().saturating_sub(1));
        let focused = self
            .settings
            .profiles
            .get(idx)
            .map(|p| {
                p.program.is_empty()
                    || win::foreground_program()
                        .map(|f| f.eq_ignore_ascii_case(&p.program))
                        .unwrap_or(false)
            })
            .unwrap_or(false);
        self.refresh_crosshair(focused);
    }

    /// 切换准星总开关。返回切换后的状态。
    pub fn toggle_crosshair(&mut self) -> bool {
        self.crosshair_armed = !self.crosshair_armed;
        let enabled = self.armed.is_some()
            || self
                .settings
                .profiles
                .get(self.settings.active_profile_index)
                .map(|p| p.program.is_empty())
                .unwrap_or(true);
        self.refresh_crosshair(enabled);
        self.crosshair_armed
    }

    pub fn drain_events(&self) -> Vec<Value> {
        let mut out = Vec::new();
        if let Ok(mut ev) = self.hook.events.try_lock() {
            for e in ev.drain(..) {
                out.push(json!({
                    "scan": e.scan,
                    "name": win::key_name(e.scan),
                    "down": e.down,
                    "repeat": e.is_repeat,
                    "swallowed": e.swallowed,
                }));
            }
        }
        out
    }

    pub fn state_view(&self) -> StateView {
        let active = self
            .settings
            .active_profile_index
            .min(self.settings.profiles.len().saturating_sub(1));

        StateView {
            profiles: self
                .settings
                .profiles
                .iter()
                .enumerate()
                .map(|(index, p)| ProfileView {
                    index,
                    program: p.program.clone(),
                    mappings: p.mappings.iter().map(mapping_view).collect(),
                    crosshair: p.crosshair,
                })
                .collect(),
            active_index: active,
            armed_program: self.armed.clone(),
            hooked: self.guard.is_some(),
            settings_path: self.path.display().to_string(),
            load_error: self.load_error.clone(),
            modes: mode_options(),
            crosshair_available: self.overlay.is_some(),
            crosshair_armed: self.crosshair_armed,
            hotkey_name: self.settings.hotkey.map(|h| match h.kind {
                crate::settings::HotkeyKind::Keyboard => win::key_name(h.code as u32),
                crate::settings::HotkeyKind::Mouse => mouse_key_name(h.code).to_string(),
            }),
            hotkey: self.settings.hotkey,
            hotkey_mode: self.settings.hotkey_mode,
            hotkey_held: self.hotkey_held,
            hotkey_registered: self
                .overlay
                .as_ref()
                .map(|o| o.raw_registered())
                .unwrap_or(false),
            raw_input_count: self.overlay.as_ref()
                .map(|o| o.raw.input_count.load(Ordering::Relaxed)).unwrap_or(0),
            raw_event_count: self.overlay.as_ref()
                .map(|o| o.raw.event_count.load(Ordering::Relaxed)).unwrap_or(0),
            crosshair_last: self.crosshair_last.clone(),
        }
    }

    /// 保存配置。失败必须让调用方知道：界面若不报错，用户会以为改动已生效，
    /// 重启后却全部丢失。
    fn save(&self) -> Result<(), String> {
        self.settings.save(&self.path).map_err(|e| {
            crate::logging::warn(&format!("保存配置失败：{e}"));
            format!("无法写入配置文件（{e}）。改动只在本次运行有效。")
        })
    }

    /// 拒绝会造成“看得见但不生效”的映射。
    ///
    /// 分成两个阶段，对应界面上两次按键：
    ///   * 源键：按第一个框时就能判断这个键能不能用；
    ///   * 配对：按第二个框时判断这两键能不能成对。
    ///
    /// 早一点报错，用户就不用把两个键都按完才发现白做。
    ///
    /// REMAP 不占用目标槽位，多个键指向同一目标完全合理。
    fn validate_source(
        &self,
        source: ScanCode,
        mode: MappingMode,
        skip_index: Option<usize>,
    ) -> Result<(), String> {
        let Some(profile) = self.active_profile() else {
            return Ok(());
        };
        check_source(source, mode, &profile.mappings, skip_index)
    }

    fn validate_pair(
        &self,
        m: &Mapping,
        skip_index: Option<usize>,
    ) -> Result<(), String> {
        let Some(profile) = self.active_profile() else {
            return Ok(());
        };
        validate_against(m, &profile.mappings, skip_index)
    }

    /// 只填了目标键时也要校验：用户可能先点右边的框。
    /// 这里不涉及"和源键配不配"，只判断这个键能不能当目标。
    fn validate_destination(
        &self,
        destination: ScanCode,
        mode: MappingMode,
        skip_index: Option<usize>,
    ) -> Result<(), String> {
        let Some(profile) = self.active_profile() else {
            return Ok(());
        };
        check_destination(destination, mode, &profile.mappings, skip_index)
    }

    fn active_profile(&self) -> Option<&Profile> {
        let idx = self
            .settings
            .active_profile_index
            .min(self.settings.profiles.len().saturating_sub(1));
        self.settings.profiles.get(idx)
    }
}

/// 把一条映射描述成「A → B」，用于错误提示里指名道姓。
fn describe(existing: &Mapping) -> String {
    format!(
        "{} → {}",
        win::key_name(existing.source),
        win::key_name(existing.destination)
    )
}

/// 扫描码必须落在查找表范围内，否则 install 会静默丢弃这条映射——
/// 界面上显示正常，实际永不生效。
const MAX_SCAN: ScanCode = crate::socd::BindTable::SIZE as ScanCode - 1;

fn check_scan_bound(key: ScanCode) -> Result<(), String> {
    if key > MAX_SCAN {
        return Err(format!("这个键的编码超出支持范围（0x{key:04X}）"));
    }
    Ok(())
}

/// 目标键能不能用。
///
/// 与源键的区别：目标键可以被重映射重复使用（多个键指向同一目标），
/// 但对向类模式下它还会被反向映射占用，只能属于一对。
fn check_destination(
    key: ScanCode,
    mode: MappingMode,
    existing_all: &[Mapping],
    skip_index: Option<usize>,
) -> Result<(), String> {
    check_scan_bound(key)?;
    for (i, existing) in existing_all.iter().enumerate() {
        if Some(i) == skip_index || existing.source == DRAFT_SCAN {
            continue;
        }
        // 已经是别的映射的源键 → 那个槽位被占
        if existing.source == key {
            return Err(format!(
                "{} 已经用在「{}」里了，不能再当目标键。
                 请先删掉那一条，或换成别的键。",
                win::key_name(key),
                describe(existing)
            ));
        }
        // 对向类：目标键只能属于一对
        if is_pairing_mode(mode)
            && is_pairing_mode(existing.mode)
            && existing.destination == key
        {
            return Err(format!(
                "{} 已经和 {} 配成对了（「{}」），一个键只能属于一对。
                 请先删掉那一条，或者把这条改用「重映射」。",
                win::key_name(key),
                win::key_name(existing.source),
                describe(existing)
            ));
        }
    }
    Ok(())
}

/// 阶段一：这个键能不能当源键（或目标键）用。
///
/// 冲突有两种：
///   * 它已经是某条映射的源键——表按源键索引，会互相覆盖；
///   * 它已是对向映射的目标键——那个槽位被反向映射占着。
fn check_source(
    key: ScanCode,
    mode: MappingMode,
    existing_all: &[Mapping],
    skip_index: Option<usize>,
) -> Result<(), String> {
    check_scan_bound(key)?;
    for (i, existing) in existing_all.iter().enumerate() {
        if Some(i) == skip_index || existing.source == DRAFT_SCAN {
            continue;
        }
        if existing.source == key {
            return Err(format!(
                "{} 已经用在「{}」里了。请先删掉那一条，或换成别的键。",
                win::key_name(key),
                describe(existing)
            ));
        }
        // 对向映射的目标键被反向映射占着，不能再当源键
        if is_pairing_mode(existing.mode) && existing.destination == key {
            return Err(format!(
                "{} 已经作为「{}」的目标键配成对了，一个键只能属于一对。
                 请先删掉那一条，或换成别的键。",
                win::key_name(key),
                describe(existing)
            ));
        }
    }
    let _ = mode;
    Ok(())
}

/// 阶段二：两个键能不能配成一对。假设源键、目标键各自都已单独查过。
///
/// 剩下的只有"两者之间的关系"：不能映射到自己，以及源键不能是
/// 别的对向映射的目标键（那种情况下它的槽位被反向映射占着）。
fn validate_against(
    m: &Mapping,
    existing_all: &[Mapping],
    skip_index: Option<usize>,
) -> Result<(), String> {
    if m.source == m.destination {
        return Err(format!(
            "{} 不能映射到它自己。请换一个目标键。",
            win::key_name(m.source)
        ));
    }

    check_source(m.source, m.mode, existing_all, skip_index)?;
    check_destination(m.destination, m.mode, existing_all, skip_index)?;

    if !is_pairing_mode(m.mode) {
        return Ok(());
    }

    for (i, existing) in existing_all.iter().enumerate() {
        if Some(i) == skip_index || existing.source == DRAFT_SCAN {
            continue;
        }
        // 源键是别的对向映射的目标键 → 它的槽位被反向映射占着
        if is_pairing_mode(existing.mode) && existing.destination == m.source {
            return Err(format!(
                "{} 已经和 {} 配成对了（「{}」），不能再和 {} 配对。
                 请先删掉那一条，或换成别的键。",
                win::key_name(m.source),
                win::key_name(existing.source),
                describe(existing),
                win::key_name(m.destination)
            ));
        }
    }
    Ok(())
}

impl App {
    /* ---------- command dispatch ---------- */

    pub fn handle(&mut self, command: &str, payload: Value) -> Result<Value, String> {
        match command {
            "getState" => Ok(serde_json::to_value(self.state_view()).map_err(|e| e.to_string())?),

            "listPrograms" => Ok(json!({ "programs": win::running_programs() })),

            "addProfile" => {
                let program = payload
                    .get("program")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if program.is_empty() {
                    return Err("请先填写程序名".into());
                }
                // 配置文件是一行一个 `program: xxx`。名字里带换行会写出半截行，
                // 加载时后半截被当成无效行丢掉；更糟的是构造 `\nPROFILE_END\n
                // PROFILE_START\nprogram: X` 能凭空多出一个配置。
                if program.contains(['\n', '\r']) {
                    return Err("程序名不能包含换行".into());
                }
                if self
                    .settings
                    .profiles
                    .iter()
                    .any(|p| p.program.eq_ignore_ascii_case(&program))
                {
                    return Err(format!("已经存在名为 {program} 的配置文件"));
                }
                self.settings.profiles.push(Profile::new(program));
                self.settings.active_profile_index = self.settings.profiles.len() - 1;
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            "deleteProfile" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                if index >= self.settings.profiles.len() {
                    return Err("这个配置文件不存在，可能已经被删除了".into());
                }
                // 全局配置是所有未匹配程序时的兜底，删掉会让映射在某些情况下
                // 完全失效。它只能被改名，不能被删除。这条优先于数量检查，
                // 否则只剩全局时给的是"至少保留一个"，用户会以为是数量限制。
                if self.settings.profiles[index].program.is_empty() {
                    return Err("全局配置不能删除，它是没有匹配程序时的兜底".into());
                }
                if self.settings.profiles.len() <= 1 {
                    return Err("至少要保留一个配置文件".into());
                }
                self.settings.profiles.remove(index);
                // 删掉的项在当前选中之前时，后面所有项都往前挪了一位，
                // 选中索引必须跟着减一，否则会莫名其妙跳到下一个配置。
                if index < self.settings.active_profile_index {
                    self.settings.active_profile_index -= 1;
                }
                if self.settings.active_profile_index >= self.settings.profiles.len() {
                    self.settings.active_profile_index = self.settings.profiles.len() - 1;
                }
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            "selectProfile" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                if index >= self.settings.profiles.len() {
                    return Err("这个配置文件不存在，可能已经被删除了".into());
                }
                self.settings.active_profile_index = index;
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            // 新建一条草稿映射。草稿没有按键，不写配置、不进查找表，
            // 等用户按下按键后由 updateMappingBinding 转正。
            "addMapping" => {
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(MappingMode::from_settings_str)
                    // 新建的映射默认用「对向·后按生效」：这是这个工具最常用的场景
                    .unwrap_or(MappingMode::Opposite);

                let draft = Mapping {
                    source: DRAFT_SCAN,
                    destination: DRAFT_SCAN,
                    mode,
                };
                let profile = self.active_profile_mut()?;
                // 只允许存在一条草稿，避免反复点“添加”堆出一堆空行
                if let Some(pos) = profile.mappings.iter().position(|m| m.source == DRAFT_SCAN) {
                    profile.mappings.remove(pos);
                }
                profile.mappings.push(draft);
                Ok(self.state()?)
            }

            // ---- 准星 ----

            "updateCrosshair" => {
                let idx = self
                    .settings
                    .active_profile_index
                    .min(self.settings.profiles.len().saturating_sub(1));
                let payload_cfg = payload.get("crosshair").cloned().unwrap_or(Value::Null);
                let profile = self.active_profile_mut()?;
                let mut c = profile.crosshair;

                if let Some(v) = payload_cfg.get("enabled").and_then(Value::as_bool) {
                    c.enabled = v;
                }
                if let Some(v) = payload_cfg.get("style").and_then(Value::as_str) {
                    if let Some(s) = crate::settings::CrosshairStyle::from_settings_str(v) {
                        c.style = s;
                    }
                }
                if let Some(arr) = payload_cfg.get("color").and_then(Value::as_array) {
                    if arr.len() == 3 {
                        let mut rgb = [0u8; 3];
                        for (i, x) in arr.iter().enumerate() {
                            rgb[i] = x.as_u64().unwrap_or(255).min(255) as u8;
                        }
                        c.color = rgb;
                    }
                }
                if let Some(v) = payload_cfg.get("thickness").and_then(Value::as_u64) {
                    c.thickness = (v as u32).clamp(1, 20);
                }
                if let Some(v) = payload_cfg.get("gap").and_then(Value::as_u64) {
                    c.gap = (v as u32).min(100);
                }
                if let Some(v) = payload_cfg.get("size").and_then(Value::as_u64) {
                    c.size = (v as u32).clamp(1, 100);
                }
                if let Some(v) = payload_cfg.get("opacity").and_then(Value::as_u64) {
                    c.opacity = (v as u8).min(100);
                }
                if let Some(v) = payload_cfg.get("outline").and_then(Value::as_u64) {
                    c.outline = (v as u32).min(10);
                }
                if let Some(v) = payload_cfg.get("offset_x").and_then(Value::as_i64) {
                    c.offset_x = (v as i32).clamp(-4000, 4000);
                }
                if let Some(v) = payload_cfg.get("offset_y").and_then(Value::as_i64) {
                    c.offset_y = (v as i32).clamp(-4000, 4000);
                }

                {
                    let profile = self.active_profile_mut()?;
                    profile.crosshair = c;
                }
                let _ = idx;
                // 界面上的启用开关和快捷键控制的"总开关"是同一个状态，
                // 用户在这里拨开关时 high 层状态必须同步，否则接着按
                // 快捷键第一次会看不出变化（两边净值相同）。
                self.crosshair_armed = c.enabled;
                self.save()?;
                self.refresh_now();
                Ok(self.state()?)
            }

            "setCrosshairKey" => {
                // kind = "KEYBOARD" | "MOUSE"，code = 扫描码或鼠标键位。
                // 传 null 表示清空快捷键。
                let hk = match payload.get("hotkey") {
                    Some(v) if !v.is_null() => {
                        let kind = v
                            .get("kind")
                            .and_then(Value::as_str)
                            .and_then(crate::settings::HotkeyKind::from_settings_str)
                            .ok_or("bad hotkey kind")?;
                        let code = v.get("code").and_then(Value::as_u64).ok_or("bad hotkey code")? as u16;
                        Some(crate::settings::Hotkey { kind, code })
                    }
                    _ => None,
                };

                self.settings.hotkey = hk;
                if let Some(overlay) = self.overlay.as_ref() {
                    overlay.set_hotkey(hk);
                }
                self.save()?;
                // 换了快捷键，按住状态从零开始
                self.hotkey_held = false;
                Ok(self.state()?)
            }

            "setHotkeyMode" => {
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(crate::settings::HotkeyMode::from_settings_str)
                    .ok_or("bad hotkey mode")?;
                self.settings.hotkey_mode = mode;
                // 切模式时按新语义重置显示状态：
                // 按住类从"没按住"开始，Toggle 保持当前值。
                self.hotkey_held = false;
                if mode == crate::settings::HotkeyMode::HoldToShow {
                    self.crosshair_armed = false;
                } else if mode == crate::settings::HotkeyMode::HoldToHide {
                    self.crosshair_armed = true;
                }
                self.save()?;
                self.refresh_now();
                Ok(self.state()?)
            }

            "toggleCrosshair" => {
                self.crosshair_armed = !self.crosshair_armed;
                self.refresh_now();
                Ok(self.state()?)
            }

            "updateMappingMode" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(MappingMode::from_settings_str)
                    .ok_or("bad mode")?;
                // 换模式同样会改变占用关系：重映射不占目标槽，对向类要占。
                // 不校验的话，把 B→D（重映射）改成对向就会和 A→D 抢 D，
                // 后装的那对镜像装不进去，界面上两条都显示正常却有一条不工作。
                let candidate = {
                    let profile = self.active_profile_mut()?;
                    let m = profile.mappings.get(index).ok_or("这条映射不存在，可能已经被删除了")?;
                    Mapping { source: m.source, destination: m.destination, mode }
                };
                let is_draft = candidate.source == DRAFT_SCAN || candidate.destination == DRAFT_SCAN;
                if !is_draft {
                    self.validate_pair(&candidate, Some(index))?;
                }
                let profile = self.active_profile_mut()?;
                let m = profile.mappings.get_mut(index).ok_or("这条映射不存在，可能已经被删除了")?;
                m.mode = mode;
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            "deleteMapping" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                let profile = self.active_profile_mut()?;
                if index >= profile.mappings.len() {
                    return Err("这条映射不存在，可能已经被删除了".into());
                }
                profile.mappings.remove(index);
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            "applyPreset" => {
                let preset = payload.get("preset").and_then(Value::as_str).ok_or("missing preset")?;
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(MappingMode::from_settings_str)
                    .ok_or("bad mode")?;
                let mappings = preset_mappings(preset, mode)?;
                {
                    let profile = self.active_profile_mut()?;
                    profile.mappings = mappings;
                }
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            "resolveVirtualKey" => {
                let vk = payload.get("vk").and_then(Value::as_u64).ok_or("missing vk")? as u32;
                let scan = win::vk_to_scan(vk).ok_or_else(|| format!("unmapped key code {vk}"))?;
                Ok(json!({ "scan": scan, "name": win::key_name(scan) }))
            }

            "updateMappingBinding" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                let source = payload.get("source").and_then(Value::as_u64).ok_or("missing source")? as ScanCode;
                let destination = payload.get("destination").and_then(Value::as_u64).ok_or("missing destination")? as ScanCode;
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(MappingMode::from_settings_str)
                    .ok_or("bad mode")?;
                // 只设了一侧时仍是草稿：不落盘、不进查找表。
                // 但**校验照做**——按第一个框时就该知道自己是不是白按。
                //
                // 源键为空时目标键没有意义（无主的映射既不能用也说不清），
                // 一并清掉，避免留下"只有目标"的半截状态。
                let destination = if source == DRAFT_SCAN { DRAFT_SCAN } else { destination };
                let complete = source != DRAFT_SCAN && destination != DRAFT_SCAN;

                if source != DRAFT_SCAN {
                    // 阶段一：这个键能不能当源键
                    self.validate_source(source, mode, Some(index))?;
                }
                if destination != DRAFT_SCAN {
                    // 目标键单独填写时也要校验——用户可能先点右边的框
                    self.validate_destination(destination, mode, Some(index))?;
                }
                if complete {
                    // 阶段二：两个键能不能配成对
                    self.validate_pair(
                        &Mapping { source, destination, mode },
                        Some(index),
                    )?;
                }
                {
                    let profile = self.active_profile_mut()?;
                    let m = profile.mappings.get_mut(index).ok_or("index out of range")?;
                    m.source = source;
                    m.destination = destination;
                    m.mode = mode;
                }

                // 草稿不参与 reapply（查找表会忽略它），但要落盘：
                // 清空已有键位时正是走到这里，不保存的话磁盘上留着旧映射，
                // 重启后又"复活"了。serialize 会跳过草稿行，所以不会写脏数据。
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            // Drives the hook pipeline with a synthetic event, exactly as a real
            // key would, and reports what the state machine decided. Used by the
            // self-test; the hook's own callback ignores injected input, so this
            // is the only way to exercise the path without a real keyboard.
            "probeKey" => {
                let scan = payload.get("scan").and_then(Value::as_u64).ok_or("missing scan")? as ScanCode;
                let down = payload.get("down").and_then(Value::as_bool).ok_or("missing down")?;
                let mut m = self.hook.machine.lock().unwrap();
                let is_repeat = m.keys.is_down(scan) && down;
                let (actions, pass) = {
                    let crate::win::Machine { table, keys } = &mut *m;
                    let out = crate::socd::handle_with(
                        table, keys, scan, down, is_repeat, win::is_modifier_scan(scan),
                    );
                    (out.actions, out.pass)
                };
                Ok(json!({
                    "actions": actions.iter().map(|a| match a {
                        crate::socd::Inject::Down(c) => json!({"down": true, "scan": c, "name": win::key_name(*c)}),
                        crate::socd::Inject::Up(c) => json!({"down": false, "scan": c, "name": win::key_name(*c)}),
                    }).collect::<Vec<_>>(),
                    "swallowed": pass == crate::socd::Pass::Swallow,
                    "isRepeat": is_repeat,
                }))
            }

            // Test hooks: install a mapping set without touching settings.socd,
            // and clear held-key state between cases.
            "setMappingsForTest" => {
                let mappings: Vec<Mapping> = serde_json::from_value(
                    payload.get("mappings").cloned().ok_or("missing mappings")?,
                )
                .map_err(|e| format!("bad mappings: {e}"))?;
                let mut m = self.hook.machine.lock().unwrap();
                m.table.install(&mappings);
                m.keys.reset();
                // Read back under the same lock: if this is empty the bug is in
                // install, and if it is not the table is being cleared later.
                let probe: Vec<Value> = mappings
                    .iter()
                    .map(|x| json!({
                        "src": x.source,
                        "readback": m.table.get(x.source).map(|y| y.destination),
                    }))
                    .collect();
                Ok(json!({ "installed": mappings.len(), "probe": probe }))
            }

            "probeDump" => {
                let m = self.hook.machine.lock().unwrap();
                let a = m.table.get(0x1E);
                let d = m.table.get(0x20);
                Ok(json!({
                    "a": a.map(|x| json!({"src": x.source, "dst": x.destination, "mode": x.mode})),
                    "d": d.map(|x| json!({"src": x.source, "dst": x.destination, "mode": x.mode})),
                    "a_down": m.keys.is_down(0x1E),
                    "d_down": m.keys.is_down(0x20),
                }))
            }

            "probeReset" => {
                self.hook.machine.lock().unwrap().keys.reset();
                Ok(json!({ "reset": true }))
            }

            "setAcceptInjected" => {
                let v = payload.get("value").and_then(Value::as_bool).unwrap_or(false);
                self.hook.accept_injected.store(v, Ordering::Relaxed);
                Ok(json!({ "acceptInjected": v }))
            }

            "hookStats" => Ok(json!({
                "enabled": self.hook.enabled.load(Ordering::Relaxed),
                "totalEvents": self.hook.total_events.load(Ordering::Relaxed),
                "lastScan": self.hook.last_event_scan.load(Ordering::Relaxed),
                "generation": self.hook.generation.load(Ordering::Relaxed),
            })),

            "drainEvents" => Ok(json!({ "events": self.drain_events() })),

            other => Err(format!("unknown command: {other}")),
        }
    }

    fn state(&self) -> Result<Value, String> {
        serde_json::to_value(self.state_view()).map_err(|e| e.to_string())
    }

    fn active_profile_mut(&mut self) -> Result<&mut Profile, String> {
        let idx = self
            .settings
            .active_profile_index
            .min(self.settings.profiles.len().saturating_sub(1));
        self.settings.profiles.get_mut(idx).ok_or_else(|| "no profile".into())
    }
}

/// WASD and arrow presets, mirroring the original's two opposite pairs.
fn preset_mappings(preset: &str, mode: MappingMode) -> Result<Vec<Mapping>, String> {
    let (up, down, left, right) = match preset {
        "wasd" => (0x11u32, 0x1F, 0x1E, 0x20),
        "arrows" => (0xE148, 0xE150, 0xE14B, 0xE14D),
        other => return Err(format!("未知的预设：{other}")),
    };
    if !matches!(
        mode,
        MappingMode::Opposite | MappingMode::Neutral | MappingMode::OppositeNoRepress
    ) {
        return Err("预设只适用于对向模式".into());
    }

    Ok(vec![
        Mapping { source: up, destination: down, mode },
        Mapping { source: down, destination: up, mode },
        Mapping { source: left, destination: right, mode },
        Mapping { source: right, destination: left, mode },
    ])
}

/// Periodically re-evaluates which profile should be armed.
pub struct FocusPoller {
    last: Instant,
    interval: Duration,
}

impl FocusPoller {
    pub fn new() -> Self {
        FocusPoller {
            last: Instant::now(),
            interval: Duration::from_millis(120),
        }
    }

    pub fn due(&mut self) -> bool {
        if self.last.elapsed() >= self.interval {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_two_pairs() {
        let m = preset_mappings("wasd", MappingMode::Opposite).unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(m[0].source, 0x11);
        assert_eq!(m[0].destination, 0x1F);
    }

    #[test]
    fn arrows_use_extended_codes() {
        let m = preset_mappings("arrows", MappingMode::Neutral).unwrap();
        assert_eq!(m[0].source, 0xE148);
        assert!(m.iter().all(|x| x.source & 0xE100 != 0));
    }

    #[test]
    fn remap_is_rejected_for_presets() {
        assert!(preset_mappings("wasd", MappingMode::Remap).is_err());
    }

    #[test]
    fn unknown_preset_rejected() {
        assert!(preset_mappings("qwerty", MappingMode::Neutral).is_err());
    }

    #[test]
    fn mode_strings_roundtrip() {
        for m in MappingMode::all() {
            assert_eq!(MappingMode::from_settings_str(m.as_settings_str()), Some(m));
        }
    }

    fn m(source: ScanCode, destination: ScanCode, mode: MappingMode) -> Mapping {
        Mapping { source, destination, mode }
    }

    #[test]
    fn allows_normal_pairs() {
        // A->D 与 W->S 是两组独立的方向对
        let existing = [m(0x1E, 0x20, MappingMode::Opposite)];
        assert!(validate_against(&m(0x11, 0x1F, MappingMode::Opposite), &existing, None).is_ok());
    }

    #[test]
    fn rejects_shared_destination() {
        // 用户实际遇到的情况：3->4 已存在，再加 2->4
        let existing = [m(3, 4, MappingMode::Opposite)];
        let err = validate_against(&m(2, 4, MappingMode::Opposite), &existing, None)
            .expect_err("目标键被占用时必须拒绝");
        assert!(err.contains("只能属于一对"), "提示不够清楚：{err}");
    }

    #[test]
    fn rejects_shared_destination_in_neutral() {
        // 相互抵消同样会补反向映射，限制一样
        let existing = [m(3, 4, MappingMode::Neutral)];
        assert!(validate_against(&m(2, 4, MappingMode::Neutral), &existing, None).is_err());
    }

    #[test]
    fn rejects_target_used_as_other_source() {
        // 已有 4->3，再配 2->4：4 已被占为源键
        let existing = [m(4, 3, MappingMode::Opposite)];
        assert!(validate_against(&m(2, 4, MappingMode::Opposite), &existing, None).is_err());
    }

    #[test]
    fn rejects_source_that_is_other_mirror() {
        // 已有 3->4（会自动占住 4），再配 4->1：源键 4 已被镜像占住
        let existing = [m(3, 4, MappingMode::Opposite)];
        assert!(validate_against(&m(4, 1, MappingMode::Opposite), &existing, None).is_err());
    }

    #[test]
    fn remap_may_share_destination() {
        // 重映射是单向的，多个键指向同一目标完全合理
        let existing = [m(3, 4, MappingMode::Remap)];
        assert!(validate_against(&m(2, 4, MappingMode::Remap), &existing, None).is_ok());
    }

    #[test]
    fn remap_into_pair_target_is_allowed() {
        // 对向映射 3->4 存在时，再加一条 2->4 的重映射是允许的：
        // 重映射不占目标键槽位，不会破坏 3 和 4 的配对
        let existing = [m(3, 4, MappingMode::Opposite)];
        assert!(validate_against(&m(2, 4, MappingMode::Remap), &existing, None).is_ok());
    }

    #[test]
    fn rejects_duplicate_source() {
        let existing = [m(3, 4, MappingMode::Opposite)];
        assert!(validate_against(&m(3, 9, MappingMode::Opposite), &existing, None).is_err());
    }

    #[test]
    fn rejects_self_mapping() {
        assert!(validate_against(&m(5, 5, MappingMode::Opposite), &[], None).is_err());
    }

    #[test]
    fn skip_index_ignores_itself() {
        // 编辑已有映射时，不该和自己冲突
        let existing = [m(3, 4, MappingMode::Opposite)];
        assert!(
            validate_against(&m(3, 4, MappingMode::Opposite), &existing, Some(0)).is_ok()
        );
    }

    #[test]
    fn draft_is_ignored() {
        let existing = [m(DRAFT_SCAN, 4, MappingMode::Opposite)];
        assert!(validate_against(&m(2, 4, MappingMode::Opposite), &existing, None).is_ok());
    }

    /* ---- 阶段一：源键按下时就该拦 ---- */

    #[test]
    fn source_stage_rejects_used_source() {
        // 3 已经是源键，第一次按框就该拒绝，不必等第二个框
        let existing = [m(3, 4, MappingMode::Opposite)];
        let err = check_source(3, MappingMode::Opposite, &existing, None)
            .expect_err("重复源键应在第一阶段就拦住");
        assert!(err.contains("已经用在"), "提示不够清楚：{err}");
    }

    #[test]
    fn source_stage_rejects_pair_target() {
        // 4 是 3->4 的目标键，被反向映射占着，不能当源键
        let existing = [m(3, 4, MappingMode::Opposite)];
        let err = check_source(4, MappingMode::Opposite, &existing, None)
            .expect_err("目标键被反向映射占用时应当拦住");
        assert!(err.contains("配成对了"), "提示不够清楚：{err}");
    }

    #[test]
    fn source_stage_allows_free_key() {
        let existing = [m(3, 4, MappingMode::Opposite)];
        assert!(check_source(9, MappingMode::Opposite, &existing, None).is_ok());
    }

    #[test]
    fn source_stage_ignores_self_when_editing() {
        let existing = [m(3, 4, MappingMode::Opposite)];
        assert!(check_source(3, MappingMode::Opposite, &existing, Some(0)).is_ok());
    }

    #[test]
    fn remap_target_can_be_used_as_source() {
        // 重映射不占目标槽位，所以 4 作为重映射的目标后，仍可当源键
        let existing = [m(3, 4, MappingMode::Remap)];
        assert!(check_source(4, MappingMode::Opposite, &existing, None).is_ok());
    }

    /* ---- 错误提示要给出解法 ---- */

    #[test]
    fn conflict_message_names_the_blocking_mapping() {
        let existing = [m(3, 4, MappingMode::Opposite)];
        let err = validate_against(&m(2, 4, MappingMode::Opposite), &existing, None)
            .expect_err("应当拒绝");
        assert!(err.contains("请先删掉"), "缺少操作指引：{err}");
        assert!(err.contains("重映射"), "缺少替代方案：{err}");
    }

    #[test]
    fn rejects_scan_code_out_of_range() {
        // 查找表只有 0xE200 个槽位，超出的会被静默丢弃
        assert!(check_source(0xF000, MappingMode::Opposite, &[], None).is_err());
        assert!(check_destination(0xF000, MappingMode::Opposite, &[], None).is_err());
        assert!(check_source(0xE14B, MappingMode::Opposite, &[], None).is_ok());
    }

    #[test]
    fn self_mapping_message_offers_action() {
        let err = validate_against(&m(5, 5, MappingMode::Opposite), &[], None)
            .expect_err("应当拒绝");
        assert!(err.contains("换一个目标键"), "缺少操作指引：{err}");
    }

    /* ---- 只填目标键时的单独校验 ---- */

    #[test]
    fn destination_stage_rejects_used_source() {
        // A 已经是源键，不能拿来当目标
        let existing = [m(0x1E, 0x20, MappingMode::Opposite)];
        let err = check_destination(0x1E, MappingMode::Opposite, &existing, None)
            .expect_err("源键不能当目标键");
        assert!(err.contains("不能再当目标键"), "提示不清楚：{err}");
    }

    #[test]
    fn destination_stage_rejects_pair_target() {
        // D 已是 A->D 的目标，再指向 D 会撞车
        let existing = [m(0x1E, 0x20, MappingMode::Opposite)];
        assert!(
            check_destination(0x20, MappingMode::Opposite, &existing, None).is_err()
        );
    }

    #[test]
    fn destination_stage_allows_free_key() {
        let existing = [m(0x1E, 0x20, MappingMode::Opposite)];
        assert!(check_destination(0x1F, MappingMode::Opposite, &existing, None).is_ok());
    }

    #[test]
    fn remap_destination_may_repeat() {
        // 重映射的目标可以重复指向
        let existing = [m(0x1E, 0x20, MappingMode::Remap)];
        assert!(check_destination(0x20, MappingMode::Remap, &existing, None).is_ok());
    }

    #[test]
    fn remap_target_usable_as_pair_destination() {
        // 重映射用过 D 之后，对向映射仍可把 D 当目标
        let existing = [m(0x1E, 0x20, MappingMode::Remap)];
        assert!(
            check_destination(0x20, MappingMode::Opposite, &existing, None).is_ok()
        );
    }
}
