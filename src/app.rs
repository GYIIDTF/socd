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

        // 立刻按当前前台判断是否生效，而不是先假定"启用"等下一次轮询纠正。
        // 否则选中某个程序的配置时，界面会短暂显示"对 X 生效中"——
        // 即便 X 根本不在前台。
        self.poll_focus();
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
