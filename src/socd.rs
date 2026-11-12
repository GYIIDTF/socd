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
}

impl Default for KeyState {
    fn default() -> Self {
        KeyState {
            down: vec![false; BindTable::SIZE],
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

    /// Clears everything — used when hooks are torn down so no stale state leaks
    /// between profiles.
    pub fn reset(&mut self) {
        self.down.iter_mut().for_each(|d| *d = false);
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
            let action = if is_down {
                Inject::Down(bind.destination)
            } else {
                Inject::Up(bind.destination)
            };
            Outcome::swallow(vec![action])
        }

        MappingMode::Opposite | MappingMode::OppositeNoRepress => {
            if is_down {
                let opposite_down = state.is_down(bind.destination);
                state.set(code, true);
                if opposite_down && !is_repeat {
                    // Last one pressed wins: release the opposite, then let this
                    // key through so the game sees the new direction.
                    Outcome::through_with(vec![Inject::Up(bind.destination)])
                } else {
                    Outcome::through()
                }
            } else {
                let opposite_down = state.is_down(bind.destination);
                state.set(code, false);
                if opposite_down && mode != MappingMode::OppositeNoRepress {
                    // The winner was released while the opposite is still held:
                    // re-press it so the character keeps moving.
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
