//! The `{...}` operators of Plover's translation language, as a port of Plover's meta plugins
//! (`plover/meta/*.py`) and the parser that picks which one a `{...}` is.
//!
//! `{^ing}` is the attach meta with `^ing`, `{-|}` the case meta with `cap_first_word`, and so on.
//! The parser tries the shapes in Plover's order, and the first that fits the whole text wins.

use super::action::{apply_case, has_word_boundary, is_space, rightmost_word, Action, Case, SPACE};
use super::formatter::Context;
use super::layout::Layout;

const ATTACH: char = '^';
const CARRY: &str = "~|";

/// A meta recognised in `{...}`: its name and what follows it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Meta<'a> {
    pub name: String,
    pub param: &'a str,
}

/// A `{:firstup}` or `{:firstup:except=KEYS}` marker in an entry's text: where it starts and ends
/// (byte offsets) and the `KEYS` text of the exception form, `None` for the plain one.
#[must_use]
pub(crate) fn first_up_marker(text: &str) -> Option<(usize, usize, Option<&str>)> {
    const HEAD: &str = "{:firstup";
    text.match_indices(HEAD).find_map(|(start, _)| {
        let rest = &text[start + HEAD.len()..];
        if rest.starts_with('}') {
            return Some((start, start + HEAD.len() + 1, None));
        }
        let keys = rest.strip_prefix(":except=")?;
        let len = keys.find('}')?;
        let end = start + HEAD.len() + ":except=".len() + len + 1;
        Some((start, end, Some(&keys[..len])))
    })
}

/// `text` without its `{:firstup}` / `{:firstup:except=KEYS}` markers.
#[must_use]
pub(crate) fn strip_first_up(text: &str) -> String {
    let mut rest = text.to_string();
    while let Some((start, end, _)) = first_up_marker(&rest) {
        rest.replace_range(start..end, "");
    }
    rest
}

pub(crate) fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// Which meta `meta` (the text between the braces) is, and its argument. `None` when it is
/// nothing Plover knows, which does nothing.
#[must_use]
pub(crate) fn parse_meta(meta: &str) -> Option<Meta<'_>> {
    let found = |name: &str, param: &'static str| {
        Some(Meta {
            name: name.to_string(),
            param,
        })
    };
    // Generic `{:name:argument}`
    if let Some(rest) = meta.strip_prefix(':') {
        let end = rest.find(':').unwrap_or(rest.len());
        if end > 0 {
            let after = &rest[end..];
            let param = after.strip_prefix(':').unwrap_or(after);
            return Some(Meta {
                name: rest[..end].to_string(),
                param,
            });
        }
    }
    if let Some(command) = strip_prefix_ignore_case(meta, "PLOVER:") {
        return Some(Meta {
            name: "command".into(),
            param: command,
        });
    }
    if let Some(combo) = meta.strip_prefix('#') {
        return Some(Meta {
            name: "key_combo".into(),
            param: combo,
        });
    }
    match meta {
        "," | ":" | ";" => {
            return Some(Meta {
                name: "comma".into(),
                param: meta,
            })
        }
        "." | "!" | "?" => {
            return Some(Meta {
                name: "stop".into(),
                param: meta,
            })
        }
        "-|" => return found("case", "cap_first_word"),
        ">" => return found("case", "lower_first_char"),
        "<" => return found("case", "upper_first_word"),
        "*-|" => return found("retro_case", "cap_first_word"),
        "*>" => return found("retro_case", "lower_first_char"),
        "*<" => return found("retro_case", "upper_first_word"),
        "$" => return found("word_end", ""),
        _ => {}
    }
    if let Some(pattern) = meta.strip_prefix('=') {
        return Some(Meta {
            name: "if_next_matches".into(),
            param: pattern,
        });
    }
    if let Some(mode) = strip_prefix_ignore_case(meta, "MODE:") {
        return Some(Meta {
            name: "mode".into(),
            param: mode,
        });
    }
    if let Some(format) = meta
        .strip_prefix("*(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        return Some(Meta {
            name: "retro_currency".into(),
            param: format,
        });
    }
    if let Some(text) = meta.strip_prefix('&') {
        return Some(Meta {
            name: "glue".into(),
            param: text,
        });
    }
    // `^?~|text^?`: carry the capitalization over the text
    let after_attach = meta.strip_prefix(ATTACH).unwrap_or(meta);
    if after_attach.starts_with(CARRY) {
        return Some(Meta {
            name: "carry_capitalize".into(),
            param: meta,
        });
    }
    if meta.starts_with(ATTACH) || meta.ends_with(ATTACH) {
        return Some(Meta {
            name: "attach".into(),
            param: meta,
        });
    }
    None
}

/// The action for `meta`, in the context of what came before. A meta that is unknown, or that
/// has an argument it can't use, does nothing.
#[must_use]
pub fn meta_to_action(meta: &str, ctx: &Context) -> Action {
    let Some(Meta { name, param }) = parse_meta(meta) else {
        return ctx.new_action();
    };
    find_meta(&name).map_or_else(|| ctx.new_action(), |known| (known.run)(ctx, param))
}

/// One meta the engine knows by name, as `{:name:param}` writes it (every meta can be written
/// that way, whatever its short form: `{^ing}` is `{:attach:^ing}`).
///
/// [`KNOWN`] is the one table of them: [`meta_to_action`] runs the entry it finds there, and
/// the dictionary validator ([`super::validate`]) reads the same table for the names to accept,
/// the verbs to check an argument against and the names to suggest, so neither can drift from
/// the other.
pub(crate) struct KnownMeta {
    pub name: &'static str,
    /// What the meta does, given its argument
    pub run: fn(&Context, &str) -> Action,
    /// The words its argument must be one of, empty when it takes anything. `{:tape:togle}` is
    /// no verb of `tape`, so it does nothing
    pub verbs: &'static [&'static str],
    /// Whether the argument may be left out although there are verbs (`{:lookup}`)
    pub bare_ok: bool,
    /// Whether it only works as an entry's whole text (the history macros, which the translator
    /// runs, not the formatter): written inside other text they do nothing
    pub whole_entry: bool,
    /// What is wrong with an argument the verbs don't cover, if anything
    pub check: fn(&str, &Layout) -> Option<String>,
}

const VIEW_VERBS: &[&str] = &["show", "hide", "toggle"];

const fn fine(_: &str, _: &Layout) -> Option<String> {
    None
}

fn nothing(ctx: &Context, _: &str) -> Action {
    ctx.new_action()
}

fn marker(ctx: &Context, _: &str) -> Action {
    ctx.copy_last_action()
}

fn run_command(ctx: &Context, param: &str) -> Action {
    let mut action = ctx.copy_last_action();
    action.command = Some(param.to_string());
    action
}

fn run_key_combo(ctx: &Context, param: &str) -> Action {
    let mut action = ctx.copy_last_action();
    action.combo = Some(param.to_string());
    action
}

fn run_word_end(ctx: &Context, _: &str) -> Action {
    let mut action = ctx.copy_last_action();
    action.word_is_finished = true;
    action
}

/// `{:lookup}` opens the live lookup, as its legacy alias `{PLOVER:lookup}` does (the same
/// engine command, so it is the same stroke class and tape line); `{:lookup:open|close|toggle}`
/// are the overlay controls.
fn run_lookup(ctx: &Context, param: &str) -> Action {
    if param.trim().is_empty() {
        let mut action = ctx.copy_last_action();
        action.command = Some("lookup".to_owned());
        return action;
    }
    super::overlay_control::meta_action(ctx, "lookup", param)
}

fn check_command(param: &str, _: &Layout) -> Option<String> {
    let name = param.split(':').next().unwrap_or_default().trim().to_lowercase();
    (!ENGINE_COMMANDS.contains(&name.as_str())).then(|| format!("`{name}` is not an engine command"))
}

fn check_key_combo(param: &str, _: &Layout) -> Option<String> {
    super::keycombo::key_events(param).err()
}

fn check_case(param: &str, _: &Layout) -> Option<String> {
    Case::parse(param).is_none().then(|| format!("`{param}` is not a case"))
}

fn check_back(param: &str, _: &Layout) -> Option<String> {
    let count = param.trim();
    (!count.is_empty() && !count.parse::<usize>().is_ok_and(|count| count > 0))
        .then(|| format!("`{count}` is not a count of characters"))
}

/// `{:firstup}` or `{:firstup:except=KEYS}`; `KEYS` one stroke of the layout. Anything else is
/// plain `{:firstup}` to the engine (see [`first_up_marker`]).
#[allow(clippy::literal_string_with_formatting_args)] // markers, not formats
fn check_first_up(param: &str, layout: &Layout) -> Option<String> {
    if param.is_empty() {
        return None;
    }
    let Some(keys) = param.strip_prefix("except=") else {
        return Some(format!("`{param}` is not an argument of firstup (only `except=KEYS`)"));
    };
    let keys = keys.trim();
    if keys.is_empty() {
        return Some("the exempt key list is empty, so the entry is plain {:firstup}".to_string());
    }
    layout.parse_stroke(keys).is_none().then(|| {
        format!("`{keys}` is not one stroke of the layout, so the entry is plain {{:firstup}}")
    })
}

fn check_no_argument(param: &str, _: &Layout) -> Option<String> {
    (!param.trim().is_empty()).then(|| format!("takes no argument, but has `{param}`"))
}

fn check_undo_count(param: &str, _: &Layout) -> Option<String> {
    check_back(param, &Layout::plover())
}

fn check_undo_or(param: &str, _: &Layout) -> Option<String> {
    let combo = super::translate::undo_or_combo(param);
    if combo.is_empty() {
        return None;
    }
    super::keycombo::key_events(&combo).err()
}

/// The engine commands, as written after the legacy `{PLOVER:` and with the ones the native metas
/// make. Which have a counterpart in keymux is [`StenoEngine`](super::StenoEngine)'s to say, and
/// [`native_spelling`] gives the native name of those that do.
pub(crate) const ENGINE_COMMANDS: [&str; 15] = [
    "suspend",
    "toggle",
    "resume",
    "lookup",
    "set_config",
    "add_translation",
    "suggestions",
    "configure",
    "focus",
    "quit",
    "deleted",
    super::engine_control::COMMAND,
    super::overlay_control::COMMAND,
    super::run_control::RUN,
    super::run_control::SHELL,
];

/// The legacy commands keymux has no counterpart for: written through the legacy spelling they
/// are accepted and do nothing (`set_config` is the one other legacy-only command, and it does
/// set the options of a session).
pub(crate) const NO_EFFECT: [&str; 6] = ["resume", "add_translation", "suggestions", "configure", "focus", "quit"];

/// The native spelling of an engine command (the text an action carries, or what follows the
/// legacy `{PLOVER:`), or `None` for a command keymux has no native name for.
#[must_use]
#[allow(clippy::literal_string_with_formatting_args)] // spellings, not formats
pub(crate) fn native_spelling(command: &str) -> Option<String> {
    let (name, args) = command.split_once(':').unwrap_or((command, ""));
    let name = name.trim().to_lowercase();
    match name.as_str() {
        "suspend" | "toggle" => Some(format!("{{:engine:{name}}}")),
        "lookup" => Some("{:lookup}".to_string()),
        "deleted" => Some("{:deleted}".to_string()),
        "run" | "shell" => Some(format!("{{:{name}:{args}}}")),
        super::engine_control::COMMAND | super::overlay_control::COMMAND => Some(format!("{{:{args}}}")),
        _ => None,
    }
}

/// How the tape and the overlay write an engine command: its native spelling, a shell command
/// without its command line (it may hold a secret), and a command with no native name as the
/// generic `{:command:NAME}`.
#[must_use]
#[allow(clippy::literal_string_with_formatting_args)] // spellings, not formats
pub(crate) fn display_command(command: &str) -> String {
    if strip_prefix_ignore_case(command, "shell:").is_some() {
        return "{:shell}".to_string();
    }
    native_spelling(command).unwrap_or_else(|| format!("{{:command:{command}}}"))
}

/// Every meta by name. Names are matched without regard to case.
pub(crate) static KNOWN: &[KnownMeta] = &[
    KnownMeta { name: "attach", run: meta_attach, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "carry_capitalize", run: meta_carry_capitalize, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "case", run: meta_case, verbs: &[], bare_ok: true, whole_entry: false, check: check_case },
    KnownMeta { name: "retro_case", run: meta_retro_case, verbs: &[], bare_ok: true, whole_entry: false, check: check_case },
    KnownMeta { name: "comma", run: meta_comma, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "stop", run: meta_stop, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "command", run: run_command, verbs: &[], bare_ok: true, whole_entry: false, check: check_command },
    KnownMeta { name: "glue", run: meta_glue, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "if_next_matches", run: meta_if_next_matches, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "key_combo", run: run_key_combo, verbs: &[], bare_ok: true, whole_entry: false, check: check_key_combo },
    KnownMeta { name: "mode", run: meta_mode, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "retro_currency", run: meta_retro_currency, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "word_end", run: run_word_end, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    // keymux's own: `{:back}` or `{:back:3}` erases that many characters of what was typed
    KnownMeta { name: "back", run: meta_back, verbs: &[], bare_ok: true, whole_entry: false, check: check_back },
    // keymux's own: `{:firstup}` marks a stroke the engine sends when its first key comes up,
    // `{:firstup:except=KEYS}` one that sends when the first key not in KEYS comes up (read by
    // `first_up_marker`). It types nothing and changes nothing around it.
    KnownMeta { name: "firstup", run: marker, verbs: &[], bare_ok: true, whole_entry: false, check: check_first_up },
    // keymux's own stroke classes (see `Translator::stroke`): `{:transparent}` is a stroke the
    // history never sees, `{:barrier}` / `{:nobarrier}` say that a stroke does or does not
    // end the outline context. Like `firstup` they type nothing and change nothing around them.
    KnownMeta { name: "transparent", run: marker, verbs: &[], bare_ok: true, whole_entry: false, check: check_no_argument },
    KnownMeta { name: "barrier", run: marker, verbs: &[], bare_ok: true, whole_entry: false, check: check_no_argument },
    KnownMeta { name: "nobarrier", run: marker, verbs: &[], bare_ok: true, whole_entry: false, check: check_no_argument },
    // Lapwing's stitching: `{:stitch:A}` is a fingerspelled letter that joins with a dash
    KnownMeta { name: "stitch", run: meta_stitch, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    KnownMeta { name: "stitch_last_word", run: meta_stitch_last_word, verbs: &[], bare_ok: true, whole_entry: false, check: fine },
    // keymux's own `{:engine:unload}` (see `engine_control`; it types nothing)
    KnownMeta { name: super::engine_control::NAME, run: super::engine_control::meta_action, verbs: &["unload", "suspend", "toggle"], bare_ok: false, whole_entry: false, check: fine },
    // `{:deleted}` as an entry's whole text hides the outline in every later dictionary (see `dict`)
    KnownMeta { name: "deleted", run: marker, verbs: &[], bare_ok: true, whole_entry: false, check: check_no_argument },
    // keymux's own `{:outline:last}` and `{:outline:next}` (see `outline_control`; they type
    // their text through the engine, not here)
    KnownMeta { name: super::outline_control::NAME, run: super::outline_control::meta_action, verbs: &["last", "next"], bare_ok: false, whole_entry: false, check: fine },
    // keymux's own `{:run:NAME}` (a command from the config) and `{:shell:CMD}` (a command line, only
    // run from a trusted dictionary): see `run_control`. They type nothing.
    KnownMeta { name: super::run_control::RUN, run: super::run_control::meta_run, verbs: &[], bare_ok: true, whole_entry: false, check: super::run_control::check_argument },
    KnownMeta { name: super::run_control::SHELL, run: super::run_control::meta_shell, verbs: &[], bare_ok: true, whole_entry: false, check: super::run_control::check_argument },
    KnownMeta { name: "lookup", run: run_lookup, verbs: &["open", "close", "toggle"], bare_ok: true, whole_entry: false, check: fine },
    // keymux's own runtime overlay controls: `{:overlay:hide}`, `{:tape:clear}`, ...
    // (see `overlay_control`; they type nothing)
    KnownMeta { name: "overlay", run: |ctx, param| super::overlay_control::meta_action(ctx, "overlay", param), verbs: VIEW_VERBS, bare_ok: false, whole_entry: false, check: fine },
    KnownMeta { name: "keyboard", run: |ctx, param| super::overlay_control::meta_action(ctx, "keyboard", param), verbs: VIEW_VERBS, bare_ok: false, whole_entry: false, check: fine },
    KnownMeta { name: "tape", run: |ctx, param| super::overlay_control::meta_action(ctx, "tape", param), verbs: &["show", "hide", "toggle", "clear"], bare_ok: false, whole_entry: false, check: fine },
    // The history macros are run by the translator when they are an entry's whole text (see
    // `mapping_to_macro`); anywhere else they do nothing
    KnownMeta { name: "undo", run: nothing, verbs: &[], bare_ok: true, whole_entry: true, check: check_undo_count },
    KnownMeta { name: "undo_word", run: nothing, verbs: &[], bare_ok: true, whole_entry: true, check: fine },
    KnownMeta { name: "undo_or", run: nothing, verbs: &[], bare_ok: true, whole_entry: true, check: check_undo_or },
    KnownMeta { name: "redo", run: nothing, verbs: &[], bare_ok: true, whole_entry: true, check: check_undo_count },
    KnownMeta { name: "again", run: nothing, verbs: &[], bare_ok: true, whole_entry: true, check: fine },
    KnownMeta { name: "swap", run: nothing, verbs: &[], bare_ok: true, whole_entry: true, check: fine },
];

/// The known meta called `name`, in any letter case.
pub(crate) fn find_meta(name: &str) -> Option<&'static KnownMeta> {
    let name = name.to_lowercase();
    KNOWN.iter().find(|known| known.name == name)
}

/// `{:stitch:word}` or `{:stitch:word:delimiter}`: glued like fingerspelling, and after another
/// glued piece joined to it with the delimiter (a dash unless told otherwise).
fn meta_stitch(ctx: &Context, cmdline: &str) -> Action {
    let mut args = cmdline.split(':');
    let word = args.next().unwrap_or_default();
    let delimiter = args.next().unwrap_or("-");
    let mut action = ctx.new_action();
    action.prev_attach = ctx.last_action.glue || ctx.last_action.next_attach;
    action.glue = true;
    action.text = Some(if ctx.last_action.glue {
        format!("{delimiter}{word}")
    } else {
        word.to_string()
    });
    action
}

/// Join the characters of each word with `delimiter`: `cat` is `c-a-t`.
fn stitch_word(text: &str, delimiter: &str) -> String {
    let mut out = String::new();
    for word in super::action::words(text) {
        let bare = word.trim_end();
        let letters: Vec<String> = bare.chars().map(String::from).collect();
        out.push_str(&letters.join(delimiter));
        out.push_str(&word[bare.len()..]);
    }
    out
}

/// `{:back}` or `{:back:count}`: erase the last character, or `count` of them, as a replaced
/// part of the earlier text. Unlike a Backspace key, keymux then knows what it erased, so
/// undoing the stroke types it back. Text it didn't type can't be erased this way, so with
/// nothing known to erase it presses Backspace instead.
fn meta_back(ctx: &Context, count: &str) -> Action {
    let count = count
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 0)
        .unwrap_or(1);
    let mut action = ctx.copy_last_action();
    let erased = ctx.last_text(count);
    if erased.is_empty() {
        action.combo = Some("BackSpace".to_string());
        return action;
    }
    action.prev_attach = true;
    action.text = Some(String::new());
    action.prev_replace = erased;
    action.word = None;
    action
}

/// `{:stitch_last_word:count:delimiter}`: the last words rewritten with dashes between letters.
fn meta_stitch_last_word(ctx: &Context, cmdline: &str) -> Action {
    let mut args = cmdline.split(':');
    let count = args
        .next()
        .filter(|n| !n.is_empty())
        .map_or(1, |n| n.parse().unwrap_or(1));
    let delimiter = args.next().unwrap_or("-");
    let mut action = ctx.copy_last_action();
    let last = ctx.last_words(count).concat();
    if !last.is_empty() {
        action.text = Some(stitch_word(&last, delimiter));
        action.prev_replace = last;
        action.prev_attach = true;
        action.word = None;
    }
    action
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

fn meta_attach(ctx: &Context, meta: &str) -> Action {
    let mut action = ctx.new_action();
    let mut meta = meta.to_string();
    let mut begin = meta.starts_with(ATTACH);
    let mut end = meta.ends_with(ATTACH);
    if !(begin || end) {
        // If not specified, attach at both ends
        meta = format!("{ATTACH}{meta}{ATTACH}");
        begin = true;
        end = true;
    }
    if begin {
        meta.remove(0);
        action.prev_attach = true;
    }
    if end {
        meta.pop();
        action.next_attach = true;
        action.word_is_finished = false;
    }
    let mut last_word = ctx.last_action.word.clone().unwrap_or_default();
    if meta.is_empty() {
        // An empty connection is a break in the orthography rules: the writer says don't
        // correct the word
        action.orthography = false;
    } else if !last_word.is_empty()
        && !is_space(&meta)
        && ctx.last_action.orthography
        && begin
        && (!end || has_word_boundary(&meta))
    {
        let new_word = ctx.orthography.add_suffix(&last_word, &meta);
        let common = common_prefix_len(&last_word, &new_word);
        let replaced: String = last_word.chars().skip(common).collect();
        action.prev_replace = ctx.last_text(replaced.chars().count());
        last_word = last_word.chars().take(common).collect();
        meta = new_word.chars().skip(common).collect();
    }
    action.text = Some(meta.clone());
    if action.prev_attach {
        action.word = Some(rightmost_word(&format!("{last_word}{meta}")));
    }
    action
}

fn meta_carry_capitalize(ctx: &Context, meta: &str) -> Action {
    // Format: ^~|content^, the attach flags optional
    let mut action = ctx.new_action();
    if ctx.last_action.next_case == Some(Case::CapFirstWord) {
        action.next_case = Some(Case::CapFirstWord);
    }
    let mut meta = meta.to_string();
    let begin = meta.starts_with(ATTACH);
    if begin {
        meta.remove(0);
        action.prev_attach = true;
    }
    meta = meta[CARRY.len().min(meta.len())..].to_string();
    let end = meta.ends_with(ATTACH);
    if end {
        meta.pop();
        action.next_attach = true;
        action.word_is_finished = false;
    }
    if !meta.is_empty() || begin || end {
        action.text = Some(meta);
    }
    action
}

fn meta_case(ctx: &Context, case: &str) -> Action {
    let Some(case) = Case::parse(case) else {
        return ctx.new_action();
    };
    let mut action = ctx.copy_last_action();
    action.next_case = Some(case);
    action
}

fn meta_retro_case(ctx: &Context, case: &str) -> Action {
    let Some(case) = Case::parse(case) else {
        return ctx.new_action();
    };
    let mut action = ctx.copy_last_action();
    action.prev_attach = true;
    if let Some(word) = ctx.last_word() {
        action.text = Some(apply_case(&word, Some(case)));
        action.prev_replace = word;
    } else {
        action.text = Some(String::new());
    }
    action
}

fn meta_comma(ctx: &Context, text: &str) -> Action {
    let mut action = ctx.new_action();
    action.text = Some(text.to_string());
    action.prev_attach = true;
    action
}

fn meta_stop(ctx: &Context, text: &str) -> Action {
    let mut action = ctx.new_action();
    action.prev_attach = true;
    action.text = Some(text.to_string());
    action.next_case = Some(Case::CapFirstWord);
    action
}

fn meta_glue(ctx: &Context, text: &str) -> Action {
    let mut action = ctx.new_action();
    action.glue = true;
    action.text = Some(text.to_string());
    if ctx.last_action.glue {
        action.prev_attach = true;
    }
    action
}

/// Plover splits `pattern/yes/no` on slashes that aren't escaped with a backslash, and then
/// takes the escapes out (`\/` and `\\`).
fn split_if_next(meta: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = meta.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('\\' | '/')) => {
                // An escaped backslash or slash is just that character
                if let Some(escaped) = chars.next() {
                    if let Some(part) = parts.last_mut() {
                        part.push(escaped);
                    }
                }
            }
            '/' if parts.len() < 3 => parts.push(String::new()),
            other => {
                if let Some(part) = parts.last_mut() {
                    part.push(other);
                }
            }
        }
    }
    // Plover drops empty parts before looking at which is which
    parts.retain(|part| !part.is_empty());
    parts
}

fn meta_if_next_matches(ctx: &Context, meta: &str) -> Action {
    let parts = split_if_next(meta);
    let [pattern, yes, no] = parts.as_slice() else {
        return ctx.new_action();
    };
    let alternative = |text: &String| {
        let mut action = ctx.new_action();
        action.text = Some(text.clone());
        action
    };
    Action::look_ahead(pattern.clone(), alternative(yes), alternative(no))
}

fn meta_mode(ctx: &Context, cmdline: &str) -> Action {
    let (mode, arg) = cmdline
        .split_once(':')
        .map_or((cmdline, None), |(mode, arg)| (mode, Some(arg)));
    let mode = mode.to_lowercase();
    let mut action = ctx.copy_last_action();
    if mode == "set_space" {
        action.space_char = arg.unwrap_or("").to_string();
        return action;
    }
    // No argument is allowed for the other modes
    if arg.is_some() {
        return ctx.new_action();
    }
    match mode.as_str() {
        "caps" => action.case = Some(Case::Upper),
        "title" => action.case = Some(Case::Title),
        "lower" => action.case = Some(Case::Lower),
        "snake" => action.space_char = "_".to_string(),
        "camel" => {
            action.case = Some(Case::Title);
            action.space_char = String::new();
            action.next_case = Some(Case::LowerFirstChar);
        }
        "reset" => {
            action.space_char = SPACE.to_string();
            action.case = None;
        }
        "reset_space" => action.space_char = SPACE.to_string(),
        "reset_case" => action.case = None,
        _ => return ctx.new_action(),
    }
    action
}

/// A whole number written with a comma between every three digits, as Python's `{:,}`.
fn group_thousands(digits: &str) -> String {
    let mut out = String::new();
    let len = digits.chars().count();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn format_int(value: i128) -> String {
    let sign = if value < 0 { "-" } else { "" };
    format!(
        "{sign}{}",
        group_thousands(&value.unsigned_abs().to_string())
    )
}

fn format_float(value: f64) -> String {
    // Python's `{:,.2f}`
    let text = format!("{:.2}", value.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, "00"));
    let sign = if value.is_sign_negative() && text != "0.00" {
        "-"
    } else {
        ""
    };
    format!("{sign}{}.{fraction}", group_thousands(whole))
}

fn meta_retro_currency(ctx: &Context, dict_format: &str) -> Action {
    let mut action = ctx.copy_last_action();
    let Some(word) = ctx.last_word() else {
        return action;
    };
    let currency = word.replace(',', "");
    let currency = currency.trim();
    let formatted = if let Ok(whole) = currency.parse::<i128>() {
        format_int(whole)
    } else if let Ok(float) = currency.parse::<f64>() {
        format_float(float)
    } else {
        return action;
    };
    action.prev_attach = true;
    action.prev_replace = word;
    action.text = Some(dict_format.replace('c', &formatted));
    action.word = None;
    action
}

#[cfg(test)]
mod tests {
    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // markers, not formats
    fn the_first_up_markers_are_found_and_stripped_for_display() {
        let text = "{:firstup:except=-FRPBLGTS}{^}{#super(1)}{^}";
        assert_eq!(first_up_marker(text), Some((0, 27, Some("-FRPBLGTS"))));
        assert_eq!(first_up_marker("x{:firstup}"), Some((1, 11, None)));
        assert_eq!(first_up_marker("{:firstupx}{:firstup:other}"), None);
        assert_eq!(strip_first_up(text), "{^}{#super(1)}{^}");
        assert_eq!(strip_first_up("{:firstup}{:transparent}{:firstup}"), "{:transparent}");
        assert_eq!(strip_first_up("{:firstup:except=T}"), "");
    }

    use super::*;

    fn name_and_param(meta: &str) -> Option<(String, String)> {
        parse_meta(meta).map(|m| (m.name, m.param.to_string()))
    }

    #[test]
    fn the_operators_of_the_translation_language_are_told_apart() {
        for (meta, name, param) in [
            ("^", "attach", "^"),
            ("^ing", "attach", "^ing"),
            ("re^", "attach", "re^"),
            ("^-to-^", "attach", "^-to-^"),
            (":attach", "attach", ""),
            (":attach:^ing", "attach", "^ing"),
            (":attach: ", "attach", " "),
            (":attach:", "attach", ""),
            ("-|", "case", "cap_first_word"),
            (":case:upper_first_word", "case", "upper_first_word"),
            ("*>", "retro_case", "lower_first_char"),
            (".", "stop", "."),
            (",", "comma", ","),
            (":", "comma", ":"),
            (":comma::", "comma", ":"),
            ("&a", "glue", "a"),
        ] {
            let got = name_and_param(meta);
            assert_eq!(got, Some((name.to_string(), param.to_string())), "{meta}");
        }
        assert_eq!(
            name_and_param("#Return"),
            Some(("key_combo".into(), "Return".into()))
        );
        assert_eq!(
            name_and_param("PLOVER:TOGGLE"),
            Some(("command".into(), "TOGGLE".into()))
        );
        assert_eq!(
            name_and_param("plover:lookup"),
            Some(("command".into(), "lookup".into()))
        );
        assert_eq!(
            name_and_param("mode:caps"),
            Some(("mode".into(), "caps".into()))
        );
        assert_eq!(
            name_and_param("*($c)"),
            Some(("retro_currency".into(), "$c".into()))
        );
        assert_eq!(
            name_and_param("=a/b/c"),
            Some(("if_next_matches".into(), "a/b/c".into()))
        );
        assert_eq!(
            name_and_param("~|\"^"),
            Some(("carry_capitalize".into(), "~|\"^".into()))
        );
        assert_eq!(
            name_and_param("^~|\""),
            Some(("carry_capitalize".into(), "^~|\"".into()))
        );
        assert_eq!(
            name_and_param("$"),
            Some(("word_end".into(), String::new()))
        );
        assert_eq!(name_and_param("nothing"), None);
    }

    #[test]
    fn if_next_splits_on_unescaped_slashes() {
        assert_eq!(split_if_next("[aeiou]/an/a"), ["[aeiou]", "an", "a"]);
        assert_eq!(split_if_next(r"a\/b/slash/none"), ["a/b", "slash", "none"]);
        assert_eq!(split_if_next(r"\\d/#/_"), [r"\d", "#", "_"]);
    }

    #[test]
    fn currency_is_grouped_like_python() {
        assert_eq!(format_int(1_234_567), "1,234,567");
        assert_eq!(format_int(-1234), "-1,234");
        assert_eq!(format_int(12), "12");
        assert_eq!(format_float(1234.5), "1,234.50");
        assert_eq!(format_float(0.456), "0.46");
        assert_eq!(format_float(-0.001), "0.00");
    }
}
