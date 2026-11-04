//! Tap-dance (`DT`): one action on a tap, another on a second tap within the window, and the
//! first held when the key stays down past the tapping term.
//!
//! DT works with any `KeyAction`, not just a key: when an action fires, it is pressed through
//! its own `emit`, and what that leaves held is let go of when the tap-dance key comes up.
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction};
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TdState {
    Undecided,
    HoldingFirst,
    Tapped,
    TappingSecond,
}

#[derive(Debug, Clone)]
pub(crate) struct TdKey {
    pub tap_action: KeyAction,
    pub double_tap_action: KeyAction,
    pub first_press_at: Instant,
    pub state: TdState,
    pub tap_count: u32,
}

impl TdKey {
    pub const fn new(
        tap_action: KeyAction,
        double_tap_action: KeyAction,
        first_press_at: Instant,
    ) -> Self {
        Self {
            tap_action,
            double_tap_action,
            first_press_at,
            state: TdState::Undecided,
            tap_count: 0,
        }
    }

    /// Milliseconds from the first press to `at`.
    pub(crate) fn elapsed_since_press(&self, at: Instant) -> u128 {
        at.saturating_duration_since(self.first_press_at)
            .as_millis()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TdResolution {
    EmitAction(KeyAction),
    Undecided,
}

// The suffix is the unit, and config.ron spells these the same way
#[allow(clippy::struct_field_names)]
pub(crate) struct TdConfig {
    pub tapping_term_ms: u32,
    pub double_tap_window_ms: u64,
    pub grace_period_ms: u64,
}

pub struct DtProcessor {
    config: TdConfig,
    tracked_keys: HashMap<KeyCode, TdKey>,
}

impl DtProcessor {
    pub fn new(config: &Config) -> Self {
        Self {
            config: TdConfig {
                tapping_term_ms: config.tapping_term_ms,
                double_tap_window_ms: config.double_tap_window_ms.unwrap_or(250),
                grace_period_ms: 500,
            },
            tracked_keys: HashMap::new(),
        }
    }

    /// Check if any keys other than the given keycode are tracked
    pub(crate) fn has_other_keys_tracked(&self, keycode: KeyCode) -> bool {
        self.tracked_keys.keys().any(|&k| k != keycode)
    }

    /// Check if a key is already in a holding state
    pub(crate) fn is_holding(&self, keycode: KeyCode) -> bool {
        self.tracked_keys
            .get(&keycode)
            .is_some_and(|td_key| matches!(td_key.state, TdState::HoldingFirst))
    }

    /// The key went down at `at`: what that decides, given its taps so far.
    pub fn resolve_action(
        &mut self,
        keycode: KeyCode,
        tap_action: &KeyAction,
        double_tap_action: &KeyAction,
        other_key_pressed: bool,
        at: Instant,
    ) -> TdResolution {
        let Some(td_key) = self.tracked_keys.get_mut(&keycode) else {
            let mut td_key = TdKey::new(tap_action.clone(), double_tap_action.clone(), at);
            td_key.state = TdState::Tapped;
            td_key.tap_count = 1;
            self.tracked_keys.insert(keycode, td_key);
            return TdResolution::Undecided;
        };
        let elapsed = td_key.elapsed_since_press(at);
        match td_key.state {
            TdState::Undecided => {
                // Permissive hold: another key pressed resolves it as a hold at once
                if other_key_pressed || elapsed > u128::from(self.config.tapping_term_ms) {
                    td_key.state = TdState::HoldingFirst;
                    return TdResolution::EmitAction(td_key.tap_action.clone());
                }
                td_key.state = TdState::Tapped;
                td_key.tap_count = 1;
                TdResolution::Undecided
            }
            TdState::Tapped => {
                if elapsed <= u128::from(self.config.double_tap_window_ms) {
                    td_key.state = TdState::TappingSecond;
                    td_key.tap_count = 2;
                    return TdResolution::EmitAction(td_key.double_tap_action.clone());
                }
                // The window passed: this press starts over
                *td_key = TdKey::new(tap_action.clone(), double_tap_action.clone(), at);
                TdResolution::Undecided
            }
            TdState::TappingSecond => {
                if elapsed <= u128::from(self.config.grace_period_ms) {
                    td_key.tap_count += 1;
                    return TdResolution::EmitAction(td_key.double_tap_action.clone());
                }
                TdResolution::Undecided
            }
            TdState::HoldingFirst => TdResolution::EmitAction(td_key.tap_action.clone()),
        }
    }

    /// The key came up at `at`. A tap inside the window stays tracked for its second tap.
    pub fn unemit_action(&mut self, keycode: KeyCode, at: Instant) {
        let window = u128::from(self.config.double_tap_window_ms);
        if let Some(td_key) = self.tracked_keys.get(&keycode) {
            let settled = matches!(td_key.state, TdState::Undecided | TdState::HoldingFirst);
            if settled || td_key.elapsed_since_press(at) > window {
                self.tracked_keys.remove(&keycode);
            }
        }
    }

    /// When the earliest tracked key next needs its timeouts checked, if any is waiting on one.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.tracked_keys
            .values()
            .filter_map(|td_key| {
                let term_ms = match td_key.state {
                    TdState::Undecided => u64::from(self.config.tapping_term_ms),
                    TdState::Tapped | TdState::TappingSecond => self.config.double_tap_window_ms,
                    TdState::HoldingFirst => return None,
                };
                // The checks count whole milliseconds and fire once the term is exceeded
                Some(td_key.first_press_at + Duration::from_millis(term_ms + 1))
            })
            .min()
    }

    /// Settle what time alone decides, as of `now`: a first press down past the term is a
    /// hold, and a tap whose window passed with no second tap is over.
    pub fn expire(&mut self, now: Instant) {
        let term = u128::from(self.config.tapping_term_ms);
        let window = u128::from(self.config.double_tap_window_ms);
        self.tracked_keys.retain(|_, td_key| {
            let elapsed = td_key.elapsed_since_press(now);
            match td_key.state {
                TdState::Undecided => {
                    if elapsed > term {
                        td_key.state = TdState::HoldingFirst;
                    }
                    true
                }
                TdState::Tapped | TdState::TappingSecond => {
                    if elapsed <= window {
                        return true;
                    }
                    if td_key.tap_count >= 2 {
                        td_key.state = TdState::Tapped;
                        td_key.tap_count = 1;
                        return true;
                    }
                    false
                }
                TdState::HoldingFirst => true,
            }
        });
    }
}

pub fn emit_dt(
    action: &KeyAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    let KeyAction::DT(tap_action, double_tap_action) = action else {
        return (EmitResult::None, None);
    };
    // Already holding the first action: nothing more to press
    if ctx.dt_processor.is_holding(keycode) {
        return (EmitResult::None, Some(HeldAction::DtManaged(None)));
    }
    let other_key_pressed = ctx.dt_processor.has_other_keys_tracked(keycode);
    let resolution = ctx.dt_processor.resolve_action(
        keycode,
        tap_action,
        double_tap_action,
        other_key_pressed,
        ctx.at,
    );
    let (result, pressed) = match resolution {
        TdResolution::EmitAction(inner) => {
            let (result, held) = inner.emit(keycode, ctx);
            (result, held.map(|held| Box::new((inner, held))))
        }
        TdResolution::Undecided => (EmitResult::None, None),
    };
    (result, Some(HeldAction::DtManaged(pressed)))
}

pub fn unemit_dt(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    let (KeyAction::DT(..), HeldAction::DtManaged(pressed)) = (action, held_action) else {
        return EmitResult::None;
    };
    ctx.dt_processor.unemit_action(keycode, ctx.at);
    pressed.map_or(EmitResult::None, |pressed| {
        let (inner, held) = *pressed;
        inner.unemit(held, keycode, ctx)
    })
}
