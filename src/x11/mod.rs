mod bspwm_daemon;
mod i3_daemon;

mod impl_mod;

pub use bspwm_daemon::run_bspwm_daemon;
pub use i3_daemon::run_i3_daemon;

pub use impl_mod::{
    get_focused_window, is_bspwm_available, is_i3_available, start_bspwm_monitor,
    start_bspwm_monitor_sync, start_i3_monitor, start_i3_monitor_sync,
};

pub use crate::window_manager::{WindowInfo, WindowManagerEvent};
