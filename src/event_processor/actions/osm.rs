/// OneShot Modifier (OSM) processor - QMK-inspired one-shot keys
///
/// Implements one-shot modifier behavior:
/// - Tap: Activates modifier for ONE subsequent keypress
/// - Hold: Acts as normal modifier (stays active while held)
/// - Timeout: Deactivates after 5 seconds of no activity (QMK default)
///
/// Follows QMK one-shot behavior:
/// - Modifier activates on tap
/// - Auto-releases after next non-modifier keypress
/// - Can stack multiple one-shots
/// - Timeout prevents accidental stuck modifiers
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
    /// When this was activated
    pub activated_at: Instant,
    /// Current state
    pub state: OsmState,
}

impl OsmKey {
    pub fn new(modifier_key: KeyCode) -> Self {
        Self {
            modifier_key,
            activated_at: Instant::now(),
            state: OsmState::Pressed,
        }
    }

    /// Time since activation
    pub fn elapsed(&self) -> u128 {
        self.activated_at.elapsed().as_millis()
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

impl Default for OsmConfig {
    fn default() -> Self {
        Self {
            oneshot_timeout_ms: 5000, // QMK default
            tapping_term_ms: 200,     // Detect hold vs tap
        }
    }
}

/// OneShot processor - manages all OSM keys
pub struct OsmProcessor {
    /// Config
    config: OsmConfig,

    /// Currently tracked OSM keys
    tracked_keys: HashMap<KeyCode, OsmKey>,

    /// Active one-shots waiting for next keypress (modifier_key -> OsmKey)
    active_oneshots: HashMap<KeyCode, OsmKey>,
}

impl OsmProcessor {
    /// Create new OSM processor
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

    pub fn handle_press(&mut self, keycode: KeyCode, modifier_key: KeyCode) -> OsmResolution {
        let _timeouts = self.check_timeouts();
        let _resolution = self.on_press(keycode, modifier_key);
        OsmResolution::None
    }

    pub fn handle_release(&mut self, keycode: KeyCode) -> OsmResolution {
        self.on_release(keycode)
    }

    pub fn handle_check_timeouts(&mut self) -> Vec<(KeyCode, OsmResolution)> {
        self.check_timeouts()
    }

    /// Handle OSM key press
    pub fn on_press(&mut self, keycode: KeyCode, modifier_key: KeyCode) -> OsmResolution {
        let osm_key = OsmKey::new(modifier_key);
        self.tracked_keys.insert(keycode, osm_key);

        // Don't emit yet - wait to see if it's a tap or hold
        OsmResolution::None
    }

    /// Handle OSM key release
    pub fn on_release(&mut self, keycode: KeyCode) -> OsmResolution {
        if let Some(mut osm_key) = self.tracked_keys.remove(&keycode) {
            let duration_ms = osm_key.elapsed();

            // Tapped (released quickly) - activate one-shot
            if duration_ms < self.config.tapping_term_ms as u128 {
                osm_key.state = OsmState::Active;
                osm_key.activated_at = Instant::now(); // Reset timer for timeout
                let modifier_key = osm_key.modifier_key;
                self.active_oneshots.insert(modifier_key, osm_key);

                // Emit modifier press
                return OsmResolution::ActivateModifier(modifier_key);
            }
            // Held (released after long press) - was acting as normal modifier
            // Release it now
            return OsmResolution::ReleaseModifier(osm_key.modifier_key);
        }

        OsmResolution::None
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

    /// Check for timeouts and deactivate expired one-shots
    pub fn check_timeouts(&mut self) -> Vec<(KeyCode, OsmResolution)> {
        let mut resolutions = Vec::new();
        let timeout_ms = self.config.oneshot_timeout_ms;

        // Find expired one-shots
        let expired: Vec<KeyCode> = self
            .active_oneshots
            .iter()
            .filter_map(|(modifier_key, osm_key)| {
                if osm_key.elapsed() > timeout_ms as u128 {
                    Some(*modifier_key)
                } else {
                    None
                }
            })
            .collect();

        // Release expired one-shots
        for modifier_key in expired {
            self.active_oneshots.remove(&modifier_key);
            resolutions.push((modifier_key, OsmResolution::ReleaseModifier(modifier_key)));
        }

        resolutions
    }
}

const fn extract_keycode(action: &KeyAction) -> Option<KeyCode> {
    match action {
        KeyAction::Key(kc) => Some(*kc),
        _ => None,
    }
}

pub fn handle_osm_action(
    osm_processor: &mut OsmProcessor,
    keycode: KeyCode,
    modifier_action: &KeyAction,
) -> OsmResolution {
    if let Some(modifier_key) = extract_keycode(modifier_action) {
        let _ = osm_processor.handle_press(keycode, modifier_key);
    }
    OsmResolution::None
}

pub fn handle_osm_release(osm_processor: &mut OsmProcessor, keycode: KeyCode) -> ProcessResult {
    let resolution = osm_processor.handle_release(keycode);
    match resolution {
        OsmResolution::ActivateModifier(mod_key) => ProcessResult::EmitKey(mod_key, true),
        OsmResolution::ReleaseModifier(mod_key) => ProcessResult::EmitKey(mod_key, false),
        OsmResolution::None => ProcessResult::None,
    }
}

pub fn emit_osm(
    action: &KeyAction,
    _keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    match action {
        KeyAction::OSM(modifier_action) => {
            let mod_key = modifier_action.as_keycode();
            if let Some(key) = mod_key {
                let _ = handle_osm_action(ctx.osm_processor, key, modifier_action);
            }
            (EmitResult::None, Some(HeldAction::OsmManaged))
        }
        _ => (EmitResult::None, None),
    }
}

pub fn unemit_osm(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    match (action, held_action) {
        (KeyAction::OSM(_), HeldAction::OsmManaged) => {
            let _ = handle_osm_release(ctx.osm_processor, keycode);
            EmitResult::None
        }
        _ => EmitResult::None,
    }
}
