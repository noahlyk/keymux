pub mod config;
pub mod config_manager;
pub mod validator;

pub use config::{
    Check, Config, DictionaryRoots, EnableDisable, EnabledKeyboardEntry, EnabledKeyboards,
    GameMode, Half, KeyAction, KeyColor, KeyMerge, Layer, LayerConfig, LayerKind, LeaveMidStroke,
    MtConfig, OverlayConfig, OverlayPlacement, Report, SkewConfig, SkewSide, StenoConfig,
    DEFAULT_LAYER_PRIORITY, MAX_SKEW_DELAY_MS,
};
pub use config_manager::ConfigManager;
pub use validator::validate_config;
