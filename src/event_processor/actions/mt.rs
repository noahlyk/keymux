use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction};
use crate::keycode::KeyCode;
use serde::{Deserialize, Serialize};
/// Advanced Mod-Tap (MT) system inspired by QMK
///
/// This module implements a comprehensive dual-role key system with:
/// - Basic tap/hold timing (like QMK MT)
/// - Same-hand roll detection (favors tap)
/// - Opposite-hand chord detection (favors hold)
/// - Multi-mod same-hand chord detection
/// - Adaptive timing per-key/per-pair
/// - Predictive intent scoring
/// - Configurable behavior options
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Hand assignment for a key (for chord detection)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hand {
    Left,
    Right,
    Unknown,
}

/// State of an MT key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtKeyState {
    /// Key is undecided (waiting for tap/hold resolution)
    Undecided,
    /// Resolved to tap
    Tap,
    /// Resolved to hold (modifier active)
    Hold,
    /// Resolved to tap and completed
    TapCompleted,
    /// Resolved to hold and completed
    HoldCompleted,
    /// Unwrapped to tap (cross-hand unwrap)
    Unwrapped,
}

/// MT key tracking state
#[derive(Debug, Clone)]
pub struct MtKey {
    /// Physical keycode
    pub keycode: KeyCode,
    /// Tap output (what key to emit on tap)
    pub tap_key: KeyCode,
    /// Hold output (what modifier to emit on hold)
    pub hold_key: KeyCode,
    /// When this key was pressed
    pub pressed_at: Instant,
    /// Current state
    pub state: MtKeyState,
    /// Hold intent score (0.0 = definitely tap, 1.0 = definitely hold)
    pub hold_intent_score: f32,
    /// Which hand this key is on
    pub hand: Hand,
}

impl MtKey {
    pub fn new(keycode: KeyCode, tap_key: KeyCode, hold_key: KeyCode, hand: Hand) -> Self {
        Self {
            keycode,
            tap_key,
            hold_key,
            pressed_at: Instant::now(),
            state: MtKeyState::Undecided,
            hold_intent_score: 0.0,
            hand,
        }
    }

    /// Get duration since press
    pub fn duration(&self) -> Duration {
        Instant::now() - self.pressed_at
    }

    /// Get duration in milliseconds
    pub fn duration_ms(&self) -> u64 {
        self.duration().as_millis() as u64
    }
}

/// Rolling statistics for adaptive timing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingStats {
    /// Average tap duration for this key (ms) - when user taps quickly
    pub avg_tap_duration: f32,
    /// Number of tap samples collected
    pub tap_sample_count: u32,
    /// Adaptive threshold for this key (ms) - stays ~30ms above avg tap
    pub adaptive_threshold: f32,
}

impl RollingStats {
    pub const fn new(base_threshold: f32) -> Self {
        Self {
            avg_tap_duration: 0.0,
            tap_sample_count: 0,
            adaptive_threshold: base_threshold,
        }
    }

    /// Update with new tap duration using exponential moving average
    /// Alpha = 2 / (N + 1) where N is window size
    /// For 100 sample window: alpha = 2/101 ≈ 0.0198
    pub fn update_tap(&mut self, duration_ms: f32, target_margin_ms: f32) {
        const ALPHA: f32 = 0.02; // Exponential smoothing for ~100 sample window

        if self.tap_sample_count == 0 {
            self.avg_tap_duration = duration_ms;
        } else {
            // Exponential moving average: EMA = α * new_value + (1 - α) * old_EMA
            self.avg_tap_duration =
                ALPHA.mul_add(duration_ms, (1.0 - ALPHA) * self.avg_tap_duration);
        }

        self.tap_sample_count += 1;

        // Adjust adaptive threshold to stay target_margin_ms above average tap
        let target_threshold = self.avg_tap_duration + target_margin_ms;

        // Smooth the threshold adjustment too
        self.adaptive_threshold =
            ALPHA.mul_add(target_threshold, (1.0 - ALPHA) * self.adaptive_threshold);

        // Clamp threshold to reasonable range [50ms, 500ms]
        self.adaptive_threshold = self.adaptive_threshold.clamp(50.0, 500.0);
    }
}

impl Default for RollingStats {
    fn default() -> Self {
        Self::new(200.0) // Default base threshold
    }
}

/// MT configuration options
#[derive(Debug, Clone)]
pub struct MtConfig {
    /// Base tapping term (ms) - keys held longer than this are considered holds
    pub tapping_term_ms: u32,

    /// Enable permissive hold - if another key is pressed while MT is pending,
    /// immediately resolve to hold
    pub permissive_hold: bool,

    /// Enable same-hand roll detection - rolls on same hand favor tap
    pub same_hand_roll_detection: bool,

    /// Enable opposite-hand chord detection - chords on opposite hands favor hold
    pub opposite_hand_chord_detection: bool,

    /// Enable multi-mod detection - multiple modifiers held simultaneously
    /// on same hand all promote to hold
    pub multi_mod_detection: bool,

    /// Minimum number of MT keys held to trigger multi-mod (typically 2)
    pub multi_mod_threshold: usize,

    /// Enable adaptive timing - adjust thresholds based on user behavior
    pub adaptive_timing: bool,

    /// Enable predictive intent scoring
    pub predictive_scoring: bool,

    /// Roll detection window (ms) - keys pressed within this window
    /// of each other are considered a roll
    pub roll_detection_window_ms: u32,

    /// Chord detection window (ms) - keys pressed within this window
    /// are considered a chord
    pub chord_detection_window_ms: u32,

    /// Enable double-tap-then-hold - double tap to hold the tap key until released
    pub double_tap_then_hold: bool,

    /// Window (ms) for detecting double-taps
    pub double_tap_window_ms: u32,

    /// Enable cross-hand unwrap - when holding a modifier on one hand,
    /// MT keys on the opposite hand will unwrap to their tap key
    pub cross_hand_unwrap: bool,

    /// Target margin (ms) to keep threshold above average tap duration
    /// Default: 30ms means threshold = avg_tap + 30ms
    pub adaptive_target_margin_ms: u32,

    /// When holding an MT key and doing nothing, emit tap on release
    /// If true, holding then releasing without other action sends the tap key
    /// If false, holding then releasing without other action does nothing
    pub hold_do_nothing_emits_tap: bool,
}

impl Default for MtConfig {
    fn default() -> Self {
        Self {
            tapping_term_ms: 200,
            permissive_hold: true,
            same_hand_roll_detection: true,
            opposite_hand_chord_detection: true,
            multi_mod_detection: true,
            multi_mod_threshold: 2,
            adaptive_timing: false,
            predictive_scoring: false,
            roll_detection_window_ms: 150,
            chord_detection_window_ms: 50,
            double_tap_then_hold: false,
            double_tap_window_ms: 300,
            cross_hand_unwrap: true,
            adaptive_target_margin_ms: 30,
            hold_do_nothing_emits_tap: true,
        }
    }
}

/// Global MT state tracker
pub struct MtProcessor {
    /// Configuration
    config: MtConfig,

    /// Currently undecided MT keys
    undecided_keys: HashMap<KeyCode, MtKey>,

    /// Currently held (resolved) MT keys
    held_keys: HashMap<KeyCode, MtKey>,

    /// Rolling statistics for adaptive timing
    /// Key: (key1, key2) pair or single key
    rolling_stats: HashMap<(KeyCode, KeyCode), RollingStats>,

    /// Hand assignment map
    hand_map: HashMap<KeyCode, Hand>,

    /// History of recent key presses (for pattern detection)
    /// Stores (keycode, timestamp) tuples
    recent_presses: Vec<(KeyCode, Instant)>,

    /// Maximum history to keep
    max_history: usize,

    /// Last tap time for each key (for double-tap detection)
    last_tap_time: HashMap<KeyCode, Instant>,

    /// Keys currently holding their tap key (double-tap-then-hold)
    holding_tap_key: HashMap<KeyCode, KeyCode>,

    /// Game mode active (when true, pause adaptive timing learning)
    game_mode_active: bool,
}

impl MtProcessor {
    /// Create new MT processor
    pub fn new(config: &Config) -> Self {
        Self {
            config: MtConfig {
                tapping_term_ms: config.tapping_term_ms,
                permissive_hold: config.mt_config.permissive_hold,
                same_hand_roll_detection: config.mt_config.same_hand_roll_detection,
                opposite_hand_chord_detection: config.mt_config.opposite_hand_chord_detection,
                multi_mod_detection: config.mt_config.multi_mod_detection,
                multi_mod_threshold: config.mt_config.multi_mod_threshold,
                adaptive_timing: config.mt_config.adaptive_timing,
                predictive_scoring: config.mt_config.predictive_scoring,
                roll_detection_window_ms: config.mt_config.roll_detection_window_ms,
                chord_detection_window_ms: config.mt_config.chord_detection_window_ms,
                double_tap_then_hold: config.mt_config.double_tap_then_hold,
                double_tap_window_ms: config.mt_config.double_tap_window_ms,
                cross_hand_unwrap: config.mt_config.cross_hand_unwrap,
                adaptive_target_margin_ms: config.mt_config.adaptive_target_margin_ms,
                hold_do_nothing_emits_tap: config.mt_config.hold_do_nothing_emits_tap,
            },
            undecided_keys: HashMap::new(),
            held_keys: HashMap::new(),
            rolling_stats: HashMap::new(),
            hand_map: Self::build_default_hand_map(),
            recent_presses: Vec::new(),
            max_history: 10,
            last_tap_time: HashMap::new(),
            holding_tap_key: HashMap::new(),
            game_mode_active: false,
        }
    }

    /// Build default hand assignment map (QWERTY layout)
    fn build_default_hand_map() -> HashMap<KeyCode, Hand> {
        let mut map = HashMap::new();

        // Left hand keys (QWERTY)
        let left_keys = vec![
            // Letters
            KeyCode::KC_Q,
            KeyCode::KC_W,
            KeyCode::KC_E,
            KeyCode::KC_R,
            KeyCode::KC_T,
            KeyCode::KC_A,
            KeyCode::KC_S,
            KeyCode::KC_D,
            KeyCode::KC_F,
            KeyCode::KC_G,
            KeyCode::KC_Z,
            KeyCode::KC_X,
            KeyCode::KC_C,
            KeyCode::KC_V,
            KeyCode::KC_B,
            // Punctuation
            KeyCode::KC_GRV,  // Grave/Backtick
            KeyCode::KC_MINS, // Minus/Underscore (sometimes considered left)
            KeyCode::KC_EQL,  // Equal/Plus (sometimes considered right)
            KeyCode::KC_LBRC, // Left bracket
            KeyCode::KC_RBRC, // Right bracket (on right side but often left hand)
            KeyCode::KC_BSLS, // Backslash
            KeyCode::KC_QUOT, // Quote (right side but varies)
            // Modifiers
            KeyCode::KC_LCTL,
            KeyCode::KC_LSFT,
            KeyCode::KC_LALT,
            KeyCode::KC_LGUI,
            KeyCode::KC_LCMD,
            // Numbers
            KeyCode::KC_1,
            KeyCode::KC_2,
            KeyCode::KC_3,
            KeyCode::KC_4,
            KeyCode::KC_5,
        ];

        // Right hand keys (QWERTY)
        let right_keys = vec![
            // Letters
            KeyCode::KC_Y,
            KeyCode::KC_U,
            KeyCode::KC_I,
            KeyCode::KC_O,
            KeyCode::KC_P,
            KeyCode::KC_H,
            KeyCode::KC_J,
            KeyCode::KC_K,
            KeyCode::KC_L,
            KeyCode::KC_SCLN, // Semicolon (right hand)
            KeyCode::KC_N,
            KeyCode::KC_M,
            KeyCode::KC_COMM, // Comma
            KeyCode::KC_DOT,  // Period
            KeyCode::KC_SLSH, // Slash
            // Modifiers
            KeyCode::KC_RCTL,
            KeyCode::KC_RSFT,
            KeyCode::KC_RALT,
            KeyCode::KC_RGUI,
            KeyCode::KC_RCMD,
            // Numbers
            KeyCode::KC_6,
            KeyCode::KC_7,
            KeyCode::KC_8,
            KeyCode::KC_9,
            KeyCode::KC_0,
        ];

        for key in left_keys {
            map.insert(key, Hand::Left);
        }

        for key in right_keys {
            map.insert(key, Hand::Right);
        }

        map
    }

    /// Get hand for a keycode
    pub fn get_hand(&self, keycode: KeyCode) -> Hand {
        self.hand_map
            .get(&keycode)
            .copied()
            .unwrap_or(Hand::Unknown)
    }

    /// Set hand for a keycode (for custom layouts)
    pub fn set_hand(&mut self, keycode: KeyCode, hand: Hand) {
        self.hand_map.insert(keycode, hand);
    }

    /// Add key press (MT key pressed)
    /// Returns Some(resolution) if double-tap detected or cross-hand unwrap triggered
    pub fn on_press(
        &mut self,
        keycode: KeyCode,
        tap_key: KeyCode,
        hold_key: KeyCode,
    ) -> Option<MtResolution> {
        // Check for double-tap
        if self.config.double_tap_then_hold {
            if let Some(last_tap) = self.last_tap_time.get(&keycode) {
                let elapsed = Instant::now().duration_since(*last_tap).as_millis() as u32;
                if elapsed < self.config.double_tap_window_ms {
                    // Double-tap detected! Hold the tap key until released
                    self.holding_tap_key.insert(keycode, tap_key);
                    return Some(MtResolution {
                        keycode,
                        action: MtAction::HoldPress(tap_key),
                    });
                }
            }
        }

        let hand = self.get_hand(keycode);
        let mut mt_key = MtKey::new(keycode, tap_key, hold_key, hand);

        // Check for cross-hand unwrap
        if self.config.cross_hand_unwrap && hand != Hand::Unknown {
            // Check if there are any held modifiers on the opposite hand
            let has_opposite_hand_mod = self.held_keys.values().any(|held_key| {
                let held_hand = held_key.hand;
                held_hand != Hand::Unknown && held_hand != hand
            });

            if has_opposite_hand_mod {
                // Unwrap to tap key - mark as unwrapped and store it
                mt_key.state = MtKeyState::Unwrapped;
                self.held_keys.insert(keycode, mt_key);

                return Some(MtResolution {
                    keycode,
                    action: MtAction::TapPress(tap_key),
                });
            }
        }

        // Calculate initial hold intent score
        if self.config.predictive_scoring {
            mt_key.hold_intent_score = self.calculate_hold_intent(&mt_key);
        }

        // Add to recent presses history
        self.recent_presses.push((keycode, Instant::now()));
        if self.recent_presses.len() > self.max_history {
            self.recent_presses.remove(0);
        }

        self.undecided_keys.insert(keycode, mt_key);
        None
    }

    /// Another key pressed while MT key is pending (permissive hold trigger)
    pub fn on_other_key_press(&mut self, other_keycode: KeyCode) -> Vec<MtResolution> {
        let mut resolutions = Vec::new();

        if !self.config.permissive_hold
            && !self.config.same_hand_roll_detection
            && !self.config.opposite_hand_chord_detection
        {
            return resolutions;
        }

        let other_hand = self.get_hand(other_keycode);
        let now = Instant::now();

        // Check each undecided key
        let undecided: Vec<_> = self.undecided_keys.keys().copied().collect();

        for keycode in undecided {
            if let Some(mt_key) = self.undecided_keys.get(&keycode) {
                let time_since_press = (now - mt_key.pressed_at).as_millis() as u32;

                // Check for same-hand roll
                if self.config.same_hand_roll_detection
                    && mt_key.hand != Hand::Unknown
                    && mt_key.hand == other_hand
                    && time_since_press < self.config.roll_detection_window_ms
                {
                    // Same-hand roll detected - resolve to tap
                    if let Some(resolved) = self.resolve_to_tap(keycode) {
                        resolutions.push(resolved);
                    }
                    continue;
                }

                // Check for opposite-hand chord
                if self.config.opposite_hand_chord_detection
                    && mt_key.hand != Hand::Unknown
                    && other_hand != Hand::Unknown
                    && mt_key.hand != other_hand
                    && time_since_press < self.config.chord_detection_window_ms
                {
                    // Opposite-hand chord detected - resolve to hold
                    if let Some(resolved) = self.resolve_to_hold(keycode) {
                        resolutions.push(resolved);
                    }
                    continue;
                }

                // Standard permissive hold
                if self.config.permissive_hold {
                    if let Some(resolved) = self.resolve_to_hold(keycode) {
                        resolutions.push(resolved);
                    }
                }
            }
        }

        // Check for multi-mod detection
        if self.config.multi_mod_detection {
            let multi_mod = self.detect_multi_mod();
            resolutions.extend(multi_mod);
        }

        resolutions
    }

    /// MT key released
    pub fn on_release(&mut self, keycode: KeyCode) -> Option<MtResolution> {
        // Check if this key is holding its tap key (double-tap-then-hold)
        if let Some(tap_key) = self.holding_tap_key.remove(&keycode) {
            // Release the held tap key
            return Some(MtResolution {
                keycode,
                action: MtAction::ReleaseHold(tap_key),
            });
        }

        // Check if it's an undecided key
        if let Some(mt_key) = self.undecided_keys.remove(&keycode) {
            let duration_ms = mt_key.duration_ms() as u32;

            // Decide based on timing and intent score
            let effective_threshold = if self.config.adaptive_timing {
                self.get_adaptive_threshold(keycode)
            } else {
                self.config.tapping_term_ms
            };

            let should_hold = if self.config.predictive_scoring {
                // Use intent score with timing
                mt_key.hold_intent_score > 0.5 || duration_ms >= effective_threshold
            } else {
                // Pure timing
                duration_ms >= effective_threshold
            };

            // Check if we should emit tap instead of hold when held past threshold
            let is_hold_timing = duration_ms >= effective_threshold;
            let emit_tap_on_hold_timeout = is_hold_timing
                && self.config.hold_do_nothing_emits_tap
                && mt_key.hold_intent_score <= 0.5; // No strong intent for hold

            if emit_tap_on_hold_timeout {
                // Hold-do-nothing-emits-tap: emit tap even though held past threshold
                // Record tap time for double-tap detection
                if self.config.double_tap_then_hold {
                    self.last_tap_time.insert(keycode, Instant::now());
                }

                // Record ONLY taps (below threshold) for adaptive timing
                // This prevents survivorship bias - only successful taps are tracked
                // Skip recording when game mode is active
                if self.config.adaptive_timing && !self.game_mode_active {
                    self.update_tap_stats(keycode, duration_ms as f32);
                }

                let resolution = MtResolution {
                    keycode,
                    action: MtAction::TapPressRelease(mt_key.tap_key),
                };

                Some(resolution)
            } else if should_hold {
                // Hold: emit modifier press and release
                let resolution = MtResolution {
                    keycode,
                    action: MtAction::HoldPressRelease(mt_key.hold_key),
                };

                Some(resolution)
            } else {
                // Tap: emit tap key press and release
                // Record tap time for double-tap detection
                if self.config.double_tap_then_hold {
                    self.last_tap_time.insert(keycode, Instant::now());
                }

                // Record ONLY taps (below threshold) for adaptive timing
                // This prevents survivorship bias - only successful taps are tracked
                // Skip recording when game mode is active
                if self.config.adaptive_timing && !self.game_mode_active {
                    self.update_tap_stats(keycode, duration_ms as f32);
                }

                let resolution = MtResolution {
                    keycode,
                    action: MtAction::TapPressRelease(mt_key.tap_key),
                };

                Some(resolution)
            }
        }
        // Check if it's a held key
        else if let Some(mt_key) = self.held_keys.remove(&keycode) {
            // Check if it was unwrapped
            if mt_key.state == MtKeyState::Unwrapped {
                // Release the unwrapped tap key
                Some(MtResolution {
                    keycode,
                    action: MtAction::ReleaseHold(mt_key.tap_key),
                })
            } else {
                // Release the hold key
                Some(MtResolution {
                    keycode,
                    action: MtAction::ReleaseHold(mt_key.hold_key),
                })
            }
        } else {
            None
        }
    }

    /// Resolve undecided key to tap
    fn resolve_to_tap(&mut self, keycode: KeyCode) -> Option<MtResolution> {
        self.undecided_keys.remove(&keycode).map(|mut mt_key| {
            mt_key.state = MtKeyState::Tap;

            // Emit tap immediately
            MtResolution {
                keycode,
                action: MtAction::TapPress(mt_key.tap_key),
            }
        })
    }

    /// Resolve undecided key to hold
    fn resolve_to_hold(&mut self, keycode: KeyCode) -> Option<MtResolution> {
        if let Some(mut mt_key) = self.undecided_keys.remove(&keycode) {
            mt_key.state = MtKeyState::Hold;
            self.held_keys.insert(keycode, mt_key.clone());

            // Emit hold key press
            Some(MtResolution {
                keycode,
                action: MtAction::HoldPress(mt_key.hold_key),
            })
        } else {
            None
        }
    }

    /// Detect multi-mod same-hand chord
    fn detect_multi_mod(&mut self) -> Vec<MtResolution> {
        let mut resolutions = Vec::new();

        // Count undecided keys per hand
        let mut left_count = 0;
        let mut right_count = 0;
        let mut left_keys = Vec::new();
        let mut right_keys = Vec::new();

        for (keycode, mt_key) in &self.undecided_keys {
            match mt_key.hand {
                Hand::Left => {
                    left_count += 1;
                    left_keys.push(*keycode);
                }
                Hand::Right => {
                    right_count += 1;
                    right_keys.push(*keycode);
                }
                Hand::Unknown => {}
            }
        }

        // If we have multiple mods on same hand, promote all to hold
        if left_count >= self.config.multi_mod_threshold {
            for keycode in left_keys {
                if let Some(resolved) = self.resolve_to_hold(keycode) {
                    resolutions.push(resolved);
                }
            }
        }

        if right_count >= self.config.multi_mod_threshold {
            for keycode in right_keys {
                if let Some(resolved) = self.resolve_to_hold(keycode) {
                    resolutions.push(resolved);
                }
            }
        }

        resolutions
    }

    /// Calculate hold intent score based on context
    fn calculate_hold_intent(&self, mt_key: &MtKey) -> f32 {
        let mut score: f32 = 0.0;

        // Check if there are other undecided keys
        let undecided_count = self.undecided_keys.len();
        if undecided_count > 0 {
            score += 0.3; // More likely to be a chord
        }

        // Check recent key press patterns
        let now = Instant::now();
        let recent_same_hand = self
            .recent_presses
            .iter()
            .filter(|(keycode, timestamp)| {
                let hand = self.get_hand(*keycode);
                hand == mt_key.hand && (now - *timestamp).as_millis() < 200
            })
            .count();

        if recent_same_hand > 1 {
            score -= 0.2; // Likely a roll, favor tap
        }

        // Clamp to [0, 1]
        score.clamp(0.0, 1.0)
    }

    /// Get adaptive threshold for a key based on tap statistics
    fn get_adaptive_threshold(&self, keycode: KeyCode) -> u32 {
        // Look up stats for this key
        if let Some(stats) = self.rolling_stats.get(&(keycode, keycode)) {
            if stats.tap_sample_count >= 1 {
                // Use learned adaptive threshold (starts learning after first tap!)
                return stats.adaptive_threshold as u32;
            }
        }

        // Fall back to default only if no samples at all
        self.config.tapping_term_ms
    }

    /// Update tap statistics - records actual tap durations and adjusts threshold
    fn update_tap_stats(&mut self, keycode: KeyCode, duration_ms: f32) {
        let key = (keycode, keycode);
        let base_threshold = self.config.tapping_term_ms as f32;
        let target_margin = self.config.adaptive_target_margin_ms as f32;

        let stats = self
            .rolling_stats
            .entry(key)
            .or_insert_with(|| RollingStats::new(base_threshold));

        stats.update_tap(duration_ms, target_margin);
    }

    /// Check if any keys are pending (for external permissive hold logic)
    pub fn has_pending_keys(&self) -> bool {
        !self.undecided_keys.is_empty()
    }

    /// Get count of undecided keys
    pub fn undecided_count(&self) -> usize {
        self.undecided_keys.len()
    }

    /// Get adaptive stats for display/debugging
    pub fn get_adaptive_stats(&self) -> Vec<(KeyCode, &RollingStats)> {
        self.rolling_stats
            .iter()
            .filter_map(|((k1, k2), stats)| {
                if k1 == k2 && stats.tap_sample_count > 0 {
                    Some((*k1, stats))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Set game mode (pauses adaptive timing learning when active)
    pub fn set_game_mode(&mut self, active: bool) {
        self.game_mode_active = active;
        tracing::info!(
            "MT processor game mode: {}",
            if active {
                "enabled (paused learning)"
            } else {
                "disabled (learning active)"
            }
        );
    }

    /// Save adaptive timing stats to file
    pub fn save_stats(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        if !self.config.adaptive_timing {
            return Ok(());
        }

        // Skip save if we have no stats (prevents overwriting with empty data)
        if self.rolling_stats.is_empty() {
            return Ok(());
        }

        // Load existing stats and merge with current stats
        let mut merged_stats = if path.exists() {
            let json = std::fs::read_to_string(path)?;
            // Load from string keys (JSON format)
            serde_json::from_str::<HashMap<String, RollingStats>>(&json)
                .unwrap_or_else(|_| HashMap::new())
        } else {
            HashMap::new()
        };

        // Merge: prefer our stats if we have them, otherwise keep existing
        // Convert tuple keys to string keys for JSON serialization
        for (key, stats) in &self.rolling_stats {
            let key_str = format!("{:?},{:?}", key.0, key.1);
            merged_stats.insert(key_str, stats.clone());
        }

        let json = serde_json::to_string_pretty(&merged_stats)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load adaptive timing stats from file
    pub fn load_stats(&mut self, path: &std::path::Path) -> Result<(), std::io::Error> {
        if !self.config.adaptive_timing {
            return Ok(()); // Don't load if adaptive timing is disabled
        }

        if !path.exists() {
            return Ok(()); // File doesn't exist yet, that's okay
        }

        let json = std::fs::read_to_string(path)?;
        // Load from string keys (JSON format) and convert back to tuple keys
        let string_map: HashMap<String, RollingStats> = serde_json::from_str(&json)?;

        self.rolling_stats.clear();
        for (key_str, stats) in string_map {
            // Parse "KC_A,KC_A" format back to (KeyCode, KeyCode)
            if let Some((key1_str, key2_str)) = key_str.split_once(',') {
                // Parse KeyCode from debug format string (e.g., "KC_A")
                if let (Ok(key1), Ok(key2)) = (
                    serde_json::from_str::<KeyCode>(&format!("\"{}\"", key1_str.trim())),
                    serde_json::from_str::<KeyCode>(&format!("\"{}\"", key2_str.trim())),
                ) {
                    self.rolling_stats.insert((key1, key2), stats);
                }
            }
        }

        Ok(())
    }
}

/// MT resolution result
#[derive(Debug, Clone)]
pub struct MtResolution {
    /// The MT keycode that was resolved
    pub keycode: KeyCode,
    /// The action to take
    pub action: MtAction,
}

/// MT action to emit
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MtAction {
    /// Emit tap key press (key is still held)
    TapPress(KeyCode),
    /// Emit tap key press and release immediately
    TapPressRelease(KeyCode),
    /// Emit hold key press
    HoldPress(KeyCode),
    /// Emit hold key press and release immediately
    HoldPressRelease(KeyCode),
    /// Release hold key
    ReleaseHold(KeyCode),
}

impl MtProcessor {
    pub fn handle_press(
        &mut self,
        keycode: KeyCode,
        tap_key: KeyCode,
        hold_key: KeyCode,
    ) -> (Vec<(KeyCode, bool)>, Option<MtResolution>) {
        let other_resolutions = self.on_other_key_press(tap_key);
        let mut events = self.resolutions_to_events(&other_resolutions);

        if let Some(resolution) = self.on_press(keycode, tap_key, hold_key) {
            events.extend(self.resolution_to_events(&resolution));
            (events, None)
        } else if !other_resolutions.is_empty() {
            (events, None)
        } else {
            (Vec::new(), None)
        }
    }

    pub fn handle_release(&mut self, keycode: KeyCode) -> Option<MtResolution> {
        self.on_release(keycode)
    }

    pub fn resolution_to_events(&self, resolution: &MtResolution) -> Vec<(KeyCode, bool)> {
        match resolution.action {
            MtAction::TapPress(key) => vec![(key, true)],
            MtAction::TapPressRelease(key) => vec![(key, true), (key, false)],
            MtAction::HoldPress(key) => vec![(key, true)],
            MtAction::HoldPressRelease(key) => vec![(key, true), (key, false)],
            MtAction::ReleaseHold(key) => vec![(key, false)],
        }
    }

    pub fn resolutions_to_events(&self, resolutions: &[MtResolution]) -> Vec<(KeyCode, bool)> {
        let mut events = Vec::new();
        for resolution in resolutions {
            events.extend(self.resolution_to_events(resolution));
        }
        events
    }

    pub fn on_other_key_press_for_resolutions(
        &mut self,
        other_keycode: KeyCode,
    ) -> Vec<MtResolution> {
        self.on_other_key_press(other_keycode)
    }
}

const fn extract_keycode(action: &KeyAction) -> Option<KeyCode> {
    match action {
        KeyAction::Key(kc) => Some(*kc),
        _ => None,
    }
}

pub fn handle_mt_action(
    mt_processor: &mut MtProcessor,
    keycode: KeyCode,
    tap_action: &KeyAction,
    hold_action: &KeyAction,
) -> (Vec<(KeyCode, bool)>, Option<MtResolution>) {
    let tap_key_opt = extract_keycode(tap_action);
    let hold_key_opt = extract_keycode(hold_action);

    if let (Some(tap_key), Some(hold_key)) = (tap_key_opt, hold_key_opt) {
        mt_processor.handle_press(keycode, tap_key, hold_key)
    } else {
        (Vec::new(), None)
    }
}

fn apply_mt_resolution(resolution: MtResolution) -> EmitResult {
    match resolution.action {
        MtAction::TapPress(key) => EmitResult::EmitKey(key, true),
        MtAction::TapPressRelease(key) => EmitResult::TapKey(key),
        MtAction::HoldPress(key) => EmitResult::EmitKey(key, true),
        MtAction::HoldPressRelease(key) => EmitResult::EmitKeys(vec![(key, true), (key, false)]),
        MtAction::ReleaseHold(key) => EmitResult::EmitKey(key, false),
    }
}

pub fn emit_mt(
    action: &KeyAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    match action {
        KeyAction::MT(tap_action, hold_action) => {
            let tap_key = tap_action.as_keycode();
            let hold_key = hold_action.as_keycode();
            if let (Some(tap_kc), Some(hold_kc)) = (tap_key, hold_key) {
                let (events, _) = handle_mt_action(
                    ctx.mt_processor,
                    keycode,
                    &KeyAction::Key(tap_kc),
                    &KeyAction::Key(hold_kc),
                );
                if events.is_empty() {
                    (EmitResult::None, Some(HeldAction::MtManaged))
                } else {
                    (EmitResult::EmitKeys(events), Some(HeldAction::MtManaged))
                }
            } else {
                (EmitResult::None, Some(HeldAction::MtManaged))
            }
        }
        _ => (EmitResult::None, None),
    }
}

pub fn unemit_mt(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    match (action, held_action) {
        (KeyAction::MT(_, _), HeldAction::MtManaged) => ctx
            .mt_processor
            .handle_release(keycode)
            .map_or(EmitResult::None, |resolution| {
                apply_mt_resolution(resolution)
            }),
        _ => EmitResult::None,
    }
}
