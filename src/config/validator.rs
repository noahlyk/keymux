//! `keymux validate`: load a config and show every check [`Config::validate`] makes, the
//! same checks the daemon makes before it takes a changed config.

use crate::config::{Config, DictionaryRoots};
use anyhow::Result;
use colored::Colorize;
use std::path::Path;

/// With `strict`, warnings (the config's and the dictionaries') fail the validation too.
pub fn validate_config(config_path: Option<&Path>, strict: bool) -> Result<()> {
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

    let mut errors: Vec<String> = report.errors().map(str::to_string).collect();
    let mut warnings: Vec<String> = report.warnings().map(str::to_string).collect();

    print!("  {} Checking steno dictionaries... ", "→".bright_blue());
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let targets = crate::steno::validate::targets_from_config(&config, config_dir, home.as_deref());
    if targets.is_empty() {
        println!("{}", "✓".bright_green().bold());
    } else {
        use crate::steno::validate::Severity;
        let found = crate::steno::validate_cli::run(&targets, config_dir);
        let bad = found.count(Severity::Error);
        let summary = format!("{} files, {} entries", found.files, found.entries);
        if bad > 0 {
            println!("{}", "✗".bright_red().bold());
        } else {
            println!("{} {summary}", "✓".bright_green().bold());
        }
        for finding in &found.findings {
            let line = format!(
                "dictionary {}{}: {}{}",
                finding.file,
                finding.outline.as_ref().map_or_else(String::new, |outline| format!(" \"{outline}\"")),
                finding.message,
                finding.suggestion.as_ref().map_or_else(String::new, |s| format!(" (suggestion: {s})")),
            );
            match finding.severity {
                Severity::Error => errors.push(line),
                Severity::Warning => warnings.push(line),
                Severity::Info => {}
            }
        }
    }
    if strict {
        errors.append(&mut warnings);
    }
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
