use anyhow::Result;
use colored::Colorize;

use keymux::config::Config;
use keymux::keyboard_id::{find_all_keyboards, KeyboardId};

/// Whether `keymux list` shows a keyboard as enabled, and the group it works as a member of.
/// A member of a group is used with it even when `enabled_keyboards` does not name it, unless
/// that names the member itself `Disable`.
fn keyboard_status<'a>(config: &'a Config, id: &str, name: &str) -> (bool, Option<&'a str>) {
    let group = config.active_group_of(id, Some(name), None);
    (
        group.is_some() || config.is_keyboard_enabled(id, Some(name), None),
        group,
    )
}

/// The ` (1 group of 2 keyboards)` note of the summary: the groups the enabled keyboards form
/// and how many keyboards they hold; empty without groups.
fn group_note(groups: &[&str]) -> String {
    let mut names: Vec<&str> = groups.to_vec();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return String::new();
    }
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    format!(
        " ({} of {})",
        plural(names.len(), "group"),
        plural(groups.len(), "keyboard")
    )
}

pub fn run_list() -> Result<()> {
    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Detected Keyboards".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    // Load current config to check enabled keyboards
    let config_path = Config::default_path()?;
    let config = Config::load_or_hint(&config_path)?;

    // Find all keyboards
    let keyboards = find_all_keyboards();

    if keyboards.is_empty() {
        println!(
            "  {} {}",
            "✗".bright_red().bold(),
            "No keyboards found!".red()
        );
        println!();
        println!("  {}", "Make sure you're in the 'input' group:".dimmed());
        println!("  {}", "sudo usermod -a -G input $USER".dimmed());
        println!("  {}", "(Then log out and back in)".dimmed());
        println!();
        return Ok(());
    }

    // Sort keyboards by name
    let mut items: Vec<(KeyboardId, String)> = keyboards
        .into_iter()
        .map(|(id, logical_kb)| (id, logical_kb.name))
        .collect();
    items.sort_by(|a, b| a.1.cmp(&b.1));

    println!(
        "  {}",
        format!("Found {} keyboard(s):", items.len())
            .bright_white()
            .bold()
    );
    println!();

    let mut grouped: Vec<&str> = Vec::new();
    let mut enabled_count = 0;
    for (id, name) in &items {
        let (is_enabled, group) = keyboard_status(&config, &id.to_string(), name);
        enabled_count += usize::from(is_enabled);
        grouped.extend(group);

        if is_enabled {
            println!("    {} {}", "✓".bright_green().bold(), name.green());
        } else {
            println!("    {} {}", "○".dimmed(), name.dimmed());
        }
        println!("      {} {}", "ID:".dimmed(), id.to_string().dimmed());
        if let Some(group) = group {
            println!("      {}", format!("In group \"{group}\"").dimmed());
        }
        println!();
    }

    println!("{}", "  ─────────────────────────────────────".dimmed());
    println!();

    // Summary
    let disabled_count = items.len() - enabled_count;

    if enabled_count == 0 {
        println!(
            "  {} {}",
            "⚠".bright_yellow(),
            "No keyboards enabled".yellow()
        );
        println!(
            "  {} Run {} to enable keyboards",
            "Tip:".bright_yellow().bold(),
            "keymux toggle".bright_white()
        );
    } else {
        println!(
            "  {} {} enabled{}, {} disabled",
            "Status:".bright_white().bold(),
            enabled_count.to_string().green(),
            group_note(&grouped),
            disabled_count.to_string().dimmed()
        );
    }

    println!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "16c0:27db:0111:0003";
    const B: &str = "16c0:27d9:0111:0003";
    const C: &str = "046d:c52b:0111:0003";

    fn parse(text: &str) -> Config {
        ron::from_str(text).unwrap()
    }

    const GROUP: &str =
        r#"keyboard_groups: { "glove80": ["16c0:27db:0111:0003", "16c0:27d9:0111:0003"] }"#;

    #[test]
    fn without_groups_the_status_is_the_enabled_keyboards_rule() {
        let config = parse(&format!(r#"(enabled_keyboards: ["{A}"])"#));
        assert_eq!(keyboard_status(&config, A, "a"), (true, None));
        assert_eq!(keyboard_status(&config, B, "b"), (false, None));
        assert_eq!(group_note(&[]), "");
    }

    #[test]
    fn a_member_is_enabled_with_its_group() {
        let config = parse(&format!(r#"(enabled_keyboards: ["{A}"], {GROUP})"#));
        assert_eq!(keyboard_status(&config, A, "a"), (true, Some("glove80")));
        assert_eq!(keyboard_status(&config, B, "b"), (true, Some("glove80")));
        assert_eq!(keyboard_status(&config, C, "c"), (false, None));
        assert_eq!(
            group_note(&["glove80", "glove80"]),
            " (1 group of 2 keyboards)"
        );
    }

    #[test]
    fn a_member_named_disable_stays_disabled() {
        let config = parse(&format!(
            r#"(enabled_keyboards: ["{A}", ("{B}", "Disable")], {GROUP})"#
        ));
        assert_eq!(keyboard_status(&config, A, "a"), (true, Some("glove80")));
        assert_eq!(keyboard_status(&config, B, "b"), (false, None));
        assert_eq!(group_note(&["glove80"]), " (1 group of 1 keyboard)");
    }

    #[test]
    fn an_absent_member_changes_nothing() {
        let config = parse(&format!(r#"(enabled_keyboards: ["{C}"], {GROUP})"#));
        assert_eq!(keyboard_status(&config, C, "c"), (true, None));
    }
}
