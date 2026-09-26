use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

/// Get keyboard IDs and names for shell completions
pub fn get_keyboard_completions() -> Vec<String> {
    use keymux::keyboard_id::find_all_keyboards;

    let mut completions = vec!["*".to_string()];

    for (id, kb) in find_all_keyboards() {
        completions.push(id.to_string());
        let base_id = id
            .to_string()
            .split('@')
            .next()
            .unwrap_or(&id.to_string())
            .to_string();
        if !completions.contains(&base_id) {
            completions.push(base_id);
        }
        if !completions.contains(&kb.name) {
            completions.push(kb.name);
        }
    }

    completions.sort();
    completions
}

/// Shell completion generator
pub fn generate_completions(shell: Shell) {
    use clap::builder::styling::{AnsiColor, Styles};

    use clap_complete::generate;
    use std::io;

    let styles = Styles::styled()
        .header(AnsiColor::Yellow.on_default().bold())
        .usage(AnsiColor::Yellow.on_default().bold())
        .literal(AnsiColor::Cyan.on_default().bold())
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default().bold())
        .valid(AnsiColor::Green.on_default().bold())
        .invalid(AnsiColor::Red.on_default().bold());

    let mut cmd = get_clap_command().styles(styles);
    let bin_name = cmd.get_name().to_string();
    generate(shell, &mut cmd, bin_name, &mut io::stdout());
}

/// Get the clap Command with styles applied for colorized help
pub fn get_clap_command() -> clap::Command {
    use clap::builder::styling::{AnsiColor, Styles};

    let styles = Styles::styled()
        .header(AnsiColor::Yellow.on_default().bold())
        .usage(AnsiColor::Yellow.on_default().bold())
        .literal(AnsiColor::Cyan.on_default().bold())
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default().bold())
        .valid(AnsiColor::Green.on_default().bold())
        .invalid(AnsiColor::Red.on_default().bold());

    Cli::command().styles(styles)
}

/// Get visible subcommands with their descriptions for completions
pub fn get_subcommands() -> Vec<(String, String)> {
    let hidden = ["daemon", "niri-daemon", "completion"];
    let cmd = Cli::command();
    cmd.get_subcommands()
        .filter(|sub| !hidden.contains(&sub.get_name()))
        .map(|sub| {
            let name = sub.get_name().to_string();
            let about = sub.get_about().map(|a| a.to_string()).unwrap_or_default();
            (name, about)
        })
        .collect()
}

#[derive(Subcommand)]
pub enum GamemodeAction {
    /// Temporary, per-app override for the currently focused window (or a
    /// given app_id). Resets to automatic detection when the daemon
    /// restarts.
    Window {
        #[command(subcommand)]
        action: WindowGamemodeAction,
    },
    /// System-wide override that ignores every per-window rule/override.
    /// Resets to `auto` when the daemon restarts.
    Global {
        #[command(subcommand)]
        action: GlobalGamemodeAction,
    },
}

#[derive(Subcommand)]
pub enum WindowGamemodeAction {
    /// Force game mode ON for this window (or the given app_id)
    On {
        /// app_id to target instead of the currently focused window
        app_id: Option<String>,
    },
    /// Force game mode OFF for this window (or the given app_id)
    Off {
        /// app_id to target instead of the currently focused window
        app_id: Option<String>,
    },
    /// Flip the currently-effective state into an explicit override
    Toggle {
        /// app_id to target instead of the currently focused window
        app_id: Option<String>,
    },
    /// Clear the override for this window (or app_id) - back to automatic
    /// detection
    Auto {
        /// app_id to target instead of the currently focused window
        app_id: Option<String>,
    },
    /// List all active per-app_id overrides
    List,
}

#[derive(Subcommand)]
pub enum GlobalGamemodeAction {
    /// Force game mode ON everywhere, ignoring per-window rules/overrides
    AlwaysOn,
    /// Force game mode OFF everywhere, ignoring per-window rules/overrides
    AlwaysOff,
    /// Default: defer to per-window overrides / automatic detection
    Auto,
    /// Show the current global override state
    Status,
}

#[derive(Parser)]
#[command(name = "keymux")]
#[command(about = "QMK-inspired keyboard middleware for Linux", long_about = None)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the keyboard middleware daemon
    #[command(hide = true)]
    Daemon {
        /// Path to config file (default: ~/.config/keymux/config.ron)
        #[arg(short, long)]
        config: Option<std::path::PathBuf>,

        /// User to run as (for root execution, uses that user's config)
        #[arg(short, long)]
        user: Option<String>,
    },

    /// Run the niri window watcher daemon
    #[command(hide = true)]
    NiriDaemon,

    /// Run the hyprland window watcher daemon
    #[command(hide = true)]
    HyprlandDaemon,

    /// Run the sway window watcher daemon
    #[command(hide = true)]
    SwayDaemon,

    /// Run the i3 window watcher daemon
    #[command(hide = true)]
    I3Daemon,

    /// Run the bspwm window watcher daemon
    #[command(hide = true)]
    BspwmDaemon,

    /// Create a starter config file if one doesn't already exist
    Init {
        /// Use the full-featured example config instead of the minimal starter
        #[arg(long)]
        full: bool,

        /// Overwrite an existing config file
        #[arg(long)]
        force: bool,
    },

    /// List all detected keyboards
    List,

    /// Toggle keyboard enable/disable state (opens selection menu)
    Toggle {
        /// Keyboard patterns to toggle (ID, name, or "*" for all)
        patterns: Vec<String>,

        /// Open multi-select menu to choose keyboards
        #[arg(long, short)]
        multi: bool,
    },

    /// Enable specific keyboards
    Enable {
        /// Keyboard patterns to enable (ID, name, or "*" for all)
        patterns: Vec<String>,

        /// Open multi-select menu to choose keyboards
        #[arg(long, short)]
        multi: bool,
    },

    /// Disable specific keyboards
    Disable {
        /// Keyboard patterns to disable (ID, name, or "*" for all)
        patterns: Vec<String>,

        /// Open multi-select menu to choose keyboards
        #[arg(long, short)]
        multi: bool,
    },

    /// Control game mode settings
    Gamemode {
        #[command(subcommand)]
        action: GamemodeAction,
    },

    /// Reload configuration from disk
    Reload,

    /// Validate configuration file for errors
    Validate {
        /// Path to config file (default: ~/.config/keymux/config.ron)
        #[arg(short = 'f', long = "file", aliases = ["config", "c"])]
        config: Option<std::path::PathBuf>,
    },

    /// Show debugging information
    Debug,

    /// Show adaptive timing statistics
    AdaptiveStats {
        /// Path to config file (default: ~/.config/keymux/config.ron)
        #[arg(short = 'f', long = "file", aliases = ["config", "c"])]
        config: Option<std::path::PathBuf>,
    },

    /// Clear all adaptive timing statistics
    ClearStats,

    /// Generate shell completions (hidden - for package scripts only)
    #[command(name = "completion", hide = true)]
    Completion {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

pub fn print_help() {
    use colored::Colorize;

    println!("{}", "Keyboard Middleware".bright_cyan().bold());
    println!("{}", "QMK-inspired keyboard remapping for Linux".dimmed());
    println!();
    println!("{}", "USAGE:".bright_yellow().bold());
    println!("  {} {}", "keymux".bright_white(), "[COMMAND]".dimmed());
    println!();
    println!("{}", "COMMANDS:".bright_yellow().bold());
    println!(
        "  {}  {}",
        "daemon".bright_green().bold(),
        "Run the keyboard middleware daemon".dimmed()
    );
    println!(
        "  {}    {}",
        "list".bright_green().bold(),
        "List all detected keyboards".dimmed()
    );
    println!(
        "  {}  {}",
        "toggle".bright_green().bold(),
        "Toggle keyboard enable/disable state".dimmed()
    );
    println!(
        "  {}  {}",
        "enable".bright_green().bold(),
        "Enable specific keyboards".dimmed()
    );
    println!(
        "  {}  {}",
        "disable".bright_green().bold(),
        "Disable specific keyboards".dimmed()
    );
    println!(
        "  {}  {}",
        "reload".bright_green().bold(),
        "Reload configuration from disk".dimmed()
    );
    println!(
        "  {}  {}",
        "validate".bright_green().bold(),
        "Validate configuration file".dimmed()
    );
    println!(
        "  {}    {}",
        "help".bright_green().bold(),
        "Print this message".dimmed()
    );
    println!();
    println!("{}", "OPTIONS:".bright_yellow().bold());
    println!(
        "  {}  {}",
        "-h, --help".bright_white(),
        "Print help".dimmed()
    );
    println!(
        "  {}  {}",
        "-V, --version".bright_white(),
        "Print version".dimmed()
    );
    println!();
    println!("{}", "EXAMPLES:".bright_yellow().bold());
    println!(
        "  {}  {}",
        "keymux daemon".bright_white(),
        "Start the daemon".dimmed()
    );
    println!(
        "  {}    {}",
        "keymux list".bright_white(),
        "Show all detected keyboards".dimmed()
    );
    println!(
        "  {}  {}",
        "keymux toggle --multi".bright_white(),
        "Select keyboards via menu".dimmed()
    );
    println!(
        "  {}  {}",
        "keymux enable 6912".bright_white(),
        "Enable keyboard by ID".dimmed()
    );
    println!(
        "  {}  {}",
        "keymux enable \"Keychron\"".bright_white(),
        "Enable keyboards by name".dimmed()
    );
    println!(
        "  {}  {}",
        "keymux disable 6912".bright_white(),
        "Disable keyboard by ID".dimmed()
    );
    println!(
        "  {}  {}",
        "keymux enable \"*\"".bright_white(),
        "Enable all keyboards".dimmed()
    );
    println!();
}
