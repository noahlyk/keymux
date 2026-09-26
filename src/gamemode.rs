use crate::cli::{GamemodeAction, GlobalGamemodeAction, WindowGamemodeAction};
use anyhow::Result;
use colored::Colorize;
use keymux::gamemode_state::{GlobalOverride, WindowOverride};
use keymux::ipc::{send_request, IpcRequest, IpcResponse};

pub fn handle_gamemode_action(action: &GamemodeAction) -> Result<()> {
    match action {
        GamemodeAction::Window { action } => handle_window_gamemode_action(action),
        GamemodeAction::Global { action } => handle_global_gamemode_action(action),
    }
}

/// Resolve which app_id a window-level command should target: the explicit
/// argument if given, otherwise whatever window the daemon last saw
/// focused (no compositor-specific code needed here at all - the daemon
/// already knows, from whichever watcher process is running).
fn resolve_app_id(explicit: &Option<String>) -> Result<Option<String>> {
    if let Some(id) = explicit {
        return Ok(Some(id.clone()));
    }
    match send_request(&IpcRequest::GetFocusedWindow)? {
        IpcResponse::FocusedWindow(window) => Ok(window.app_id),
        _ => Ok(None),
    }
}

pub fn handle_window_gamemode_action(action: &WindowGamemodeAction) -> Result<()> {
    if matches!(action, WindowGamemodeAction::List) {
        return print_window_overrides();
    }

    let app_id_arg = match action {
        WindowGamemodeAction::On { app_id }
        | WindowGamemodeAction::Off { app_id }
        | WindowGamemodeAction::Toggle { app_id }
        | WindowGamemodeAction::Auto { app_id } => app_id,
        WindowGamemodeAction::List => unreachable!("handled above"),
    };

    let app_id = match resolve_app_id(app_id_arg) {
        Ok(Some(id)) => id,
        Ok(None) => {
            println!(
                "  {} Could not determine the focused window's app_id (no watcher running, or nothing focused). Pass an app_id explicitly.",
                "✗".bright_red()
            );
            return Ok(());
        }
        Err(e) => {
            print_daemon_unreachable(&e);
            return Ok(());
        }
    };

    let request = match action {
        WindowGamemodeAction::On { .. } => IpcRequest::SetWindowOverride {
            app_id: app_id.clone(),
            state: Some(WindowOverride::On),
        },
        WindowGamemodeAction::Off { .. } => IpcRequest::SetWindowOverride {
            app_id: app_id.clone(),
            state: Some(WindowOverride::Off),
        },
        WindowGamemodeAction::Auto { .. } => IpcRequest::SetWindowOverride {
            app_id: app_id.clone(),
            state: None,
        },
        WindowGamemodeAction::Toggle { .. } => IpcRequest::ToggleWindowOverride {
            app_id: app_id.clone(),
        },
        WindowGamemodeAction::List => unreachable!("handled above"),
    };

    match send_request(&request) {
        Ok(IpcResponse::Ok) => {
            let label = match action {
                WindowGamemodeAction::On { .. } => "forced ON".green().bold().to_string(),
                WindowGamemodeAction::Off { .. } => "forced OFF".red().bold().to_string(),
                WindowGamemodeAction::Toggle { .. } => "toggled".cyan().bold().to_string(),
                WindowGamemodeAction::Auto { .. } => {
                    "reset to automatic detection".dimmed().to_string()
                }
                WindowGamemodeAction::List => unreachable!("handled above"),
            };
            println!(
                "  {} {}: {}",
                "✓".bright_green().bold(),
                app_id.bright_white(),
                label
            );
        }
        Ok(other) => print_unexpected_response(&other),
        Err(e) => print_daemon_unreachable(&e),
    }

    Ok(())
}

fn print_window_overrides() -> Result<()> {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Window Game Mode Overrides".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    match send_request(&IpcRequest::ListWindowOverrides) {
        Ok(IpcResponse::WindowOverrides(overrides)) => {
            if overrides.is_empty() {
                println!(
                    "  {}",
                    "(none - everything is using automatic detection)".dimmed()
                );
            } else {
                for (app_id, state) in overrides {
                    let label = match state {
                        WindowOverride::On => "ON".green().bold(),
                        WindowOverride::Off => "OFF".red().bold(),
                    };
                    println!("  {} {}", app_id.bright_white(), label);
                }
            }
        }
        Ok(other) => print_unexpected_response(&other),
        Err(e) => print_daemon_unreachable(&e),
    }
    println!();
    Ok(())
}

pub fn handle_global_gamemode_action(action: &GlobalGamemodeAction) -> Result<()> {
    if matches!(action, GlobalGamemodeAction::Status) {
        return print_global_status();
    }

    let state = match action {
        GlobalGamemodeAction::AlwaysOn => GlobalOverride::AlwaysOn,
        GlobalGamemodeAction::AlwaysOff => GlobalOverride::AlwaysOff,
        GlobalGamemodeAction::Auto => GlobalOverride::Auto,
        GlobalGamemodeAction::Status => unreachable!("handled above"),
    };

    match send_request(&IpcRequest::SetGlobalOverride(state)) {
        Ok(IpcResponse::Ok) => {
            println!(
                "  {} Global game mode: {}",
                "✓".bright_green().bold(),
                describe_global(state)
            );
        }
        Ok(other) => print_unexpected_response(&other),
        Err(e) => print_daemon_unreachable(&e),
    }
    Ok(())
}

fn print_global_status() -> Result<()> {
    match send_request(&IpcRequest::GetGlobalOverride) {
        Ok(IpcResponse::GlobalOverrideStatus(state)) => {
            println!("  Global game mode: {}", describe_global(state));
        }
        Ok(other) => print_unexpected_response(&other),
        Err(e) => print_daemon_unreachable(&e),
    }
    Ok(())
}

fn describe_global(state: GlobalOverride) -> String {
    match state {
        GlobalOverride::AlwaysOn => "ALWAYS ON".green().bold().to_string(),
        GlobalOverride::AlwaysOff => "ALWAYS OFF".red().bold().to_string(),
        GlobalOverride::Auto => "auto (per-window rules apply)".dimmed().to_string(),
    }
}

fn print_unexpected_response(response: &IpcResponse) {
    println!(
        "  {} {}",
        "⚠".bright_yellow(),
        format!("Unexpected response from daemon: {:?}", response).yellow()
    );
}

fn print_daemon_unreachable(e: &anyhow::Error) {
    println!(
        "  {} {}",
        "⚠".bright_yellow(),
        format!("Daemon not running ({e})").yellow()
    );
    println!(
        "  {} Start it with: {}",
        "Tip:".bright_yellow().bold(),
        "sudo systemctl start keymux".dimmed()
    );
}
