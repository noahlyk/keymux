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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessResult {
    EmitKey(KeyCode, bool),
    TapKeyPressRelease(KeyCode),
    MultipleEvents(Vec<(KeyCode, bool)>),
    TypeString(String, bool),
    /// Delete `backspaces` characters, then type `text` (used for undo)
    Retype { backspaces: usize, text: String },
    None,
}

impl From<SocdResolution> for ProcessResult {
    fn from(res: SocdResolution) -> Self {
        match res {
            SocdResolution::EmitKey(key, pressed) => Self::EmitKey(key, pressed),
            SocdResolution::MultipleEvents(events) => Self::MultipleEvents(events),
            SocdResolution::None => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitResult {
    EmitKey(KeyCode, bool),
    EmitKeys(Vec<(KeyCode, bool)>),
    TapKey(KeyCode),
    LayerAction(Layer),
    None,
}

impl From<SocdResolution> for EmitResult {
    fn from(res: SocdResolution) -> Self {
        match res {
            SocdResolution::EmitKey(key, pressed) => Self::EmitKey(key, pressed),
            SocdResolution::MultipleEvents(events) => Self::EmitKeys(events),
            SocdResolution::None => Self::None,
        }
    }
}

impl EmitResult {
    pub fn to_process_result(self) -> ProcessResult {
        match self {
            Self::EmitKey(kc, pressed) => ProcessResult::EmitKey(kc, pressed),
            Self::EmitKeys(events) => {
                if events.is_empty() {
                    ProcessResult::None
                } else if events.len() == 1 {
                    ProcessResult::EmitKey(events[0].0, events[0].1)
                } else {
                    ProcessResult::MultipleEvents(events)
                }
            }
            Self::TapKey(kc) => ProcessResult::TapKeyPressRelease(kc),
            Self::LayerAction(_) | Self::None => ProcessResult::None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HeldAction {
    RegularKey(KeyCode),
    Layer(Layer),
    MtManaged,
    SocdManaged,
    DtManaged {
        tap_action: KeyAction,
        double_tap_action: KeyAction,
    },
    OsmManaged,
    /// A stroke key captured by the steno layer with this name. Released by the
    /// keymap processor, which owns the chord state.
    StenoManaged(Layer),
    /// A key pressed on a steno layer that isn't a stroke key or a remap. Its press
    /// was swallowed, so its release is swallowed too.
    SwallowedBySteno,
}

pub struct HandleContext<'a> {
    pub mt_processor: &'a mut MtProcessor,
    pub dt_processor: &'a mut DtProcessor,
    pub osm_processor: &'a mut OsmProcessor,
    pub socd_processor: &'a mut SocdProcessor,
    pub layer_stack: &'a mut LayerStack,
    pub config_dir: std::path::PathBuf,
    pub user_id: u32,
}

pub fn handle_action_release(
    action: HeldAction,
    keycode: KeyCode,
    ctx: HandleContext<'_>,
) -> ProcessResult {
    match action {
        HeldAction::RegularKey(key) => ProcessResult::EmitKey(key, false),
        HeldAction::Layer(layer) => {
            ctx.layer_stack.deactivate_layer(&layer);
            ProcessResult::None
        }
        HeldAction::MtManaged => ctx
            .mt_processor
            .handle_release(keycode)
            .map_or(ProcessResult::None, |resolution| {
                apply_mt_resolution_to_process_result(resolution)
            }),
        HeldAction::SocdManaged => {
            let result: ProcessResult = ctx.socd_processor.handle_release(keycode).into();
            result
        }
        HeldAction::DtManaged {
            tap_action,
            double_tap_action,
        } => ctx
            .dt_processor
            .unemit_action(keycode, &tap_action, &double_tap_action),
        HeldAction::OsmManaged => {
            let _ = osm::handle_osm_release(ctx.osm_processor, keycode);
            ProcessResult::None
        }
        // Steno releases are intercepted by KeymapProcessor before reaching here
        HeldAction::StenoManaged(_) | HeldAction::SwallowedBySteno => ProcessResult::None,
    }
}

fn apply_mt_resolution_to_process_result(resolution: MtResolution) -> ProcessResult {
    use mt::MtAction;
    match resolution.action {
        MtAction::TapPress(key) => ProcessResult::EmitKey(key, true),
        MtAction::TapPressRelease(key) => ProcessResult::TapKeyPressRelease(key),
        MtAction::HoldPress(key) => ProcessResult::EmitKey(key, true),
        MtAction::HoldPressRelease(key) => {
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
            Self::Key(output_key) => {
                let events = ctx
                    .mt_processor
                    .on_other_key_press_for_resolutions(*output_key);
                if !events.is_empty() {
                    let mut all_events = ctx.mt_processor.resolutions_to_events(&events);
                    all_events.push((*output_key, true));
                    (
                        EmitResult::EmitKeys(all_events),
                        Some(HeldAction::RegularKey(*output_key)),
                    )
                } else {
                    (
                        EmitResult::EmitKey(*output_key, true),
                        Some(HeldAction::RegularKey(*output_key)),
                    )
                }
            }
            Self::MT(..) => emit_mt(self, keycode, ctx),
            Self::TO(..) | Self::TG(..) | Self::MO(..) => {
                emit_layer(self, keycode, ctx.layer_stack)
            }
            Self::SOCD(..) => emit_socd(self, keycode, ctx),
            Self::CMD(..) => emit_cmd(self, keycode, ctx),
            Self::OSM(..) => emit_osm(self, keycode, ctx),
            Self::DT(..) => emit_dt(self, keycode, ctx),
            Self::Transparent => {
                let resolutions = ctx.mt_processor.on_other_key_press_for_resolutions(keycode);
                if !resolutions.is_empty() {
                    let mut events = ctx.mt_processor.resolutions_to_events(&resolutions);
                    events.push((keycode, true));
                    (
                        EmitResult::EmitKeys(events),
                        Some(HeldAction::RegularKey(keycode)),
                    )
                } else {
                    (
                        EmitResult::EmitKey(keycode, true),
                        Some(HeldAction::RegularKey(keycode)),
                    )
                }
            }
        }
    }

    pub fn unemit(
        &self,
        action: HeldAction,
        keycode: KeyCode,
        ctx: &mut HandleContext<'_>,
    ) -> EmitResult {
        match (&self, action.clone()) {
            (_, HeldAction::RegularKey(key)) => EmitResult::EmitKey(key, false),
            (Self::TO(..) | Self::TG(..) | Self::MO(..), HeldAction::Layer(_)) => {
                unemit_layer(self, action, keycode, ctx.layer_stack)
            }
            (Self::MT(..), HeldAction::MtManaged) => unemit_mt(self, action, keycode, ctx),
            (Self::SOCD(..), HeldAction::SocdManaged) => unemit_socd(self, action, keycode, ctx),
            (Self::DT(..), HeldAction::DtManaged { .. }) => unemit_dt(self, action, keycode, ctx),
            (Self::OSM(..), HeldAction::OsmManaged) => unemit_osm(self, action, keycode, ctx),
            (Self::CMD(..), _) => unemit_cmd(self, action, keycode, ctx),
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
pub use cmd::{emit_cmd, unemit_cmd};
pub use dt::{emit_dt, handle_dt_action, handle_dt_release, unemit_dt, DtProcessor, TdResolution};
pub use layer::{emit_layer, unemit_layer};
pub use mt::{
    emit_mt, handle_mt_action, unemit_mt, MtAction, MtProcessor, MtResolution, RollingStats,
};
pub use osm::{emit_osm, handle_osm_action, handle_osm_release, unemit_osm, OsmProcessor};
pub use socd::{emit_socd, handle_socd_action, unemit_socd, SocdProcessor, SocdResolution};
