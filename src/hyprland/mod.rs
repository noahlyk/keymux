pub mod hyprland_daemon;
pub mod mod_impl;
pub mod sway_daemon;

pub use hyprland_daemon::run_hyprland_daemon;
pub use mod_impl::{
    detect_wayland_compositor, get_focused_window, is_hyprland_available, is_sway_available,
    start_hyprland_monitor, start_hyprland_monitor_sync, start_sway_monitor,
    start_sway_monitor_sync, WaylandCompositor,
};
pub use sway_daemon::run_sway_daemon;

pub use crate::window_manager::{WindowInfo, WindowManagerEvent};
