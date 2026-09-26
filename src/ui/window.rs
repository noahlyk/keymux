use std::collections::HashMap;
use std::process::Command;

use crate::gamemode_state::{GlobalOverride, WindowOverride};
pub use crate::niri::gamemode_detection::GameModeState;

#[derive(Debug, Clone)]
pub struct Window {
    pub id: u32,
    pub title: String,
    pub app_id: String,
    pub pid: u32,
    pub is_floating: bool,
    pub is_focused: bool,
}

impl Window {
    /// Check if this window should be in game mode by the programmed
    /// heuristics alone (does not account for any active override - use
    /// `effective_game_mode_state` for that).
    pub fn game_mode_state(&self) -> GameModeState {
        crate::niri::gamemode_detection::detect_game_mode(
            Some(&self.app_id),
            Some(self.pid),
            Some(&self.title),
        )
    }

    /// Like `game_mode_state`, but also applies the daemon's current
    /// global/per-app_id overrides (`keymux gamemode`), so callers like
    /// `keymux debug` show what's actually active, not just the raw
    /// heuristic guess.
    pub fn effective_game_mode_state(
        &self,
        global_override: GlobalOverride,
        window_overrides: &HashMap<String, WindowOverride>,
    ) -> GameModeState {
        crate::niri::gamemode_detection::resolve_effective_game_mode(
            Some(&self.app_id),
            Some(self.pid),
            Some(&self.title),
            global_override,
            window_overrides,
        )
    }
}

/// Get all windows from niri
pub fn get_all_windows() -> Result<Vec<Window>, String> {
    let output = Command::new("niri")
        .args(["msg", "windows"])
        .output()
        .map_err(|e| format!("Failed to run niri: {}", e))?;

    if !output.status.success() {
        return Err("niri command failed".to_string());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    parse_niri_windows(&text)
}

/// Parse niri windows output
fn parse_niri_windows(text: &str) -> Result<Vec<Window>, String> {
    let mut windows = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();

        // Look for window header: "Window ID <id>: (focused)"
        if let Some(window_start) = line.strip_prefix("Window ID ") {
            let parts: Vec<&str> = window_start.split(':').collect();
            if !parts.is_empty() {
                let id_part = parts[0].trim();
                if let Ok(id) = id_part.parse::<u32>() {
                    let is_focused = line.contains("(focused)");

                    // Parse window properties
                    let mut window = Window {
                        id,
                        title: String::new(),
                        app_id: String::new(),
                        pid: 0,
                        is_floating: false,
                        is_focused,
                    };

                    // Look ahead for properties
                    let mut j = i + 1;
                    while j < lines.len() && lines[j].starts_with("  ") {
                        let prop_line = lines[j].trim();

                        if let Some(title) = prop_line.strip_prefix("Title: ") {
                            window.title = title.trim_matches('"').to_string();
                        } else if let Some(app_id) = prop_line.strip_prefix("App ID: ") {
                            window.app_id = app_id.trim_matches('"').to_string();
                        } else if let Some(pid_str) = prop_line.strip_prefix("PID: ") {
                            window.pid = pid_str.parse().unwrap_or(0);
                        } else if prop_line == "Is floating: yes" {
                            window.is_floating = true;
                        }

                        j += 1;
                    }

                    windows.push(window);
                    i = j - 1; // Adjust for outer loop increment
                }
            }
        }

        i += 1;
    }

    Ok(windows)
}

/// Get terminal width for responsive formatting
pub fn get_terminal_width() -> usize {
    match crossterm::terminal::size() {
        Ok((width, _)) => width as usize,
        Err(_) => 80, // Default width
    }
}
