//! One-shot modifiers (`OSM`), after QMK.
//!
//! Tapped, the modifier is held for the next key; held past the tapping term, it is an
//! ordinary modifier for as long as it is down; armed and then left alone past the timeout,
//! it lets go.
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction, ProcessResult};
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::time::Instant;

/// State of a one-shot modifier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OsmState {
    /// Pressed, waiting to see if it's a tap or hold
    Pressed,
    /// Released (tapped) - modifier is now active for next keypress
    Active,
    /// Queued for release after current key finishes
    QueuedRelease,
}

/// OneShot modifier tracking
#[derive(Debug, Clone)]
pub(crate) struct OsmKey {
    /// Modifier key to emit
    pub modifier_key: KeyCode,
    /// When this was pressed, and then when it was armed
    pub activated_at: Instant,
    /// Current state
    pub state: OsmState,
}

impl OsmKey {
    pub const fn new(modifier_key: KeyCode, activated_at: Instant) -> Self {
        Self {
            modifier_key,
            activated_at,
            state: OsmState::Pressed,
        }
    }

    /// Milliseconds from activation to `at`.
    pub fn elapsed(&self, at: Instant) -> u128 {
        at.saturating_duration_since(self.activated_at).as_millis()
    }
}

/// Result of OSM processing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsmResolution {
    /// Emit modifier press (activate one-shot)
    ActivateModifier(KeyCode),
    /// Emit modifier release (deactivate one-shot)
    ReleaseModifier(KeyCode),
    /// Continue waiting
    None,
}

/// OneShot processor configuration
#[derive(Debug, Clone)]
pub(crate) struct OsmConfig {
    /// Timeout in milliseconds (QMK default: 5000ms)
    pub oneshot_timeout_ms: u64,
    /// Tapping term - hold longer than this = normal hold (not one-shot)
    pub tapping_term_ms: u32,
}

/// OneShot processor - manages all OSM keys
pub struct OsmProcessor {
    config: OsmConfig,
    /// One-shot keys down right now, by the physical key
    tracked_keys: HashMap<KeyCode, OsmKey>,
    /// Active one-shots waiting for next keypress (modifier_key -> OsmKey)
    active_oneshots: HashMap<KeyCode, OsmKey>,
}

impl OsmProcessor {
    pub fn new(config: &Config) -> Self {
        Self {
            config: OsmConfig {
                oneshot_timeout_ms: config.oneshot_timeout_ms.unwrap_or(5000),
                tapping_term_ms: config.tapping_term_ms,
            },
            tracked_keys: HashMap::new(),
            active_oneshots: HashMap::new(),
        }
    }

    /// The key went down at `at`. Tap or hold is decided when it comes up.
    pub fn on_press(&mut self, keycode: KeyCode, modifier_key: KeyCode, at: Instant) {
        self.tracked_keys
            .insert(keycode, OsmKey::new(modifier_key, at));
    }

    /// The key came up at `at`: a tap arms the modifier for the next key, a hold lets it go.
    pub fn on_release(&mut self, keycode: KeyCode, at: Instant) -> OsmResolution {
        let Some(mut osm_key) = self.tracked_keys.remove(&keycode) else {
            return OsmResolution::None;
        };
        if osm_key.elapsed(at) < u128::from(self.config.tapping_term_ms) {
            osm_key.state = OsmState::Active;
            osm_key.activated_at = at;
            let modifier_key = osm_key.modifier_key;
            self.active_oneshots.insert(modifier_key, osm_key);
            return OsmResolution::ActivateModifier(modifier_key);
        }
        OsmResolution::ReleaseModifier(osm_key.modifier_key)
    }

    /// Called when ANY other key is pressed
    /// Returns modifiers that should be released after this key
    pub fn on_other_key_press(&mut self, keycode: KeyCode) -> Vec<(KeyCode, OsmResolution)> {
        let mut resolutions = Vec::new();

        // Don't consume one-shot on modifier keys
        if keycode.is_modifier() {
            return resolutions;
        }

        // Mark all active one-shots for release after this key
        for (modifier_key, osm_key) in &mut self.active_oneshots {
            if osm_key.state == OsmState::Active {
                osm_key.state = OsmState::QueuedRelease;
                resolutions.push((*modifier_key, OsmResolution::None));
            }
        }

        resolutions
    }

    /// Let go of any armed modifier that has waited past the timeout, as of `now`.
    pub fn check_timeouts(&mut self, now: Instant) -> Vec<(KeyCode, OsmResolution)> {
        let timeout = u128::from(self.config.oneshot_timeout_ms);
        let expired: Vec<KeyCode> = self
            .active_oneshots
            .iter()
            .filter(|(_, osm_key)| osm_key.elapsed(now) > timeout)
            .map(|(modifier_key, _)| *modifier_key)
            .collect();
        for modifier_key in &expired {
            self.active_oneshots.remove(modifier_key);
        }
        expired
            .into_iter()
            .map(|modifier_key| (modifier_key, OsmResolution::ReleaseModifier(modifier_key)))
            .collect()
    }
}

pub fn handle_osm_release(
    osm_processor: &mut OsmProcessor,
    keycode: KeyCode,
    at: Instant,
) -> ProcessResult {
    match osm_processor.on_release(keycode, at) {
        OsmResolution::ActivateModifier(mod_key) => ProcessResult::EmitKey(mod_key, true),
        OsmResolution::ReleaseModifier(mod_key) => ProcessResult::EmitKey(mod_key, false),
        OsmResolution::None => ProcessResult::None,
    }
}

pub fn emit_osm(
    action: &KeyAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    let KeyAction::OSM(modifier_action) = action else {
        return (EmitResult::None, None);
    };
    if let Some(modifier_key) = modifier_action.as_keycode() {
        let _ = ctx.osm_processor.check_timeouts(ctx.at);
        ctx.osm_processor.on_press(keycode, modifier_key, ctx.at);
    }
    (EmitResult::None, Some(HeldAction::OsmManaged))
}

pub fn unemit_osm(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    if matches!((action, held_action), (KeyAction::OSM(_), HeldAction::OsmManaged)) {
        let _ = handle_osm_release(ctx.osm_processor, keycode, ctx.at);
    }
    EmitResult::None
}
