use crate::gamemode_state::{GlobalOverride, WindowOverride};
use std::collections::HashMap;
use std::fs;
use tracing::debug;

/// Result of game mode detection for a window
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameModeState {
    Normal,
    GameMode(String), // Contains reason
}

impl GameModeState {
    pub const fn is_game_mode(&self) -> bool {
        matches!(self, Self::GameMode(_))
    }
}

/// Detect whether a window should be in game mode based on app_id, pid, and title.
///
/// This is the single source of truth for game mode detection. Used by:
/// - `keymux debug` (Window Info table)
/// - `keymux niri-daemon` (IPC game mode signaling)
/// - Inline daemon window manager monitor
pub fn detect_game_mode(
    app_id: Option<&str>,
    pid: Option<u32>,
    title: Option<&str>,
) -> GameModeState {
    if let Some(app_id) = app_id {
        // Check app ID first (fastest check)
        if app_id == "gamescope" {
            return GameModeState::GameMode("gamescope window".to_string());
        }

        // Steam games (app ID format: steam_app_<appid>)
        if app_id.starts_with("steam_app_") {
            return GameModeState::GameMode("Steam game".to_string());
        }

        // Wine/.exe-suffixed apps. A `.exe` (or bare "wine"/"wine-*") app_id
        // only proves this is a Windows binary running under Wine - it says
        // nothing about whether it's a game vs. any other Windows program
        // (an installer, a utility, or the Battle.net/GOG Galaxy launcher
        // UI itself would all match too). Require a secondary confirmation:
        // the process's working directory (or command line, as a fallback)
        // looks like a known game-library location. Real PC games on Linux
        // almost always land in one of a handful of well-known directories
        // (Steam's steamapps/common, GOG, Epic, itch.io, Lutris/Heroic's
        // default install dirs, or a generic "Games" folder) - anything
        // that doesn't match one of those, we deliberately don't guess is a
        // game (false negative preferred over false positive here).
        let looks_like_wine_app =
            app_id == "wine" || app_id.starts_with("wine-") || app_id.contains(".exe");
        if looks_like_wine_app && pid.is_some_and(looks_like_game_install_path) {
            return GameModeState::GameMode("Wine game".to_string());
        }
        // Otherwise (no pid, or nothing confirms this is a game) fall
        // through to the IS_GAME/process-tree checks below rather than
        // guessing from the app_id alone.

        // Roblox
        if app_id == "com.roblox.RobloxPlayer" || app_id.contains("roblox") {
            return GameModeState::GameMode("Roblox".to_string());
        }

        // Epic Games Launcher. Bare "epic" was dropped: it matched any
        // app_id merely containing that fragment, unrelated to the launcher.
        if app_id.contains("epicgames") {
            return GameModeState::GameMode("Epic Games".to_string());
        }

        // Lutris games. Substring match kept for now (lower false-positive
        // risk than "epic"/"proton"/"wine" alone), but still a known trap if
        // some unrelated app_id ever contains this fragment.
        if app_id.contains("lutris") {
            return GameModeState::GameMode("Lutris game".to_string());
        }

        // Heroic Games Launcher. Same caveat as Lutris above.
        if app_id.contains("heroic") {
            return GameModeState::GameMode("Heroic Games".to_string());
        }

        // Sober (virtualization)
        if app_id == "org.vinegarhq.Sober" {
            return GameModeState::GameMode("Sober virtualization".to_string());
        }

        // NOTE: a former "Proton games" rule (`app_id.contains("proton")`) was
        // removed here. It was both a false-positive risk (would match e.g. a
        // Proton Mail/VPN Linux app_id) and redundant: real Proton games
        // surface as a `steam_app_*` id (handled above) or as a Wine process
        // caught by the Wine rule above / the process-tree check below.
        //
        // NOTE: a former "Flatpak games" rule (`app_id.contains("com") &&
        // app_id.contains(".")`) was removed here. It was not real Flatpak
        // detection — it matched any reverse-DNS-style app_id containing the
        // substring "com" and a dot anywhere (e.g. `com.obsproject.Studio`,
        // `com.discordapp.Discord`, `com.spotify.Client`), misclassifying
        // ordinary non-game apps as games. Packaging format is not a game
        // signal; anything genuinely missed here falls through to the
        // PID/env checks below, or can be corrected with a manual
        // `keymux gamemode window` override.

        // .NET applications: app_id "dotnet" alone just means "some
        // WinForms/WPF/Avalonia GUI program" - not necessarily a game (it
        // could be any ordinary .NET desktop app). Only flag it when the
        // window title matches a specific known game; an unrecognized
        // title is deliberately left as Normal rather than guessing (the
        // old ".NET game" catch-all for unknown titles was exactly the
        // same class of false positive as the bare Wine/.exe match above).
        if app_id == "dotnet" {
            if let Some(title) = title {
                let title_lower = title.to_lowercase();
                let known_game = if title_lower.contains("terraria") {
                    Some("Terraria")
                } else if title_lower.contains("stardew") {
                    Some("Stardew Valley")
                } else if title_lower.contains("minecraft") {
                    Some("Minecraft")
                } else if title_lower.contains("hollow knight") {
                    Some("Hollow Knight")
                } else if title_lower.contains("celeste") {
                    Some("Celeste")
                } else if title_lower.contains("cuphead") {
                    Some("Cuphead")
                } else if title_lower.contains("ori") {
                    Some("Ori series")
                } else if title_lower.contains("dead cells") {
                    Some("Dead Cells")
                } else if title_lower.contains("hades") {
                    Some("Hades")
                } else if title_lower.contains("slay the spire") {
                    Some("Slay the Spire")
                } else {
                    None
                };
                if let Some(game_name) = known_game {
                    return GameModeState::GameMode(game_name.to_string());
                }
            }
        }
    }

    // Check environment variables for IS_GAME=1
    if let Some(pid) = pid {
        if check_is_game_env(pid) {
            return GameModeState::GameMode("IS_GAME=1 environment".to_string());
        }

        // Check process tree for gamescope/gamemode
        let (has_gamescope, has_gamemode) = check_process_tree(pid);
        if has_gamescope || has_gamemode {
            let cmdline = get_process_cmdline(pid);
            let cmdline_lower = cmdline.to_lowercase();

            let reason = if has_gamescope && has_gamemode {
                "gamescope + gamemode"
            } else if has_gamescope {
                if cmdline_lower.contains("steam") || cmdline_lower.contains("steamapps") {
                    "Steam + gamescope"
                } else if cmdline_lower.contains("lutris") {
                    "Lutris + gamescope"
                } else if cmdline_lower.contains("heroic") {
                    "Heroic + gamescope"
                } else {
                    "gamescope wrapper"
                }
            } else if has_gamemode {
                if cmdline_lower.contains("steam") {
                    "Steam + gamemode"
                } else if cmdline_lower.contains("lutris") {
                    "Lutris + gamemode"
                } else if cmdline_lower.contains("heroic") {
                    "Heroic + gamemode"
                } else {
                    "gamemode"
                }
            } else {
                "game launcher"
            };
            return GameModeState::GameMode(reason.to_string());
        }
    }

    GameModeState::Normal
}

/// Resolve the *effective* game mode state for a window.
///
/// Applies the two-tier override system on top of the programmed detection
/// rules (`detect_game_mode`). This is what every real call site should use;
/// `detect_game_mode` itself stays override-unaware so it remains cleanly
/// unit-testable in isolation.
///
/// Precedence, highest first:
/// 1. `global_override` (if not `Auto`) - ignores everything else.
/// 2. `window_overrides` entry for this app_id (if present).
/// 3. `detect_game_mode` (the programmed heuristics).
pub fn resolve_effective_game_mode(
    app_id: Option<&str>,
    pid: Option<u32>,
    title: Option<&str>,
    global_override: GlobalOverride,
    window_overrides: &HashMap<String, WindowOverride>,
) -> GameModeState {
    match global_override {
        GlobalOverride::AlwaysOn => {
            return GameModeState::GameMode("Global override: always-on".to_string())
        }
        GlobalOverride::AlwaysOff => return GameModeState::Normal,
        GlobalOverride::Auto => {}
    }

    if let Some(id) = app_id {
        match window_overrides.get(id) {
            Some(WindowOverride::On) => {
                return GameModeState::GameMode("Window override: on".to_string())
            }
            Some(WindowOverride::Off) => return GameModeState::Normal,
            None => {}
        }
    }

    detect_game_mode(app_id, pid, title)
}

/// Check if a process has `IS_GAME=1` in its environment
fn check_is_game_env(pid: u32) -> bool {
    let env_path = format!("/proc/{pid}/environ");
    match fs::read(&env_path) {
        Ok(contents) => {
            let env_str = String::from_utf8_lossy(&contents);
            for var in env_str.split('\0') {
                if var == "IS_GAME=1" {
                    debug!("Found IS_GAME=1 for PID {pid}");
                    return true;
                }
            }
            false
        }
        Err(e) => {
            debug!("Cannot read environ for PID {pid}: {e}");
            false
        }
    }
}

/// Get process command line for more specific detection
fn get_process_cmdline(pid: u32) -> String {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    fs::read(&cmdline_path).map_or_else(
        |_| String::new(),
        |contents| String::from_utf8_lossy(&contents).replace('\0', " "),
    )
}

/// Known game-library directory fragments, checked case-insensitively
/// against a process's working directory and command line. Covers Steam
/// (incl. Proton games, which run as ordinary Wine processes under a
/// `steamapps/common/...` cwd), GOG, Epic, itch.io, Lutris, Heroic, and a
/// generic "Games" folder some users organize manually installed games in.
const GAME_PATH_HINTS: &[&str] = &[
    "steamapps/common",
    "steamapps\\common",
    "gog games",
    "epic games",
    "/heroic/",
    "\\heroic\\",
    "lutris",
    ".itch",
    "/games/",
    "\\games\\",
];

/// Pure fragment-matching logic, extracted so it's unit-testable without
/// needing a real process. Real callers go through
/// `looks_like_game_install_path` below.
fn contains_game_path_hint(text: &str) -> bool {
    let lower = text.to_lowercase();
    GAME_PATH_HINTS.iter().any(|hint| lower.contains(hint))
}

/// Secondary confirmation that a Wine/`.exe` app_id is actually a game:
/// checks the process's working directory (a real POSIX path - reliable
/// even under Wine/Proton, where `/proc/{pid}/exe` itself just resolves to
/// the Wine loader binary, not the guest .exe) and, if that's unavailable,
/// falls back to its command line.
fn looks_like_game_install_path(pid: u32) -> bool {
    if let Ok(cwd) = fs::read_link(format!("/proc/{pid}/cwd")) {
        if contains_game_path_hint(&cwd.to_string_lossy()) {
            return true;
        }
    }
    contains_game_path_hint(&get_process_cmdline(pid))
}

/// Check if a process is running through gamescope, gamemode, or custom-gamescope
/// by examining its command line and parent process chain
fn check_process_tree(process_id: u32) -> (bool, bool) {
    let mut has_gamescope = false;
    let mut has_gamemode = false;
    let mut current_pid = process_id;

    // Walk up the process tree (max 10 levels to avoid infinite loops)
    for _ in 0..10 {
        let cmdline_path = format!("/proc/{current_pid}/cmdline");
        if let Ok(contents) = fs::read(&cmdline_path) {
            let cmdline = String::from_utf8_lossy(&contents);
            let cmd_lower = cmdline.to_lowercase();

            if cmd_lower.contains("gamescope") || cmd_lower.contains("custom-gamescope") {
                has_gamescope = true;
            }
            if cmd_lower.contains("gamemode") || cmd_lower.contains("gamemoded") {
                has_gamemode = true;
            }
        }

        // Get parent PID
        let stat_path = format!("/proc/{current_pid}/stat");
        let parent_pid = fs::read_to_string(&stat_path).ok().and_then(|stat| {
            let parts: Vec<&str> = stat.rsplitn(2, ')').collect();
            if parts.len() == 2 {
                parts[0].split_whitespace().nth(1)?.parse::<u32>().ok()
            } else {
                None
            }
        });

        match parent_pid {
            Some(parent) if parent > 1 => current_pid = parent,
            _ => break,
        }
    }

    (has_gamescope, has_gamemode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obs_studio_is_not_game_mode() {
        // Regression test for the false positive that prompted this fix:
        // OBS's app_id used to trip the bogus "Flatpak application" rule.
        assert_eq!(
            detect_game_mode(Some("com.obsproject.Studio"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn other_reverse_dns_apps_are_not_game_mode() {
        for app_id in [
            "com.discordapp.Discord",
            "com.spotify.Client",
            "com.slack.Slack",
            "org.gimp.GIMP",
            "org.blender.Blender",
            "com.github.Extractor",
        ] {
            assert_eq!(
                detect_game_mode(Some(app_id), None, None),
                GameModeState::Normal,
                "{app_id} should not be classified as game mode"
            );
        }
    }

    #[test]
    fn steam_app_prefix_is_game_mode() {
        assert!(detect_game_mode(Some("steam_app_570"), None, None).is_game_mode());
    }

    #[test]
    fn gamescope_is_game_mode() {
        assert!(detect_game_mode(Some("gamescope"), None, None).is_game_mode());
    }

    #[test]
    fn exe_suffix_without_game_path_confirmation_is_normal() {
        // battle.net.exe is the Battle.net LAUNCHER itself, not a game -
        // exactly the false-positive class this gate exists to prevent.
        // With no pid (so no path confirmation is even possible), we must
        // not guess.
        assert_eq!(
            detect_game_mode(Some("battle.net.exe"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn winehq_utility_is_not_game_mode() {
        // Regression test for the tightened "wine" rule: WineHQ's own
        // config tool is not a game, unlike an actual "wine"/"wine-*" prefix.
        assert_eq!(
            detect_game_mode(Some("org.winehq.Wine"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn bare_wine_app_id_without_pid_is_normal() {
        // No pid means no way to confirm a game-library path, so we must
        // not assume it's a game from the app_id alone.
        assert_eq!(
            detect_game_mode(Some("wine"), None, None),
            GameModeState::Normal
        );
        assert_eq!(
            detect_game_mode(Some("wine-Some.Game"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn game_path_hints_match_known_library_locations() {
        // The exact real-world case that prompted this: Proton runs a game
        // with cwd resolving to something like
        // "Z:\home\fib\.local\share\Steam\steamapps\common\Getting Over It\..."
        assert!(contains_game_path_hint(
            r"Z:\home\fib\.local\share\Steam\steamapps\common\Getting Over It\GettingOverIt.exe"
        ));
        assert!(contains_game_path_hint(
            "/home/fib/.local/share/Steam/steamapps/common/Getting Over It"
        ));
        assert!(contains_game_path_hint("/home/fib/GOG Games/Some Game"));
        assert!(contains_game_path_hint(
            "/home/fib/Games/Heroic/SomeGame/game.exe"
        ));
        assert!(contains_game_path_hint("/home/fib/.itch/some-game"));
    }

    #[test]
    fn game_path_hints_do_not_match_ordinary_locations() {
        // A Wine-run non-game program (an installer, a utility, an office
        // app) living outside any known game-library folder must not match.
        assert!(!contains_game_path_hint(
            "/home/fib/.wine/drive_c/Program Files/Adobe/Reader"
        ));
        assert!(!contains_game_path_hint("/home/fib/Downloads/setup.exe"));
        assert!(!contains_game_path_hint("/home/fib/code/keymux"));
    }

    #[test]
    fn looks_like_game_install_path_is_false_for_a_real_non_game_pid() {
        // End-to-end test against a real /proc entry (no mocking needed):
        // this test binary's own pid has a cwd/cmdline that don't look
        // like a game install path, confirming the pid-reading codepath is
        // actually wired up, not just the pure `contains_game_path_hint`
        // logic above.
        //
        // Deliberately calls `looks_like_game_install_path` directly
        // rather than going through `detect_game_mode`: this machine may
        // have Feral GameMode (`gamemoded`) running as a real ancestor of
        // the test process, which would make the *separate*,
        // pre-existing process-tree heuristic (checked later in
        // `detect_game_mode`, unrelated to this Wine-path-hint logic)
        // independently flag it - that's correct behavior for that rule,
        // not something this test is about.
        let own_pid = std::process::id();
        assert!(!looks_like_game_install_path(own_pid));
    }

    #[test]
    fn epic_fragment_alone_is_not_game_mode() {
        // Regression test for the tightened "epic" rule.
        assert_eq!(
            detect_game_mode(Some("com.epicdesign.NotesApp"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn epic_games_launcher_is_game_mode() {
        assert!(detect_game_mode(Some("com.epicgames.Launcher"), None, None).is_game_mode());
    }

    #[test]
    fn proton_named_app_is_not_game_mode() {
        // Regression test for the removed "proton" fragment rule.
        assert_eq!(
            detect_game_mode(Some("me.proton.Mail"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn dotnet_known_game_title_is_game_mode() {
        let state = detect_game_mode(Some("dotnet"), None, Some("Terraria v1.4"));
        assert!(matches!(state, GameModeState::GameMode(ref reason) if reason == "Terraria"));
    }

    #[test]
    fn dotnet_unknown_title_is_normal() {
        // An unrecognized title under a "dotnet" app_id could be any
        // ordinary WinForms/WPF/Avalonia GUI program - don't guess it's a
        // game (this replaces the old generic ".NET game" catch-all, which
        // was the same class of false positive as the bare Wine/.exe rule).
        assert_eq!(
            detect_game_mode(Some("dotnet"), None, Some("Some .NET App")),
            GameModeState::Normal
        );
    }

    #[test]
    fn dotnet_with_no_title_is_normal() {
        assert_eq!(
            detect_game_mode(Some("dotnet"), None, None),
            GameModeState::Normal
        );
    }

    #[test]
    fn no_app_id_no_pid_is_normal() {
        assert_eq!(detect_game_mode(None, None, None), GameModeState::Normal);
    }

    // NOTE: check_is_game_env and check_process_tree read real /proc paths
    // and are not covered by unit tests here (no filesystem-injection seam
    // exists for them yet) — this is a known, intentional gap, not an
    // oversight, and behavior for those paths is verified manually.

    #[test]
    fn global_always_on_overrides_a_normal_app() {
        let overrides = HashMap::new();
        let state = resolve_effective_game_mode(
            Some("com.obsproject.Studio"),
            None,
            None,
            GlobalOverride::AlwaysOn,
            &overrides,
        );
        assert!(state.is_game_mode());
    }

    #[test]
    fn global_always_off_overrides_a_game() {
        let overrides = HashMap::new();
        let state = resolve_effective_game_mode(
            Some("gamescope"),
            None,
            None,
            GlobalOverride::AlwaysOff,
            &overrides,
        );
        assert_eq!(state, GameModeState::Normal);
    }

    #[test]
    fn window_override_beats_heuristic_but_loses_to_global() {
        let mut overrides = HashMap::new();
        overrides.insert("gamescope".to_string(), WindowOverride::Off);

        // Window override wins over the heuristic when global is Auto.
        assert_eq!(
            resolve_effective_game_mode(
                Some("gamescope"),
                None,
                None,
                GlobalOverride::Auto,
                &overrides
            ),
            GameModeState::Normal
        );

        // Global override still wins over the window override.
        assert!(resolve_effective_game_mode(
            Some("gamescope"),
            None,
            None,
            GlobalOverride::AlwaysOn,
            &overrides
        )
        .is_game_mode());
    }

    #[test]
    fn no_override_falls_back_to_heuristic() {
        let overrides = HashMap::new();
        assert_eq!(
            resolve_effective_game_mode(
                Some("com.obsproject.Studio"),
                None,
                None,
                GlobalOverride::Auto,
                &overrides
            ),
            GameModeState::Normal
        );
    }
}
