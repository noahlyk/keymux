#![allow(clippy::pedantic, clippy::module_inception)]

pub mod config;
pub mod daemon;
pub mod event_processor;
pub mod gamemode_state;
pub mod hyprland;
pub mod ipc;
pub mod keyboard_id;
pub mod keycode;
pub mod niri;
pub mod session_manager;
pub mod steno;
pub mod ui;
pub mod window_manager;
pub mod x11;

use std::path::PathBuf;

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
