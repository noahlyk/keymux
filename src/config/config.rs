use crate::keycode::KeyCode;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Enable or Disable action for a keyboard entry
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum EnableDisable {
    #[default]
    Enable,
    Disable,
}

/// A single entry in the enabled_keyboards list
/// Can be a bare string (defaults to Enable) or "pattern": Enable/Disable for explicit action
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum EnabledKeyboardEntry {
    /// Explicit enable/disable with syntax: "1234": Enable or "1234": Disable
    Explicit(String, EnableDisable),
    /// Bare string - defaults to Enable
    Bare(String),
}

impl<'de> serde::Deserialize<'de> for EnabledKeyboardEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};
        use std::fmt;

        struct EntryVisitor;

        impl<'de> Visitor<'de> for EntryVisitor {
            type Value = EnabledKeyboardEntry;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string, tuple (pattern, action), or map {pattern: action}")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(EnabledKeyboardEntry::Bare(value.to_string()))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let pattern: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;

                let action_str: String = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;

                let action = match action_str.as_str() {
                    "enable" | "Enable" | "ENABLE" => EnableDisable::Enable,
                    "disable" | "Disable" | "DISABLE" => EnableDisable::Disable,
                    _ => return Err(de::Error::custom(format!("Unknown action: {}", action_str))),
                };

                Ok(EnabledKeyboardEntry::Explicit(pattern, action))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let pattern: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("Empty map"))?;

                let action_str: String = map.next_value()?;

                let action = match action_str.as_str() {
                    "enable" | "Enable" | "ENABLE" => EnableDisable::Enable,
                    "disable" | "Disable" | "DISABLE" => EnableDisable::Disable,
                    _ => return Err(de::Error::custom(format!("Unknown action: {}", action_str))),
                };

                Ok(EnabledKeyboardEntry::Explicit(pattern, action))
            }
        }

        deserializer.deserialize_any(EntryVisitor)
    }
}

impl EnabledKeyboardEntry {
    /// Get the pattern (string) part of this entry
    pub fn pattern(&self) -> &str {
        match self {
            Self::Explicit(pattern, _) => pattern,
            Self::Bare(pattern) => pattern,
        }
    }

    /// Get the enable/disable action
    pub const fn action(&self) -> EnableDisable {
        match self {
            Self::Explicit(_, action) => *action,
            Self::Bare(_) => EnableDisable::Enable,
        }
    }
}

impl From<String> for EnabledKeyboardEntry {
    fn from(s: String) -> Self {
        Self::Bare(s)
    }
}

impl From<&str> for EnabledKeyboardEntry {
    fn from(s: &str) -> Self {
        Self::Bare(s.to_string())
    }
}

/// Layer identifier - fully generic string-based layers
/// "base" and "game_mode" are reserved layer names
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Layer(pub String);

impl Layer {
    /// Base layer (always exists)
    pub fn base() -> Self {
        Self("base".to_string())
    }

    /// Check if this is the base layer
    pub fn is_base(&self) -> bool {
        self.0 == "base"
    }

    /// Create a new layer from string
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

// Custom deserializer to allow both TO("nav") and TO(Layer("nav"))
impl<'de> serde::Deserialize<'de> for Layer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct LayerVisitor;

        impl<'de> serde::de::Visitor<'de> for LayerVisitor {
            type Value = Layer;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a layer name string or Layer struct")
            }

            fn visit_str<E>(self, value: &str) -> Result<Layer, E>
            where
                E: serde::de::Error,
            {
                Ok(Layer(value.to_string()))
            }

            fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Layer, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                String::deserialize(deserializer).map(Layer)
            }
        }

        deserializer.deserialize_any(LayerVisitor)
    }
}

/// Key action - what happens when a key is pressed
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyAction {
    /// Direct key mapping
    Key(KeyCode),
    /// QMK-style Mod-Tap: advanced tap/hold with configurable behavior
    /// MT(tap_action, hold_action) - Tap for tap_action, hold for hold_action
    /// Supports: permissive hold, roll detection, chord detection, adaptive timing
    /// Now fully recursive - can nest any actions!
    /// Example: MT(Key(KC_TAB), TO("nav")) - tap for Tab, hold for nav layer
    MT(Box<Self>, Box<Self>),
    /// Switch to layer
    TO(Layer),
    /// Toggle layer (press to activate, press again to deactivate)
    TG(Layer),
    /// Momentary layer - hold for layer
    MO(Layer),
    /// SOCD (Simultaneous Opposite Cardinal Direction) - fully generic
    /// When this key is pressed, unpress all opposing keys
    /// Format: SOCD(this_action, [opposing_actions...])
    /// Example: SOCD(Key(KC_W), [Key(KC_S)]) or with the preprocessor: SOCD(KC_W, [KC_S])
    SOCD(Box<Self>, Vec<Box<Self>>),
    /// OneShot Modifier - tap once, modifier stays active for next keypress only
    /// Perfect for typing capital letters without holding shift
    /// Format: OSM(modifier_action)
    /// Example: OSM(Key(KC_LSFT)) - tap for one-shot shift
    OSM(Box<Self>),
    /// Double-Tap action (QMK-style tap dance)
    /// Single tap: performs first action, Double tap: performs second action
    /// Format: DT(single_tap_action, double_tap_action)
    /// Example: DT(Key(KC_LALT), TO("nav")) - single tap for alt, double tap for nav layer
    DT(Box<Self>, Box<Self>),
    /// Run arbitrary shell command
    /// Example: CMD("/usr/bin/notify-send 'Hello'")
    CMD(String),
    /// Transparent - fall through to lower layer
    /// Like QMK's underscore key - ignores this position on current layer
    /// and looks it up on the next layer down (or base)
    Transparent,
}

impl KeyAction {
    /// Check if this action is Transparent
    pub const fn is_transparent(&self) -> bool {
        matches!(self, Self::Transparent)
    }

    /// Check if this action is a layer switch (To, Tg, Mo)
    pub const fn is_layer_action(&self) -> bool {
        matches!(self, Self::TO(_) | Self::TG(_) | Self::MO(_))
    }

    /// Extract layer from layer actions
    pub const fn get_layer(&self) -> Option<&Layer> {
        match self {
            Self::TO(layer) | Self::TG(layer) | Self::MO(layer) => Some(layer),
            _ => None,
        }
    }

    /// Whether this action can emit key events (vs purely logical like layers/commands)
    pub const fn is_key_emitter(&self) -> bool {
        matches!(
            self,
            Self::Key(_) | Self::MT(_, _) | Self::DT(_, _) | Self::OSM(_) | Self::SOCD(_, _)
        )
    }
}

/// Layer configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerConfig {
    /// What the layer does. Defaults to ordinary remapping.
    #[serde(default)]
    pub kind: LayerKind,
    /// Key remaps. On a steno layer these apply to non-stroke keys only (e.g. an
    /// exit key); unmapped non-stroke keys are swallowed while the layer is active.
    #[serde(default)]
    pub remaps: HashMap<KeyCode, KeyAction>,
    /// Stop capturing strokes while game mode is active. Only affects steno layers.
    #[serde(default)]
    pub disabled_in_game_mode: bool,
}

/// What a layer does with the keys pressed while it's active.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayerKind {
    /// Remap keys; anything unmapped falls through to the layers below.
    #[default]
    Remap,
    /// Capture stroke keys as a steno chord and type dictionary text.
    Steno(StenoConfig),
}

/// Settings for a steno layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StenoConfig {
    /// Built-in layout preset to start from.
    #[serde(default = "default_steno_layout")]
    pub layout: String,
    /// Per-key changes to the preset, e.g. `{ "S-": Key(KC_Q) }`.
    #[serde(default)]
    pub layout_overrides: HashMap<String, KeyAction>,
    /// Plover-format dictionaries, highest priority first. `~/` expands to the
    /// session user's home; relative paths resolve against the config directory.
    #[serde(default)]
    pub dictionaries: Vec<String>,
    /// Safety net for a key that never comes up: the stroke is finished after this
    /// long. Strokes normally finish when the last key is released, with no delay.
    #[serde(default = "default_stroke_timeout_ms")]
    pub stroke_timeout_ms: u32,
    /// No longer used. Translation is immediate and corrects earlier text when a
    /// longer entry matches. Setting it is a validation warning.
    #[serde(default)]
    pub translation_timeout_ms: Option<u32>,
}

fn default_steno_layout() -> String {
    "qwerty".to_string()
}

const fn default_stroke_timeout_ms() -> u32 {
    5000
}

/// Game mode configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GameMode {
    #[serde(default)]
    pub remaps: HashMap<KeyCode, KeyAction>,
}

/// Per-keyboard override configuration
///
/// This has the EXACT same structure as the main Config, but all fields are optional
/// This allows you to copy the global config and paste it here - it will just override the specified fields
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PerKeyboardConfig {
    pub tapping_term_ms: Option<u32>,
    pub mt_config: Option<MtConfig>,
    pub double_tap_window_ms: Option<u64>,
    pub oneshot_timeout_ms: Option<u64>,
    pub remaps: Option<HashMap<KeyCode, KeyAction>>,
    pub layers: Option<HashMap<Layer, LayerConfig>>,
    pub game_mode: Option<GameMode>,
}

/// MT (Mod-Tap) configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MtConfig {
    /// Enable permissive hold - if another key is pressed while MT is pending,
    /// immediately resolve to hold (default: true)
    #[serde(default = "default_true")]
    pub permissive_hold: bool,

    /// Enable same-hand roll detection - rolls on same hand favor tap (default: true)
    #[serde(default = "default_true", alias = "enable_roll_detection")]
    pub same_hand_roll_detection: bool,

    /// Enable opposite-hand chord detection - chords on opposite hands favor hold (default: true)
    #[serde(default = "default_true", alias = "enable_chord_detection")]
    pub opposite_hand_chord_detection: bool,

    /// Enable multi-mod detection - multiple modifiers held simultaneously
    /// on same hand all promote to hold (default: true)
    #[serde(default = "default_true", alias = "enable_multi_mod_detection")]
    pub multi_mod_detection: bool,

    /// Minimum number of MT keys held to trigger multi-mod (default: 2)
    #[serde(default = "default_multi_mod_threshold")]
    pub multi_mod_threshold: usize,

    /// Enable adaptive timing - adjust thresholds based on user behavior (default: false)
    #[serde(default, alias = "enable_adaptive_timing")]
    pub adaptive_timing: bool,

    /// Enable predictive intent scoring (default: false)
    #[serde(default, alias = "enable_predictive_scoring")]
    pub predictive_scoring: bool,

    /// Roll detection window in ms (default: 150)
    #[serde(default = "default_roll_window", alias = "roll_threshold_ms")]
    pub roll_detection_window_ms: u32,

    /// Chord detection window in ms (default: 50)
    #[serde(default = "default_chord_window", alias = "chord_threshold_ms")]
    pub chord_detection_window_ms: u32,

    /// Enable double-tap-then-hold - double tap to hold the tap key until released (default: false)
    #[serde(default, alias = "enable_double_tap_hold")]
    pub double_tap_then_hold: bool,

    /// Window (ms) for detecting double-taps (default: 300)
    #[serde(default = "default_double_tap_window")]
    pub double_tap_window_ms: u32,

    /// Enable cross-hand unwrap - when holding a modifier on one hand,
    /// MT keys on the opposite hand will unwrap to their tap key (default: true)
    /// Example: Hold ; (right hand, becomes Win), press f (left hand MT) → types 'f' not Shift
    #[serde(default = "default_true", alias = "enable_cross_hand_unwrap")]
    pub cross_hand_unwrap: bool,

    /// Target margin (ms) to keep adaptive threshold above average tap duration (default: 30)
    /// Example: If your average tap is 45ms, threshold becomes 45 + 30 = 75ms
    #[serde(default = "default_adaptive_margin", alias = "target_margin_ms")]
    pub adaptive_target_margin_ms: u32,

    /// Pause adaptive learning in game mode (default: true)
    #[serde(default = "default_true")]
    pub pause_learning_in_game_mode: bool,

    /// Exponential moving average alpha for adaptive learning (default: 0.02)
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f32,

    /// Auto-save adaptive stats interval in seconds (default: 30)
    #[serde(default = "default_auto_save_interval")]
    pub auto_save_interval_secs: u32,

    /// When holding an MT key and doing nothing, emit tap on release (default: true)
    /// If true, holding then releasing without other action sends the tap key
    /// If false, holding then releasing without other action does nothing
    #[serde(default = "default_true")]
    pub hold_do_nothing_emits_tap: bool,
}

const fn default_ema_alpha() -> f32 {
    0.02
}

const fn default_auto_save_interval() -> u32 {
    30
}

const fn default_true() -> bool {
    true
}
const fn default_multi_mod_threshold() -> usize {
    2
}
const fn default_roll_window() -> u32 {
    150
}
const fn default_chord_window() -> u32 {
    50
}
const fn default_double_tap_window() -> u32 {
    300
}
const fn default_adaptive_margin() -> u32 {
    30
}

impl Default for MtConfig {
    fn default() -> Self {
        Self {
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
            pause_learning_in_game_mode: true,
            ema_alpha: 0.02,
            auto_save_interval_secs: 30,
            hold_do_nothing_emits_tap: true,
        }
    }
}

/// Wrapper to track if enabled_keyboards was explicitly set in config
/// This allows distinguishing between "field absent" vs "field set to None"
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EnabledKeyboards {
    /// Field explicitly set to None (disable all)
    ExplicitNone,
    /// Some(None) - legacy format that means disable all
    SomeNone,
    /// Field set to a list of entries (bare [....])
    List(Vec<EnabledKeyboardEntry>),
    /// Field set to Some([....]) - legacy format, unwrapped to List
    SomeList(Vec<EnabledKeyboardEntry>),
}

impl EnabledKeyboards {
    /// Check if this is ExplicitNone (field was set to None)
    pub const fn is_explicit_none(&self) -> bool {
        matches!(self, Self::ExplicitNone | Self::SomeNone)
    }

    /// Get the entries if this is a list
    pub fn entries(&self) -> Option<&[EnabledKeyboardEntry]> {
        match self {
            Self::ExplicitNone | Self::SomeNone => None,
            Self::List(entries) | Self::SomeList(entries) => Some(entries),
        }
    }

    /// Normalize to canonical form (convert legacy Some* variants)
    pub fn normalize(&self) -> Self {
        match self {
            Self::ExplicitNone => Self::ExplicitNone,
            Self::SomeNone => Self::ExplicitNone,
            Self::List(entries) => Self::List(entries.clone()),
            Self::SomeList(entries) => Self::List(entries.clone()),
        }
    }
}

impl Default for EnabledKeyboards {
    fn default() -> Self {
        Self::List(vec![EnabledKeyboardEntry::Bare("*".to_string())])
    }
}

/// Main configuration structure
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_tapping_term")]
    pub tapping_term_ms: u32,
    #[serde(default)]
    pub mt_config: MtConfig,
    #[serde(default)]
    pub enabled_keyboards: EnabledKeyboards,
    #[serde(default)]
    pub remaps: HashMap<KeyCode, KeyAction>,
    #[serde(default)]
    pub layers: HashMap<Layer, LayerConfig>,
    #[serde(default)]
    pub game_mode: GameMode,
    #[serde(default)]
    pub per_keyboard_overrides: HashMap<String, PerKeyboardConfig>,

    /// Double-tap window (milliseconds) - QMK tap dance inspired
    /// Default: 250ms (configurable, sensible default)
    pub double_tap_window_ms: Option<u64>,

    /// OneShot timeout (milliseconds) - QMK one-shot keys inspired
    /// Default: 5000ms (5 seconds, like QMK)
    pub oneshot_timeout_ms: Option<u64>,

    /// Enable hot config reload - automatically reload config when file changes (default: false)
    /// When enabled, changes to config.ron are immediately applied without restarting daemon
    #[serde(default)]
    pub hot_config_reload: bool,

    /// Per-keyboard configs inherit global layout by default (default: true)
    /// - true: per_keyboard_overrides merge with global config (override specific fields)
    /// - false: per_keyboard_overrides replace global config (build from scratch)
    #[serde(default = "default_true_bool")]
    pub per_keyboard_inherits_global_layout: bool,
}

const fn default_tapping_term() -> u32 {
    130
}

const fn default_true_bool() -> bool {
    true
}

impl Config {
    /// Preprocess config text to allow bare KeyCode syntax everywhere
    /// Transforms `KC_*` → `Key(KC_*)` in all contexts except HashMap keys
    ///
    /// This heuristic allows cleaner config syntax:
    /// - `KC_CAPS: KC_ESC,` → `KC_CAPS: Key(KC_ESC),`
    /// - `MT(KC_A, KC_LGUI)` → `MT(Key(KC_A), Key(KC_LGUI))`
    /// - `"1234": Enable` → `("1234", Enable)`
    fn find_enabled_keyboards_array(content: &str) -> Option<(usize, usize)> {
        let marker = "enabled_keyboards:";
        if let Some(start) = content.find(marker) {
            let after_marker = start + marker.len();
            let rest = &content[after_marker..];

            // Skip whitespace and track the offset
            let leading_ws = rest.len() - rest.trim_start().len();
            let rest = rest.trim_start();

            if rest.starts_with("None") {
                return Some((after_marker + leading_ws, after_marker + leading_ws + 4));
            }

            if !rest.starts_with('[') {
                return None;
            }

            // Find matching ] - track brackets and strings
            let mut depth = 0;
            let mut in_string = false;
            let mut escaped = false;

            for (i, ch) in rest.char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                match ch {
                    '\\' => escaped = true,
                    '"' => in_string = !in_string,
                    '[' if !in_string => depth += 1,
                    ']' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            // Return positions in original string
                            let arr_start = after_marker + leading_ws;
                            let arr_end = after_marker + leading_ws + i + 1;
                            return Some((arr_start, arr_end));
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    fn preprocess_config(content: &str) -> String {
        use regex::Regex;

        // First, preprocess enabled_keyboards entries: "pattern": Enable/Disable -> ("pattern", "Enable") etc
        let re_enabled = Regex::new(r#""([^"]+)"\s*:\s*(\w+)"#).unwrap();

        let mut result = String::with_capacity(content.len() * 2);
        let last_end = 0;

        // Find and preprocess enabled_keyboards array
        if let Some((arr_start, arr_end)) = Self::find_enabled_keyboards_array(content) {
            // Copy content before and within the array, but do replacements only inside array
            let before_array = &content[last_end..arr_start];
            let arr_content = &content[arr_start..arr_end];
            let after_array = &content[arr_end..];

            // Do KC_* preprocessing on content before array (original logic)
            let _re_kc = Regex::new(r"\b(KC_[A-Z0-9_]+)\b").unwrap();
            let processed_before = Self::preprocess_kc_only(before_array);
            result.push_str(&processed_before);

            // Preprocess the array content - convert Enable->"Enable", Disable->"Disable"
            let preprocessed = re_enabled.replace_all(arr_content, |caps: &regex::Captures| {
                let pattern = &caps[1];
                let action_raw = &caps[2];
                let action = match action_raw.to_lowercase().as_str() {
                    "enable" => "\"Enable\"",
                    "disable" => "\"Disable\"",
                    _ => action_raw,
                };
                format!("(\"{}\", {})", pattern, action)
            });

            result.push_str(&preprocessed);

            // Do KC_* preprocessing on content after array
            let processed_after = Self::preprocess_kc_only(after_array);
            result.push_str(&processed_after);
        } else {
            // No enabled_keyboards found, just do KC_* preprocessing on everything
            result = Self::preprocess_kc_only(content);
        }

        result
    }

    fn preprocess_kc_only(content: &str) -> String {
        use regex::Regex;
        let re = Regex::new(r"\b(KC_[A-Z0-9_]+)\b").unwrap();

        let mut result = String::with_capacity(content.len() * 2);
        let mut last_end = 0;

        for cap in re.find_iter(content) {
            let start = cap.start();
            let end = cap.end();
            let keycode = cap.as_str();

            result.push_str(&content[last_end..start]);

            let suffix = &content[end..];
            let next_char = suffix.trim_start().chars().next();

            let prefix = &content[..start];
            let prev_trimmed = prefix.trim_end();
            let prev_char = prev_trimmed.chars().last();

            let is_hashmap_key = next_char == Some(':');
            let already_wrapped = prev_trimmed.ends_with("Key(");

            let should_wrap = !is_hashmap_key
                && !already_wrapped
                && matches!(prev_char, Some(':') | Some('(') | Some(',') | Some('['));

            if should_wrap {
                result.push_str(&format!("Key({})", keycode));
            } else {
                result.push_str(keycode);
            }

            last_end = end;
        }

        result.push_str(&content[last_end..]);
        result
    }

    /// Load config from RON file
    #[allow(clippy::missing_errors_doc)]
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;

        // Preprocess to support bare KeyCode syntax
        let preprocessed = Self::preprocess_config(&content);

        let config = ron::from_str(&preprocessed)
            .map_err(|e| anyhow::anyhow!("Config parsing error: {}", e))?;
        Ok(config)
    }

    /// Save config to RON file
    #[allow(clippy::missing_errors_doc)]
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let pretty = ron::ser::PrettyConfig::default();
        let content = ron::ser::to_string_pretty(self, pretty)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Load config from RON file, but with an actionable error message
    /// (mentioning the exact path and `keymux init`) when the file simply
    /// doesn't exist yet, instead of a bare OS error. Prefer this over
    /// `load` in CLI commands the user runs directly.
    #[allow(clippy::missing_errors_doc)]
    pub fn load_or_hint(path: &std::path::Path) -> anyhow::Result<Self> {
        if !path.exists() {
            anyhow::bail!(
                "No config found at {}\n\n  Run `keymux init` to create a starter config, or copy one from config.example.ron.",
                path.display()
            );
        }
        Self::load(path)
    }

    /// Get default config path
    #[allow(clippy::missing_errors_doc)]
    pub fn default_path() -> anyhow::Result<std::path::PathBuf> {
        let (uid, is_sudo) = crate::get_actual_user_uid();

        if is_sudo {
            // When run with sudo, use actual user's home directory
            let home_dir = crate::get_user_home_dir(uid)?;
            Ok(home_dir.join(".config").join("keymux").join("config.ron"))
        } else {
            // Normal case: use dirs crate
            let config_dir =
                dirs::config_dir().ok_or_else(|| anyhow::anyhow!("Failed to get config dir"))?;
            Ok(config_dir.join("keymux").join("config.ron"))
        }
    }

    /// Check if a keyboard should be enabled based on the enabled_keyboards config
    ///
    /// Parsing rules (applied in order):
    /// 1. Field absent entirely → treat as ["*": Enable] (enable all)
    /// 2. None (ExplicitNone) → disable all
    /// 3. List → use the list (last match wins)
    /// 4. Bare string → defaults to Enable
    pub fn is_keyboard_enabled(
        &self,
        keyboard_id: &str,
        keyboard_name: Option<&str>,
        event_path: Option<&str>,
    ) -> bool {
        // Check the EnabledKeyboards wrapper (normalize first to handle legacy Some* variants)
        match self.enabled_keyboards.normalize() {
            // ExplicitNone/SomeNone means field was set to None → disable all
            EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => false,
            // List/SomeList of entries - apply matching rules
            EnabledKeyboards::List(entries) | EnabledKeyboards::SomeList(entries) => {
                // If list is empty, disable all
                if entries.is_empty() {
                    return false;
                }

                // Track if we matched "*" (enable all glob)
                let mut matched_star_enable = false;
                // Track the final decision (last match wins)
                let mut final_action: Option<EnableDisable> = None;

                for entry in entries {
                    let pattern = entry.pattern();
                    let action = entry.action();

                    // Check for glob "*"
                    if pattern == "*" {
                        if action == EnableDisable::Enable {
                            matched_star_enable = true;
                        }
                        final_action = Some(action);
                        continue;
                    }

                    // Check if this entry matches the keyboard
                    // Match against: event path, ID (partial or exact), or keyboard name
                    let normalized_event =
                        event_path.map(|e| e.strip_prefix("/dev/input/").unwrap_or(e));

                    let normalized_pattern = pattern.strip_prefix("/dev/input/").unwrap_or(pattern);

                    let matches = if let Some(event) = normalized_event {
                        // Check event path match (e.g., "event17" or "/dev/input/event17")
                        pattern == event
                            || normalized_pattern == event
                            || keyboard_id.contains(pattern)
                            || keyboard_id.contains(normalized_pattern)
                            || keyboard_id.starts_with(pattern)
                            || keyboard_id.starts_with(normalized_pattern)
                            || keyboard_name
                                .map(|name| name.contains(pattern))
                                .unwrap_or(false)
                    } else {
                        // Just check ID match or name match
                        keyboard_id.contains(pattern)
                            || keyboard_id.contains(normalized_pattern)
                            || keyboard_id.starts_with(pattern)
                            || keyboard_id.starts_with(normalized_pattern)
                            || keyboard_name
                                .map(|name| name.contains(pattern))
                                .unwrap_or(false)
                    };

                    if matches {
                        final_action = Some(action);
                    }
                }

                // If we matched "*" with enable and nothing else overrode, enable the keyboard
                if matched_star_enable && final_action.is_none() {
                    return true;
                }

                // Return the final action, or true if no matches (enable by default for explicit items)
                final_action.unwrap_or(EnableDisable::Enable) == EnableDisable::Enable
            }
        }
    }

    /// Get the entries for serialization (converts to appropriate format)
    /// This handles backwards compatibility for saving
    pub fn get_enabled_keyboards_entries(&self) -> Option<Vec<EnabledKeyboardEntry>> {
        match self.enabled_keyboards.normalize() {
            EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => None,
            EnabledKeyboards::List(entries) | EnabledKeyboards::SomeList(entries) => Some(entries),
        }
    }

    /// Get effective config for a specific keyboard
    /// Applies per-keyboard overrides on top of the global config (or replaces it)
    #[must_use]
    #[allow(clippy::option_if_let_else)] // Complex nested logic, keeping for readability
    pub fn for_keyboard(&self, keyboard_id: &str) -> Self {
        // Match per_keyboard_overrides using prefix logic for backwards compatibility:
        // a key without "@port" matches any port of that hardware ID; "@port" is exact.
        let our_base = keyboard_id.split('@').next().unwrap_or(keyboard_id);
        let override_cfg = self.per_keyboard_overrides.iter().find_map(|(key, cfg)| {
            let matches = if key.contains('@') {
                key == keyboard_id
            } else {
                key.as_str() == our_base
            };
            if matches {
                Some(cfg)
            } else {
                None
            }
        });
        if let Some(override_cfg) = override_cfg {
            if self.per_keyboard_inherits_global_layout {
                // INHERITING MODE: Start with global config, merge/override with per-keyboard settings
                let mut config = self.clone();

                // Apply all overrides - each field is optional and only overrides if Some
                if let Some(term) = override_cfg.tapping_term_ms {
                    config.tapping_term_ms = term;
                }
                if let Some(mt) = &override_cfg.mt_config {
                    config.mt_config = mt.clone();
                }

                // MERGE remaps: extend global remaps with per-keyboard remaps
                // Per-keyboard remaps override global ones for the same keys
                if let Some(remaps) = &override_cfg.remaps {
                    config.remaps.extend(remaps.clone());
                }

                // MERGE layers: extend global layers with per-keyboard layers
                // Per-keyboard layers override global ones for the same layer names
                if let Some(layers) = &override_cfg.layers {
                    config.layers.extend(layers.clone());
                }

                // MERGE game_mode: extend global game_mode remaps with per-keyboard game_mode remaps
                if let Some(game_mode) = &override_cfg.game_mode {
                    config.game_mode.remaps.extend(game_mode.remaps.clone());
                }

                config
            } else {
                // NON-INHERITING MODE: Build from scratch with per-keyboard config only
                // Use defaults for any fields not specified in per-keyboard config
                Self {
                    tapping_term_ms: override_cfg
                        .tapping_term_ms
                        .unwrap_or_else(default_tapping_term),
                    mt_config: override_cfg.mt_config.clone().unwrap_or_default(),
                    enabled_keyboards: self.enabled_keyboards.clone(), // Keep global enabled_keyboards
                    remaps: override_cfg.remaps.clone().unwrap_or_default(),
                    layers: override_cfg.layers.clone().unwrap_or_default(),
                    game_mode: override_cfg.game_mode.clone().unwrap_or_default(),
                    per_keyboard_overrides: HashMap::new(), // Don't nest overrides
                    double_tap_window_ms: override_cfg
                        .double_tap_window_ms
                        .or(self.double_tap_window_ms),
                    oneshot_timeout_ms: override_cfg.oneshot_timeout_ms.or(self.oneshot_timeout_ms),
                    hot_config_reload: self.hot_config_reload, // Keep global hot reload setting
                    per_keyboard_inherits_global_layout: self.per_keyboard_inherits_global_layout, // Keep global setting
                }
            }
        } else {
            // No per-keyboard override found, return global config as-is
            self.clone()
        }
    }

    /// Save only `enabled_keyboards` field, preserving rest of file
    #[allow(clippy::missing_errors_doc)]
    /// Save only the enabled_keyboards field, preserving all other formatting
    pub fn save_enabled_keyboards_only(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.save_enabled_keyboards_only_with_comments(path, None)
    }

    /// Save enabled_keyboards with optional comments for each pattern
    #[allow(clippy::missing_errors_doc)]
    pub fn save_enabled_keyboards_only_with_comments(
        &self,
        path: &std::path::Path,
        comments: Option<&std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<()> {
        // Read the original file
        let content = std::fs::read_to_string(path)?;

        // Find the enabled_keyboards field
        let start_marker = "enabled_keyboards:";

        if let Some(start_pos) = content.find(start_marker) {
            // Find where the field starts (after the colon)
            let field_start = start_pos + start_marker.len();

            // Find the end of this field (next field or closing paren)
            let remaining = &content[field_start..];

            // Find the end by looking for comma at the end of the value
            let mut end_pos = field_start;
            let mut depth = 0;
            let mut in_string = false;
            let mut found_value_start = false;

            for (i, ch) in remaining.char_indices() {
                match ch {
                    '"' if i == 0
                        || remaining.as_bytes().get(i.wrapping_sub(1)) != Some(&b'\\') =>
                    {
                        in_string = !in_string;
                    }
                    '[' if !in_string => {
                        depth += 1;
                        found_value_start = true;
                    }
                    ']' if !in_string => {
                        depth -= 1;
                        if depth == 0 && found_value_start {
                            // Found the end of the array
                            end_pos = field_start + i + 1;
                            break;
                        }
                    }
                    'N' if !in_string
                        && !found_value_start
                        && remaining[i..].starts_with("None") =>
                    {
                        end_pos = field_start + i + 4;
                        break;
                    }
                    'S' if !in_string
                        && !found_value_start
                        && remaining[i..].starts_with("Some") =>
                    {
                        found_value_start = true;
                    }
                    '(' if !in_string && found_value_start => depth += 1,
                    ')' if !in_string && found_value_start => {
                        depth -= 1;
                        if depth == 0 {
                            end_pos = field_start + i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }

            if end_pos == field_start {
                // Couldn't parse, fall back to full save
                return self.save(path);
            }

            // Generate the new value (normalize to handle legacy Some* variants)
            let new_value = match self.enabled_keyboards.normalize() {
                EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => " None".to_string(),
                EnabledKeyboards::List(keyboards) | EnabledKeyboards::SomeList(keyboards) => {
                    if keyboards.is_empty() {
                        " []".to_string()
                    } else {
                        let mut result = " [\n".to_string();
                        for kbd in keyboards {
                            match kbd {
                                EnabledKeyboardEntry::Bare(ref pattern) => {
                                    let comment = comments
                                        .and_then(|c| c.get(pattern))
                                        .map(|name| format!(" // {}", name))
                                        .unwrap_or_default();
                                    result.push_str(&format!(
                                        "        \"{}\",{}\n",
                                        pattern, comment
                                    ));
                                }
                                EnabledKeyboardEntry::Explicit(ref pattern, action) => {
                                    let action_str = match action {
                                        EnableDisable::Enable => "Enable",
                                        EnableDisable::Disable => "Disable",
                                    };
                                    let comment = comments
                                        .and_then(|c| c.get(pattern))
                                        .map(|name| format!(" // {}", name))
                                        .unwrap_or_default();
                                    result.push_str(&format!(
                                        "        \"{}\": {},{}\n",
                                        pattern, action_str, comment
                                    ));
                                }
                            }
                        }
                        result.push_str("    ]");
                        result
                    }
                }
            };
            let new_content = format!(
                "{}{}{}",
                &content[..field_start],
                new_value,
                &content[end_pos..]
            );

            // Write it back
            std::fs::write(path, new_content)?;
            Ok(())
        } else {
            // enabled_keyboards field not found, fall back to full save
            self.save(path)
        }
    }

    /// Validate config without printing - returns errors as a Vec<String>
    pub fn validate_silent(&self) -> Result<()> {
        use std::collections::{HashMap, HashSet};

        let mut errors: Vec<String> = Vec::new();

        // Validation 1: Check SOCD pairs are symmetric
        let mut socd_map: HashMap<KeyCode, KeyCode> = HashMap::new();

        let mut extract_socd = |remaps: &HashMap<KeyCode, KeyAction>| {
            let mut pairs = Vec::new();
            for (key, action) in remaps {
                if let KeyAction::SOCD(this_action, opposing_actions) = action {
                    // Extract KeyCode from Action (only validate Key actions)
                    if let KeyAction::Key(this_key) = this_action.as_ref() {
                        if key != this_key {
                            errors.push(format!(
                                "SOCD key mismatch: {:?} maps to SOCD({:?}, ...)",
                                key, this_key
                            ));
                        }
                        for opposing_action in opposing_actions {
                            if let KeyAction::Key(opposing_key) = opposing_action.as_ref() {
                                pairs.push((*this_key, *opposing_key));
                            }
                        }
                    }
                }
            }
            pairs
        };

        for (key1, key2) in extract_socd(&self.remaps) {
            socd_map.insert(key1, key2);
        }
        for layer_config in self.layers.values() {
            for (key1, key2) in extract_socd(&layer_config.remaps) {
                socd_map.insert(key1, key2);
            }
        }
        for (key1, key2) in extract_socd(&self.game_mode.remaps) {
            socd_map.insert(key1, key2);
        }

        // Check symmetry
        let mut socd_checked = HashSet::new();
        for (key1, key2) in &socd_map {
            if socd_checked.contains(key1) {
                continue;
            }
            if let Some(reverse) = socd_map.get(key2) {
                if reverse != key1 {
                    errors.push(format!(
                        "SOCD pair asymmetric: {:?} → {:?}, but {:?} → {:?}",
                        key1, key2, key2, reverse
                    ));
                }
                socd_checked.insert(*key1);
                socd_checked.insert(*key2);
            } else {
                errors.push(format!(
                    "SOCD missing reverse pair: {:?} → {:?}, but {:?} not defined",
                    key1, key2, key2
                ));
            }
        }

        // Validation 2: Check timing values are reasonable
        if self.tapping_term_ms == 0 || self.tapping_term_ms > 1000 {
            errors.push(format!(
                "tapping_term_ms out of reasonable range (0-1000): {}",
                self.tapping_term_ms
            ));
        }

        // Validate MT config timing
        if self.mt_config.double_tap_window_ms == 0 || self.mt_config.double_tap_window_ms > 1000 {
            errors.push(format!(
                "mt_config.double_tap_window_ms out of reasonable range (0-1000): {}",
                self.mt_config.double_tap_window_ms
            ));
        }

        // Validation 3: Check layer references
        let mut referenced_layers = HashSet::new();

        let extract_layer_refs = |remaps: &HashMap<KeyCode, KeyAction>| {
            let mut refs = Vec::new();
            for action in remaps.values() {
                if let KeyAction::TO(layer) = action {
                    refs.push(layer.0.clone());
                }
            }
            refs
        };

        for layer_name in extract_layer_refs(&self.remaps) {
            referenced_layers.insert(layer_name);
        }
        for layer_config in self.layers.values() {
            for layer_name in extract_layer_refs(&layer_config.remaps) {
                referenced_layers.insert(layer_name);
            }
        }

        for layer_name in &referenced_layers {
            if layer_name != "base" && !self.layers.contains_key(&Layer(layer_name.clone())) {
                errors.push(format!("Referenced layer not defined: \"{}\"", layer_name));
            }
        }

        // Validation 4: Steno layers have a valid layout
        for (layer, layer_config) in &self.layers {
            if let LayerKind::Steno(steno) = &layer_config.kind {
                if let Err(layout_errors) = crate::steno::layout::preset(&steno.layout)
                    .map_err(|e| vec![e])
                    .and_then(|preset| {
                        crate::steno::layout::build_key_map(&preset, &steno.layout_overrides)
                    })
                {
                    for layout_error in layout_errors {
                        errors.push(format!("steno layer \"{}\": {}", layer.0, layout_error));
                    }
                }
            }
        }

        if !errors.is_empty() {
            Err(anyhow::anyhow!(
                "Config validation failed: {}",
                errors.join("; ")
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_layers_default_to_remap() {
        let layer: LayerConfig = ron::from_str("(remaps: { KC_A: Key(KC_B) })").unwrap();
        assert_eq!(layer.kind, LayerKind::Remap);
        assert!(!layer.disabled_in_game_mode);
    }

    #[test]
    fn steno_layer_parses_with_defaults() {
        let layer: LayerConfig = ron::from_str(
            r#"(
                kind: Steno((
                    layout_overrides: { "S-": Key(KC_Q) },
                    dictionaries: ["~/.config/keymux/steno/main.json"],
                )),
                disabled_in_game_mode: true,
            )"#,
        )
        .unwrap();
        assert!(layer.disabled_in_game_mode);
        let LayerKind::Steno(steno) = layer.kind else {
            panic!("expected a steno layer");
        };
        assert_eq!(steno.layout, "qwerty");
        assert_eq!(steno.stroke_timeout_ms, 5000);
        assert_eq!(steno.translation_timeout_ms, None);
        assert_eq!(steno.dictionaries.len(), 1);
    }

    #[test]
    fn test_preprocess_bare_keycode() {
        // Test bare KC_* after colon
        let input = "KC_CAPS: KC_ESC,";
        let expected = "KC_CAPS: Key(KC_ESC),";
        assert_eq!(Config::preprocess_config(input), expected);

        // Test already wrapped - should not double-wrap
        let input = "KC_CAPS: Key(KC_ESC),";
        let expected = "KC_CAPS: Key(KC_ESC),";
        assert_eq!(Config::preprocess_config(input), expected);

        // Test KC_* inside function calls - should wrap (function arguments)
        let input = "KC_A: MT(KC_A, KC_LCTL),";
        let expected = "KC_A: MT(Key(KC_A), Key(KC_LCTL)),";
        assert_eq!(Config::preprocess_config(input), expected);

        // Test multiple bare keycodes
        let input = "KC_CAPS: KC_ESC,\nKC_ESC: KC_GRV,";
        let expected = "KC_CAPS: Key(KC_ESC),\nKC_ESC: Key(KC_GRV),";
        assert_eq!(Config::preprocess_config(input), expected);

        // Test with closing brace
        let input = "KC_CAPS: KC_ESC}";
        let expected = "KC_CAPS: Key(KC_ESC)}";
        assert_eq!(Config::preprocess_config(input), expected);

        // Test with whitespace variations
        let input = "KC_CAPS:KC_ESC,";
        let expected = "KC_CAPS:Key(KC_ESC),";
        assert_eq!(Config::preprocess_config(input), expected);

        let input = "KC_CAPS:  KC_ESC,";
        let expected = "KC_CAPS:  Key(KC_ESC),";
        assert_eq!(Config::preprocess_config(input), expected);
    }

    #[test]
    fn test_preprocess_preserves_other_actions() {
        // TO action
        let input = r#"KC_LALT: TO("nav"),"#;
        assert_eq!(Config::preprocess_config(input), input);

        // SOCD action - KC inside function calls should be wrapped
        let input = "KC_W: SOCD(KC_W, [KC_S]),";
        let expected = "KC_W: SOCD(Key(KC_W), [Key(KC_S)]),";
        assert_eq!(Config::preprocess_config(input), expected);

        // CMD action
        let input = r#"KC_F1: CMD("/usr/bin/test"),"#;
        assert_eq!(Config::preprocess_config(input), input);
    }
}
