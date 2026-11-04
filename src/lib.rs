#![allow(clippy::module_inception)]
// Pedantic lints opted out of one by one rather than wholesale: integer and float casts (pixel
// and timing arithmetic), documentation shape, and style churn that would touch hundreds of
// sites for no reader. Everything else in pedantic is on.
#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::float_cmp,
    clippy::format_push_string,
    clippy::implicit_hasher,
    clippy::items_after_statements,
    clippy::many_single_char_names,
    clippy::map_unwrap_or,
    clippy::match_same_arms,
    clippy::missing_errors_doc,
    clippy::missing_fields_in_debug,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::redundant_closure_for_method_calls,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::unused_self
)]

pub mod config;
pub mod daemon;
pub mod event_processor;
pub mod gamemode_state;
pub mod hyprland;
pub mod ipc;
pub mod keyboard_id;
pub mod keycode;
pub mod niri;
#[cfg(feature = "overlay")]
pub mod overlay;
pub mod session_manager;
pub mod steno;
pub mod ui;
pub mod window_manager;
pub mod x11;

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock even if a thread died holding it: what it guards is plain data, and the best there is.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Get the actual user UID, respecting SUDO context
/// Returns (uid, is_sudo) where is_sudo indicates if running under sudo
pub fn get_actual_user_uid() -> (u32, bool) {
    // Check if running under sudo
    if let Ok(sudo_uid) = std::env::var("SUDO_UID") {
        if let Ok(uid) = sudo_uid.parse::<u32>() {
            return (uid, true);
        }
    }

    // Fall back to current effective UID
    (unsafe { libc::getuid() }, false)
}

/// Get user's home directory from UID using getent
/// Works even when running as root/sudo
pub fn get_user_home_dir(uid: u32) -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("getent passwd {} | cut -d: -f6", uid))
        .output()?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "Failed to get home directory for UID {}",
            uid
        ));
    }

    let home = String::from_utf8(output.stdout)?.trim().to_string();

    if home.is_empty() {
        return Err(anyhow::anyhow!("Empty home directory for UID {}", uid));
    }

    Ok(PathBuf::from(home))
}
