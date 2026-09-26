use anyhow::Result;
use colored::Colorize;

use keymux::config::Config;
use keymux::daemon::DaemonDisplay;
use keymux::gamemode_state::GlobalOverride;
use keymux::ipc::{send_request, IpcRequest, IpcResponse};
use keymux::ui::display::{
    ConfigDisplay, DeviceDisplay, KeyboardDisplay, PermissionsDisplay, SessionDisplay,
};
use keymux::ui::window::{get_all_windows, GameModeState};
use std::collections::HashMap;

pub fn run_debug(config_path: Option<&std::path::Path>) -> Result<()> {
    let config_path = if let Some(p) = config_path {
        p.to_path_buf()
    } else {
        Config::default_path()?
    };
    println!();
    println!("{}", "═════════════════════════════".bright_cyan());
    println!("  {}", "Debug Information".bright_cyan().bold());
    println!();
    println!("{}", "🧵 Process Info:".bright_yellow().bold());
    println!("  PID: {}", std::process::id().to_string().bright_white());
    println!("  Thread ID: {:?}", std::thread::current().id());

    let (actual_uid, is_sudo) = keymux::get_actual_user_uid();

    if is_sudo {
        println!(
            "  User: {} {}",
            std::env::var("SUDO_USER")
                .unwrap_or_else(|_| "unknown".to_string())
                .bright_white(),
            format!("(UID: {})", actual_uid).dimmed()
        );
        println!("  Running with: {}", "sudo".bright_yellow());
    } else {
        println!(
            "  User: {}",
            std::env::var("USER")
                .unwrap_or_else(|_| "unknown".to_string())
                .bright_white()
        );
        println!(
            "  UID: {}",
            unsafe { libc::getuid() }.to_string().bright_white()
        );
    }
    println!();

    // Config info
    ConfigDisplay::new(config_path).print_config_info();

    // Device watching info
    DeviceDisplay::new().print_device_watching();

    // Keyboard mapping info
    KeyboardDisplay::new().print_keyboard_mapping();

    // Permissions info
    PermissionsDisplay::new().print_permissions_info();

    // Daemon status
    DaemonDisplay::new().print_daemon_status();

    println!();

    // Window info
    println!("{}", "🪟 Window Info:".bright_yellow().bold());

    // Best-effort: pull the daemon's current gamemode overrides so the table
    // reflects what's actually active, not just the raw heuristic guess. If
    // the daemon isn't reachable, fall back to no overrides (heuristic-only
    // display) rather than failing the whole command.
    let global_override = match send_request(&IpcRequest::GetGlobalOverride) {
        Ok(IpcResponse::GlobalOverrideStatus(state)) => state,
        _ => GlobalOverride::default(),
    };
    let window_overrides: HashMap<_, _> = match send_request(&IpcRequest::ListWindowOverrides) {
        Ok(IpcResponse::WindowOverrides(list)) => list.into_iter().collect(),
        _ => HashMap::new(),
    };

    match get_all_windows() {
        Ok(windows) => {
            let terminal_width = keymux::ui::window::get_terminal_width();

            println!(
                "  Total Windows: {}",
                windows.len().to_string().bright_blue()
            );
            println!();

            // Calculate required width for table format
            let windows_with_gamemode: Vec<_> = windows
                .iter()
                .map(|w| {
                    (
                        w,
                        w.effective_game_mode_state(global_override, &window_overrides),
                    )
                })
                .collect();

            // Calculate column widths based on content
            let mut max_id_width = 6;
            let mut max_title_width = 5; // "Title"
            let mut max_app_id_width = 7; // "App ID"
            let mut max_pid_width = 3; // "PID"
            let max_focused_width = 7; // "Focused"
            let mut max_game_mode_width = 9; // "Game Mode"

            for (window, game_state) in &windows_with_gamemode {
                max_id_width = max_id_width.max(window.id.to_string().len());
                max_title_width = max_title_width.min(20).max(window.title.len().min(20));
                max_app_id_width = max_app_id_width.max(window.app_id.len().min(15));
                max_pid_width = max_pid_width.max(window.pid.to_string().len());

                let game_mode_len = match game_state {
                    GameModeState::Normal => 6,                          // "Normal"
                    GameModeState::GameMode(reason) => reason.len() + 2, // "✓ " + reason
                };
                max_game_mode_width = max_game_mode_width.max(game_mode_len);
            }

            // Add spacing between columns (2 spaces)
            let total_table_width = max_id_width
                + max_title_width
                + max_app_id_width
                + max_pid_width
                + max_focused_width
                + max_game_mode_width
                + 10; // 2 spaces * 5 gaps

            // Determine if table fits
            let use_table_format = total_table_width + 4 <= terminal_width; // 4 for "  " margin

            if use_table_format && !windows.is_empty() {
                // Table format
                let title_header = "Title".bright_white().bold();
                let app_id_header = "App ID".bright_white().bold();
                let focused_header = "Focused".bright_white().bold();
                let game_mode_header = "Game Mode".bright_white().bold();

                // Header row
                println!(
                    "  {:<width_id$}  {:<width_title$}  {:<width_app$}  {:<width_pid$}  {:<width_focused$}  {:<width_game$}",
                    "ID".bright_white().bold(),
                    title_header,
                    app_id_header,
                    "PID".bright_white().bold(),
                    focused_header,
                    game_mode_header,
                    width_id = max_id_width,
                    width_title = max_title_width,
                    width_app = max_app_id_width,
                    width_pid = max_pid_width,
                    width_focused = max_focused_width,
                    width_game = max_game_mode_width
                );

                // Separator line
                let separator = "─".repeat(total_table_width);
                println!("  {}", separator.dimmed());

                // Data rows
                for (window, game_state) in &windows_with_gamemode {
                    let focused = if window.is_focused { "✓" } else { "○" };
                    let game_mode = match game_state {
                        GameModeState::Normal => "○ Normal".dimmed().to_string(),
                        GameModeState::GameMode(reason) => {
                            format!("✓ {}", reason.bright_green())
                        }
                    };

                    let title = if window.title.len() > max_title_width {
                        format!("{}...", &window.title[..max_title_width.saturating_sub(3)])
                    } else {
                        window.title.clone()
                    };

                    println!(
                        "  {:<width_id$}  {:<width_title$}  {:<width_app$}  {:<width_pid$}  {:<width_focused$}  {:<width_game$}",
                        window.id.to_string().bright_white(),
                        title,
                        window.app_id,
                        window.pid.to_string(),
                        focused,
                        game_mode,
                        width_id = max_id_width,
                        width_title = max_title_width,
                        width_app = max_app_id_width,
                        width_pid = max_pid_width,
                        width_focused = max_focused_width,
                        width_game = max_game_mode_width
                    );
                }
            } else {
                // Paragraph format for narrow terminals or no windows
                for window in &windows {
                    let game_state =
                        window.effective_game_mode_state(global_override, &window_overrides);
                    let game_info = match game_state {
                        GameModeState::Normal => "○ Normal".to_string(),
                        GameModeState::GameMode(reason) => {
                            format!("✓ {}", reason.bright_green())
                        }
                    };

                    println!("  Window ID: {}", window.id.to_string().bright_white());
                    println!("    Title: {}", window.title.bright_white());
                    println!("    App ID: {}", window.app_id.bright_white());
                    println!("    PID: {}", window.pid.to_string().bright_white());
                    println!("    Game Mode: {}", game_info);
                    println!();
                }
            }
        }
        Err(e) => {
            println!("  {}", format!("✗ Error: {}", e).bright_red());
        }
    }

    println!();
    println!("{}", "👤 User Sessions:".bright_yellow().bold());

    SessionDisplay::new().print_user_sessions();

    println!();
    println!("{}", "═════════════════════════════".bright_cyan());
    println!();

    Ok(())
}
