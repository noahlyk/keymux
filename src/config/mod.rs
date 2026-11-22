pub mod config;
pub mod config_manager;
pub mod validator;

pub use config::{
    Check, Config, DictionaryRoots, EnableDisable, EnabledKeyboardEntry, EnabledKeyboards,
    GameMode, Half, KeyAction, KeyColor, KeyMerge, Layer, LayerConfig, LayerKind, MtConfig, OverlayConfig,
    OverlayPlacement, Report, SkewConfig, SkewSide, StenoConfig, MAX_SKEW_DELAY_MS, DEFAULT_LAYER_PRIORITY,
};
pub use config_manager::ConfigManager;
pub use validator::validate_config;
