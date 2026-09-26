use anyhow::Result;

pub fn clear_adaptive_stats() -> Result<()> {
    use colored::Colorize;
    use std::io::{self, Write};

    println!();
    println!(
        "{}",
        "⚠ WARNING: This will delete ALL adaptive timing statistics!"
            .bright_red()
            .bold()
    );
    println!();
    print!("  Are you REALLY sure? [y/N]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();

    if input != "y" && input != "yes" {
        println!();
        println!("  {} Cancelled. No stats were deleted.", "✓".bright_green());
        println!();
        return Ok(());
    }

    let (uid, _) = keymux::get_actual_user_uid();
    let home = keymux::get_user_home_dir(uid).expect("Failed to get user home directory");
    let config_dir = home.join(".config").join("keymux");

    let mt_stats = config_dir.join("adaptive_stats.json");
    let all_stats = config_dir.join("all_key_stats.json");

    let mut deleted = 0;
    if mt_stats.exists() {
        std::fs::remove_file(&mt_stats)?;
        deleted += 1;
    }
    if all_stats.exists() {
        std::fs::remove_file(&all_stats)?;
        deleted += 1;
    }

    println!();
    if deleted > 0 {
        println!(
            "  {} Deleted {} stats file(s).",
            "✓".bright_green(),
            deleted
        );
    } else {
        println!("  {} No stats files found.", "ℹ".bright_blue());
    }
    println!();

    Ok(())
}

pub fn show_adaptive_stats(config_path: Option<&std::path::Path>) -> Result<()> {
    use colored::Colorize;
    use keymux::config::Config;
    use keymux::keycode::KeyCode;

    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Adaptive Timing Statistics".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    let config_path = config_path.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        let (uid, _) = keymux::get_actual_user_uid();
        let home = keymux::get_user_home_dir(uid).expect("Failed to get user home directory");
        home.join(".config").join("keymux").join("config.ron")
    });

    print!("  → Loading config... ");
    let config = Config::load_or_hint(&config_path)?;
    println!("{}", "✓".bright_green());

    print!("  → Requesting fresh stats from daemon... ");
    match keymux::ipc::send_request(&keymux::ipc::IpcRequest::SaveAdaptiveStats) {
        Ok(keymux::ipc::IpcResponse::Ok) => {
            println!("{}", "✓".bright_green());
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Ok(_) => {
            println!("{}", "⚠ unexpected response".bright_yellow());
        }
        Err(_) => {
            println!(
                "{}",
                "⚠ daemon not running (showing cached data)".bright_yellow()
            );
        }
    }

    if !config.mt_config.adaptive_timing {
        println!();
        println!(
            "  {} Adaptive timing is disabled in config",
            "!".bright_yellow()
        );
        println!("  Enable it with: mt_config: ( adaptive_timing: true, ... )");
        println!();
        return Ok(());
    }

    let all_stats_path = config_path.parent().unwrap().join("all_key_stats.json");
    let stats = if all_stats_path.exists() {
        let json = std::fs::read_to_string(&all_stats_path).unwrap_or_default();
        let stats_map: std::collections::HashMap<
            String,
            keymux::event_processor::actions::mt::RollingStats,
        > = serde_json::from_str(&json).unwrap_or_default();

        let mut result = Vec::new();
        for (key_str, stats) in stats_map {
            let key_json = format!("\"KC_{}\"", key_str);
            if let Ok(keycode) = serde_json::from_str::<KeyCode>(&key_json) {
                result.push((keycode, stats));
            }
        }
        result
    } else {
        Vec::new()
    };

    if stats.is_empty() {
        println!();
        println!(
            "  {} No adaptive statistics collected yet",
            "ℹ".bright_blue()
        );
        println!("  Start typing to build statistics!");
        println!();
        return Ok(());
    }

    println!();
    println!(
        "  Base: {}ms  │  Margin: {}ms",
        config.tapping_term_ms.to_string().bright_yellow(),
        config
            .mt_config
            .adaptive_target_margin_ms
            .to_string()
            .bright_yellow()
    );
    println!();

    let mut sorted_stats = stats;
    sorted_stats.sort_by_key(|(k, _)| format!("{:?}", k));

    println!("  ┌───────┬────────┬─────────┬──────────┐");
    println!(
        "  │ {:^5} │ {:^6} │ {:^7} │ {:^8} │",
        "Key".bright_white().bold(),
        "Samples".bright_white().bold(),
        "Avg(ms)".bright_white().bold(),
        "Thresh(ms)".bright_white().bold()
    );
    println!("  ├───────┼────────┼─────────┼──────────┤");

    for (keycode, key_stats) in sorted_stats {
        let key_name = format!("{:?}", keycode).replace("KC_", "");
        let samples = key_stats.tap_sample_count;
        let avg_tap = key_stats.avg_tap_duration;
        let threshold = key_stats.adaptive_threshold;

        println!(
            "  │ {:^5} │ {:^6} │ {:^7} │ {:^8} │",
            key_name.bright_cyan(),
            samples.to_string().bright_green(),
            format!("{:.1}", avg_tap).bright_blue(),
            format!("{:.1}", threshold).bright_yellow()
        );
    }

    println!("  └───────┴────────┴─────────┴──────────┘");

    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    Ok(())
}
