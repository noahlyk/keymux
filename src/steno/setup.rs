//! Getting steno working without hand-editing paths.
//!
//! `keymux steno setup` downloads Plover's dictionary into `<config>/steno/`.
//! Steno layers read from that directory when they don't list dictionaries of
//! their own, so setup is one command and no config edits. When a steno layer is
//! used before anything is installed, the daemon points at this command.

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

/// Lapwing's JSON dictionaries from plover-lapwing-aio, highest priority first. Its
/// `.modal` and `.py` files need Plover's plugin runtime, so they aren't used here.
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

/// Which theory `keymux steno theory` chose, stored in `<config>/steno/theory`.
const THEORY_FILE: &str = "theory";

/// The chosen theory, `plover` unless `keymux steno theory lapwing` was run.
#[must_use]
pub fn read_theory(config_dir: &Path) -> String {
    std::fs::read_to_string(steno_dir(config_dir).join(THEORY_FILE))
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "plover".to_string())
}

/// Dictionaries used when a steno layer doesn't list any: the user's own first,
/// then the chosen theory's: Plover's main dictionary, or Lapwing's set.
#[must_use]
pub fn default_dictionaries(config_dir: &Path) -> Vec<PathBuf> {
    let dir = steno_dir(config_dir);
    let mut paths = vec![dir.join("user.json")];
    if read_theory(config_dir) == "lapwing" {
        paths.extend(LAPWING_FILES.iter().map(|file| dir.join("lapwing").join(file)));
    } else {
        paths.push(dir.join("main.json"));
    }
    paths
}

/// `keymux steno theory [plover|lapwing]`: with no argument, show the current theory.
/// Choosing Lapwing downloads its dictionaries first, so a failed download leaves the
/// current theory in place.
pub fn run_theory(config_dir: &Path, choice: Option<&str>) -> Result<()> {
    let Some(choice) = choice else {
        println!("steno theory: {}", read_theory(config_dir));
        return Ok(());
    };
    let dir = steno_dir(config_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    match choice {
        "plover" => {
            if !dir.join("main.json").exists() {
                bail!("Plover's dictionary isn't installed yet; run `keymux steno setup` first");
            }
        }
        "lapwing" => download_lapwing(&dir)?,
        other => bail!("unknown theory `{other}`: use `plover` or `lapwing`"),
    }
    std::fs::write(dir.join(THEORY_FILE), format!("{choice}\n"))
        .context("saving the theory choice")?;
    println!("steno theory set to {choice}. Run `keymux reload` to apply it.");
    Ok(())
}

/// Download Lapwing's dictionaries into `<steno>/lapwing/`, checking each one parses.
fn download_lapwing(dir: &Path) -> Result<()> {
    let lapwing = dir.join("lapwing");
    std::fs::create_dir_all(&lapwing).with_context(|| format!("creating {}", lapwing.display()))?;
    for file in LAPWING_FILES {
        println!("downloading {file}...");
        let (entries, _) = download_dictionary(&format!("{LAPWING_BASE_URL}{file}"), &lapwing.join(file))?;
        println!("installed {entries} entries");
    }
    println!(
        "Lapwing's inflected forms (accorded, arranging, ...) live in its .modal files, \
         which keymux doesn't read, so those words won't type."
    );
    Ok(())
}

/// `keymux steno setup`
pub fn run_setup(config_dir: &Path) -> Result<()> {
    let dir = steno_dir(config_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let user = dir.join("user.json");
    if !user.exists() {
        std::fs::write(&user, "{}\n").with_context(|| format!("writing {}", user.display()))?;
        println!("created {} for your own entries", user.display());
    }

    let main = dir.join("main.json");
    println!("downloading Plover's main dictionary...");
    let (entries, checksum) = download_dictionary(MAIN_DICT_URL, &main)?;
    println!("installed {entries} entries at {}", main.display());
    if checksum == MAIN_DICT_SHA256 {
        println!("checksum matches this keymux release");
    } else {
        println!(
            "warning: Plover's dictionary changed since this keymux release.\n  \
             expected sha256 {MAIN_DICT_SHA256}\n  \
             got      sha256 {checksum}\n  \
             It's installed anyway. Report it if strokes stop matching."
        );
    }

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
        println!("\nThen run `keymux reload` (or save the config).");
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

    #[test]
    fn default_dictionaries_live_under_steno() {
        let paths = default_dictionaries(Path::new("/home/a/.config/keymux"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/a/.config/keymux/steno/user.json"),
                PathBuf::from("/home/a/.config/keymux/steno/main.json"),
            ]
        );
    }

    #[test]
    fn lapwing_theory_loads_its_files_after_the_user_dictionary() {
        let config = std::env::temp_dir().join(format!("keymux-theory-test-{}", std::process::id()));
        let steno = steno_dir(&config);
        std::fs::create_dir_all(&steno).unwrap();
        std::fs::write(steno.join(THEORY_FILE), "lapwing\n").unwrap();

        let paths = default_dictionaries(&config);
        assert_eq!(read_theory(&config), "lapwing");
        assert_eq!(paths[0], steno.join("user.json"));
        assert_eq!(paths.len(), 1 + LAPWING_FILES.len());
        assert_eq!(paths[1], steno.join("lapwing").join("lapwing-base.json"));
        assert!(!paths.iter().any(|path| path.ends_with("main.json")));

        std::fs::remove_dir_all(&config).unwrap();
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
