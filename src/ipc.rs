#![allow(clippy::cast_possible_truncation)]

use crate::gamemode_state::{GlobalOverride, WindowOverride};
use crate::window_manager::WindowInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// IPC message from client to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Check if daemon is alive
    Ping,
    /// List all detected keyboards with their status
    ListKeyboards,
    /// Toggle enabled status of keyboards (returns current list for interactive selection)
    ToggleKeyboards,
    /// Enable specific keyboard by hardware ID
    EnableKeyboard(String),
    /// Disable specific keyboard by hardware ID
    DisableKeyboard(String),
    /// Reload configuration from disk
    Reload,
    /// Force save adaptive timing stats immediately
    SaveAdaptiveStats,
    /// Shutdown daemon
    Shutdown,

    /// Reported by a per-compositor watcher process on every focus change.
    /// The daemon resolves the effective game mode itself (heuristics +
    /// overrides) and applies it - watchers no longer compute a boolean.
    WindowFocusChanged {
        app_id: Option<String>,
        pid: Option<u32>,
        title: Option<String>,
    },
    /// Ask the daemon for the last window focus event it received (used by
    /// `keymux gamemode window` to default to "the currently focused app"
    /// without any compositor-specific code in the CLI).
    GetFocusedWindow,
    /// Set (or clear, with `state: None`) a temporary per-app_id override.
    SetWindowOverride {
        app_id: String,
        state: Option<WindowOverride>,
    },
    /// Flip the currently-effective state for an app_id into an explicit
    /// override (On -> Off, Off/heuristic-Normal -> On).
    ToggleWindowOverride { app_id: String },
    /// List all active per-app_id overrides
    ListWindowOverrides,
    /// Set the global override (Auto/AlwaysOn/AlwaysOff)
    SetGlobalOverride(GlobalOverride),
    /// Get the current global override
    GetGlobalOverride,
}

/// IPC response from daemon to client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    /// Daemon is alive
    Pong,
    /// List of keyboards with their info
    KeyboardList(Vec<KeyboardInfo>),
    /// Operation succeeded
    Ok,
    /// Operation failed with error message
    Error(String),
    /// The last window the daemon saw focused (empty fields if none yet)
    FocusedWindow(WindowInfo),
    /// Active per-app_id overrides, sorted by app_id
    WindowOverrides(Vec<(String, WindowOverride)>),
    /// Current global override
    GlobalOverrideStatus(GlobalOverride),
}

/// Information about a detected keyboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardInfo {
    /// Persistent hardware ID including USB port (e.g. "362d:0210:0111:0003@3-4.2")
    pub hardware_id: String,
    /// Human-readable name
    pub name: String,
    /// Current device path (can change between boots)
    pub device_path: String,
    /// Whether this keyboard is currently enabled
    pub enabled: bool,
    /// Whether this keyboard is currently connected
    pub connected: bool,
    /// True when enabled via a portless config entry (e.g. "362d:0210:0111:0003")
    /// rather than an explicit port entry (e.g. "362d:0210:0111:0003@3-4.2").
    /// Used by the display layer to annotate the ID with a hint.
    pub enabled_by_portless: bool,
    /// The config rule pattern that matched (e.g., "*", "1234", "Keychron")
    /// None if implicitly enabled/disabled (no explicit rule matched)
    pub matched_rule: Option<String>,
}

/// Get the IPC socket path for root daemon
pub fn get_root_socket_path() -> PathBuf {
    Path::new("/run").join("keymux.sock")
}

/// Get the IPC socket path for user daemon (legacy, for compatibility)
pub fn get_user_socket_path() -> PathBuf {
    let (uid, _) = crate::get_actual_user_uid();
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| format!("/run/user/{}", uid));
    Path::new(&runtime_dir).join("keymux.sock")
}

/// Get the IPC socket path (tries root first, falls back to user)
pub fn get_socket_path() -> PathBuf {
    let root_sock = get_root_socket_path();
    if root_sock.exists() {
        root_sock
    } else {
        get_user_socket_path()
    }
}

/// Send an IPC request and receive response
pub fn send_request(request: &IpcRequest) -> Result<IpcResponse> {
    let socket_path = get_socket_path();
    let mut stream =
        UnixStream::connect(&socket_path).context("Failed to connect to daemon. Is it running?")?;

    // Serialize and send request
    let encoded = bincode::serialize(request)?;
    let len = (encoded.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&encoded)?;
    stream.flush()?;

    // Read response length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Read response
    let mut response_buf = vec![0u8; len];
    stream.read_exact(&mut response_buf)?;
    let response: IpcResponse = bincode::deserialize(&response_buf)?;

    Ok(response)
}
