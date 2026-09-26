pub mod gamemode_detection;
pub mod niri;
pub mod niri_daemon;

pub use gamemode_detection::{detect_game_mode, GameModeState};
pub use niri::{
    get_focused_window, is_niri_available, start_niri_monitor, start_niri_monitor_sync,
};
pub use niri_daemon::run_niri_daemon;

pub use crate::window_manager::{WindowInfo, WindowManagerEvent};
