pub mod config;
pub mod config_manager;
pub mod validator;

pub use config::{
    Check, Config, DictionaryRoots, EnableDisable, EnabledKeyboardEntry, EnabledKeyboards,
    GameMode, Half, KeyAction, Layer, LayerConfig, LayerKind, MtConfig, OverlayConfig,
    OverlayPlacement, Report, StenoConfig, DEFAULT_LAYER_PRIORITY,
};
pub use config_manager::ConfigManager;
pub use validator::validate_config;
