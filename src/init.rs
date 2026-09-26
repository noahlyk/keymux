use anyhow::{Context, Result};
use colored::Colorize;
use keymux::config::Config;

// Bundled at compile time so `keymux init` works no matter how the binary
// was installed (cargo build, AUR package, ...) - it never depends on a
// runtime path to a docs directory that may or may not exist.
const MINIMAL_CONFIG: &str = include_str!("../config.minimal.ron");
const FULL_EXAMPLE_CONFIG: &str = include_str!("../config.example.ron");

pub fn run_init(full: bool, force: bool) -> Result<()> {
    let path = Config::default_path()?;

    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Initialize Config".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    if path.exists() && !force {
        println!(
            "  {} Config already exists: {}",
            "✗".bright_red().bold(),
            path.display().to_string().bright_white()
        );
        println!();
        println!(
            "  {} Pass {} to overwrite it (this discards your current config).",
            "Tip:".bright_yellow().bold(),
            "--force".bright_white()
        );
        println!();
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }

    let content = if full { FULL_EXAMPLE_CONFIG } else { MINIMAL_CONFIG };
    std::fs::write(&path, content)
        .with_context(|| format!("Failed to write config: {}", path.display()))?;

    println!(
        "  {} Created {} config: {}",
        "✓".bright_green().bold(),
        if full { "full-featured" } else { "minimal" },
        path.display().to_string().bright_white()
    );
    println!();
    println!("  {}", "Next steps:".bright_white().bold());
    println!(
        "    {}  {}",
        "keymux validate".bright_cyan(),
        "check the config for errors".dimmed()
    );
    println!(
        "    {}  {}",
        "keymux list".bright_cyan(),
        "see detected keyboards".dimmed()
    );
    println!(
        "    {}  {}",
        "sudo systemctl restart keymux".bright_cyan(),
        "apply it, if the daemon is already running".dimmed()
    );
    println!();

    Ok(())
}
