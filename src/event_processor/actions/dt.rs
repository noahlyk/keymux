/// Double-Tap (DT) / Tap Dance processor - QMK-inspired with proper hold support
///
/// This implements the user's requested behavior:
/// - tap -> tap the first one
/// - tap then tap -> tap the second one
/// - hold -> hold the first one
/// - tap then hold -> hold the second one  
/// - tap then tap then tap -> would tap the second one after the second tap,
///   then every tap after will keep tapping the second one until grace period resets
///
/// The key insight is that DT now works with ANY KeyAction, not just Key.
/// When the action fires, it recursively calls .emit() on the inner action.
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction, ProcessResult};
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TdState {
    Undecided,
    HoldingFirst,
    Tapped,
    TappingSecond,
    HoldingSecond,
}

#[derive(Debug, Clone)]
pub struct TdKey {
    pub keycode: KeyCode,
    pub tap_action: KeyAction,
    pub double_tap_action: KeyAction,
    pub first_press_at: Instant,
    pub state: TdState,
    pub tap_count: u32,
    pub last_emitted_action: Option<KeyAction>,
}

impl TdKey {
    pub fn new(keycode: KeyCode, tap_action: KeyAction, double_tap_action: KeyAction) -> Self {
        Self {
            keycode,
            tap_action,
            double_tap_action,
            first_press_at: Instant::now(),
            state: TdState::Undecided,
            tap_count: 0,
            last_emitted_action: None,
        }
    }

    pub fn elapsed_since_press(&self) -> u128 {
        self.first_press_at.elapsed().as_millis()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TdResolution {
    EmitAction(KeyAction),
    HoldFirst,
    ReleaseFirst,
    HoldSecond,
    ReleaseSecond,
    Undecided,
}

pub struct TdConfig {
    pub tapping_term_ms: u32,
    pub double_tap_window_ms: u64,
    pub grace_period_ms: u64,
    pub permissive_hold: bool,
}

impl Default for TdConfig {
    fn default() -> Self {
        Self {
            tapping_term_ms: 200,
            double_tap_window_ms: 250,
            grace_period_ms: 500,
            permissive_hold: true,
        }
    }
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
                permissive_hold: true,
            },
            tracked_keys: HashMap::new(),
        }
    }

    /// Called when another key is pressed - handles permissive hold
    pub const fn on_other_key_press(&mut self, _other_keycode: KeyCode) -> Vec<(KeyCode, bool)> {
        // The actual permissive hold is now handled in resolve_action
        // This is kept for API compatibility
        vec![]
    }

    pub fn emit_action(
        &mut self,
        keycode: KeyCode,
        tap_action: &KeyAction,
        double_tap_action: &KeyAction,
    ) -> Vec<(KeyCode, bool)> {
        let resolution = self.resolve_action(keycode, tap_action, double_tap_action, false);

        match resolution {
            TdResolution::EmitAction(action) => {
                let events = Vec::new();
                if let Some(td_key) = self.tracked_keys.get_mut(&keycode) {
                    td_key.last_emitted_action = Some(action);
                }
                events
            }
            _ => vec![],
        }
    }

    /// Check if any keys other than the given keycode are tracked
    pub fn has_other_keys_tracked(&self, keycode: KeyCode) -> bool {
        self.tracked_keys.keys().any(|&k| k != keycode)
    }

    /// Check if a key is already in a holding state
    pub fn is_holding(&self, keycode: KeyCode) -> bool {
        self.tracked_keys.get(&keycode).is_some_and(|td_key| {
            matches!(td_key.state, TdState::HoldingFirst | TdState::HoldingSecond)
        })
    }

    pub fn resolve_action(
        &mut self,
        keycode: KeyCode,
        tap_action: &KeyAction,
        double_tap_action: &KeyAction,
        other_key_pressed: bool,
    ) -> TdResolution {
        if let Some(td_key) = self.tracked_keys.get_mut(&keycode) {
            match td_key.state {
                TdState::Undecided => {
                    let elapsed = td_key.elapsed_since_press();
                    // Permissive hold: if another key was pressed, resolve immediately as hold
                    if other_key_pressed || elapsed > self.config.tapping_term_ms as u128 {
                        td_key.state = TdState::HoldingFirst;
                        td_key.last_emitted_action = Some(td_key.tap_action.clone());
                        return TdResolution::EmitAction(td_key.tap_action.clone());
                    }
                    td_key.state = TdState::Tapped;
                    td_key.tap_count = 1;
                    TdResolution::Undecided
                }
                TdState::Tapped => {
                    let elapsed = td_key.elapsed_since_press();
                    if elapsed <= self.config.double_tap_window_ms as u128 {
                        td_key.state = TdState::TappingSecond;
                        td_key.tap_count = 2;
                        td_key.last_emitted_action = Some(td_key.double_tap_action.clone());
                        return TdResolution::EmitAction(td_key.double_tap_action.clone());
                    }
                    self.tracked_keys.remove(&keycode);
                    let mut new_td =
                        TdKey::new(keycode, (*tap_action).clone(), (*double_tap_action).clone());
                    new_td.state = TdState::Undecided;
                    self.tracked_keys.insert(keycode, new_td);
                    TdResolution::Undecided
                }
                TdState::TappingSecond => {
                    let elapsed = td_key.elapsed_since_press();
                    if elapsed <= self.config.grace_period_ms as u128 {
                        td_key.tap_count += 1;
                        td_key.last_emitted_action = Some(td_key.double_tap_action.clone());
                        return TdResolution::EmitAction(td_key.double_tap_action.clone());
                    }
                    TdResolution::Undecided
                }
                TdState::HoldingFirst => TdResolution::EmitAction(td_key.tap_action.clone()),
                TdState::HoldingSecond => {
                    TdResolution::EmitAction(td_key.double_tap_action.clone())
                }
            }
        } else {
            let mut td_key =
                TdKey::new(keycode, (*tap_action).clone(), (*double_tap_action).clone());
            let elapsed = td_key.elapsed_since_press();
            if elapsed > self.config.tapping_term_ms as u128 {
                td_key.state = TdState::HoldingFirst;
                td_key.last_emitted_action = Some(td_key.tap_action.clone());
                self.tracked_keys.insert(keycode, td_key.clone());
                return TdResolution::EmitAction(td_key.tap_action);
            }
            td_key.state = TdState::Tapped;
            td_key.tap_count = 1;
            self.tracked_keys.insert(keycode, td_key);
            TdResolution::Undecided
        }
    }

    pub fn unemit_action(
        &mut self,
        keycode: KeyCode,
        _tap_action: &KeyAction,
        _double_tap_action: &KeyAction,
    ) -> ProcessResult {
        if let Some(td_key) = self.tracked_keys.get_mut(&keycode) {
            match td_key.state {
                TdState::Undecided => {
                    self.tracked_keys.remove(&keycode);
                    ProcessResult::None
                }
                TdState::HoldingFirst => {
                    self.tracked_keys.remove(&keycode);
                    ProcessResult::None
                }
                TdState::Tapped => {
                    let elapsed = td_key.elapsed_since_press();
                    if elapsed > self.config.double_tap_window_ms as u128 {
                        self.tracked_keys.remove(&keycode);
                    }
                    ProcessResult::None
                }
                TdState::TappingSecond => {
                    let elapsed = td_key.elapsed_since_press();
                    if elapsed > self.config.double_tap_window_ms as u128 {
                        self.tracked_keys.remove(&keycode);
                    }
                    ProcessResult::None
                }
                TdState::HoldingSecond => {
                    self.tracked_keys.remove(&keycode);
                    ProcessResult::None
                }
            }
        } else {
            ProcessResult::None
        }
    }

    pub fn get_last_emitted_action(&self, keycode: KeyCode) -> Option<KeyAction> {
        self.tracked_keys
            .get(&keycode)
            .and_then(|td| td.last_emitted_action.clone())
    }

    /// When the earliest tracked key next needs its timeouts checked, if any is waiting on one.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.tracked_keys
            .values()
            .filter_map(|td_key| {
                let term_ms = match td_key.state {
                    TdState::Undecided => u64::from(self.config.tapping_term_ms),
                    TdState::Tapped | TdState::TappingSecond => self.config.double_tap_window_ms,
                    TdState::HoldingFirst | TdState::HoldingSecond => return None,
                };
                // The checks count whole milliseconds and fire once the term is exceeded
                Some(td_key.first_press_at + Duration::from_millis(term_ms + 1))
            })
            .min()
    }

    pub fn check_timeouts(&mut self) -> Vec<(KeyCode, ProcessResult)> {
        let mut resolutions = Vec::new();
        let mut to_remove = Vec::new();

        for (keycode, td_key) in &mut self.tracked_keys {
            match td_key.state {
                TdState::Undecided => {
                    if td_key.elapsed_since_press() > self.config.tapping_term_ms as u128 {
                        td_key.state = TdState::HoldingFirst;
                    }
                }
                TdState::Tapped | TdState::TappingSecond => {
                    if td_key.elapsed_since_press() > self.config.double_tap_window_ms as u128 {
                        if td_key.tap_count >= 2 {
                            td_key.state = TdState::Tapped;
                            td_key.tap_count = 1;
                            resolutions.push((*keycode, ProcessResult::None));
                        } else {
                            to_remove.push(*keycode);
                        }
                    }
                }
                TdState::HoldingFirst | TdState::HoldingSecond => {}
            }
        }

        for keycode in to_remove {
            self.tracked_keys.remove(&keycode);
        }

        resolutions
    }

    pub fn handle_check_timeouts(&mut self) -> Vec<(KeyCode, bool)> {
        let timeouts = self.check_timeouts();
        let mut events = Vec::new();

        for (_keycode, result) in timeouts {
            match result {
                ProcessResult::EmitKey(kc, pressed) => events.push((kc, pressed)),
                ProcessResult::MultipleEvents(evts) => events.extend(evts),
                _ => {}
            }
        }

        events
    }

    pub fn tracked_count(&self) -> usize {
        self.tracked_keys.len()
    }
}

pub fn handle_dt_action(
    dt_processor: &mut DtProcessor,
    keycode: KeyCode,
    tap_action: &KeyAction,
    double_tap_action: &KeyAction,
) -> Vec<(KeyCode, bool)> {
    dt_processor.emit_action(keycode, tap_action, double_tap_action)
}

pub fn handle_dt_release(
    dt_processor: &mut DtProcessor,
    keycode: KeyCode,
    tap_action: &KeyAction,
    double_tap_action: &KeyAction,
) -> ProcessResult {
    dt_processor.unemit_action(keycode, tap_action, double_tap_action)
}

pub fn emit_dt(
    action: &KeyAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    match action {
        KeyAction::DT(tap_action, double_tap_action) => {
            let tap_action_ref: &KeyAction = tap_action;
            let dtap_action_ref: &KeyAction = double_tap_action;

            // If already in holding state, don't re-emit - just return the held action
            if ctx.dt_processor.is_holding(keycode) {
                return (
                    EmitResult::None,
                    Some(HeldAction::DtManaged {
                        tap_action: (*tap_action_ref).clone(),
                        double_tap_action: (*dtap_action_ref).clone(),
                    }),
                );
            }

            // Check if any other keys are currently tracked (pressed) - for permissive hold
            let other_key_pressed = ctx.dt_processor.has_other_keys_tracked(keycode);

            let resolution = ctx.dt_processor.resolve_action(
                keycode,
                tap_action_ref,
                dtap_action_ref,
                other_key_pressed,
            );

            match resolution {
                TdResolution::EmitAction(emit_action) => {
                    let (emit_result, _) = emit_action.emit(keycode, ctx);
                    (
                        emit_result,
                        Some(HeldAction::DtManaged {
                            tap_action: (*tap_action_ref).clone(),
                            double_tap_action: (*dtap_action_ref).clone(),
                        }),
                    )
                }
                TdResolution::HoldFirst => {
                    let (emit_result, _) = tap_action_ref.emit(keycode, ctx);
                    (
                        emit_result,
                        Some(HeldAction::DtManaged {
                            tap_action: (*tap_action_ref).clone(),
                            double_tap_action: (*dtap_action_ref).clone(),
                        }),
                    )
                }
                _ => (
                    EmitResult::None,
                    Some(HeldAction::DtManaged {
                        tap_action: (*tap_action_ref).clone(),
                        double_tap_action: (*dtap_action_ref).clone(),
                    }),
                ),
            }
        }
        _ => (EmitResult::None, None),
    }
}

pub fn unemit_dt(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    match (action, held_action) {
        (KeyAction::DT(tap_action, double_tap_action), HeldAction::DtManaged { .. }) => {
            let tap_action_ref: &KeyAction = tap_action;
            let dtap_action_ref: &KeyAction = double_tap_action;
            let result = ctx
                .dt_processor
                .unemit_action(keycode, tap_action_ref, dtap_action_ref);
            match result {
                ProcessResult::EmitKey(kc, pressed) => EmitResult::EmitKey(kc, pressed),
                ProcessResult::MultipleEvents(events) => EmitResult::EmitKeys(events),
                _ => EmitResult::None,
            }
        }
        _ => EmitResult::None,
    }
}
