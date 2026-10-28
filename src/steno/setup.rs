//! Getting steno dictionaries installed, switched and found.
//!
//! Dictionaries live in `<config>/steno/`, and every `.json` file in it, however deeply
//! nested, is part of the dictionary (see [`super::source`] for the order they apply in). A new
//! install has nothing there: a steno layer types the strokes as they are, and one command
//! installs a theory's dictionary:
//!
//! - `keymux steno theory plover` or `keymux steno theory lapwing` downloads it into its own
//!   folder, `steno/plover/` or `steno/lapwing/`, and switches the other theory's folder off by
//!   renaming it to `disabled_...`. Nothing is ever deleted.
//! - Your own dictionaries go anywhere else in `steno/`. The theory's folder has no number, so
//!   it ranks last (see [`super::source`]): a numbered file or folder of yours, or a plain file
//!   at the top, beats it, so you override entries without touching it.
//!
//! The daemon notices changes to any of this on its own, so there is nothing to reload.

use super::source::{is_disabled, DISABLED_PREFIX};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Plover's main dictionary, as shipped in its repository
pub const MAIN_DICT_URL: &str =
    "https://raw.githubusercontent.com/openstenoproject/plover/master/plover/assets/main.json";

/// SHA-256 of the `main.json` this release was tested against. A different
/// download still installs, with a warning, since Plover's master changes.
pub const MAIN_DICT_SHA256: &str =
    "6570873c7f703ecd39c9870083f643db3475b49462a8db80fe22fe33ac3cfe92";

/// Shown when the config has no steno layer. Right Ctrl toggles steno on and off.
const LAYER_SNIPPET: &str = r#"
  In the top-level `remaps`:
      KC_RCTL: TG("steno"),

  In `layers`:
      "steno": (
          kind: Steno((
              layout: "qwerty",
          )),
          remaps: {
              KC_RCTL: TG("steno"),
          },
          disabled_in_game_mode: true,
      ),
"#;

/// Directory steno dictionaries live in, under the keymux config directory
#[must_use]
pub fn steno_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("steno")
}

/// Binary copies of the loaded dictionaries, so starting up doesn't parse the JSON again.
#[must_use]
pub fn cache_dir(config_dir: &Path) -> PathBuf {
    steno_dir(config_dir).join("cache")
}

/// Lapwing's JSON dictionaries from plover-lapwing-aio. Its `.modal` and `.py` files need
/// Plover's plugin runtime, so they aren't used here.
pub const LAPWING_BASE_URL: &str =
    "https://raw.githubusercontent.com/aerickt/plover-lapwing-aio/main/plover_lapwing/dictionaries/";
pub const LAPWING_FILES: [&str; 6] = [
    "lapwing-base.json",
    "lapwing-numbers.json",
    "lapwing-commands.json",
    "lapwing-semi-modal-movement.json",
    "lapwing-prefixed-proper-nouns.json",
    "lapwing-uk-additions.json",
];

/// The theories that can be installed. Each lives in a folder of this name, with no number, so
/// it ranks below everything numbered and every plain file.
const THEORIES: [&str; 2] = ["plover", "lapwing"];

/// What a theory's folder is called when keymux installs it. No number, so it is the plainest
/// kind of folder, after everything numbered and every plain file.
fn theory_folder(theory: &str) -> String {
    theory.to_string()
}

/// Written in every theory folder keymux installs, with the theory's name in it. A folder is
/// recognised by this file, not by what it is called, so you can rename it, number it
/// differently or move it. It is hidden, so it is never read as a dictionary.
const MARKER: &str = ".keymux-theory";

/// How far down the steno folder to look for marked folders
const SEARCH_DEPTH: usize = 6;

/// A folder marked as a theory, and whether it is switched on: no part of its path, from the
/// steno folder down, starts with `disabled`.
struct Found {
    path: PathBuf,
    active: bool,
}

/// Every folder in `dir` marked as `theory`, wherever it is and whatever it is called.
fn find_theory(dir: &Path, theory: &str) -> Vec<Found> {
    let mut found = Vec::new();
    look(dir, dir, theory, 0, &mut found);
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn look(root: &Path, dir: &Path, theory: &str, depth: usize, found: &mut Vec<Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path == root.join("cache") {
            continue;
        }
        if std::fs::read_to_string(path.join(MARKER)).is_ok_and(|text| text.trim() == theory) {
            let active = path.strip_prefix(root).is_ok_and(|relative| {
                relative
                    .components()
                    .all(|part| !is_disabled(part.as_os_str()))
            });
            found.push(Found {
                path: path.clone(),
                active,
            });
        }
        if depth < SEARCH_DEPTH {
            look(root, &path, theory, depth + 1, found);
        }
    }
}

/// Mark `folder` as holding `theory`.
fn stamp(folder: &Path, theory: &str) -> std::io::Result<()> {
    let marker = folder.join(MARKER);
    std::fs::write(&marker, format!("{theory}\n"))?;
    hand_to_owner_of_parent(&marker);
    Ok(())
}

/// Folders installed before markers existed are known only by name. Mark the ones that are
/// clearly a theory: the right name, with its main file inside. Anything else is left alone,
/// so a folder of your own that happens to be called `plover` is not taken for the theory.
fn stamp_older_folders(dir: &Path) {
    for (theory, main_file) in [("plover", "main.json"), ("lapwing", "lapwing-base.json")] {
        if !find_theory(dir, theory).is_empty() {
            continue;
        }
        let name = theory_folder(theory);
        for folder in [
            dir.join(&name),
            dir.join(format!("{DISABLED_PREFIX}_{name}")),
        ] {
            if folder.join(main_file).is_file() && stamp(&folder, theory).is_ok() {
                tracing::info!("marked {} as the {theory} theory", folder.display());
            }
        }
    }
}

/// The name a switched-off folder goes back to: `disabled_lapwing` becomes `lapwing`.
fn switched_on_name(name: &str, theory: &str) -> String {
    if !is_disabled(std::ffi::OsStr::new(name)) {
        return name.to_string();
    }
    let rest = name[DISABLED_PREFIX.len()..].trim_start_matches(['_', '-', ' ', '.']);
    if rest.is_empty() {
        theory_folder(theory)
    } else {
        rest.to_string()
    }
}

/// Where a layer reads its dictionaries from when it doesn't list any: the whole steno folder.
#[must_use]
pub fn default_dictionaries(config_dir: &Path) -> Vec<PathBuf> {
    vec![steno_dir(config_dir)]
}

/// Give `path` to whoever owns the folder it is in. The daemon runs as root, so what it
/// creates in someone's home would otherwise be theirs to look at but not to change.
pub fn hand_to_owner_of_parent(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: plain system call with no arguments
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    let (Ok(meta), Ok(parent_meta)) = (std::fs::symlink_metadata(path), std::fs::metadata(parent))
    else {
        return;
    };
    if (meta.uid(), meta.gid()) != (parent_meta.uid(), parent_meta.gid()) {
        let _ = std::os::unix::fs::chown(path, Some(parent_meta.uid()), Some(parent_meta.gid()));
    }
}

/// Make sure the steno folder exists, empty if it is new, and bring an older layout up to date.
///
/// Run whenever keymux starts with a steno layer, so there is always a place to drop a
/// dictionary. Failures are logged, not fatal: steno works without it, just with no dictionary.
pub fn prepare_dir(config_dir: &Path) {
    let dir = steno_dir(config_dir);
    if !dir.is_dir() {
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                hand_to_owner_of_parent(&dir);
                tracing::info!("created {} for steno dictionaries", dir.display());
                // Only when the folder is new: delete the file and it stays deleted
                let user = dir.join(USER_DICTIONARY);
                if std::fs::write(&user, STARTER_DICTIONARY).is_ok() {
                    hand_to_owner_of_parent(&user);
                }
            }
            Err(e) => {
                tracing::warn!("couldn't create {}: {e}", dir.display());
                return;
            }
        }
    }
    // A config that names a file in `dictionaries` keeps it where it is
    let config_text = std::fs::read_to_string(config_dir.join("config.ron")).unwrap_or_default();
    migrate_older_layout(&dir, &config_text);
    stamp_older_folders(&dir);
}

/// Older versions kept Plover's `main.json` at the top of the folder, chose the theory with a
/// `theory` file, and called the user's own dictionary `user.json`. Move those into the
/// layout used now, once. Only renames, and never over anything that is already there, or
/// a file the config (`config_text`) mentions by name, since that path would stop working.
fn migrate_older_layout(dir: &Path, config_text: &str) {
    let theory_file = dir.join("theory");
    let main = dir.join("main.json");
    if (theory_file.is_file() || main.is_file()) && !config_text.contains("main.json") {
        let chose_lapwing = std::fs::read_to_string(&theory_file)
            .is_ok_and(|text| text.trim() == "lapwing");
        let _ = std::fs::remove_file(&theory_file);
        // Plover's folder is the active one unless Lapwing was chosen
        let plover = if chose_lapwing {
            format!("{DISABLED_PREFIX}_{}", theory_folder("plover"))
        } else {
            theory_folder("plover")
        };
        if main.is_file() && !dir.join(&plover).exists() && install_state(dir, "plover") == Install::Missing {
            let moved = std::fs::create_dir(dir.join(&plover))
                .and_then(|()| std::fs::rename(&main, dir.join(&plover).join("main.json")));
            if let Err(e) = moved {
                tracing::warn!("couldn't move {} into {plover}/: {e}", main.display());
            }
        }
        // Without Lapwing chosen, a Lapwing folder left from trying it is switched off
        // Marks first, so the folder is found whatever it is called
        stamp_older_folders(dir);
        if !chose_lapwing {
            for folder in find_theory(dir, "lapwing").into_iter().filter(|folder| folder.active) {
                let name = folder.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let _ = std::fs::rename(&folder.path, folder.path.with_file_name(format!("{DISABLED_PREFIX}_{name}")));
            }
        }
        tracing::info!("steno folder {} moved to the current layout", dir.display());
    }
    let (old, new) = (dir.join("user.json"), dir.join(USER_DICTIONARY));
    if old.is_file() && !new.exists() && !config_text.contains("user.json") {
        // An older setup left `{}` there. The starter entries show what the file is for.
        let empty = std::fs::read_to_string(&old)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text).ok())
            .is_some_and(|entries| entries.is_empty());
        if std::fs::rename(&old, &new).is_ok() && empty {
            let _ = std::fs::write(&new, STARTER_DICTIONARY);
        }
    }
}

/// What a new `00-user.json` holds: two entries every steno theory agrees on, to show the
/// format and to edit.
const STARTER_DICTIONARY: &str = "{\n    \"KAT\": \"cat\",\n    \"-T\": \"the\"\n}\n";

/// A dictionary of this many entries or more counts as a real one. The two starter entries, and
/// a few of your own, don't: steno still tells you to install a theory.
pub const MIN_REAL_DICTIONARY: usize = 100;

/// The dictionary `keymux steno add` writes to. The `00-` puts it first among the files in the
/// steno folder, so what you add overrides everything else.
pub const USER_DICTIONARY: &str = "00-user.json";

/// How a theory stands in the steno folder.
#[derive(Debug, PartialEq, Eq)]
enum Install {
    Active,
    /// Installed, switched off by `disabled` in the name of its folder or one above it
    Parked,
    Missing,
}

fn install_state(dir: &Path, theory: &str) -> Install {
    let found = find_theory(dir, theory);
    if found.iter().any(|folder| folder.active) {
        Install::Active
    } else if found.is_empty() {
        Install::Missing
    } else {
        Install::Parked
    }
}

/// `keymux steno theory [plover|lapwing]`: with no argument, show what is installed. Choosing a
/// theory installs it if it isn't, switches it on, and switches the other off.
pub fn run_theory(config_dir: &Path, choice: Option<&str>) -> Result<()> {
    prepare_dir(config_dir);
    let dir = steno_dir(config_dir);
    let Some(choice) = choice else {
        for theory in THEORIES {
            let found = find_theory(&dir, theory);
            let folder = found
                .iter()
                .find(|folder| folder.active)
                .or_else(|| found.first())
                .and_then(|folder| folder.path.strip_prefix(&dir).ok())
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            let state = match install_state(&dir, theory) {
                Install::Active => format!("in use ({folder}/)"),
                Install::Parked => format!("installed, switched off ({folder}/)"),
                Install::Missing => "not installed".to_string(),
            };
            println!("{theory}: {state}");
        }
        println!("dictionaries are read from {}", dir.display());
        return Ok(());
    };
    if !THEORIES.contains(&choice) {
        bail!("unknown theory `{choice}`: use `plover` or `lapwing`");
    }
    switch_theory(&dir, choice, |folder| match choice {
        "plover" => download_plover(folder),
        _ => download_lapwing(folder),
    })?;
    println!(
        "steno theory is now {choice}. Running keymux picks it up on its own; nothing to restart."
    );
    Ok(())
}

/// Make `choice` the theory in use: bring it back if it was switched off, or fill a new folder
/// with `download` (into a folder that is switched off until it is complete, so a failed
/// download leaves things as they were), then switch off the other theory's folder. Folders are
/// found by their marker, so they can be called anything.
fn switch_theory(dir: &Path, choice: &str, download: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    // Where each folder that has to go off goes: its own name with `disabled_` in front
    let mut parking = Vec::new();
    for other in THEORIES.iter().filter(|other| **other != choice) {
        for folder in find_theory(dir, other).into_iter().filter(|folder| folder.active) {
            let name = folder.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let parked = folder.path.with_file_name(format!("{DISABLED_PREFIX}_{name}"));
            if parked.exists() {
                bail!(
                    "can't switch {other} off: {} is already there. Remove or rename one.",
                    parked.display()
                );
            }
            parking.push((folder.path, parked));
        }
    }

    let found = find_theory(dir, choice);
    if !found.iter().any(|folder| folder.active) {
        if let Some(parked) = found.first() {
            // Back to the name it had before it was switched off
            let name = parked.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let back = if is_disabled(std::ffi::OsStr::new(&name)) {
                parked.path.with_file_name(switched_on_name(&name, choice))
            } else {
                // Switched off by a folder above it, so it comes out to the top
                dir.join(theory_folder(choice))
            };
            if back.exists() {
                bail!("can't switch {choice} on: {} is in the way. Remove or rename it.", back.display());
            }
            std::fs::rename(&parked.path, &back)
                .with_context(|| format!("switching {choice} on"))?;
        } else {
            let active = dir.join(theory_folder(choice));
            if active.exists() {
                bail!(
                    "{} exists but isn't marked as the {choice} theory. Remove or rename it.",
                    active.display()
                );
            }
            let partial = dir.join(format!("{DISABLED_PREFIX}_{}.part", theory_folder(choice)));
            let _ = std::fs::remove_dir_all(&partial);
            std::fs::create_dir_all(&partial)
                .with_context(|| format!("creating {}", partial.display()))?;
            let done = download(&partial)
                .and_then(|()| stamp(&partial, choice).context("marking the folder"))
                .and_then(|()| {
                    std::fs::rename(&partial, &active)
                        .with_context(|| format!("installing {}", active.display()))
                });
            if let Err(e) = done {
                let _ = std::fs::remove_dir_all(&partial);
                return Err(e);
            }
        }
    }
    for (from, to) in parking {
        std::fs::rename(&from, &to).with_context(|| format!("switching {} off", from.display()))?;
    }
    Ok(())
}

/// Download Plover's main dictionary into `folder`.
fn download_plover(folder: &Path) -> Result<()> {
    println!("downloading Plover's main dictionary...");
    let (entries, checksum) = download_dictionary(MAIN_DICT_URL, &folder.join("main.json"))?;
    println!("installed {entries} entries");
    if checksum != MAIN_DICT_SHA256 {
        println!(
            "warning: Plover's dictionary changed since this keymux release.\n  \
             expected sha256 {MAIN_DICT_SHA256}\n  \
             got      sha256 {checksum}\n  \
             It's installed anyway. Report it if strokes stop matching."
        );
    }
    Ok(())
}

/// Download Lapwing's dictionaries into `folder`, checking each one parses.
fn download_lapwing(folder: &Path) -> Result<()> {
    for file in LAPWING_FILES {
        println!("downloading {file}...");
        let (entries, _) = download_dictionary(&format!("{LAPWING_BASE_URL}{file}"), &folder.join(file))?;
        println!("installed {entries} entries");
    }
    println!(
        "Lapwing's inflected forms (accorded, arranging, ...) live in its .modal files, \
         which keymux doesn't read, so those words won't type."
    );
    Ok(())
}

/// `keymux steno setup`: install Plover's dictionary, and show how to turn steno on if the
/// config has no steno layer yet.
pub fn run_setup(config_dir: &Path) -> Result<()> {
    run_theory(config_dir, Some("plover"))?;
    let config_file = config_dir.join("config.ron");
    let has_steno_layer = std::fs::read_to_string(&config_file)
        .map(|text| text.contains("Steno("))
        .unwrap_or(false);
    if has_steno_layer {
        println!("your config already has a steno layer. You're set.");
    } else {
        println!(
            "\nAdd this to the layers in {} to turn steno on:{LAYER_SNIPPET}",
            config_file.display()
        );
        println!("\nThen save the config (or run `keymux reload`).");
    }
    Ok(())
}

/// Download a dictionary to `dest`, checking that it parses. Returns its entry
/// count and SHA-256 in hex. The file is written to a temporary name first, so a
/// failed download never replaces a working dictionary.
fn download_dictionary(url: &str, dest: &Path) -> Result<(usize, String)> {
    let partial = dest.with_extension("json.part");
    let status = Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--max-time", "120", "--output"])
        .arg(&partial)
        .arg(url)
        .status()
        .context("running curl (install curl and try again)")?;
    if !status.success() {
        let _ = std::fs::remove_file(&partial);
        bail!("couldn't download {url}");
    }

    let text = std::fs::read_to_string(&partial)
        .with_context(|| format!("reading {}", partial.display()))?;
    let entries: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&text)
        .context("the download isn't a Plover dictionary, so nothing was installed")?;
    if entries.is_empty() {
        let _ = std::fs::remove_file(&partial);
        bail!("the download has no entries, so nothing was installed");
    }

    let checksum = format!("{:x}", Sha256::digest(text.as_bytes()));
    std::fs::rename(&partial, dest).with_context(|| format!("installing {}", dest.display()))?;
    Ok((entries.len(), checksum))
}

/// Desktop notification for a user, sent through their session like the daemon does.
/// Fire and forget: a missing notifier never affects typing.
pub fn notify_user(uid: u32, title: &str, body: &str) {
    let Some(name) = username(uid) else {
        return;
    };
    let _ = Command::new("runuser")
        .args(["-u", &name, "--", "/usr/bin/notify-send", "-u", "normal"])
        .args([title, body])
        .spawn();
}

fn username(uid: u32) -> Option<String> {
    let output = Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output()
        .ok()?;
    let line = String::from_utf8(output.stdout).ok()?;
    line.split(':').next().map(str::to_string)
}

/// Lets an event happen at most once per period, so repeated key presses don't
/// spam notifications.
#[derive(Debug)]
pub struct Cooldown {
    last: Option<Instant>,
    period: Duration,
}

impl Cooldown {
    #[must_use]
    pub const fn new(period: Duration) -> Self {
        Self { last: None, period }
    }

    /// True if the event may fire now. Starts the cooldown when it does.
    pub fn ready(&mut self, now: Instant) -> bool {
        let quiet = self
            .last
            .is_none_or(|last| now.duration_since(last) >= self.period);
        if quiet {
            self.last = Some(now);
        }
        quiet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn config(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keymux-setup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Stands in for a download: a folder with one file in it
    fn fake_download(folder: &Path) -> Result<()> {
        fs::write(folder.join("fake.json"), "{}").map_err(Into::into)
    }

    #[test]
    fn a_layer_reads_the_whole_steno_folder_by_default() {
        assert_eq!(
            default_dictionaries(Path::new("/home/a/.config/keymux")),
            vec![PathBuf::from("/home/a/.config/keymux/steno")]
        );
    }

    #[test]
    fn starting_with_a_steno_layer_makes_a_steno_folder_with_a_starter_dictionary() {
        let config = config("fresh");
        prepare_dir(&config);
        let dir = steno_dir(&config);
        let entries: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "just the starter");
        let user: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(USER_DICTIONARY)).unwrap()).unwrap();
        assert_eq!(user, serde_json::json!({"KAT": "cat", "-T": "the"}));

        // Deleted, it stays deleted: the folder isn't new any more
        fs::remove_file(dir.join(USER_DICTIONARY)).unwrap();
        fs::write(dir.join("mine.json"), "{}").unwrap();
        prepare_dir(&config);
        assert!(!dir.join(USER_DICTIONARY).exists());
        assert!(dir.join("mine.json").exists());
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn an_older_empty_user_dictionary_gets_the_starter_entries_but_yours_is_kept() {
        let config = config("older-user");
        let dir = steno_dir(&config);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("user.json"), "{}\n").unwrap();
        prepare_dir(&config);
        assert_eq!(
            fs::read_to_string(dir.join(USER_DICTIONARY)).unwrap(),
            STARTER_DICTIONARY
        );
        fs::remove_dir_all(&config).unwrap();

        let config = self::config("older-user-filled");
        let dir = steno_dir(&config);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("user.json"), r#"{"TKOG": "dog"}"#).unwrap();
        prepare_dir(&config);
        assert_eq!(
            fs::read_to_string(dir.join(USER_DICTIONARY)).unwrap(),
            r#"{"TKOG": "dog"}"#
        );
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn choosing_a_theory_installs_it_and_switches_the_other_off_without_deleting() {
        let dir = config("switch");
        switch_theory(&dir, "plover", fake_download).unwrap();
        assert!(dir.join("plover/fake.json").is_file());
        assert_eq!(
            fs::read_to_string(dir.join("plover").join(MARKER)).unwrap().trim(),
            "plover",
            "marked, so it can be found by what it is rather than what it is called"
        );

        switch_theory(&dir, "lapwing", fake_download).unwrap();
        assert!(dir.join("lapwing/fake.json").is_file());
        assert!(!dir.join("plover").exists());
        assert!(dir.join("disabled_plover/fake.json").is_file(), "kept, not deleted");

        // Back again: nothing is downloaded, the folders just trade places
        switch_theory(&dir, "plover", |_| bail!("must not download again")).unwrap();
        assert!(dir.join("plover/fake.json").is_file());
        assert!(dir.join("disabled_lapwing/fake.json").is_file());
        assert!(!dir.join("lapwing").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_theory_folder_is_found_by_its_marker_whatever_it_is_called_or_where_it_is() {
        let dir = config("renamed");
        switch_theory(&dir, "plover", fake_download).unwrap();

        // Renamed to anything, and moved inside another folder
        fs::create_dir_all(dir.join("mine")).unwrap();
        fs::rename(dir.join("plover"), dir.join("mine/10-my-steno")).unwrap();
        assert_eq!(install_state(&dir, "plover"), Install::Active);

        // Switching away switches that folder off under its own name, where it is
        switch_theory(&dir, "lapwing", fake_download).unwrap();
        assert!(dir.join("mine/disabled_10-my-steno/fake.json").is_file());
        assert_eq!(install_state(&dir, "plover"), Install::Parked);

        // And back: it returns to the name it had, with nothing downloaded
        switch_theory(&dir, "plover", |_| bail!("must not download")).unwrap();
        assert!(dir.join("mine/10-my-steno/fake.json").is_file());
        assert_eq!(install_state(&dir, "lapwing"), Install::Parked);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_theory_switched_off_by_a_folder_above_it_comes_back_out_to_the_top() {
        let dir = config("nested-off");
        switch_theory(&dir, "plover", fake_download).unwrap();
        fs::create_dir_all(dir.join("disabled_old")).unwrap();
        fs::rename(dir.join("plover"), dir.join("disabled_old/inside")).unwrap();
        assert_eq!(install_state(&dir, "plover"), Install::Parked);

        switch_theory(&dir, "plover", |_| bail!("must not download")).unwrap();
        assert!(dir.join("plover/fake.json").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_without_a_marker_is_never_taken_for_a_theory() {
        let dir = config("unmarked");
        // Yours, and called plover, with a dictionary in it. Not Plover's main.json.
        fs::create_dir_all(dir.join("plover")).unwrap();
        fs::write(dir.join("plover/mine.json"), "{}").unwrap();
        stamp_older_folders(&dir);
        assert_eq!(install_state(&dir, "plover"), Install::Missing);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_name_that_is_already_taken_by_something_else_stops_the_switch() {
        let dir = config("taken");
        switch_theory(&dir, "plover", fake_download).unwrap();
        // A folder of yours is already sitting where the theory would be switched off to
        fs::create_dir_all(dir.join("disabled_plover")).unwrap();
        let result = switch_theory(&dir, "lapwing", |_| bail!("must not download"));
        assert!(result.unwrap_err().to_string().contains("Remove or rename one"));
        assert!(dir.join("plover/fake.json").is_file(), "nothing was moved");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_download_leaves_everything_as_it_was() {
        let dir = config("failed");
        switch_theory(&dir, "plover", fake_download).unwrap();
        let result = switch_theory(&dir, "lapwing", |folder| {
            fs::write(folder.join("half.json"), "{").unwrap();
            bail!("no network")
        });
        assert!(result.is_err());
        assert!(dir.join("plover/fake.json").is_file(), "the working theory is still on");
        assert!(!dir.join("lapwing").exists());
        assert!(!dir.join("disabled_lapwing.part").exists(), "no half download left");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_from_before_markers_are_marked_once_they_look_like_the_real_thing() {
        let config = config("older-names");
        let dir = steno_dir(&config);
        fs::create_dir_all(dir.join("plover")).unwrap();
        fs::write(dir.join("plover/main.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("disabled_lapwing")).unwrap();
        fs::write(dir.join("disabled_lapwing/lapwing-base.json"), "{}").unwrap();
        prepare_dir(&config);
        assert_eq!(install_state(&dir, "plover"), Install::Active);
        assert_eq!(install_state(&dir, "lapwing"), Install::Parked);

        // Switching to Lapwing switches Plover's folder off under its own name
        switch_theory(&dir, "lapwing", |_| bail!("must not download")).unwrap();
        assert!(dir.join("disabled_plover/main.json").is_file());
        assert!(dir.join("lapwing/lapwing-base.json").is_file(), "back under its own name");
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn an_older_layout_is_moved_into_the_current_one() {
        let config = config("older");
        let dir = steno_dir(&config);
        fs::create_dir_all(dir.join("lapwing")).unwrap();
        fs::write(dir.join("lapwing/lapwing-base.json"), "{}").unwrap();
        fs::write(dir.join("main.json"), "{}").unwrap();
        fs::write(dir.join("theory"), "plover\n").unwrap();
        fs::write(dir.join("user.json"), "{}").unwrap();
        fs::write(dir.join("test.json"), "{}").unwrap();

        prepare_dir(&config);
        assert!(dir.join("plover/main.json").is_file());
        assert!(dir.join("disabled_lapwing/lapwing-base.json").is_file());
        assert!(!dir.join("theory").exists());
        assert!(dir.join("00-user.json").is_file());
        assert!(dir.join("test.json").is_file(), "your own files stay where they are");
        assert!(!dir.join("main.json").exists());
        assert_eq!(install_state(&dir, "plover"), Install::Active);
        assert_eq!(install_state(&dir, "lapwing"), Install::Parked);
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn an_older_layout_that_chose_lapwing_keeps_lapwing_on() {
        let config = config("older-lapwing");
        let dir = steno_dir(&config);
        fs::create_dir_all(dir.join("lapwing")).unwrap();
        fs::write(dir.join("lapwing/lapwing-base.json"), "{}").unwrap();
        fs::write(dir.join("main.json"), "{}").unwrap();
        fs::write(dir.join("theory"), "lapwing\n").unwrap();

        prepare_dir(&config);
        assert!(dir.join("lapwing").is_dir());
        assert!(dir.join("disabled_plover/main.json").is_file());
        assert!(!dir.join("plover").exists());
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn a_file_the_config_names_is_not_moved() {
        let config = config("named");
        let dir = steno_dir(&config);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("main.json"), "{}").unwrap();
        fs::write(dir.join("user.json"), "{}").unwrap();
        fs::write(
            config.join("config.ron"),
            r#"( dictionaries: ["steno/user.json", "steno/main.json"] )"#,
        )
        .unwrap();

        prepare_dir(&config);
        assert!(dir.join("main.json").is_file());
        assert!(dir.join("user.json").is_file());
        assert!(!dir.join("plover").exists());
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn the_current_layout_is_left_alone() {
        let config = config("current");
        let dir = steno_dir(&config);
        fs::create_dir_all(dir.join("plover")).unwrap();
        fs::create_dir_all(dir.join("disabled_lapwing")).unwrap();
        fs::write(dir.join("plover/main.json"), "{}").unwrap();
        fs::write(dir.join("disabled_lapwing/lapwing-base.json"), "{}").unwrap();
        prepare_dir(&config);
        assert!(dir.join("plover/main.json").is_file());
        assert!(dir.join("disabled_lapwing").is_dir());
        assert!(!dir.join("lapwing").exists());
        fs::remove_dir_all(&config).unwrap();
    }

    #[test]
    fn cooldown_suppresses_repeats_within_the_period() {
        let t0 = Instant::now();
        let mut cooldown = Cooldown::new(Duration::from_secs(3));
        assert!(cooldown.ready(t0));
        assert!(!cooldown.ready(t0 + Duration::from_secs(1)));
        assert!(!cooldown.ready(t0 + Duration::from_millis(2999)));
        assert!(cooldown.ready(t0 + Duration::from_secs(3)));
    }
}
