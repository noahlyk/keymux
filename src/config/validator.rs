//! `keymux validate`: load a config and show every check [`Config::validate`] makes, the
//! same checks the daemon makes before it takes a changed config.

use crate::config::{Config, DictionaryRoots};
use anyhow::Result;
use colored::Colorize;
use std::path::Path;

pub fn validate_config(config_path: Option<&Path>) -> Result<()> {
    let rule = "═══════════════════════════════════════".bright_cyan();
    println!();
    println!("{rule}");
    println!("  {}", "Config Validation".bright_cyan().bold());
    println!("{rule}");
    println!();

    let config_path = match config_path {
        Some(path) => path.to_path_buf(),
        None => Config::default_path()?,
    };
    println!(
        "  {} {}",
        "Config file:".bright_yellow(),
        config_path.display().to_string().dimmed()
    );
    println!();

    print!("  {} Loading config... ", "→".bright_blue());
    let config = match Config::load_or_hint(&config_path) {
        Ok(config) => {
            println!("{}", "✓".bright_green().bold());
            config
        }
        Err(e) => {
            println!("{}", "✗".bright_red().bold());
            println!();
            println!("  {} {e}", "Error:".bright_red().bold());
            println!();
            return Err(e);
        }
    };

    let (uid, _) = crate::get_actual_user_uid();
    let home = crate::get_user_home_dir(uid).ok();
    let roots = DictionaryRoots {
        config_dir: config_path.parent().unwrap_or_else(|| Path::new(".")),
        home: home.as_deref(),
    };
    let report = config.validate(Some(roots));
    for check in &report.checks {
        print!("  {} Checking {}... ", "→".bright_blue(), check.name);
        if !check.errors.is_empty() {
            println!("{}", "✗".bright_red().bold());
        } else if check.summary.is_empty() {
            println!("{}", "✓".bright_green().bold());
        } else {
            println!("{} {}", "✓".bright_green().bold(), check.summary);
        }
    }

    let errors: Vec<&str> = report.errors().collect();
    let warnings: Vec<&str> = report.warnings().collect();
    println!();
    println!("{rule}");
    if errors.is_empty() && warnings.is_empty() {
        println!(
            "  {} {}",
            "✓".bright_green().bold(),
            "Config is valid!".bright_green()
        );
    }
    if !errors.is_empty() {
        println!(
            "  {} {}",
            "✗".bright_red().bold(),
            format!("{} error(s)", errors.len()).bright_red()
        );
        for error in &errors {
            println!("    {} {error}", "•".bright_red());
        }
    }
    if !warnings.is_empty() {
        println!(
            "  {} {}",
            "!".bright_yellow().bold(),
            format!("{} warning(s)", warnings.len()).bright_yellow()
        );
        for warning in &warnings {
            println!("    {} {warning}", "•".bright_yellow());
        }
    }
    println!("{rule}");
    println!();

    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Config validation failed with {} error(s)",
            errors.len()
        ))
    }
}
