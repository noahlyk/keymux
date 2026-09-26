use anyhow::Result;
use colored::Colorize;
use dialoguer::MultiSelect;
use std::collections::HashMap;

use keymux::config::{Config, EnableDisable, EnabledKeyboardEntry, EnabledKeyboards};
use keymux::ipc::{send_request, IpcRequest, IpcResponse};
use keymux::keyboard_id::{find_all_keyboards, KeyboardId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToggleAction {
    Toggle,
    Enable,
    Disable,
}

pub fn run_toggle(multi: bool, action_patterns: Option<(bool, Vec<String>)>) -> Result<()> {
    // Load config and keyboards
    let config_path = Config::default_path()?;
    let mut config = Config::load_or_hint(&config_path)?;
    let keyboards = find_all_keyboards();

    if keyboards.is_empty() {
        println!();
        println!(
            "{}",
            "═══════════════════════════════════════".bright_cyan()
        );
        println!("  {}", "No Keyboards Found".bright_cyan().bold());
        println!(
            "{}",
            "═══════════════════════════════════════".bright_cyan()
        );
        println!();
        println!(
            "  {} {}",
            "✗".bright_red().bold(),
            "No keyboards found!".red()
        );
        println!();
        return Ok(());
    }

    // Build list of keyboard items
    let mut items: Vec<(KeyboardId, String)> = keyboards
        .into_iter()
        .map(|(id, logical_kb)| (id, logical_kb.name))
        .collect();
    items.sort_by(|a, b| a.1.cmp(&b.1));

    // Handle CLI patterns (non-multi mode)
    if let Some((enable, patterns)) = action_patterns {
        if !multi && !patterns.is_empty() {
            return handle_cli_patterns(&mut config, &config_path, enable, patterns, &items);
        }
        // If multi flag is set OR patterns are empty, run the appropriate multi-select
        let action = if enable {
            ToggleAction::Enable
        } else {
            ToggleAction::Disable
        };
        return run_multi_select(&mut config, &config_path, &items, action);
    }

    // Multi-select mode for toggle (no args)
    run_multi_select(&mut config, &config_path, &items, ToggleAction::Toggle)
}

fn handle_cli_patterns(
    config: &mut Config,
    config_path: &std::path::Path,
    enable: bool,
    patterns: Vec<String>,
    items: &[(KeyboardId, String)],
) -> Result<()> {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!(
        "  {}",
        if enable {
            "Enable Keyboards".bright_cyan().bold()
        } else {
            "Disable Keyboards".bright_cyan().bold()
        }
    );
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    // Get current entries (normalize to handle legacy Some* variants)
    let current_entries = match config.enabled_keyboards.normalize() {
        EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => vec![],
        EnabledKeyboards::List(entries) | EnabledKeyboards::SomeList(entries) => entries,
    };

    let mut new_entries: Vec<EnabledKeyboardEntry> = Vec::new();
    let mut matched_any = false;
    let mut pattern_comments: HashMap<String, String> = HashMap::new();

    for pattern in &patterns {
        let entry = if *pattern == "*" {
            // "*" is always just "*" (no action suffix needed for enable, but we add Disable for disable)
            if enable {
                EnabledKeyboardEntry::Bare("*".to_string())
            } else {
                EnabledKeyboardEntry::Explicit("*".to_string(), EnableDisable::Disable)
            }
        } else {
            // Normalize event path patterns: "event29" -> "event29", "/dev/input/event29" -> "event29"
            let normalized_pattern = pattern
                .strip_prefix("/dev/input/")
                .unwrap_or(pattern)
                .to_string();

            // Check if this pattern matches any keyboard and record the name
            for (id, name) in items {
                let id_str = id.to_string();
                let base_id = id_str.split('@').next().unwrap_or(&id_str);
                if base_id.contains(&normalized_pattern)
                    || base_id.contains(pattern)
                    || name.contains(pattern)
                {
                    matched_any = true;
                    // Store the base_id (without @...) as the key for comment
                    pattern_comments.insert(pattern.clone(), name.clone());
                }
            }

            if enable {
                EnabledKeyboardEntry::Bare(pattern.clone())
            } else {
                EnabledKeyboardEntry::Explicit(pattern.clone(), EnableDisable::Disable)
            }
        };
        new_entries.push(entry);
    }

    if !matched_any && !patterns.contains(&"*".to_string()) {
        println!(
            "  {} Note: No currently connected keyboards matched: {}",
            "⚠".bright_yellow(),
            patterns.join(", ")
        );
        println!(
            "  {} The pattern will still be added and may match when keyboard is connected.",
            "ℹ".bright_blue()
        );
        println!();
    }

    // Merge and deduplicate based on action
    let final_entries = if enable {
        if patterns.contains(&"*".to_string()) {
            // "*" means enable all - replace with just "*"
            new_entries
        } else {
            // Merge and deduplicate
            merge_and_deduplicate(&current_entries, &new_entries, EnableDisable::Enable)
        }
    } else {
        // For disable, always merge
        merge_and_deduplicate(&current_entries, &new_entries, EnableDisable::Disable)
    };

    config.enabled_keyboards = if final_entries.is_empty() {
        EnabledKeyboards::ExplicitNone
    } else {
        EnabledKeyboards::List(final_entries)
    };

    // Save and notify
    config.save_enabled_keyboards_only_with_comments(config_path, Some(&pattern_comments))?;
    send_reload_notification()?;

    let status = if enable { "enabled" } else { "disabled" };
    println!(
        "  {} {}: {}",
        "✓".bright_green().bold(),
        "Keyboards".green(),
        status.green()
    );
    for pattern in &patterns {
        println!("    - {}", pattern.bright_white());
    }
    println!();

    Ok(())
}

pub fn handle_toggle_patterns(
    config: &mut Config,
    config_path: &std::path::Path,
    patterns: Vec<String>,
    items: &[(KeyboardId, String)],
) -> Result<()> {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Toggle Keyboards".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    // Get current enabled keyboards
    let current_entries = match config.enabled_keyboards.normalize() {
        EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => vec![],
        EnabledKeyboards::List(entries) | EnabledKeyboards::SomeList(entries) => entries,
    };

    // Determine which patterns to enable vs disable
    let mut enable_patterns: Vec<String> = Vec::new();
    let mut disable_patterns: Vec<String> = Vec::new();
    let pattern_comments: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for pattern in &patterns {
        // Check if this pattern matches any keyboard and determine current state
        let mut is_currently_enabled = false;
        let mut matched_name = None;

        for (id, name) in items {
            let id_str = id.to_string();
            let base_id = id_str.split('@').next().unwrap_or(&id_str);
            if base_id.contains(pattern) || name.contains(pattern) {
                // Check if this keyboard is currently enabled
                // More specific patterns take precedence over "*"
                let mut found_specific = false;
                for entry in &current_entries {
                    let entry_pattern = entry.pattern();
                    if entry_pattern == "*" {
                        // Only use "*" if we haven't found a more specific pattern
                        if !found_specific {
                            is_currently_enabled = entry.action() == EnableDisable::Enable;
                        }
                    } else if base_id.contains(entry_pattern) {
                        // Found a specific pattern for this keyboard
                        is_currently_enabled = entry.action() == EnableDisable::Enable;
                        found_specific = true;
                        matched_name = Some(name.clone());
                    }
                }
                if matched_name.is_some() {
                    break;
                }
            }
        }

        if is_currently_enabled {
            disable_patterns.push(pattern.clone());
        } else {
            enable_patterns.push(pattern.clone());
        }
    }

    // Handle enable patterns if any
    if !enable_patterns.is_empty() {
        let enable_entries: Vec<EnabledKeyboardEntry> = enable_patterns
            .iter()
            .map(|p| EnabledKeyboardEntry::Bare(p.clone()))
            .collect();

        let final_entries =
            merge_and_deduplicate(&current_entries, &enable_entries, EnableDisable::Enable);

        config.enabled_keyboards = if final_entries.is_empty() {
            EnabledKeyboards::ExplicitNone
        } else {
            EnabledKeyboards::List(final_entries)
        };

        config.save_enabled_keyboards_only_with_comments(config_path, Some(&pattern_comments))?;
        send_reload_notification()?;

        println!(
            "  {} {}: {}",
            "✓".bright_green().bold(),
            "Enabled".green(),
            enable_patterns.join(", ").green()
        );
    }

    // Handle disable patterns if any
    if !disable_patterns.is_empty() {
        let disable_entries: Vec<EnabledKeyboardEntry> = disable_patterns
            .iter()
            .map(|p| EnabledKeyboardEntry::Explicit(p.clone(), EnableDisable::Disable))
            .collect();

        let current_after_enable = match config.enabled_keyboards.normalize() {
            EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => vec![],
            EnabledKeyboards::List(entries) | EnabledKeyboards::SomeList(entries) => entries,
        };

        let final_entries = merge_and_deduplicate(
            &current_after_enable,
            &disable_entries,
            EnableDisable::Disable,
        );

        config.enabled_keyboards = if final_entries.is_empty() {
            EnabledKeyboards::ExplicitNone
        } else {
            EnabledKeyboards::List(final_entries)
        };

        config.save_enabled_keyboards_only_with_comments(config_path, Some(&pattern_comments))?;
        send_reload_notification()?;

        println!(
            "  {} {}: {}",
            "✓".bright_green().bold(),
            "Disabled".red(),
            disable_patterns.join(", ").red()
        );
    }

    println!();
    Ok(())
}

fn run_multi_select(
    config: &mut Config,
    config_path: &std::path::Path,
    items: &[(KeyboardId, String)],
    action: ToggleAction,
) -> Result<()> {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!(
        "  {}",
        match action {
            ToggleAction::Toggle => "Keyboard Configuration".bright_cyan().bold(),
            ToggleAction::Enable => "Enable Keyboards".bright_cyan().bold(),
            ToggleAction::Disable => "Disable Keyboards".bright_cyan().bold(),
        }
    );
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    // Show current status
    println!("  {}", "Current Status:".bright_white().bold());
    println!();

    let mut has_enabled = false;
    let mut has_disabled = false;

    for (id, name) in items {
        let is_enabled = config.is_keyboard_enabled(&id.to_string(), Some(name), None);
        if is_enabled {
            println!("    {} {}", "✓".bright_green(), name.green());
            has_enabled = true;
        }
    }

    if !has_enabled {
        println!("    {}", "(none enabled)".dimmed());
    }

    println!();

    for (id, name) in items {
        let is_enabled = config.is_keyboard_enabled(&id.to_string(), Some(name), None);
        if !is_enabled {
            println!("    {} {}", "○".dimmed(), name.dimmed());
            has_disabled = true;
        }
    }

    if !has_disabled {
        println!("    {}", "(all keyboards enabled)".dimmed());
    }

    println!();
    println!("{}", "  ─────────────────────────────────────".dimmed());
    println!();

    // Build display items - add "*" at the top
    let mut display_items: Vec<String> = vec!["* (all keyboards)".to_string()];
    display_items.extend(items.iter().map(|(_, name)| name.clone()));

    // Determine pre-selected state based on action
    let defaults: Vec<bool> = match action {
        ToggleAction::Toggle => {
            // v1.0.12 behavior: pre-select currently enabled keyboards
            std::iter::once(true) // "*" selected by default
                .chain(items.iter().map(|(id, name)| {
                    config.is_keyboard_enabled(&id.to_string(), Some(name), None)
                }))
                .collect()
        }
        ToggleAction::Enable => {
            // Start with nothing selected (gray), select to add enables
            std::iter::once(false) // "*" not selected
                .chain(items.iter().map(|_| false))
                .collect()
        }
        ToggleAction::Disable => {
            // Start with nothing selected (gray), select to add disables
            std::iter::once(false) // "*" not selected
                .chain(items.iter().map(|_| false))
                .collect()
        }
    };

    println!(
        "  {}",
        match action {
            ToggleAction::Toggle => "Select keyboards to enable:".bright_white(),
            ToggleAction::Enable => "Select keyboards to add to enabled:".bright_white(),
            ToggleAction::Disable => "Select keyboards to add to disabled:".bright_white(),
        }
    );
    println!("  {}", "(Space to toggle, Enter to confirm)".dimmed());
    println!();

    // Show multi-select dialog
    use console::Style;
    use dialoguer::theme::ColorfulTheme;

    let theme = ColorfulTheme {
        checked_item_prefix: Style::new().green().apply_to("✓".to_string()),
        unchecked_item_prefix: Style::new().dim().apply_to("○".to_string()),
        active_item_prefix: Style::new().cyan().apply_to(">".to_string()),
        inactive_item_prefix: Style::new().apply_to(" ".to_string()),
        active_item_style: Style::new().cyan(),
        ..ColorfulTheme::default()
    };

    let selections = MultiSelect::with_theme(&theme)
        .items(&display_items)
        .defaults(&defaults)
        .interact()?;

    // Get current entries for merging
    let current_entries: Vec<EnabledKeyboardEntry> = match config.enabled_keyboards.normalize() {
        EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => vec![],
        EnabledKeyboards::List(entries) | EnabledKeyboards::SomeList(entries) => entries,
    };

    // Handle "*" selection
    let has_star = selections.contains(&0);

    // Check if any keyboards were selected (before moving into the match)
    let has_any_selected = selections.iter().any(|&i| i > 0);

    // Build new entries based on action
    let new_entries: Vec<EnabledKeyboardEntry> = selections
        .iter()
        .filter(|&&i| i > 0) // Skip "*" (index 0)
        .map(|&i| {
            let (id, _) = &items[i - 1]; // Adjust for "*" offset
            let id_str = id.to_string();
            let base_id = id_str.split('@').next().unwrap_or(&id_str);
            match action {
                ToggleAction::Toggle | ToggleAction::Enable => {
                    EnabledKeyboardEntry::Bare(base_id.to_string())
                }
                ToggleAction::Disable => {
                    EnabledKeyboardEntry::Explicit(base_id.to_string(), EnableDisable::Disable)
                }
            }
        })
        .collect();

    // Build comments mapping for multi-select
    let mut pattern_comments: HashMap<String, String> = HashMap::new();
    for (id, name) in items {
        let id_str = id.to_string();
        let base_id = id_str.split('@').next().unwrap_or(&id_str).to_string();
        pattern_comments.insert(base_id, name.clone());
    }

    // Merge and deduplicate based on action
    let final_entries = match action {
        ToggleAction::Toggle => {
            if has_star {
                // "*" selected - enable all
                vec![EnabledKeyboardEntry::Bare("*".to_string())]
            } else if !has_any_selected {
                // Nothing selected - disable all
                vec![]
            } else {
                new_entries
            }
        }
        ToggleAction::Enable => {
            if has_star {
                // "*" selected - enable all (replace with just "*")
                vec![EnabledKeyboardEntry::Bare("*".to_string())]
            } else if !has_any_selected {
                // Nothing selected - keep existing
                current_entries
            } else {
                // Merge: remove older entries of same pattern, then append new ones
                merge_and_deduplicate(&current_entries, &new_entries, EnableDisable::Enable)
            }
        }
        ToggleAction::Disable => {
            if has_star {
                // "*" selected with disable - disable all (keep existing enables, add "*": Disable)
                let mut combined = current_entries;
                // Remove any existing "*" entries
                combined.retain(|e| e.pattern() != "*");
                combined.push(EnabledKeyboardEntry::Explicit(
                    "*".to_string(),
                    EnableDisable::Disable,
                ));
                combined
            } else if !has_any_selected {
                // Nothing selected - keep existing
                current_entries
            } else {
                // Merge: remove older entries of same pattern, then append new ones
                merge_and_deduplicate(&current_entries, &new_entries, EnableDisable::Disable)
            }
        }
    };

    // Set the final config
    config.enabled_keyboards = if final_entries.is_empty() {
        EnabledKeyboards::ExplicitNone
    } else {
        EnabledKeyboards::List(final_entries)
    };

    // Save config
    config.save_enabled_keyboards_only_with_comments(config_path, Some(&pattern_comments))?;

    // Send reload notification
    send_reload_notification()?;

    println!();
    println!(
        "  {} {}",
        "✓".bright_green().bold(),
        "Configuration saved!".green()
    );
    println!();

    // Show what was changed
    match action {
        ToggleAction::Toggle => {
            if has_star {
                println!("  {} All keyboards enabled", "✓".bright_green());
            } else if !has_any_selected {
                println!("  {} All keyboards disabled", "○".dimmed());
            } else {
                println!("  {}", "Enabled keyboards:".bright_white());
                for &i in selections.iter().filter(|&&i| i > 0) {
                    let (_, name) = &items[i - 1];
                    println!("    {} {}", "✓".bright_green(), name.green());
                }
            }
        }
        ToggleAction::Enable => {
            if has_star {
                println!("  {} All keyboards enabled", "✓".bright_green());
            } else if has_any_selected {
                println!("  {}", "Added to enabled:".bright_green());
                for &i in selections.iter().filter(|&&i| i > 0) {
                    let (_, name) = &items[i - 1];
                    println!("    + {}", name.green());
                }
            }
        }
        ToggleAction::Disable => {
            if has_star {
                println!("  {} All keyboards disabled", "○".dimmed());
            } else if has_any_selected {
                println!("  {}", "Added to disabled:".bright_red());
                for &i in selections.iter().filter(|&&i| i > 0) {
                    let (_, name) = &items[i - 1];
                    println!("    - {}", name.red());
                }
            }
        }
    }

    println!();

    Ok(())
}

fn merge_and_deduplicate(
    existing: &[EnabledKeyboardEntry],
    new_entries: &[EnabledKeyboardEntry],
    _action: EnableDisable,
) -> Vec<EnabledKeyboardEntry> {
    // Remove entries from existing that match patterns in new_entries
    let new_patterns: Vec<&str> = new_entries.iter().map(|e| e.pattern()).collect();
    let filtered: Vec<EnabledKeyboardEntry> = existing
        .iter()
        .filter(|e| !new_patterns.contains(&e.pattern()))
        .cloned()
        .collect();

    // Append new entries
    let mut result = filtered;
    result.extend(new_entries.iter().cloned());
    result
}

fn send_reload_notification() -> Result<()> {
    match send_request(&IpcRequest::ToggleKeyboards) {
        Ok(IpcResponse::Ok) => {}
        Ok(_) => {
            println!(
                "  {} {}",
                "⚠".bright_yellow(),
                "Unexpected response from daemon".yellow()
            );
        }
        Err(e) => {
            println!(
                "  {} {}",
                "⚠".bright_yellow(),
                format!("Daemon not running: {e}").yellow()
            );
            println!(
                "  {} Start it with: {}",
                "Tip:".bright_yellow().bold(),
                "sudo systemctl start keymux".dimmed()
            );
        }
    }
    Ok(())
}
