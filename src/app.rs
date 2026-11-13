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
    pub modes: Vec<ModeOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeOption {
    pub value: String,
    pub label: String,
    pub hint: String,
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
}

impl App {
    pub fn load() -> App {
        let path = settings_path();
        // 首次运行没有配置文件是正常状态，不是错误：用默认值启动，并把这份
        // 默认配置写盘，下次启动就有文件了。
        let first_run = !path.exists();
        let settings = match Settings::load(&path) {
            Ok(s) => s,
            Err(e) => {
                if !first_run {
                    crate::logging::warn(&format!("读取配置失败（{e}），使用默认值"));
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
        };

        if app.settings.profiles.is_empty() {
            app.settings.profiles.push(Profile::new(""));
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
            m.table.install(&effective);
            m.keys.reset();
        }
        self.hook.generation.fetch_add(1, Ordering::Relaxed);

        // An empty program name means "apply everywhere", matching the
        // original's fallback to the first profile.
        let armed = !profile.program.is_empty();
        self.armed = if armed {
            Some(profile.program.clone())
        } else {
            None
        };
        self.hook.enabled.store(armed, Ordering::Relaxed);
    }

    /// Called on a timer: enables the hook only while the armed program is focused.
    pub fn poll_focus(&mut self) {
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
            // 跨越启停边界时不保留按键状态，否则会漏发 up 事件
            self.hook.machine.lock().unwrap().keys.reset();
            // 让界面能立刻看到状态变化
            self.armed = if should_enable {
                Some(program.clone())
            } else {
                None
            };
        }
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
                })
                .collect(),
            active_index: active,
            armed_program: self.armed.clone(),
            hooked: self.guard.is_some(),
            settings_path: self.path.display().to_string(),
            modes: mode_options(),
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
    /// 三类问题：
    ///   * 源键重复：表里按源键索引，后写的会覆盖先写的，界面上却显示两条；
    ///   * 映射到自己：REMAP 会吞掉按键又注入同一个键，等于把这个键废掉；
    ///   * 目标键共享：对向类模式靠“目标键的反向映射”找回配对，
    ///     一个键只能属于一对。两条指向同一个目标键时，后一条的反向映射
    ///     装不进去，那一对就永远不会互相感知——配置看着成功，实际不工作。
    ///
    /// REMAP 不受第三条限制：它本就是单向的，多个键指向同一目标完全合理。
    fn validate_mapping(&self, m: &Mapping, skip_index: Option<usize>) -> Result<(), String> {
        let idx = self
            .settings
            .active_profile_index
            .min(self.settings.profiles.len().saturating_sub(1));
        let Some(profile) = self.settings.profiles.get(idx) else {
            return validate_against(m, &[], skip_index);
        };
        validate_against(m, &profile.mappings, skip_index)
    }
}

/// 校验主体，脱离 `App` 以便单测。
fn validate_against(
    m: &Mapping,
    existing_all: &[Mapping],
    skip_index: Option<usize>,
) -> Result<(), String> {
    if m.source == m.destination {
        return Err("不能把按键映射到它自己".into());
    }

    for (i, existing) in existing_all.iter().enumerate() {
        if Some(i) == skip_index {
            continue;
        }
        // 草稿没有真实按键，不参与查重
        if existing.source == DRAFT_SCAN {
            continue;
        }
        if existing.source == m.source {
            return Err(format!(
                "{} 已经有一条映射了，请先删除或改成别的键",
                win::key_name(m.source)
            ));
        }

        // 对向类映射会额外占用目标键的槽位（用来存反向映射）。
        // 槽位冲突意味着镜像装不进去，那一对永远不会互相感知。
        //
        // 占位规则：映射 S→D 占 S；若为对向类，还要占 D。
        // 于是新映射与已有映射冲突，当且仅当：
        //   a) 新映射的源 S 撞上已有的对向占位 D'
        //   b) 两者都是对向类，且目标相同（都想占 D'）
        //   c) 新映射的对向占位 D 撞上已有映射的源 S'
        let m_pairing = is_pairing_mode(m.mode);
        let e_pairing = is_pairing_mode(existing.mode);
        if m_pairing {
            let same_target = existing.destination == m.destination;
            let steals_slot = existing.source == m.destination;
            let clobbers_mirror = e_pairing && existing.destination == m.source;

            if same_target && e_pairing {
                return Err(format!(
                    "{} 已经和 {} 组成了一对，一个键只能属于一对映射",
                    win::key_name(m.destination),
                    win::key_name(existing.source)
                ));
            }
            if steals_slot || clobbers_mirror {
                return Err(format!(
                    "{} 已经有一条对向映射了，一个键只能属于一对映射",
                    win::key_name(if steals_slot { m.destination } else { m.source })
                ));
            }
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
                    return Err("编号超出范围".into());
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
                    return Err("编号超出范围".into());
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

            "updateMappingMode" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                let mode = payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .and_then(MappingMode::from_settings_str)
                    .ok_or("bad mode")?;
                let profile = self.active_profile_mut()?;
                let m = profile.mappings.get_mut(index).ok_or("index out of range")?;
                m.mode = mode;
                self.save()?;
                self.reapply();
                Ok(self.state()?)
            }

            "deleteMapping" => {
                let index = payload.get("index").and_then(Value::as_u64).ok_or("missing index")? as usize;
                let profile = self.active_profile_mut()?;
                if index >= profile.mappings.len() {
                    return Err("编号超出范围".into());
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
                // 只设了一侧时仍是草稿：不查重、不落盘、不进查找表。
                // 两侧都齐了才算一条可用的映射。
                let complete = source != DRAFT_SCAN && destination != DRAFT_SCAN;

                if complete {
                    // 改键同样要查重，否则两条映射会指向同一个源键
                    self.validate_mapping(
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

                if complete {
                    self.save()?;
                }
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
                    let out = crate::socd::handle(table, keys, scan, down, is_repeat);
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
}
