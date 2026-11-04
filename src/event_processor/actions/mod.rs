//! Action processors for keyboard events
//!
//! This module contains all the specialized processors for different action types:
//! - MT (Mod-Tap): Tap/hold dual-function keys
//! - DT (Double-Tap): Tap dance with single/double-tap detection
//! - OSM (OneShot Modifier): One-shot modifiers that auto-release
//! - SOCD (Simultaneous Opposite Cardinal Direction): Handling for opposing keys
//! - CMD: Shell command execution
//! - Layer: Layer switching (TO, TG, MO)

pub mod cmd;
pub mod dt;
pub mod layer;
pub mod mt;
pub mod osm;
pub mod socd;

use crate::config::{KeyAction, Layer};
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;
use std::time::Instant;

/// What the keymap wants sent to the virtual keyboard for one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessResult {
    EmitKey(KeyCode, bool),
    MultipleEvents(Vec<(KeyCode, bool)>),
    TypeString(String),
    /// Delete `backspaces` characters, then type `text` (used for undo)
    Retype {
        backspaces: usize,
        text: String,
    },
    /// Several things in order: typing, deleting, key presses (what a steno stroke can ask for)
    Ops(Vec<crate::steno::translate::OutputOp>),
    None,
}

impl ProcessResult {
    /// Key events as a result: none, the one, or all of them.
    #[must_use]
    pub fn from_events(events: Vec<(KeyCode, bool)>) -> Self {
        match events.as_slice() {
            [] => Self::None,
            [(key, pressed)] => Self::EmitKey(*key, *pressed),
            _ => Self::MultipleEvents(events),
        }
    }
}

/// What one action's press or release sends. Layer switches send nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitResult {
    EmitKey(KeyCode, bool),
    EmitKeys(Vec<(KeyCode, bool)>),
    TapKey(KeyCode),
    None,
}

impl EmitResult {
    pub fn to_process_result(self) -> ProcessResult {
        match self {
            Self::EmitKey(kc, pressed) => ProcessResult::EmitKey(kc, pressed),
            Self::EmitKeys(events) => ProcessResult::from_events(events),
            Self::TapKey(kc) => ProcessResult::MultipleEvents(vec![(kc, true), (kc, false)]),
            Self::None => ProcessResult::None,
        }
    }

    /// The key events in this result, in order.
    pub fn into_events(self) -> Vec<(KeyCode, bool)> {
        match self {
            Self::EmitKey(key, pressed) => vec![(key, pressed)],
            Self::EmitKeys(events) => events,
            Self::TapKey(key) => vec![(key, true), (key, false)],
            Self::None => Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum HeldAction {
    RegularKey(KeyCode),
    Layer(Layer),
    MtManaged,
    /// A SOCD key, with the key it sent: the one to take out of its group on release
    SocdManaged(KeyCode),
    /// A tap-dance key, with the action its press ended up pressing (and what that left held),
    /// if any
    DtManaged(Option<Box<(KeyAction, Self)>>),
    OsmManaged,
    /// A stroke key captured by the steno layer with this name. Released by the
    /// keymap processor, which owns the chord state.
    StenoManaged(Layer),
    /// A key pressed on a steno layer that isn't a stroke key or a remap. Its press
    /// was swallowed, so its release is swallowed too.
    SwallowedBySteno,
    /// The children of a MULTI, each with the held action its press left, if any
    MultiManaged(Vec<(KeyAction, Option<Self>)>),
}

pub struct HandleContext<'a> {
    pub mt_processor: &'a mut MtProcessor,
    pub dt_processor: &'a mut DtProcessor,
    pub osm_processor: &'a mut OsmProcessor,
    pub socd_processor: &'a mut SocdProcessor,
    pub layer_stack: &'a mut LayerStack,
    pub config_dir: &'a std::path::Path,
    pub user_id: u32,
    /// When the key event being handled happened: the moment the keyboard reported it, which
    /// is earlier than now for an event that waited to be read. Every tap-or-hold decision
    /// and every duration measures against this, never against the clock.
    pub at: Instant,
}

pub fn handle_action_release(
    action: HeldAction,
    keycode: KeyCode,
    mut ctx: HandleContext<'_>,
) -> ProcessResult {
    match action {
        HeldAction::RegularKey(key) => ProcessResult::EmitKey(key, false),
        HeldAction::Layer(layer) => {
            ctx.layer_stack.deactivate_layer(&layer);
            ProcessResult::None
        }
        HeldAction::MtManaged => ctx
            .mt_processor
            .handle_release(keycode, ctx.at)
            .map_or(ProcessResult::None, |resolution| {
                apply_mt_resolution_to_process_result(resolution)
            }),
        HeldAction::SocdManaged(key) => ProcessResult::from_events(ctx.socd_processor.release(key)),
        HeldAction::DtManaged(pressed) => {
            dt::release_dt(pressed, keycode, &mut ctx).to_process_result()
        }
        HeldAction::OsmManaged => {
            let _ = osm::handle_osm_release(ctx.osm_processor, keycode, ctx.at);
            ProcessResult::None
        }
        // Steno releases are intercepted by KeymapProcessor before reaching here
        HeldAction::StenoManaged(_) | HeldAction::SwallowedBySteno => ProcessResult::None,
        HeldAction::MultiManaged(children) => {
            unemit_multi(children, keycode, &mut ctx).to_process_result()
        }
    }
}

/// A plain key goes down. Any mod-tap still undecided resolves first (permissive hold), so
/// its modifier is down before this key is.
fn emit_plain_key(key: KeyCode, ctx: &mut HandleContext<'_>) -> (EmitResult, Option<HeldAction>) {
    let resolutions = ctx.mt_processor.on_other_key_press(key, ctx.at);
    let result = if resolutions.is_empty() {
        EmitResult::EmitKey(key, true)
    } else {
        let mut events = ctx.mt_processor.resolutions_to_events(&resolutions);
        events.push((key, true));
        EmitResult::EmitKeys(events)
    };
    (result, Some(HeldAction::RegularKey(key)))
}

/// Press each child of a MULTI, in order, and report their events as one output.
fn emit_multi(
    children: &[KeyAction],
    keycode: KeyCode,
    ctx: &mut HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    let mut results = Vec::new();
    let mut held = Vec::new();
    for child in children {
        let (result, child_held) = child.emit(keycode, ctx);
        results.push(result);
        held.push((child.clone(), child_held));
    }
    (merge_results(results), Some(HeldAction::MultiManaged(held)))
}

/// Release each child of a MULTI that still holds something, in the order it was pressed.
fn unemit_multi(
    children: Vec<(KeyAction, Option<HeldAction>)>,
    keycode: KeyCode,
    ctx: &mut HandleContext<'_>,
) -> EmitResult {
    let results = children
        .into_iter()
        .filter_map(|(child, held)| held.map(|held| child.unemit(held, keycode, ctx)))
        .collect();
    merge_results(results)
}

/// Combine several children's results into one, keeping every key event in order.
fn merge_results(results: Vec<EmitResult>) -> EmitResult {
    EmitResult::EmitKeys(
        results
            .into_iter()
            .flat_map(EmitResult::into_events)
            .collect(),
    )
}

fn apply_mt_resolution_to_process_result(resolution: MtResolution) -> ProcessResult {
    use mt::MtAction;
    match resolution.action {
        MtAction::TapPress(key) | MtAction::HoldPress(key) => ProcessResult::EmitKey(key, true),
        MtAction::TapPressRelease(key) | MtAction::HoldPressRelease(key) => {
            ProcessResult::MultipleEvents(vec![(key, true), (key, false)])
        }
        MtAction::ReleaseHold(key) => ProcessResult::EmitKey(key, false),
    }
}

impl KeyAction {
    pub fn emit(
        &self,
        keycode: KeyCode,
        ctx: &mut HandleContext<'_>,
    ) -> (EmitResult, Option<HeldAction>) {
        match self {
            Self::Key(output_key) => emit_plain_key(*output_key, ctx),
            Self::MT(..) => emit_mt(self, keycode, ctx),
            Self::TO(..) | Self::TG(..) | Self::MO(..) => {
                (EmitResult::None, emit_layer(self, ctx.layer_stack))
            }
            Self::MULTI(children) => emit_multi(children, keycode, ctx),
            Self::SOCD(..) => emit_socd(self, ctx),
            Self::CMD(command) => emit_cmd(command, ctx),
            Self::OSM(..) => emit_osm(self, keycode, ctx),
            Self::DT(..) => emit_dt(self, keycode, ctx),
            Self::Transparent => emit_plain_key(keycode, ctx),
        }
    }

    /// Let go of what this action's press left held.
    pub fn unemit(
        &self,
        held: HeldAction,
        keycode: KeyCode,
        ctx: &mut HandleContext<'_>,
    ) -> EmitResult {
        match held {
            HeldAction::RegularKey(key) => EmitResult::EmitKey(key, false),
            HeldAction::Layer(_) => unemit_layer(&held, ctx.layer_stack),
            HeldAction::SocdManaged(key) => EmitResult::EmitKeys(ctx.socd_processor.release(key)),
            HeldAction::MultiManaged(children) => unemit_multi(children, keycode, ctx),
            HeldAction::MtManaged if matches!(self, Self::MT(..)) => {
                unemit_mt(self, held, keycode, ctx)
            }
            HeldAction::DtManaged(_) if matches!(self, Self::DT(..)) => {
                unemit_dt(self, held, keycode, ctx)
            }
            HeldAction::OsmManaged if matches!(self, Self::OSM(..)) => {
                unemit_osm(self, held, keycode, ctx)
            }
            _ => EmitResult::None,
        }
    }

    pub const fn as_keycode(&self) -> Option<KeyCode> {
        match self {
            Self::Key(kc) => Some(*kc),
            _ => None,
        }
    }
}

// Re-export commonly used types and emit/unemit functions
use cmd::emit_cmd;
pub use dt::{emit_dt, unemit_dt, DtProcessor};
use layer::{emit_layer, unemit_layer};
pub use mt::{emit_mt, unemit_mt, MtAction, MtProcessor, MtResolution, RollingStats};
pub use osm::{emit_osm, handle_osm_release, unemit_osm, OsmProcessor};
pub use socd::{emit_socd, SocdProcessor};

/// Events are judged by when the keyboard reported them. A press and release 50ms apart that
/// both waited a second to be read are still a 50ms tap.
#[cfg(test)]
mod timing_tests {
    use crate::config::Config;
    use crate::event_processor::{KeymapProcessor, ProcessResult};
    use crate::keycode::KeyCode;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn processor() -> KeymapProcessor {
        let config: Config = ron::from_str(
            r"(
                tapping_term_ms: 130,
                mt_config: (hold_do_nothing_emits_tap: false, enable_adaptive_timing: false),
                remaps: {
                    KC_A: MT(Key(KC_A), Key(KC_LSFT)),
                    KC_B: DT(Key(KC_B), Key(KC_C)),
                },
            )",
        )
        .unwrap();
        KeymapProcessor::new(&config, PathBuf::from("config.ron"), 0)
    }

    /// A moment well before now: events stamped here have waited to be read.
    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the process has been up for a second")
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_mod_tap_read_late_is_judged_by_when_it_happened() {
        let mut p = processor();
        let t = long_ago();
        assert_eq!(
            p.process_key_at(KeyCode::KC_A, true, t),
            ProcessResult::None
        );
        assert_eq!(
            p.process_key_at(KeyCode::KC_A, false, t + ms(50)),
            ProcessResult::MultipleEvents(vec![(KeyCode::KC_A, true), (KeyCode::KC_A, false)])
        );
        // Held past the term by the keyboard's clock: a hold, however promptly it was read
        let t = t + ms(1000);
        p.process_key_at(KeyCode::KC_A, true, t);
        assert_eq!(
            p.process_key_at(KeyCode::KC_A, false, t + ms(300)),
            ProcessResult::MultipleEvents(vec![
                (KeyCode::KC_LSFT, true),
                (KeyCode::KC_LSFT, false)
            ])
        );
    }

    #[test]
    fn a_tap_dance_read_late_still_sees_its_second_tap_inside_the_window() {
        let mut p = processor();
        let t = long_ago();
        assert_eq!(
            p.process_key_at(KeyCode::KC_B, true, t),
            ProcessResult::None
        );
        assert_eq!(
            p.process_key_at(KeyCode::KC_B, false, t + ms(50)),
            ProcessResult::None
        );
        // 100ms after the first press is inside the 250ms window
        assert_eq!(
            p.process_key_at(KeyCode::KC_B, true, t + ms(100)),
            ProcessResult::EmitKey(KeyCode::KC_C, true)
        );
        // And what the second tap pressed is released with it
        assert_eq!(
            p.process_key_at(KeyCode::KC_B, false, t + ms(150)),
            ProcessResult::EmitKey(KeyCode::KC_C, false)
        );
    }

    #[test]
    fn adaptive_stats_learn_the_tap_as_the_keyboard_timed_it() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(KeyCode::KC_A, true, t);
        p.process_key_at(KeyCode::KC_A, false, t + ms(50));
        let stats = p.get_all_key_stats();
        let a = &stats[&KeyCode::KC_A];
        assert_eq!(a.tap_sample_count, 1);
        assert!(
            (a.avg_tap_duration - 50.0).abs() < 1.0,
            "{}",
            a.avg_tap_duration
        );
    }
}

#[cfg(test)]
mod multi_tests {
    use crate::config::Config;
    use crate::event_processor::{KeymapProcessor, ProcessResult};
    use crate::keycode::KeyCode;
    use std::path::PathBuf;

    fn processor() -> KeymapProcessor {
        let config: Config = ron::from_str(
            r#"(
                remaps: {
                    KC_F1: MULTI([Key(KC_LSFT), Key(KC_A)]),
                    KC_F2: MULTI([Key(KC_B), MO("nav")]),
                },
                layers: { "nav": (kind: Remap, remaps: {}) },
            )"#,
        )
        .unwrap();
        KeymapProcessor::new(&config, PathBuf::from("config.ron"), 0)
    }

    #[test]
    fn multi_presses_every_child_and_releases_them_together() {
        let mut p = processor();
        assert_eq!(
            p.process_key(KeyCode::KC_F1, true),
            ProcessResult::MultipleEvents(vec![(KeyCode::KC_LSFT, true), (KeyCode::KC_A, true),])
        );
        assert_eq!(
            p.process_key(KeyCode::KC_F1, false),
            ProcessResult::MultipleEvents(vec![(KeyCode::KC_LSFT, false), (KeyCode::KC_A, false),])
        );
    }

    #[test]
    fn a_layer_inside_multi_is_held_until_the_multi_is_released() {
        let mut p = processor();
        assert_eq!(
            p.process_key(KeyCode::KC_F2, true),
            ProcessResult::EmitKey(KeyCode::KC_B, true)
        );
        assert_eq!(p.current_layer_name(), "nav");
        assert_eq!(
            p.process_key(KeyCode::KC_F2, false),
            ProcessResult::EmitKey(KeyCode::KC_B, false)
        );
        assert_eq!(p.current_layer_name(), "base");
    }
}
