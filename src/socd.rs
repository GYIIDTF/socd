//! The SOCD / remap state machine.
//!
//! Deliberately free of Win32 calls so it can be unit tested. The platform layer
//! feeds it physical key events and performs whatever `Action`s come back.

use crate::settings::{Mapping, MappingMode, ScanCode};

/// What the platform layer should do with an injected key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inject {
    /// Inject a key-down for this scan code.
    Down(ScanCode),
    /// Inject a key-up for this scan code.
    Up(ScanCode),
}

/// Verdict on the physical event that produced these actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// Let the original key continue to the focused program.
    Through,
    /// Swallow the original key.
    Swallow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub actions: Vec<Inject>,
    pub pass: Pass,
}

impl Outcome {
    fn through() -> Self {
        Outcome {
            actions: Vec::new(),
            pass: Pass::Through,
        }
    }

    fn swallow(actions: Vec<Inject>) -> Self {
        Outcome {
            actions,
            pass: Pass::Swallow,
        }
    }
}

/// Flat scan-code indexed lookup table, mirroring the original's `active_binds`.
/// Reading is O(1) and lock-free from the hook thread once installed.
#[derive(Debug, Clone)]
pub struct BindTable {
    /// `None` means "no mapping for this scan code".
    binds: Vec<Option<Mapping>>,
}

// Must allocate the full table: a derived Default would hand out an empty Vec
// and every `set` would silently no-op.
impl Default for BindTable {
    fn default() -> Self {
        Self::new()
    }
}

impl BindTable {
    pub const SIZE: usize = 0xE200;

    pub fn new() -> Self {
        BindTable {
            binds: vec![None; Self::SIZE],
        }
    }

    pub fn clear(&mut self) {
        self.binds.iter_mut().for_each(|b| *b = None);
    }

    fn set(&mut self, code: ScanCode, mapping: Mapping) {
        if let Some(slot) = self.binds.get_mut(code as usize) {
            *slot = Some(mapping);
        }
    }

    #[cfg(test)]
    fn binds_len(&self) -> usize {
        self.binds.len()
    }

    pub fn get(&self, code: ScanCode) -> Option<Mapping> {
        self.binds.get(code as usize).copied().flatten()
    }

    /// Installs a profile's mappings.
    ///
    /// Opposite-like modes need both directions in the table so the state
    /// machine can look up "the key opposite this one". When the config already
    /// lists both directions (the presets do, and users may too) the explicit
    /// entry wins — mirroring it on top would overwrite the pairing and make
    /// each key point at itself.
    pub fn install(&mut self, mappings: &[Mapping]) {
        self.clear();
        for m in mappings {
            self.set(m.source, *m);
        }
        for m in mappings {
            let mirrored = Mapping {
                source: m.destination,
                destination: m.source,
                mode: m.mode,
            };
            if matches!(
                m.mode,
                MappingMode::Opposite | MappingMode::OppositeNoRepress | MappingMode::Neutral
            ) && self.get(m.destination).is_none()
            {
                self.set(m.destination, mirrored);
            }
        }
    }
}

/// Tracks whether each physical key is currently held.
///
/// `GetAsyncKeyState` is not usable from inside a low-level hook, so we keep
/// our own copy — same reason the original maintained `kb_real_state`.
#[derive(Debug, Clone)]
pub struct KeyState {
    down: Vec<bool>,
    /// 该键当前是否被对向伙伴压制（游戏那边认为它已松开）。
    /// 自动重复事件没有别的办法区分"赢家的重复"和"输家的重复"。
    suppressed: Vec<bool>,
    /// 每个目标键当前被多少个源键"按着"。
    ///
    /// 多个重映射可以指向同一个目标键（Q→W、E→W 都合法）。松开其中一个源键
    /// 时目标键不能立刻松开，否则 W 会在 E 还按着的时候被提前释放。
    /// 计数归零才发 up。
    ///
    /// 同时它也是钩子关闭时的补发清单：非零项就是仍处于按下状态的目标键。
    held_by: Vec<u32>,
    /// 物理事件被完全藏起来的修饰键。
    ///
    /// 系统从没见过它的按下，所以后续的重复、松开都不能放出去——否则会
    /// 看到一个孤立的 keydown/keyup 对，照样触发窗口菜单。
    hidden: Vec<bool>,
}

impl Default for KeyState {
    fn default() -> Self {
        KeyState {
            down: vec![false; BindTable::SIZE],
            suppressed: vec![false; BindTable::SIZE],
            held_by: vec![0; BindTable::SIZE],
            hidden: vec![false; BindTable::SIZE],
        }
    }
}

impl KeyState {
    pub fn is_down(&self, code: ScanCode) -> bool {
        self.down.get(code as usize).copied().unwrap_or(false)
    }

    pub fn set(&mut self, code: ScanCode, down: bool) {
        if let Some(slot) = self.down.get_mut(code as usize) {
            *slot = down;
        }
    }

    pub fn suppressed(&self, code: ScanCode) -> bool {
        self.suppressed.get(code as usize).copied().unwrap_or(false)
    }

    pub fn set_suppressed(&mut self, code: ScanCode, v: bool) {
        if let Some(slot) = self.suppressed.get_mut(code as usize) {
            *slot = v;
        }
    }

    /// Clears everything — used when hooks are torn down so no stale state leaks
    /// between profiles.
    pub fn is_hidden(&self, code: ScanCode) -> bool {
        self.hidden.get(code as usize).copied().unwrap_or(false)
    }

    pub fn set_hidden(&mut self, code: ScanCode, v: bool) {
        if let Some(slot) = self.hidden.get_mut(code as usize) {
            *slot = v;
        }
    }

    /// 目标键多了一个源键按住。返回 true 表示需要真正注入 down。
    pub fn acquire(&mut self, code: ScanCode) -> bool {
        match self.held_by.get_mut(code as usize) {
            Some(n) => {
                *n = n.saturating_add(1);
                *n == 1
            }
            None => false,
        }
    }

    /// 目标键少了一个源键。返回 true 表示计数归零、需要真正注入 up。
    pub fn release(&mut self, code: ScanCode) -> bool {
        match self.held_by.get_mut(code as usize) {
            Some(n) if *n > 0 => {
                *n -= 1;
                *n == 0
            }
            _ => true,
        }
    }

    /// 取出仍处于"已按下"状态的目标键，并清零。
    pub fn take_pending(&mut self) -> Vec<ScanCode> {
        let mut out = Vec::new();
        for (i, n) in self.held_by.iter_mut().enumerate() {
            if *n > 0 {
                *n = 0;
                out.push(i as ScanCode);
            }
        }
        out
    }

    pub fn reset(&mut self) {
        self.down.iter_mut().for_each(|d| *d = false);
        self.suppressed.iter_mut().for_each(|d| *d = false);
        self.hidden.iter_mut().for_each(|d| *d = false);
        // injected 不清空：调用方要用它补发 up
    }
}

/// Decides what to do with one physical key event.
///
/// `is_repeat` suppresses the key-repeat storm Windows sends for a held key;
/// the original ignored repeats in the opposite/neutral branches for the same
/// reason.
pub fn handle(
    table: &BindTable,
    state: &mut KeyState,
    code: ScanCode,
    is_down: bool,
    is_repeat: bool,
) -> Outcome {
    handle_with(table, state, code, is_down, is_repeat, false)
}

/// 同 `handle`，额外告知"这个键是否是修饰键"（Alt/Ctrl/Shift/Win）。
///
/// 修饰键需要特殊处理：Windows 看到裸 Alt 的 keydown 就会激活窗口菜单，
/// 而那个判断发生在低层钩子之外——只要放行就来不及了。所以当修饰键作为
/// 对向映射的源键、并且它的伙伴已经按着时，连物理事件一起吞掉。
///
/// 只在"伙伴已按下"这一种情况下吞：单独按 Alt、Alt+F4、Alt+Tab 都不受影响。
pub fn handle_with(
    table: &BindTable,
    state: &mut KeyState,
    code: ScanCode,
    is_down: bool,
    is_repeat: bool,
    is_modifier: bool,
) -> Outcome {
    let bind = match table.get(code) {
        Some(b) => b,
        None => {
            state.set(code, is_down);
            return Outcome::through();
        }
    };

    let mode = bind.mode;
    match mode {
        MappingMode::Remap => {
            state.set(code, is_down);
            // 自动重复不是新的一次按下，不能重复计数——否则计数永远归不了零，
            // 松开时就不会发 up，目标键卡住。
            if is_repeat {
                return Outcome::swallow(vec![]);
            }
            // 多个源键可以指向同一个目标键。按下时只有第一个真正注入 down，
            // 松开时只有最后一个才注入 up——否则 Q→W、E→W 同时按住时，
            // 松开 Q 会把还该按着的 W 提前释放。
            let action = if is_down {
                if state.acquire(bind.destination) {
                    Some(Inject::Down(bind.destination))
                } else {
                    None
                }
            } else if state.release(bind.destination) {
                Some(Inject::Up(bind.destination))
            } else {
                None
            };
            // 原键一律吞掉，有没有注入是另一回事
            match action {
                Some(a) => Outcome::swallow(vec![a]),
                None => Outcome::swallow(vec![]),
            }
        }

        MappingMode::Opposite | MappingMode::OppositeNoRepress => {
            if is_down {
                let opposite_down = state.is_down(bind.destination);
                // 重复事件里的"是否被压制"不能用对面是否按下来判断——
                // 赢家自己也会重复，而它的对面同样按着。真正该问的是：
                // 上一次这台机器把哪个键判定为生效方向。
                let suppressed = state.suppressed(code);
                state.set(code, true);
                if is_repeat {
                    // hidden 优先：修饰键的物理事件被完全藏起来之后，
                    // 它的每一次重复都不能让系统看见。
                    if state.is_hidden(code) || suppressed {
                        Outcome::swallow(vec![])
                    } else {
                        Outcome::through()
                    }
                } else if opposite_down {
                    // Last one pressed wins: release the opposite, then let this
                    // key through so the game sees the new direction.
                    state.set_suppressed(bind.destination, true);
                    if is_modifier {
                        // 物理 Alt 的 keydown 会让 Windows 激活窗口菜单，而那个
                        // 判断发生在钩子之外——所以物理事件必须吞掉。
                        //
                        // 但用户要的是 "Alt 生效"，不是 "Alt 消失"。解决办法：
                        // 吞掉物理事件后注入一个等价的 Alt。注入事件带
                        // LLKHF_INJECTED 标记，系统不会拿它触发菜单（实测确认），
                        // 却会正常更新按键状态，游戏和其他程序都能看到 Alt 按下。
                        //
                        // hidden 继续标记：它约束的是"物理事件"的处理方式，
                        // 后续的重复和松开都要按隐藏键的规则走。
                        state.set_suppressed(code, true);
                        state.set_hidden(code, true);
                        // 先释放伙伴，再注入自己：顺序反了会让系统短暂看到
                        // 两个键都不按，菜单有机会抢先。
                        Outcome::swallow(vec![
                            Inject::Up(bind.destination),
                            Inject::Down(code),
                        ])
                    } else {
                        state.set_suppressed(code, false);
                        Outcome::through_with(vec![Inject::Up(bind.destination)])
                    }
                } else {
                    state.set_suppressed(code, false);
                    Outcome::through()
                }
            } else {
                let opposite_down = state.is_down(bind.destination);
                let was_hidden = state.is_hidden(code);
                state.set(code, false);
                state.set_suppressed(code, false);
                if was_hidden {
                    // 这个键的物理按下被吞了、改由注入代替，所以物理松开
                    // 同样不能放行——要发出和注入按下对应的 up。
                    state.set_hidden(code, false);
                    // 伙伴当时被我们释放了，现在轮到它重新生效。
                    // 顺序与按下时相反：先松开自己，再按回伙伴，
                    // 避免系统看到"两个都不按"的空档。
                    let mut actions = vec![Inject::Up(code)];
                    if opposite_down && mode != MappingMode::OppositeNoRepress {
                        state.set_suppressed(bind.destination, false);
                        actions.push(Inject::Down(bind.destination));
                    }
                    return Outcome::swallow(actions);
                }
                if opposite_down && mode != MappingMode::OppositeNoRepress {
                    // The winner was released while the opposite is still held:
                    // re-press it so the character keeps moving.
                    state.set_suppressed(bind.destination, false);
                    Outcome::through_with(vec![Inject::Down(bind.destination)])
                } else {
                    Outcome::through()
                }
            }
        }

        MappingMode::Neutral => {
            if is_down {
                let opposite_down = state.is_down(bind.destination);
                state.set(code, true);
                if opposite_down {
                    // Both cancel out: release the opposite and swallow this one.
                    Outcome::swallow(vec![Inject::Up(bind.destination)])
                } else {
                    Outcome::through()
                }
            } else {
                state.set(code, false);
                Outcome::through()
            }
        }
    }
}

impl Outcome {
    fn through_with(actions: Vec<Inject>) -> Self {
        Outcome {
            actions,
            pass: Pass::Through,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: ScanCode = 0x11;
    const S: ScanCode = 0x1F;
    const A: ScanCode = 0x1E;
    const D: ScanCode = 0x20;

    fn table_with(mode: MappingMode, a: ScanCode, b: ScanCode) -> BindTable {
        let mut t = BindTable::new();
        t.install(&[
            Mapping {
                source: a,
                destination: b,
                mode,
            },
            Mapping {
                source: b,
                destination: a,
                mode,
            },
        ]);
        t
    }

    #[test]
    fn remap_swallows_and_injects() {
        let mut t = BindTable::new();
        t.install(&[Mapping {
            source: W,
            destination: S,
            mode: MappingMode::Remap,
        }]);
        let mut st = KeyState::default();

        let down = handle(&t, &mut st, W, true, false);
        assert_eq!(down.pass, Pass::Swallow);
        assert_eq!(down.actions, vec![Inject::Down(S)]);

        let up = handle(&t, &mut st, W, false, false);
        assert_eq!(up.pass, Pass::Swallow);
        assert_eq!(up.actions, vec![Inject::Up(S)]);
    }

    /// 两个源键指向同一个目标：松开一个不能让目标提前释放。
    #[test]
    fn remap_shared_target_holds_until_last_release() {
        let mut t = BindTable::new();
        t.install(&[
            Mapping { source: W, destination: S, mode: MappingMode::Remap },
            Mapping { source: 0x12, destination: S, mode: MappingMode::Remap }, // E -> S
        ]);
        let mut st = KeyState::default();

        let q = handle(&t, &mut st, W, true, false);
        assert_eq!(q.actions, vec![Inject::Down(S)], "第一个源键要注入 down");

        let e = handle(&t, &mut st, 0x12, true, false);
        assert!(e.actions.is_empty(), "第二个源键不能再注入一次 down");

        let q_up = handle(&t, &mut st, W, false, false);
        assert!(q_up.actions.is_empty(), "还有一个源键按着，不能释放目标");

        let e_up = handle(&t, &mut st, 0x12, false, false);
        assert_eq!(e_up.actions, vec![Inject::Up(S)], "最后一个源键松开才发 up");
    }

    /// 自动重复不是新按下，不能把计数抬高（否则永远归不了零）。
    #[test]
    fn remap_repeat_does_not_double_count() {
        let mut t = BindTable::new();
        t.install(&[Mapping { source: W, destination: S, mode: MappingMode::Remap }]);
        let mut st = KeyState::default();

        handle(&t, &mut st, W, true, false);
        let r = handle(&t, &mut st, W, true, true);
        assert!(r.actions.is_empty(), "重复不该再注入");

        let up = handle(&t, &mut st, W, false, false);
        assert_eq!(up.actions, vec![Inject::Up(S)], "松开时必须还能发出 up");
    }

    #[test]
    fn unmapped_keys_pass_through() {
        let t = BindTable::new();
        let mut st = KeyState::default();
        let out = handle(&t, &mut st, 0x39, true, false);
        assert_eq!(out.pass, Pass::Through);
        assert!(out.actions.is_empty());
        assert!(st.is_down(0x39));
    }

    #[test]
    fn opposite_last_pressed_wins() {
        let t = table_with(MappingMode::Opposite, A, D);
        let mut st = KeyState::default();

        // Press A alone: passes through, nothing injected.
        let a = handle(&t, &mut st, A, true, false);
        assert_eq!(a.pass, Pass::Through);
        assert!(a.actions.is_empty());
        assert!(st.is_down(A));

        // Press D while A held: release A, let D through.
        let d = handle(&t, &mut st, D, true, false);
        assert_eq!(d.pass, Pass::Through);
        assert_eq!(d.actions, vec![Inject::Up(A)]);
        assert!(st.is_down(D));

        // Release D while A still physically held: re-press A.
        let d_up = handle(&t, &mut st, D, false, false);
        assert_eq!(d_up.pass, Pass::Through);
        assert_eq!(d_up.actions, vec![Inject::Down(A)]);
        assert!(!st.is_down(D));
    }

    #[test]
    fn opposite_ignores_key_repeat() {
        let t = table_with(MappingMode::Opposite, A, D);
        let mut st = KeyState::default();

        handle(&t, &mut st, A, true, false);
        handle(&t, &mut st, D, true, false);

        // A held, D auto-repeats: must not emit another Up(A).
        let repeat = handle(&t, &mut st, D, true, true);
        assert!(repeat.actions.is_empty());
        // D 是赢家，重复应当照常放行给游戏
        assert_eq!(repeat.pass, Pass::Through);
    }

    /// 被压制的键自动重复时必须吞掉。
    ///
    /// 游戏那边它已经被注入过 Up（对手抢占时），如果这时再放行一个
    /// keydown，游戏就会认为两边同时按下——正是 SOCD 要消除的状态。
    #[test]
    fn opposite_suppressed_key_repeat_is_swallowed() {
        let t = table_with(MappingMode::Opposite, A, D);
        let mut st = KeyState::default();

        handle(&t, &mut st, A, true, false); // A 按下
        handle(&t, &mut st, D, true, false); // D 按下，A 被注入 Up

        // A 仍按住并自动重复
        let repeat = handle(&t, &mut st, A, true, true);
        assert!(repeat.actions.is_empty());
        assert_eq!(repeat.pass, Pass::Swallow, "被压制键的重复不能放行");
    }

    /// 修饰键作为源键、伙伴已按下时，物理事件必须吞掉——否则 Windows
    /// 看到裸 Alt 会激活窗口菜单，用户配的 Alt↔空格 就废了。
    #[test]
    fn modifier_swallowed_when_partner_held() {
        const ALT: ScanCode = 0x38;
        const SPACE: ScanCode = 0x39;
        let t = table_with(MappingMode::Opposite, ALT, SPACE);
        let mut st = KeyState::default();

        handle_with(&t, &mut st, SPACE, true, false, false);   // 先按空格
        let alt = handle_with(&t, &mut st, ALT, true, false, true); // 再按 Alt
        assert_eq!(alt.pass, Pass::Swallow, "物理事件必须吞掉，不能激活系统菜单");
        // 吞掉物理事件后注入一个等价 Alt，让"Alt 生效"名副其实
        assert_eq!(
            alt.actions,
            vec![Inject::Up(SPACE), Inject::Down(ALT)],
            "应先释放伙伴再注入 Alt"
        );
    }

    /// 按住修饰键产生的自动重复也必须吞掉。
    ///
    /// 只吞"按下那一刻"是不够的：按住不放，系统会持续产生重复 keydown，
    /// 每一个都会重新触发窗口菜单。用户看到的正是"单点没事、按住就弹"。
    #[test]
    fn modifier_repeat_stays_swallowed() {
        const ALT: ScanCode = 0x38;
        const SPACE: ScanCode = 0x39;
        let t = table_with(MappingMode::Opposite, ALT, SPACE);
        let mut st = KeyState::default();

        handle_with(&t, &mut st, SPACE, true, false, false);
        let first = handle_with(&t, &mut st, ALT, true, false, true);
        assert_eq!(first.pass, Pass::Swallow, "首次按下要吞");

        // 按住 Alt 产生的重复：每一次都必须吞
        for i in 0..5 {
            let r = handle_with(&t, &mut st, ALT, true, true, true);
            assert_eq!(r.pass, Pass::Swallow, "第 {i} 次重复被放行了");
            assert!(r.actions.is_empty(), "重复不该再注入");
        }
    }

    /// 松开伙伴后，被吞的修饰键仍不能把物理事件放出去。
    ///
    /// Alt 的 keydown 早就被吞了，系统那边从来没有"Alt 按下"这回事。
    /// 此时若因为伙伴松开而放行后续的 Alt 事件，系统会看到一个孤立的
    /// keydown/keyup 对，照样触发菜单。
    #[test]
    fn modifier_stays_hidden_after_partner_released() {
        const ALT: ScanCode = 0x38;
        const SPACE: ScanCode = 0x39;
        let t = table_with(MappingMode::Opposite, ALT, SPACE);
        let mut st = KeyState::default();

        handle_with(&t, &mut st, SPACE, true, false, false);
        handle_with(&t, &mut st, ALT, true, false, true);   // Alt 被吞
        // 空格松开：按"重按先按方向"的规则会注入 Down(空格)
        let sp_up = handle_with(&t, &mut st, SPACE, false, false, false);
        // Alt 仍在按着，它的重复/松开都不该让系统看到
        let alt_rep = handle_with(&t, &mut st, ALT, true, true, true);
        assert_eq!(alt_rep.pass, Pass::Swallow, "伙伴松开后 Alt 的重复仍要吞");
        // 松开 Alt 时要发出与注入按下配对的 up
        let alt_up = handle_with(&t, &mut st, ALT, false, false, true);
        assert_eq!(alt_up.pass, Pass::Swallow);
        assert_eq!(alt_up.actions, vec![Inject::Up(ALT)], "必须补发注入键的 up");
        let _ = sp_up;
    }

    /// 松开修饰键后，被它压制过的伙伴必须重新生效。
    ///
    /// 按住空格 → 按 Alt（空格被释放、Alt 生效）→ 松开 Alt，
    /// 此时空格还按着，应该重新亮起来。漏了这步的话，用户松开 Alt 后
    /// 两个键都是熄灭状态——他明明还按着空格。
    #[test]
    fn modifier_release_restores_partner() {
        const ALT: ScanCode = 0x38;
        const SPACE: ScanCode = 0x39;
        let t = table_with(MappingMode::Opposite, ALT, SPACE);
        let mut st = KeyState::default();

        handle_with(&t, &mut st, SPACE, true, false, false);       // 空格按下
        handle_with(&t, &mut st, ALT, true, false, true);          // Alt 抢占

        // 松开 Alt：空格仍按着，必须重新按下
        let alt_up = handle_with(&t, &mut st, ALT, false, false, true);
        assert_eq!(alt_up.pass, Pass::Swallow);
        assert_eq!(
            alt_up.actions,
            vec![Inject::Up(ALT), Inject::Down(SPACE)],
            "松开 Alt 后必须把空格按回来"
        );
    }

    /// 单独按 Alt 不能被吞——否则 Alt+Tab、Alt+F4 全废。
    #[test]
    fn modifier_alone_passes_through() {
        const ALT: ScanCode = 0x38;
        const SPACE: ScanCode = 0x39;
        let t = table_with(MappingMode::Opposite, ALT, SPACE);
        let mut st = KeyState::default();

        let alt = handle_with(&t, &mut st, ALT, true, false, true);
        assert_eq!(alt.pass, Pass::Through, "单独按 Alt 必须放行");
        assert!(alt.actions.is_empty());
    }

    /// 非修饰键不受这条规则影响，行为与原来一致。
    #[test]
    fn non_modifier_still_passes_through() {
        let t = table_with(MappingMode::Opposite, A, D);
        let mut st = KeyState::default();

        handle_with(&t, &mut st, A, true, false, false);
        let d = handle_with(&t, &mut st, D, true, false, false);
        assert_eq!(d.pass, Pass::Through, "普通键照常放行");
        assert_eq!(d.actions, vec![Inject::Up(A)]);
    }

    #[test]
    fn opposite_no_repress_suppressed_repeat_is_swallowed() {
        let t = table_with(MappingMode::OppositeNoRepress, A, D);
        let mut st = KeyState::default();

        handle(&t, &mut st, A, true, false);
        handle(&t, &mut st, D, true, false);

        let repeat = handle(&t, &mut st, A, true, true);
        assert_eq!(repeat.pass, Pass::Swallow);
    }

    #[test]
    fn opposite_no_repress_does_not_repress() {
        let t = table_with(MappingMode::OppositeNoRepress, A, D);
        let mut st = KeyState::default();

        handle(&t, &mut st, A, true, false);
        let d = handle(&t, &mut st, D, true, false);
        assert_eq!(d.actions, vec![Inject::Up(A)]);

        // Releasing the winner leaves the opposite released.
        let d_up = handle(&t, &mut st, D, false, false);
        assert!(d_up.actions.is_empty());
    }

    #[test]
    fn neutral_cancels_both() {
        let t = table_with(MappingMode::Neutral, A, D);
        let mut st = KeyState::default();

        handle(&t, &mut st, A, true, false);
        let d = handle(&t, &mut st, D, true, false);
        assert_eq!(d.pass, Pass::Swallow);
        assert_eq!(d.actions, vec![Inject::Up(A)]);

        // Releasing either one does nothing special.
        let d_up = handle(&t, &mut st, D, false, false);
        assert_eq!(d_up.pass, Pass::Through);
        assert!(d_up.actions.is_empty());
    }

    #[test]
    fn neutral_first_key_alone_goes_through() {
        let t = table_with(MappingMode::Neutral, A, D);
        let mut st = KeyState::default();
        let a = handle(&t, &mut st, A, true, false);
        assert_eq!(a.pass, Pass::Through);
        assert!(a.actions.is_empty());
    }

    #[test]
    fn default_table_is_fully_sized() {
        let t = BindTable::default();
        assert_eq!(t.binds_len(), BindTable::SIZE);
        let mut t = BindTable::default();
        t.install(&[Mapping {
            source: 0x1E,
            destination: 0x20,
            mode: MappingMode::Opposite,
        }]);
        assert!(t.get(0x1E).is_some());
    }

    #[test]
    fn install_one_direction_lands_in_the_table() {
        let mut t = BindTable::new();
        t.install(&[Mapping {
            source: 0x1E,
            destination: 0x20,
            mode: MappingMode::Opposite,
        }]);
        let got = t.get(0x1E);
        assert!(got.is_some(), "source slot empty after install");
        assert_eq!(got.unwrap().destination, 0x20);
        assert!(t.get(0x20).is_some(), "mirrored slot empty after install");
    }

    #[test]
    fn explicit_both_directions_are_not_clobbered_by_mirroring() {
        // Presets ship both directions; mirroring on top used to overwrite the
        // second entry so each key pointed at itself.
        let t = table_with(MappingMode::Opposite, A, D);
        assert_eq!(t.get(A).unwrap().destination, D);
        assert_eq!(t.get(D).unwrap().destination, A);

        // Same thing through the raw install path, one direction only.
        let mut t2 = BindTable::new();
        t2.install(&[Mapping {
            source: A,
            destination: D,
            mode: MappingMode::Opposite,
        }]);
        assert_eq!(t2.get(A).unwrap().destination, D);
        assert_eq!(t2.get(D).unwrap().destination, A);
    }

    #[test]
    fn install_registers_both_directions_for_opposite() {
        let t = table_with(MappingMode::Opposite, A, D);
        assert!(t.get(A).is_some());
        assert!(t.get(D).is_some());
        assert_eq!(t.get(A).unwrap().destination, D);
        assert_eq!(t.get(D).unwrap().destination, A);
    }

    #[test]
    fn install_does_not_mirror_plain_remap() {
        let mut t = BindTable::new();
        t.install(&[Mapping {
            source: W,
            destination: S,
            mode: MappingMode::Remap,
        }]);
        assert!(t.get(W).is_some());
        assert!(t.get(S).is_none());
    }

    #[test]
    fn extended_scan_codes_are_addressable() {
        let mut t = BindTable::new();
        let up_ext = 0xE148u32;
        let down_ext = 0xE150u32;
        t.install(&[Mapping {
            source: up_ext,
            destination: down_ext,
            mode: MappingMode::Neutral,
        }]);
        assert!(t.get(up_ext).is_some());
        assert_eq!(t.get(up_ext).unwrap().destination, down_ext);
    }

    #[test]
    fn wasd_preset_shape() {
        // Mirrors the original's preset: W/S and A/D as two opposite pairs.
        let t = table_with(MappingMode::Opposite, W, S);
        assert_eq!(t.get(W).unwrap().destination, S);
        assert_eq!(t.get(S).unwrap().destination, W);
    }
}

#[cfg(test)]
mod adversarial {
    use super::*;

    /// 同一个键既是某条 REMAP 的源，又是另一条对向映射的目标：
    /// 表里只能存一条，后写的覆盖前面的。确认不会 panic，且行为可预测。
    #[test]
    fn conflicting_roles_do_not_panic() {
        let mut t = BindTable::new();
        t.install(&[
            Mapping { source: 0x11, destination: 0x1F, mode: MappingMode::Remap },
            Mapping { source: 0x1E, destination: 0x11, mode: MappingMode::Opposite },
        ]);
        let mut st = KeyState::default();
        // 不管装成什么，处理这些键都不能崩
        for code in [0x11u32, 0x1E, 0x1F] {
            let _ = handle(&t, &mut st, code, true, false);
            let _ = handle(&t, &mut st, code, false, false);
        }
    }

    /// 映射到自己：REMAP 会把按键吞掉且注入同一个键，等于该键永久失效。
    /// 这类配置应该在写入层被拒绝；这里记录当前行为。
    #[test]
    fn self_mapping_swallows_but_injects_same_key() {
        let mut t = BindTable::new();
        t.install(&[Mapping { source: 0x11, destination: 0x11, mode: MappingMode::Remap }]);
        let mut st = KeyState::default();
        let out = handle(&t, &mut st, 0x11, true, false);
        assert_eq!(out.pass, Pass::Swallow);
        assert_eq!(out.actions, vec![Inject::Down(0x11)]);
    }

    /// 重复的源键：后一条覆盖前一条，用户看到两条但只有一条生效。
    #[test]
    fn duplicate_source_last_wins() {
        let mut t = BindTable::new();
        t.install(&[
            Mapping { source: 0x11, destination: 0x1F, mode: MappingMode::Remap },
            Mapping { source: 0x11, destination: 0x20, mode: MappingMode::Remap },
        ]);
        assert_eq!(t.get(0x11).unwrap().destination, 0x20);
    }

    /// 按住一个键期间切换 profile：状态被重置，但物理键还按着。
    /// 之后松开时状态机认为它本来就没按下，不会发出 up。
    #[test]
    fn release_after_reset_while_held() {
        let mut t = BindTable::new();
        t.install(&[Mapping { source: 0x11, destination: 0x1F, mode: MappingMode::Remap }]);
        let mut st = KeyState::default();

        handle(&t, &mut st, 0x11, true, false);   // 按下
        st.reset();                                // profile 切换
        let up = handle(&t, &mut st, 0x11, false, false);
        // REMAP 的 keyup 仍然会注入，这是对的
        assert_eq!(up.actions, vec![Inject::Up(0x1F)]);
    }
}
