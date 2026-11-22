//! Running commands from a stroke: `{:run:NAME}`, `{:shell:COMMAND}` and `{PLOVER:SHELL:COMMAND}`.
//!
//! `{:run:NAME}` runs the command `NAME` that the config's `commands` table defines, so a
//! dictionary names commands and only the user's config says what they are. The inline forms
//! carry the command line in the entry itself, and a dictionary is often downloaded, so they run
//! only when the file that supplied the entry is trusted ([`Trust`]); from any other file they are
//! cut out of the entry when it is looked up ([`strip_shells`]), which types and runs nothing.
//!
//! Both run as the user the daemon serves, never as root, through the same path as the `CMD`
//! remap action ([`crate::event_processor::actions::cmd`]), off the typing thread. An entry
//! made only of these is a transparent stroke (see `is_transparent_mapping`): it types nothing,
//! stays out of the history and does not repeat when held.
//!
//! Escaping is Plover's `plover-run-shell`: a literal brace in the command is written `\{` or `\}`.

// The metas it talks about are written `{:name}`, markers and not format strings
#![allow(clippy::literal_string_with_formatting_args)]

use super::action::Action;
use super::formatter::{atom_regex, get_meta, unescape_atom, Context};
use super::meta::parse_meta;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The engine command a `{:run:NAME}` makes, `run:NAME` (also `{PLOVER:RUN:NAME}`).
pub const RUN: &str = "run";

/// The engine command a `{:shell:CMD}` or `{PLOVER:SHELL:CMD}` makes, `shell:CMD`.
pub const SHELL: &str = "shell";

/// Which kind of command an entry's `{...}` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A name from the config's `commands`
    Run,
    /// A command line written in the entry
    Shell,
}

/// A run or shell command found in an entry's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Byte range of the `{...}` in the text
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
    /// The name, or the command line with `\{` `\}` made `{` `}`
    pub arg: String,
}

/// Which command a parsed meta is, and its argument, however it is written: `{:run:x}`,
/// `{:shell:x}`, `{PLOVER:SHELL:x}`, `{:command:shell:x}`.
#[must_use]
pub fn classify(name: &str, param: &str) -> Option<(Kind, String)> {
    let kind_of = |word: &str| match word.trim().to_lowercase().as_str() {
        RUN => Some(Kind::Run),
        SHELL => Some(Kind::Shell),
        _ => None,
    };
    let (kind, arg) = if name.eq_ignore_ascii_case("command") {
        let (head, rest) = param.split_once(':').unwrap_or((param, ""));
        (kind_of(head)?, rest)
    } else if name.eq_ignore_ascii_case(RUN) {
        (Kind::Run, param)
    } else if name.eq_ignore_ascii_case(SHELL) {
        (Kind::Shell, param)
    } else {
        return None;
    };
    Some((kind, if kind == Kind::Run { arg.trim() } else { arg }.to_string()))
}

/// Every run and shell command in an entry's text, in order.
#[must_use]
pub fn found_in(text: &str) -> Vec<Found> {
    atom_regex()
        .find_iter(text)
        .filter_map(|m| {
            let meta = unescape_atom(get_meta(m.as_str())?);
            let parsed = parse_meta(&meta)?;
            let (kind, arg) = classify(&parsed.name, parsed.param)?;
            Some(Found { start: m.start(), end: m.end(), kind, arg })
        })
        .collect()
}

/// Whether `text` could hold a shell command, which is what the lookup path asks first.
fn may_have_shell(text: &str) -> bool {
    text.as_bytes().windows(5).any(|window| window.eq_ignore_ascii_case(b"shell"))
}

/// `text` without its inline shell commands, or `None` when it has none. Nothing left of an entry
/// makes it `{:transparent}`, a stroke that does nothing at all.
#[must_use]
pub fn strip_shells(text: &str) -> Option<String> {
    if !may_have_shell(text) {
        return None;
    }
    let shells: Vec<Found> = found_in(text).into_iter().filter(|f| f.kind == Kind::Shell).collect();
    if shells.is_empty() {
        return None;
    }
    let mut kept = String::with_capacity(text.len());
    let mut at = 0;
    for found in &shells {
        kept.push_str(&text[at..found.start]);
        at = found.end;
    }
    kept.push_str(&text[at..]);
    Some(if kept.trim().is_empty() { "{:transparent}".to_string() } else { kept })
}

/// Whether the entry is nothing but run and shell commands (and `{:firstup}` / `{:transparent}`
/// markers), so its stroke is a control: nothing typed.
#[must_use]
pub fn is_only_commands(text: &str) -> bool {
    let mut commands = 0;
    for atom in atom_regex().find_iter(text) {
        let atom = atom.as_str().trim_matches(' ');
        if atom.is_empty() {
            continue;
        }
        let Some(meta) = get_meta(atom).map(unescape_atom) else {
            return false;
        };
        let Some(parsed) = parse_meta(&meta) else {
            return false;
        };
        if classify(&parsed.name, parsed.param).is_some() {
            commands += 1;
        } else if !["firstup", "transparent"].iter().any(|marker| parsed.name.eq_ignore_ascii_case(marker)) {
            return false;
        }
    }
    commands > 0
}

/// Whether an entry carries one of these metas in their own spelling (`{PLOVER:...}` is caught
/// with the other engine commands), so its stroke never repeats when held.
#[must_use]
pub fn is_in(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("{:run:") || lower.contains("{:shell:")
}

fn meta(kind: &str, ctx: &Context, param: &str) -> Action {
    let mut action = ctx.copy_last_action();
    action.command = Some(format!("{kind}:{param}"));
    action
}

/// The action for `{:run:NAME}`: no text, the engine command `run:NAME`; a missing name does nothing.
#[must_use]
pub fn meta_run(ctx: &Context, param: &str) -> Action {
    if param.trim().is_empty() {
        return ctx.new_action();
    }
    meta(RUN, ctx, param.trim())
}

/// The action for `{:shell:CMD}`: no text, the engine command `shell:CMD`; an empty one does nothing.
/// Whether it may run is decided where the entry is looked up, not here.
#[must_use]
pub fn meta_shell(ctx: &Context, param: &str) -> Action {
    if param.trim().is_empty() {
        return ctx.new_action();
    }
    meta(SHELL, ctx, param)
}

/// What is wrong with the argument of `{:run:}`, `{:shell:}`.
#[must_use]
pub fn check_argument(param: &str, _: &super::layout::Layout) -> Option<String> {
    param.trim().is_empty().then(|| "has no argument, so it does nothing".to_string())
}

/// The dictionary files trusted to run inline shell commands.
///
/// The paths in the steno folder (files or folders) that `trusted_dictionaries` names. A name matches only at the top of the
/// steno folder, so a theory's folder can't carry a trusted name along with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Trust {
    paths: Vec<PathBuf>,
    names: Vec<String>,
}

impl Trust {
    /// The paths `names` stand for in `steno_dir`. A name with a `..` in it trusts nothing.
    #[must_use]
    pub fn new(steno_dir: &Path, names: &[String]) -> Self {
        let mut trust = Self::default();
        for name in names {
            let name = name.trim().trim_end_matches('/');
            let path = Path::new(name);
            if name.is_empty() || path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
                continue;
            }
            trust.names.push(name.to_string());
            trust.paths.push(steno_dir.join(path));
        }
        trust
    }

    /// The default: the user's own file `00-user.json` and the folder `user`.
    #[must_use]
    pub fn standard(steno_dir: &Path) -> Self {
        let names: Vec<String> = crate::config::config::DEFAULT_TRUSTED_DICTIONARIES.iter().map(|n| (*n).to_string()).collect();
        Self::new(steno_dir, &names)
    }

    /// The trust a config sets, for the steno folder in `config_dir`.
    #[must_use]
    pub fn of_config(config: &crate::config::Config, config_dir: &Path) -> Self {
        let steno_dir = super::setup::steno_dir(config_dir);
        config.trusted_dictionaries.as_ref().map_or_else(|| Self::standard(&steno_dir), |names| Self::new(&steno_dir, names))
    }

    /// Whether `file` is trusted: it is a named file, or inside a named folder.
    #[must_use]
    pub fn is_trusted(&self, file: &Path) -> bool {
        self.paths.iter().any(|path| file.starts_with(path))
    }

    /// The names, for a message.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.names.is_empty() {
            "none".to_string()
        } else {
            self.names.join(", ")
        }
    }
}

/// Something that runs a command line for the user. The daemon's is [`UserRunner`]; tests give one
/// that only records, so they never start a process.
pub trait Runner: Send + Sync {
    /// Start `command` (`~` expanded, through a shell when it needs one) and return at once.
    fn run(&self, command: &str);
}

/// Runs commands as the user with id `uid`, as `CMD` does, each on a thread of its own. With no
/// uid, or root, or a user that can't be looked up, it runs nothing and logs why.
pub struct UserRunner {
    pub config_dir: PathBuf,
    pub uid: Option<u32>,
}

impl Runner for UserRunner {
    fn run(&self, command: &str) {
        let (command, dir, uid) = (command.to_string(), self.config_dir.clone(), self.uid);
        std::thread::spawn(move || {
            if let Err(why) = crate::event_processor::actions::cmd::run_as_user(&command, &dir, uid) {
                tracing::error!("steno command `{command}` not run: {why}");
            }
        });
    }
}

/// What a layer runs commands with: the config's named commands, the runner, and the names it
/// has already complained about.
#[derive(Clone)]
pub struct Launcher {
    commands: Arc<HashMap<String, String>>,
    runner: Arc<dyn Runner>,
    unknown: HashSet<String>,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launcher").field("commands", &self.commands.keys().collect::<Vec<_>>()).finish_non_exhaustive()
    }
}

impl Launcher {
    #[must_use]
    pub fn new(commands: HashMap<String, String>, runner: Arc<dyn Runner>) -> Self {
        Self { commands: Arc::new(commands), runner, unknown: HashSet::new() }
    }

    /// `{:run:NAME}`: the named command, or one error log for a name that isn't defined.
    pub fn run_named(&mut self, name: &str) {
        let name = name.trim();
        match self.commands.get(name) {
            Some(command) => self.runner.run(command),
            None if self.unknown.insert(name.to_string()) => {
                let mut known: Vec<&str> = self.commands.keys().map(String::as_str).collect();
                known.sort_unstable();
                tracing::error!("steno `{{:run:{name}}}`: no command `{name}` in the config's commands ({})", known.join(", "));
            }
            None => {}
        }
    }

    /// How many different unknown names have been logged.
    #[cfg(test)]
    pub(crate) fn unknown_count(&self) -> usize {
        self.unknown.len()
    }

    /// `{:shell:CMD}`: the command line as written.
    pub fn run_shell(&self, command: &str) {
        if !command.trim().is_empty() {
            self.runner.run(command.trim());
        }
    }
}

/// Everything a steno layer needs to run commands and to judge which dictionaries may.
#[derive(Clone)]
pub struct Runtime {
    pub trust: Trust,
    pub launcher: Launcher,
}

impl Runtime {
    /// For the session user `uid`, from the config.
    #[must_use]
    pub fn for_user(config: &crate::config::Config, config_dir: &Path, uid: u32) -> Self {
        let runner = Arc::new(UserRunner { config_dir: config_dir.to_path_buf(), uid: Some(uid) });
        Self { trust: Trust::of_config(config, config_dir), launcher: Launcher::new(config.commands.clone(), runner) }
    }

    /// No commands, the standard trust, and a runner that refuses: what a layer built without a
    /// config has.
    #[must_use]
    pub fn standard(config_dir: &Path) -> Self {
        let runner = Arc::new(UserRunner { config_dir: config_dir.to_path_buf(), uid: None });
        Self { trust: Trust::standard(&super::setup::steno_dir(config_dir)), launcher: Launcher::new(HashMap::new(), runner) }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked to run.
    #[derive(Default)]
    pub struct Recorder(pub Mutex<Vec<String>>);

    impl Runner for Recorder {
        fn run(&self, command: &str) {
            self.0.lock().unwrap().push(command.to_string());
        }
    }

    impl Recorder {
        pub fn ran(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    #[test]
    fn every_spelling_of_a_command_is_found_with_its_escapes_undone() {
        let text = r"a{:run: password }{:shell:echo \{x\}}{PLOVER:SHELL:ls -l}{:command:shell:id}{plover:run:b}";
        let found: Vec<(Kind, String)> = found_in(text).into_iter().map(|f| (f.kind, f.arg)).collect();
        assert_eq!(
            found,
            [
                (Kind::Run, "password".to_string()),
                (Kind::Shell, "echo {x}".to_string()),
                (Kind::Shell, "ls -l".to_string()),
                (Kind::Shell, "id".to_string()),
                (Kind::Run, "b".to_string()),
            ]
        );
        assert!(found_in("shell{:tape:toggle}{^}run").is_empty(), "plain text and other metas are not commands");
    }

    #[test]
    fn stripping_cuts_out_only_the_shell_commands() {
        assert_eq!(strip_shells("hi{:shell:rm -rf x} there").as_deref(), Some("hi there"));
        assert_eq!(strip_shells("{PLOVER:SHELL:x}").as_deref(), Some("{:transparent}"));
        assert_eq!(strip_shells("{:run:a}{:shell:x}").as_deref(), Some("{:run:a}"));
        assert_eq!(strip_shells("{:run:a}"), None);
        assert_eq!(strip_shells("the shell"), None);
    }

    #[test]
    fn an_entry_of_only_commands_is_a_control() {
        assert!(is_only_commands("{:run:a}"));
        assert!(is_only_commands("{:firstup}{:run:a}{PLOVER:SHELL:x}"));
        assert!(!is_only_commands("{:run:a}text"));
        assert!(!is_only_commands("{:run:a}{^}"));
        assert!(!is_only_commands("{:firstup}"));
    }

    #[test]
    fn trust_names_files_and_folders_at_the_top_of_the_steno_folder() {
        let steno = Path::new("/c/steno");
        let trust = Trust::standard(steno);
        assert!(trust.is_trusted(Path::new("/c/steno/00-user.json")));
        assert!(trust.is_trusted(Path::new("/c/steno/user/any/deeper.json")));
        assert!(!trust.is_trusted(Path::new("/c/steno/lyk/00-user.json")), "a theory can't bring the name along");
        assert!(!trust.is_trusted(Path::new("/c/steno/lyk/user/x.json")));
        assert!(!trust.is_trusted(Path::new("/c/steno/user-extra/x.json")));
        let own = Trust::new(steno, &["mine.json".to_string(), "../x".to_string(), "team/".to_string()]);
        assert!(own.is_trusted(Path::new("/c/steno/team/a.json")) && own.is_trusted(Path::new("/c/steno/mine.json")));
        assert!(!own.is_trusted(Path::new("/c/steno/00-user.json")), "a set list replaces the default");
        assert_eq!(own.describe(), "mine.json, team");
    }

    #[test]
    fn the_config_sets_the_commands_and_the_trust_and_a_new_config_replaces_both() {
        let parse = |text: &str| ron::from_str::<crate::config::Config>(text).unwrap();
        let first = parse(r#"(commands: { "password": "~/p.sh" })"#);
        let second = parse(r#"(commands: { "other": "o" }, trusted_dictionaries: Some(["mine.json", "team/"]))"#);
        let dir = Path::new("/c");
        let (one, two) = (Runtime::for_user(&first, dir, 1000), Runtime::for_user(&second, dir, 1000));
        assert_eq!(one.launcher.commands.get("password").map(String::as_str), Some("~/p.sh"));
        assert_eq!(two.launcher.commands.keys().collect::<Vec<_>>(), ["other"], "a reload builds from the new table");
        assert_eq!(one.trust.describe(), "00-user.json, user");
        assert_eq!(two.trust.describe(), "mine.json, team");
        let none = Runtime::for_user(&parse("(trusted_dictionaries: Some([]))"), dir, 1000);
        assert_eq!(none.trust.describe(), "none", "an empty list trusts nothing");
        assert!(parse("()").commands.is_empty());
    }

    #[test]
    fn an_unknown_name_is_logged_once_and_runs_nothing() {
        let recorder = Arc::new(Recorder::default());
        let mut launcher = Launcher::new(HashMap::from([("a".to_string(), "do-a".to_string())]), recorder.clone());
        launcher.run_named("nope");
        launcher.run_named("nope");
        launcher.run_named(" a ");
        launcher.run_shell("  echo hi ");
        launcher.run_shell(" ");
        assert_eq!(recorder.ran(), ["do-a", "echo hi"]);
        assert_eq!(launcher.unknown_count(), 1);
    }
}
