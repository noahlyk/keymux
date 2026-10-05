#![allow(clippy::pedantic)]

use anyhow::Result;
use clap::{CommandFactory, Parser};

mod adaptive_stats;
mod cli;
mod gamemode;

mod debug;
mod init;
pub mod keycode;
mod list;
mod toggle;

pub use keymux::{get_actual_user_uid, get_user_home_dir};

use cli::Cli;
use keymux::daemon::AsyncDaemon;

fn main() -> Result<()> {
    // Handle dynamic shell completions manually (clap_complete's dynamic feature doesn't support subcommands)
    if let Ok(shell_name) = std::env::var("COMPLETE") {
        handle_dynamic_completion(&shell_name);
        std::process::exit(0);
    }

    let cli = Cli::parse();

    match &cli.command {
        Some(cli::Commands::Daemon { config, user }) => {
            tracing_subscriber::fmt()
                .with_target(false)
                .with_thread_ids(false)
                .with_file(false)
                .init();

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;

            runtime.block_on(async {
                let mut daemon = AsyncDaemon::new(config.clone(), user.clone())?;
                daemon.run().await
            })?;
        }
        Some(cli::Commands::NiriDaemon) => {
            keymux::niri::run_niri_daemon()?;
        }
        Some(cli::Commands::HyprlandDaemon) => {
            keymux::hyprland::run_hyprland_daemon()?;
        }
        Some(cli::Commands::SwayDaemon) => {
            keymux::hyprland::run_sway_daemon()?;
        }
        Some(cli::Commands::I3Daemon) => {
            keymux::x11::run_i3_daemon()?;
        }
        Some(cli::Commands::BspwmDaemon) => {
            keymux::x11::run_bspwm_daemon()?;
        }
        Some(cli::Commands::Init { full, force }) => {
            init::run_init(*full, *force)?;
        }
        Some(cli::Commands::List) => {
            list::run_list()?;
        }
        Some(cli::Commands::Toggle { patterns, multi }) => {
            if !*multi && patterns.is_empty() {
                // Run interactive toggle menu
                toggle::run_toggle(false, None)?;
            } else if !*multi && !patterns.is_empty() {
                // Handle toggle patterns directly
                let config_path = keymux::config::Config::default_path()?;
                let mut config = keymux::config::Config::load_or_hint(&config_path)?;
                let keyboards = keymux::keyboard_id::find_all_keyboards();
                let items: Vec<_> = keyboards
                    .into_iter()
                    .map(|(id, kb)| (id, kb.name))
                    .collect();
                toggle::handle_toggle_patterns(
                    &mut config,
                    &config_path,
                    patterns.clone(),
                    &items,
                )?;
            } else {
                toggle::run_toggle(*multi, None)?;
            }
        }
        Some(cli::Commands::Enable { patterns, multi }) => {
            if !*multi && patterns.is_empty() {
                if let Some(sub) = Cli::command()
                    .get_subcommands()
                    .find(|c| c.get_name() == "enable")
                {
                    sub.clone().print_help().unwrap();
                }
                std::process::exit(0);
            }
            toggle::run_toggle(*multi, Some((true, patterns.clone())))?;
        }
        Some(cli::Commands::Disable { patterns, multi }) => {
            if !*multi && patterns.is_empty() {
                if let Some(sub) = Cli::command()
                    .get_subcommands()
                    .find(|c| c.get_name() == "disable")
                {
                    sub.clone().print_help().unwrap();
                }
                std::process::exit(0);
            }
            toggle::run_toggle(*multi, Some((false, patterns.clone())))?;
        }
        Some(cli::Commands::Gamemode { action }) => {
            gamemode::handle_gamemode_action(action)?;
        }
        Some(cli::Commands::Reload) => {
            run_reload()?;
        }
        Some(cli::Commands::Steno { action }) => {
            let config_path = keymux::config::Config::default_path()?;
            let config_dir = config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            match action {
                cli::StenoAction::Setup => keymux::steno::setup::run_setup(config_dir)?,
                cli::StenoAction::Lookup { word } => {
                    keymux::steno::commands::lookup(config_dir, word);
                }
                cli::StenoAction::Stroke { stroke } => {
                    keymux::steno::commands::show_stroke(config_dir, stroke)?;
                }
                cli::StenoAction::Add { stroke, text } => {
                    keymux::steno::commands::add(config_dir, stroke, text)?;
                }
                cli::StenoAction::Tape { follow, lines } => {
                    let (uid, _) = keymux::get_actual_user_uid();
                    let home = keymux::get_user_home_dir(uid)?;
                    keymux::steno::commands::tail_tape(&home, *lines, *follow)?;
                }
            }
        }
        Some(cli::Commands::Validate { config }) => {
            keymux::config::validate_config(config.as_deref())?;
        }
        Some(cli::Commands::Debug) => {
            debug::run_debug(None)?;
        }
        Some(cli::Commands::AdaptiveStats { config }) => {
            adaptive_stats::show_adaptive_stats(config.as_deref())?;
        }
        Some(cli::Commands::ClearStats) => {
            adaptive_stats::clear_adaptive_stats()?;
        }
        Some(cli::Commands::Completion { shell }) => {
            cli::generate_completions(*shell);
        }
        None => {
            cli::print_help();
        }
    }

    Ok(())
}

fn run_reload() -> Result<()> {
    use colored::Colorize;

    println!();
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!("  {}", "Reload Configuration".bright_cyan().bold());
    println!(
        "{}",
        "═══════════════════════════════════════".bright_cyan()
    );
    println!();

    print!("  {} Sending reload request... ", "→".bright_blue());

    match keymux::ipc::send_request(&keymux::ipc::IpcRequest::Reload) {
        Ok(keymux::ipc::IpcResponse::Ok) => {
            println!("{}", "✓".bright_green().bold());
            println!();
            println!(
                "  {} {}",
                "✓".bright_green().bold(),
                "Configuration reloaded successfully!".green()
            );
            println!();
        }
        Ok(keymux::ipc::IpcResponse::Error(msg)) => {
            println!("{}", "✗".bright_red().bold());
            println!();
            println!("  {} {}", "✗".bright_red().bold(), msg.red());
            println!();
            anyhow::bail!("Config reload failed");
        }
        Ok(response) => {
            println!("{}", "✗".bright_red().bold());
            println!();
            println!(
                "  {} Unexpected response: {:?}",
                "✗".bright_red().bold(),
                response
            );
            println!();
            anyhow::bail!("Unexpected response from daemon");
        }
        Err(e) => {
            println!("{}", "✗".bright_red().bold());
            println!();
            println!(
                "  {} {}",
                "✗".bright_red().bold(),
                format!("Failed to connect to daemon: {}", e).red()
            );
            println!();
            println!(
                "  {} {}",
                "Tip:".bright_yellow().bold(),
                "Make sure the daemon is running (usually via systemd)".dimmed()
            );
            println!();
            anyhow::bail!("Failed to reload configuration");
        }
    }

    Ok(())
}

fn handle_dynamic_completion(shell_name: &str) {
    use keymux::keyboard_id::find_all_keyboards;

    // Check if this is a completion request (has _CLAP_COMPLETE_INDEX) or registration
    let is_completion = std::env::var("_CLAP_COMPLETE_INDEX").is_ok();

    // Get the binary path
    let bin_path = std::env::args_os()
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let bin_name = bin_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "keymux".to_string());

    if !is_completion {
        // Output registration script based on shell
        match shell_name {
            "bash" => {
                println!(
                    r#"_keymux_completion() {{
    local IFS=$'\013'
    local _CLAP_COMPLETE_INDEX=${{COMP_CWORD}}
    local _CLAP_COMPLETE_COMP_TYPE=${{COMP_TYPE:-${{COMP_TYPE:-9}}}}
    local _CLAP_COMPLETE_SPACE=true
    COMPREPLY= $( $COMPLETE_IFS="$IFS" _CLAP_COMPLETE_INDEX="$_CLAP_COMPLETE_INDEX" COMPLETE="bash" "{bin}" "${{COMP_WORDS[@]}}" 2>/dev/null )
    if [[ $? != 0 ]]; then
        unset COMPREPLY
    fi
}}
complete -o bashdefault -o default -F _keymux_completion {bin_name}
unset COMPLETE
"#,
                    bin = bin_path.display(),
                    bin_name = bin_name
                );
            }
            "zsh" => {
                println!(
                    r#"#compdef {bin_name}
_keymux() {{
    local -a completions
    local _CLAP_COMPLETE_INDEX=$(expr $CURRENT - 1)
    local line
    while IFS= read -r line; do
        completions+=("$line")
    done < <(COMPLETE=zsh _CLAP_COMPLETE_INDEX="$_CLAP_COMPLETE_INDEX" {bin} ${{words[@]}} 2>/dev/null)
    _describe 'values' completions || compadd "$@"
}}
unset COMPLETE
compdef _keymux {bin_name}
"#,
                    bin = bin_path.display(),
                    bin_name = bin_name
                );
            }
            "fish" => {
                println!(
                    r#"function _keymux_completion
    set -l completions (COMPLETE=fish {bin} ${{argv}} 2>/dev/null)
    for comp in $completions
        echo $comp
    end
end
complete -c {bin_name} -f -a '(_keymux_completion)'
set -e COMPLETE
"#,
                    bin = bin_path.display(),
                    bin_name = bin_name
                );
            }
            _ => {
                eprintln!("Unsupported shell: {}", shell_name);
                std::process::exit(1);
            }
        }
        return;
    }

    // Handle actual completion request
    // skip(1) removes the binary name from args (argv[0])
    // But when called via shell function, binary name may also be in args (from ${words[@]})
    let mut args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| s != "--")
        .collect();

    // If first arg matches binary name, skip it too
    let bin_name_check = std::env::args_os()
        .next()
        .map(|p| {
            std::path::PathBuf::from(p)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if let Some(first) = args.first() {
        if first == &bin_name_check || first == "keymux" {
            args.remove(0);
        }
    }

    let mut completions: Vec<String> = Vec::new();

    fn escape_for_zsh(value: &str) -> String {
        value.replace(':', "\\:")
    }

    match args.len() {
        0 => {
            // Get subcommands dynamically from clap
            completions = cli::get_subcommands()
                .iter()
                .map(|(cmd, desc)| format!("{}:{}", escape_for_zsh(cmd), escape_for_zsh(desc)))
                .collect();
        }
        1 => {
            // One arg provided - could be subcommand name or argument to subcommand
            let first_arg = args.first().cloned().unwrap_or_default();

            // Check if it's a subcommand that takes keyboard arguments
            match first_arg.as_str() {
                "toggle" | "enable" | "disable" => {
                    completions.push("*:all keyboards".to_string());
                    for (id, kb) in find_all_keyboards() {
                        let id_str = id.to_string();
                        let short_id = id_str.split('@').next().unwrap_or(&id_str).to_string();
                        let clean_id = short_id;
                        completions.push(format!(
                            "{}:{}",
                            escape_for_zsh(&clean_id),
                            escape_for_zsh(&kb.name)
                        ));
                    }
                }
                "gamemode" => {
                    completions = vec![
                        "window:Temporary override for the focused window".to_string(),
                        "global:System-wide game mode override".to_string(),
                    ];
                }
                _ => {
                    // Get subcommands dynamically from clap
                    let subcommands = cli::get_subcommands();
                    if first_arg.is_empty() {
                        completions = subcommands
                            .iter()
                            .map(|(cmd, desc)| {
                                format!("{}:{}", escape_for_zsh(cmd), escape_for_zsh(desc))
                            })
                            .collect();
                    } else {
                        completions = subcommands
                            .iter()
                            .filter(|(cmd, _)| cmd.starts_with(&first_arg))
                            .map(|(cmd, desc)| {
                                format!("{}:{}", escape_for_zsh(cmd), escape_for_zsh(desc))
                            })
                            .collect();
                    }
                }
            }
        }
        _ => {
            if let Some(first_arg) = args.first() {
                match first_arg.as_str() {
                    "toggle" | "enable" | "disable" => {
                        completions.push("*:all keyboards".to_string());
                        for (id, kb) in find_all_keyboards() {
                            let id_str = id.to_string();
                            let short_id = id_str.split('@').next().unwrap_or(&id_str).to_string();
                            let clean_id = short_id;
                            completions.push(format!(
                                "{}:{}",
                                escape_for_zsh(&clean_id),
                                escape_for_zsh(&kb.name)
                            ));
                        }
                    }
                    "gamemode" => {
                        completions = vec![
                            "window:Temporary override for the focused window".to_string(),
                            "global:System-wide game mode override".to_string(),
                        ];
                    }
                    _ => {}
                }
            }
        }
    }

    // Only filter when completing keyboard patterns (args.len() > 1)
    let current_input = args.last().cloned().unwrap_or_default();
    if args.len() > 1
        && !current_input.is_empty()
        && !current_input.starts_with('-')
        && !completions.is_empty()
    {
        completions.retain(|c| c.to_lowercase().contains(&current_input.to_lowercase()));
    }

    let ifs = std::env::var("COMPLETE_IFS").unwrap_or_else(|_| "\n".to_string());
    println!("{}", completions.join(&ifs));
}
