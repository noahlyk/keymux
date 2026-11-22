//! Turns strokes into translations and translations into typing: a port of Plover's
//! `plover/translation.py` and its macros, so a dictionary written for Plover behaves the same.
//!
//! Each stroke is translated greedily. The latest strokes are tried together with earlier ones
//! to find the longest outline in the dictionary, and a longer match replaces the translations
//! it covers (`cat` typed, then `HROG`, becomes `catalog`). Undoing a translation puts back what
//! it replaced. Formatting the translations into text is [`super::formatter`]'s job.

use super::action::Action;
use super::dict::Dictionary;
use super::formatter::{Formatter, Op, Options};
use super::journal::{self, History, MachineId, Origin, SharedHistory};
use super::keycombo::key_events;
use super::layout::render_stroke;
use super::plugin::{Before, Chain, CONTEXT_BYTES};
use super::strokes::StrokeLog;
use super::worker::Budget;
use crate::keycode::KeyCode;
use smallvec::SmallVec;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long a stroke waits on a plugin's reading before it is typed without it.
///
/// The config can say otherwise (`plugin_deadline_ms`). Tests, which run many at once on a busy
/// machine, wait far longer unless they ask for a deadline, so none is late by accident.
pub const DEFAULT_PLUGIN_DEADLINE: Duration = if cfg!(test) {
    Duration::from_secs(10)
} else {
    Duration::from_millis(30)
};

/// A stroke typed without a plugin's reading, which came too late: put right once it lands,
/// while the stroke's translation is still the last thing typed.
#[derive(Debug, Clone)]
struct Late {
    stroke: u32,
    /// The outline the plugin was late with
    strokes: Vec<u32>,
    /// The text before that outline, as the plugin was asked it
    before: String,
    /// The strokes before it, as the plugin was asked them: the stroke log, and where in it the
    /// outline starts
    log: StrokeLog,
    base: u64,
    /// Where the text before the stroke ended on screen (see [`Translator::stroke_start`])
    start: usize,
    /// What the stroke made without it
    translation: Translation,
}

/// An outline being tried against the dictionary. The search tries many per stroke typed, and
/// nearly all are short, so they live on the stack.
type Strokes = SmallVec<[u32; 8]>;

/// The stroke that undoes the last translation: the star on its own.
pub(crate) const UNDO_STROKE: u32 = 1 << 9;

/// The star key of a stroke.
const STAR: u32 = 1 << 9;

/// Keys that can be folded into a stroke as a suffix when the stroke has no entry of its own:
/// `-Z`, `-D`, `-S`, `-G`, in the order Plover tries them.
pub(crate) const SUFFIX_KEYS: [u32; 4] = [1 << 21, 1 << 20, 1 << 19, 1 << 17];

/// How many translations stay undoable at least. Plover's default.
pub(crate) const UNDO_LENGTH: usize = 100;

/// What undo does when there is nothing left of the steno output to take back: delete a word
/// with the keys that do it.
const BACK_STRING: &str = "{#Control_L(BackSpace)}{^}";

/// A sequence of strokes and what they translate to.
#[derive(Debug, Clone)]
pub struct Translation {
    pub strokes: Vec<u32>,
    /// The dictionary's translation, or `None` for strokes that matched nothing
    pub english: Option<String>,
    /// The translations this one replaced, put back if it is undone
    pub replaced: Vec<Self>,
    /// What the formatter made of it
    pub formatting: Vec<Action>,
    pub is_retrospective_command: bool,
    /// Its engine command (`{:lookup}`, ...) already ran. A command runs once, when the
    /// stroke that wrote it is struck, and never again: not when the translation is put back by
    /// redo, nor when undoing a longer entry puts back the translations it replaced. The
    /// formatter sets this the first time it sends the command and drops the command afterwards.
    pub command_ran: bool,
    /// Which set of dictionaries made it
    pub machine: MachineId,
    /// Strokes, or text that was typed or erased by hand
    pub origin: Origin,
    /// Where the text before it ends on screen (a byte offset into the history's `screen`)
    /// when it was made: what a plugin is told precedes an outline starting here. `None` when
    /// not known.
    pub begins: Option<usize>,
}

impl Translation {
    #[must_use]
    pub const fn new(strokes: Vec<u32>, english: Option<String>) -> Self {
        Self {
            strokes,
            english,
            replaced: Vec::new(),
            formatting: Vec::new(),
            is_retrospective_command: false,
            command_ran: false,
            machine: 0,
            origin: Origin::Stroke,
            begins: None,
        }
    }

    /// The strokes as Plover writes them.
    #[must_use]
    pub fn rtfcre(&self) -> Vec<String> {
        self.strokes
            .iter()
            .map(|&stroke| render_stroke(stroke))
            .collect()
    }

    fn same(&self, other: &Self) -> bool {
        self.strokes == other.strokes && self.english == other.english
    }

    /// Whether undoing it takes anything back: it typed something, or deleted something, or
    /// replaced other translations.
    #[must_use]
    pub(crate) fn has_undo(&self) -> bool {
        if self.formatting.is_empty() || !self.replaced.is_empty() {
            return true;
        }
        self.formatting.iter().any(|a| {
            let a = a.effective();
            a.text.as_deref().is_some_and(|t| !t.is_empty()) || !a.prev_replace.is_empty()
        })
    }
}

/// One thing a stroke asks of the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputOp {
    /// Delete this many characters before the cursor
    Backspaces(usize),
    /// Type this text
    Text(String),
    /// Press and release keys: `(key, true)` is a press and `(key, false)` a release
    Keys(Vec<(KeyCode, bool)>),
    /// A command for the steno engine (`toggle` for `{:engine:toggle}`)
    Command(String),
}

/// What the caller should do with the keyboard after a stroke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StenoOutput {
    /// Type this text
    Type(String),
    /// Delete `backspaces` characters, then type `text`
    Retype { backspaces: usize, text: String },
    /// Several things in order: text, key presses, engine commands
    Ops(Vec<OutputOp>),
}

impl StenoOutput {
    /// The simplest output for `ops`, or `None` if there is nothing to do.
    #[must_use]
    pub fn from_ops(ops: Vec<OutputOp>) -> Option<Self> {
        // Text right after text is one piece of text, and so are deletions
        let mut merged: Vec<OutputOp> = Vec::with_capacity(ops.len());
        for op in ops {
            match (merged.last_mut(), op) {
                (Some(OutputOp::Text(before)), OutputOp::Text(after)) => before.push_str(&after),
                (Some(OutputOp::Backspaces(before)), OutputOp::Backspaces(after)) => {
                    *before += after;
                }
                (_, op) => merged.push(op),
            }
        }
        let ops = merged;
        match ops.as_slice() {
            [] => None,
            [OutputOp::Text(text)] => Some(Self::Type(text.clone())),
            [OutputOp::Backspaces(backspaces)] => Some(Self::Retype {
                backspaces: *backspaces,
                text: String::new(),
            }),
            [OutputOp::Backspaces(backspaces), OutputOp::Text(text)] => Some(Self::Retype {
                backspaces: *backspaces,
                text: text.clone(),
            }),
            _ => Some(Self::Ops(ops)),
        }
    }

    /// The same thing as a list of operations.
    #[must_use]
    pub fn into_ops(self) -> Vec<OutputOp> {
        match self {
            Self::Type(text) => vec![OutputOp::Text(text)],
            Self::Retype { backspaces, text } => {
                let mut ops = Vec::new();
                if backspaces > 0 {
                    ops.push(OutputOp::Backspaces(backspaces));
                }
                if !text.is_empty() {
                    ops.push(OutputOp::Text(text));
                }
                ops
            }
            Self::Ops(ops) => ops,
        }
    }
}

/// The key combination `{:undo_or:...}` falls back to, in Plover's key-name syntax. The
/// canonical form is `Control_L(BackSpace)`; `ctrl+backspace` style (names joined by `+`,
/// the last one pressed while the others are held) is accepted as a shorthand, with `ctrl`,
/// `shift`, `alt` and `super` standing for their left-hand keys.
pub(crate) fn undo_or_combo(argument: &str) -> String {
    let argument = argument.trim();
    if !argument.contains('+') || argument.contains(['(', ' ']) || argument == "+" {
        return argument.to_string();
    }
    let names: Vec<String> = argument
        .split('+')
        .map(|name| match name.trim().to_lowercase().as_str() {
            "ctrl" | "control" => "Control_L".to_string(),
            "shift" => "Shift_L".to_string(),
            "alt" => "Alt_L".to_string(),
            "super" | "win" | "meta" => "Super_L".to_string(),
            other => other.to_string(),
        })
        .collect();
    let Some((last, held)) = names.split_last() else {
        return String::new();
    };
    let mut combo = String::new();
    for name in held {
        combo.push_str(name);
        combo.push('(');
    }
    combo.push_str(last);
    combo.push_str(&")".repeat(held.len()));
    combo
}

/// Whether a dictionary entry makes a transparent stroke: it carries `{:transparent}`, it is an
/// overlay control (`{:overlay:toggle}`, ...), or it is
/// nothing but the command that opens the live lookup (`{:lookup}`; `{PLOVER:lookup}` is its legacy alias).
#[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
pub(crate) fn is_transparent_mapping(text: &str) -> bool {
    text.contains("{:transparent}")
        || text.trim().eq_ignore_ascii_case("{plover:lookup}")
        || text.trim().eq_ignore_ascii_case("{:lookup}")
        || super::overlay_control::is_in(text)
        || super::engine_control::is_transparent(text)
        || super::outline_control::is_in(text)
        || super::run_control::is_only_commands(text)
}

/// The keys of Plover's key-name set that move the cursor or delete text around it. A key
/// combination with one of them is a barrier; Return, Tab and the letters are not.
const CURSOR_KEYS: [&str; 14] = [
    "left",
    "right",
    "up",
    "down",
    "home",
    "end",
    "page_up",
    "page_down",
    "prior",
    "next",
    "backspace",
    "delete",
    "kp_home",
    "kp_end",
];

/// Whether a translation ends the outline context: a key combination that moves the cursor or
/// deletes text, `{:back}`, or an entry that says `{:barrier}`; `{:nobarrier}` says it is not
/// one even so.
#[allow(clippy::literal_string_with_formatting_args)] // a marker, not a format
fn is_barrier(translation: &Translation) -> bool {
    let english = translation.english.as_deref().unwrap_or_default();
    if english.contains("{:nobarrier}") {
        return false;
    }
    if english.contains("{:barrier}") || english.contains("{:back") {
        return true;
    }
    translation.formatting.iter().any(|action| {
        action.effective().combo.as_deref().is_some_and(|combo| {
            combo
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|name| CURSOR_KEYS.iter().any(|key| name.eq_ignore_ascii_case(key)))
        })
    })
}

/// Whether a translation only types whitespace (a space, a newline, a tab): it separates what
/// is before it from a continuation after it.
fn is_separator(translation: &Translation) -> bool {
    let mut typed = false;
    for action in &translation.formatting {
        if let Some(text) = action.effective().text.as_deref().filter(|t| !t.is_empty()) {
            if !text.trim().is_empty() {
                return false;
            }
            typed = true;
        }
    }
    typed
}

/// The names of the macros a dictionary entry can run (`=undo`, `{:redo}`, `{*}`'s
/// `retro_toggle_asterisk`): defined once here, so `translate_macro` can only match names that
/// are in [`macro_name::ALL`], which the dictionary validator reads to tell a macro from a typo.
macro_rules! macro_names {
    ($($id:ident = $name:literal),* $(,)?) => {
        pub(crate) mod macro_name {
            $(pub const $id: &str = $name;)*
            pub const ALL: &[&str] = &[$($name),*];
        }
    };
}

macro_names! {
    UNDO = "undo",
    UNDO_WORD = "undo_word",
    UNDO_OR = "undo_or",
    REDO = "redo",
    AGAIN = "again",
    SWAP = "swap",
    REPEAT_LAST_STROKE = "repeat_last_stroke",
    RETRO_TOGGLE_ASTERISK = "retro_toggle_asterisk",
    REPEAT_LAST_TRANSLATION = "repeat_last_translation",
    REPEAT_LAST_WORD = "repeat_last_word",
    REPEAT_LAST_FRAGMENT = "repeat_last_fragment",
    REPEAT_LAST_CHARACTER = "repeat_last_character",
    RETRO_DELETE_SPACE = "retro_delete_space",
    RETROSPECTIVE_DELETE_SPACE = "retrospective_delete_space",
    RETRO_INSERT_SPACE = "retro_insert_space",
    RETROSPECTIVE_INSERT_SPACE = "retrospective_insert_space",
    RETROSPECTIVE_TOGGLE_ASTERISK = "retrospective_toggle_asterisk",
}

/// Plover's `Macro`: a translation that is an action on the translator, not text.
pub(crate) struct Macro {
    pub name: String,
    pub stroke: u32,
    pub cmdline: String,
}

/// The macro a mapping stands for. Nothing mapped is the undo stroke's macro when the stroke is
/// the undo stroke.
pub(crate) fn mapping_to_macro(mapping: Option<&str>, stroke: u32) -> Option<Macro> {
    let Some(mapping) = mapping else {
        return (stroke == UNDO_STROKE).then(|| Macro {
            name: "undo".into(),
            stroke,
            cmdline: String::new(),
        });
    };
    let legacy = match mapping {
        "{*}" => Some("retro_toggle_asterisk"),
        "{*!}" => Some("retro_delete_space"),
        "{*?}" => Some("retro_insert_space"),
        "{*+}" => Some("repeat_last_stroke"),
        _ => None,
    };
    if let Some(name) = legacy {
        return Some(Macro {
            name: name.into(),
            stroke,
            cmdline: String::new(),
        });
    }
    // keymux's own, also written like its metas: `{:redo}`, `{:undo:3}`
    if let Some(inner) = mapping
        .strip_prefix("{:")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        let (name, cmdline) = inner.split_once(':').unwrap_or((inner, ""));
        // `{:undo_or_ctrl+backspace}`: the combination written into the name
        let (name, cmdline) = match name.strip_prefix("undo_or_") {
            Some(combo) if cmdline.is_empty() => ("undo_or", combo),
            _ => (name, cmdline),
        };
        // The macros keymux adds to Plover's, which work on the history as a whole: undo some or a
        // word, redo, say it again, swap the last two
        if super::meta::find_meta(name).is_some_and(|known| known.whole_entry) {
            return Some(Macro {
                name: name.to_string(),
                stroke,
                cmdline: cmdline.to_string(),
            });
        }
    }
    // `=name` or `=name:arguments`
    let rest = mapping.strip_prefix('=')?;
    let name_len = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if name_len == 0 || !(name_len == rest.len() || rest[name_len..].starts_with(':')) {
        return None;
    }
    Some(Macro {
        name: rest[..name_len].to_string(),
        stroke,
        cmdline: rest[name_len..]
            .strip_prefix(':')
            .unwrap_or_default()
            .to_string(),
    })
}

/// Converts a stream of strokes into typing.
#[derive(Debug)]
pub struct Translator {
    dict: Arc<Chain>,
    /// Which set of dictionaries this is, stamped on what it makes
    machine: MachineId,
    /// What every translator of the user shares: what was typed, and what can be undone
    shared: SharedHistory,
    /// The shared history while a stroke is being translated, and empty otherwise
    hist: History,
    unmatched: bool,
    /// Whether the last stroke was a transparent one (see [`Self::stroke`])
    transparent: bool,
    /// Whether the stroke classes (`{:transparent}`, the lookup command's transparency, and
    /// barriers) are honoured; `disable_transparent` turns them off
    classes_enabled: bool,
    /// Whether undo stops at a barrier (`undo_stops_at_barrier`)
    undo_stops_at_barrier: bool,
    /// How long a stroke waits on plugins
    plugin_deadline: Duration,
    /// The stroke's own wait on plugins, and what they read
    budget: Budget,
    /// The last stroke, typed without a plugin's reading that was late
    late: Option<Late>,
    /// The plugin, and its reading, that the last stroke typed, if a plugin's reading is what it
    /// typed
    plugin_read: Option<(String, String)>,
    /// Where the text before the stroke being translated ends on screen (a byte offset into the
    /// history's `screen`): what a plugin is told precedes an outline starting with it
    stroke_start: usize,
}

/// A copy of a translator has a copy of the history too, so trying strokes on it (a lookup
/// does) doesn't touch what the original has typed.
impl Clone for Translator {
    fn clone(&self) -> Self {
        let mut history = journal::lock(&self.shared).clone();
        history.strokes = history.strokes.copy();
        Self {
            dict: Arc::clone(&self.dict),
            machine: self.machine,
            shared: Arc::new(std::sync::Mutex::new(history)),
            hist: History::default(),
            unmatched: self.unmatched,
            transparent: self.transparent,
            classes_enabled: self.classes_enabled,
            undo_stops_at_barrier: self.undo_stops_at_barrier,
            plugin_deadline: self.plugin_deadline,
            budget: Budget::default(),
            late: None,
            plugin_read: None,
            stroke_start: 0,
        }
    }
}

impl Translator {
    /// A translator with keymux's defaults: the first word attaches to what is there.
    #[must_use]
    pub fn new(dict: Dictionary) -> Self {
        Self::with_shared(
            Arc::new(Chain::from(dict)),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    /// A translator over a dictionary chain others share, with these options. Its history is
    /// its own.
    #[must_use]
    pub(crate) fn with_shared(dict: Arc<Chain>, options: Options) -> Self {
        Self::with_history(dict, journal::new_shared(Formatter::new(options)), 0)
    }

    /// A translator over the history `shared`, which other translators write to as well.
    #[must_use]
    pub fn with_history(dict: Arc<Chain>, shared: SharedHistory, machine: MachineId) -> Self {
        Self {
            dict,
            machine,
            shared,
            hist: History::default(),
            unmatched: false,
            transparent: false,
            classes_enabled: true,
            undo_stops_at_barrier: true,
            plugin_deadline: DEFAULT_PLUGIN_DEADLINE,
            budget: Budget::default(),
            late: None,
            plugin_read: None,
            stroke_start: 0,
        }
    }

    /// How long a stroke waits on a plugin's reading before it is typed without it (see
    /// [`Self::settle_late`]).
    pub const fn set_plugin_deadline(&mut self, deadline: Duration) {
        self.plugin_deadline = deadline;
    }

    /// Whether the last stroke was typed without a plugin's reading that came too late, and
    /// is waiting for it.
    #[must_use]
    pub const fn has_late(&self) -> bool {
        self.late.is_some()
    }

    /// Whether the reading the last stroke waits for is in (or nothing more is coming), so
    /// [`Self::settle_late`] would not wait.
    #[must_use]
    pub fn late_ready(&self) -> bool {
        self.late.as_ref().is_some_and(|late| {
            self.dict.settled(
                &late.strokes,
                Before {
                    text: &late.before,
                    base: late.base,
                    log: Some(&late.log),
                },
            )
        })
    }

    /// The plugin, and its reading, that the last stroke (or the last [`Self::settle_late`])
    /// typed, if a plugin's fallback reading is what it typed.
    #[must_use]
    pub fn plugin_read(&self) -> Option<(String, String)> {
        self.plugin_read.clone()
    }

    /// Put right a stroke typed without a plugin's reading that came too late (see
    /// [`Self::has_late`]): if what it made is still the last thing typed, take it back and
    /// translate the stroke again, the reading now in hand, and say what to type to match.
    /// `None` if nothing changes: a newer stroke, an undo or text typed by hand came first, and
    /// the reading is dropped.
    pub fn settle_late(&mut self) -> Option<StenoOutput> {
        let late = self.late.take()?;
        self.unmatched = false;
        self.transparent = false;
        self.plugin_read = None;
        self.check_out();
        let last = self
            .hist
            .state
            .translations
            .last()
            .filter(|last| {
                last.same(&late.translation)
                    && last.machine == late.translation.machine
                    && last.origin.is_stroke()
            })
            .cloned();
        let Some(last) = last else {
            self.check_in();
            return None;
        };
        self.budget.start(Instant::now() + self.plugin_deadline);
        // Nothing was typed since (the stroke's translation is still the last thing), so the
        // text before the stroke is where it was
        self.stroke_start = late.start;
        let mut ops = Vec::new();
        self.untranslate_translation(&last);
        self.translate_stroke(late.stroke, &mut ops);
        ops.extend(self.flush(None));
        let output = self.finish(ops);
        self.check_in();
        output
    }

    /// A new stroke: a fresh wait on plugins, and nothing late any more.
    fn begin_stroke(&mut self) {
        self.unmatched = false;
        self.transparent = false;
        self.late = None;
        self.plugin_read = None;
        self.budget.start(Instant::now() + self.plugin_deadline);
    }

    /// Turn the stroke classes on or off: `{:transparent}` (and the lookup command being
    /// transparent) and barriers. On by default; off, every stroke is an ordinary one.
    pub const fn set_stroke_classes(&mut self, enabled: bool) {
        self.classes_enabled = enabled;
    }

    /// Make undo stop at a barrier instead of reaching back past it (Plover reaches back). Off by default.
    pub const fn set_undo_stops_at_barrier(&mut self, stops: bool) {
        self.undo_stops_at_barrier = stops;
    }

    /// Whether the last stroke was transparent: it ran its commands but left the history alone.
    #[must_use]
    pub const fn was_transparent(&self) -> bool {
        self.transparent
    }

    /// The history this translator writes to.
    #[must_use]
    pub fn history(&self) -> SharedHistory {
        Arc::clone(&self.shared)
    }

    /// A translator over the same dictionary with nothing typed, and a history of its own. Cheap:
    /// the dictionary is shared.
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self::with_shared(Arc::clone(&self.dict), self.options())
    }

    /// Forget what was typed, as when the layer is entered: the next word starts a sentence.
    pub fn start_fresh(&mut self) {
        journal::lock(&self.shared).fence();
    }

    /// Swap in a dictionary chain that other translators share. What is already typed is
    /// untouched, so `*` still takes it back; only the strokes from now on use the new one.
    pub fn set_shared(&mut self, dict: Arc<Chain>) {
        self.late = None;
        self.dict = dict;
    }

    /// Change how spaces are placed and how a session starts.
    pub fn set_options(&mut self, options: Options) {
        journal::lock(&self.shared).formatter.options = options;
    }

    /// How spaces are placed and how a session starts.
    #[must_use]
    pub fn options(&self) -> Options {
        journal::lock(&self.shared).formatter.options.clone()
    }

    /// Whether a theory's plugin declares a stroke with these bits a continuation of the word
    /// before it (see [`Chain::joins`]).
    #[must_use]
    pub fn joins(&self, bits: u32) -> bool {
        self.dict.joins(bits)
    }

    /// Whether a plugin wants the stroke in progress (`stroke`) sent now, with `held` still down
    /// (see [`Chain::send_early`]).
    #[must_use]
    pub fn sends_early(&self, held: u32, stroke: u32) -> bool {
        self.dict.send_early(held, stroke)
    }

    /// The dictionary strokes are translated with: the merged view of every dictionary file,
    /// ignoring where any plugin fell, since reverse lookup and completions have nothing a
    /// plugin could meaningfully join in on.
    #[must_use]
    pub fn dictionary(&self) -> &Dictionary {
        self.dict.as_dictionary()
    }

    /// Whether a plugin is loaded: something besides the dictionary that can read a stroke.
    #[must_use]
    pub fn has_plugin(&self) -> bool {
        self.dict.plugin_count() > 0
    }

    /// The dictionary, to look things up on another thread. `None` when nothing loaded.
    #[must_use]
    pub fn dictionary_handle(&self) -> Option<Arc<Dictionary>> {
        (!self.dict.is_empty()).then(|| self.dict.dictionary())
    }

    /// Whether the dictionary has at least `count` entries. Cheap enough for every key press,
    /// where counting them all is not.
    #[must_use]
    pub fn has_entries(&self, count: usize) -> bool {
        self.dict.has_at_least(count)
    }

    /// Whether the last stroke matched nothing: it was typed as its keys.
    #[must_use]
    pub const fn was_unmatched(&self) -> bool {
        self.unmatched
    }

    /// The text on screen, as far as this translator knows it.
    #[must_use]
    pub fn text(&self) -> String {
        journal::lock(&self.shared).screen.clone()
    }

    /// Whether the dictionary entry for this stroke alone carries `{:firstup}` or
    /// `{:firstup:except=KEYS}`: that stroke is sent when its first key comes up instead of its
    /// last. `Some(exempt)` if it does, the stroke bits whose coming up does not send it (`0`
    /// for the plain marker); an exception list that does not parse as one stroke of the layout
    /// exempts nothing, so the entry is then plain `{:firstup}`.
    #[must_use]
    pub fn first_up_exempt(&self, stroke: u32) -> Option<u32> {
        // The dictionary files alone: a plugin's reading is a guess at a word, never a marker,
        // and this is asked on every key that comes up mid-stroke
        let text = self.dict.as_dictionary().lookup(&[stroke])?;
        let (_, _, keys) = super::meta::first_up_marker(text)?;
        Some(keys.and_then(|keys| self.dict.layout().parse_stroke(keys.trim())).unwrap_or(0))
    }

    /// Whether the stroke makes an engine command (`{:engine:toggle}`, `{:run:NAME}`, ... or the legacy `{PLOVER:...}`), which must never repeat.
    #[must_use]
    pub fn is_command(&self, stroke: u32) -> bool {
        self.dict
            .as_dictionary()
            .lookup(&[stroke])
            .is_some_and(|text| {
                text.as_bytes()
                    .windows(8)
                    .any(|window| window.eq_ignore_ascii_case(b"{plover:"))
                    || (self.classes_enabled && is_transparent_mapping(text))
                    || super::overlay_control::is_in(text)
                    || super::engine_control::is_in(text)
                    || super::outline_control::is_in(text)
                    || super::run_control::is_in(text)
            })
    }

    /// Whether the stroke alone arms `{:outline:next}`: striking it again disarms the mode.
    #[must_use]
    pub fn arms_outline(&self, stroke: u32) -> bool {
        self.dict
            .as_dictionary()
            .lookup(&[stroke])
            .is_some_and(super::outline_control::is_next)
    }

    /// The strokes of the last translation that strokes made (`{:outline:last}`): not text typed
    /// by hand, and not the notation `{:outline:...}` typed. Empty with none.
    #[must_use]
    pub fn last_outline(&self) -> Vec<u32> {
        let shared = journal::lock(&self.shared);
        shared
            .state
            .translations
            .iter()
            .rev()
            .find(|t| t.origin.is_stroke() && !t.strokes.is_empty())
            .map(|t| t.strokes.clone())
            .unwrap_or_default()
    }

    /// Type `text` as a translation of its own, with no strokes: spaced and capitalised like a
    /// word, and taken back by undo like one. This is the notation `{:outline:...}` types.
    pub fn type_text(&mut self, text: &str) -> Option<StenoOutput> {
        self.begin_stroke();
        self.check_out();
        self.stroke_start = self.hist.screen.len();
        self.hist.redo.clear();
        self.translate_translation(Translation::new(Vec::new(), Some(text.to_string())));
        let ops = self.flush(None);
        let output = self.finish(ops);
        self.check_in();
        output
    }

    /// Translate a finished stroke and say what to do with the keyboard.
    ///
    /// Strokes come in three classes. A *transparent* stroke (an entry carrying
    /// `{:transparent}`, or one that is just `{:lookup}`) runs its commands and key
    /// combinations and is otherwise not there: not added to the history, so it flushes nothing,
    /// resets no pending join, spacing or capitalisation, cannot be undone, and does not clear
    /// what undo left to redo. A *barrier* (a key combination that moves the cursor or edits
    /// earlier text, or an entry carrying `{:barrier}`) is an ordinary stroke that also ends the
    /// outline context. Every other stroke is *normal*; a stroke that only types whitespace
    /// separates a continuation from what came before.
    pub fn stroke(&mut self, stroke: u32) -> Option<StenoOutput> {
        self.begin_stroke();
        self.check_out();
        self.stroke_start = self.hist.screen.len();
        let mut ops = Vec::new();
        self.translate_stroke(stroke, &mut ops);
        ops.extend(self.flush(None));
        let output = self.finish(ops);
        self.check_in();
        output
    }

    /// Take the shared history to work on. Nothing else uses it until [`Self::check_in`].
    fn check_out(&mut self) {
        let mut shared = journal::lock(&self.shared);
        std::mem::swap(&mut self.hist, &mut *shared);
    }

    /// Hand the history back.
    fn check_in(&mut self) {
        let mut shared = journal::lock(&self.shared);
        std::mem::swap(&mut self.hist, &mut *shared);
    }

    /// The operations a stroke makes, before key combinations become key events. Tests use this
    /// to compare with Plover, which reports the combination as written.
    #[cfg(test)]
    pub(crate) fn stroke_ops(&mut self, stroke: u32) -> Vec<Op> {
        self.begin_stroke();
        self.check_out();
        self.stroke_start = self.hist.screen.len();
        let mut ops = Vec::new();
        self.translate_stroke(stroke, &mut ops);
        ops.extend(self.flush(None));
        for op in &ops {
            match op {
                Op::Backspaces(count) => {
                    let keep = self.hist.screen.chars().count().saturating_sub(*count);
                    self.hist.screen = self.hist.screen.chars().take(keep).collect();
                }
                Op::Text(text) => self.hist.screen.push_str(text),
                Op::Combo(_) | Op::Command(_) => {}
            }
        }
        self.check_in();
        ops
    }

    /// Settle the output: keep the screen in step and turn the key combinations into key events.
    fn finish(&mut self, ops: Vec<Op>) -> Option<StenoOutput> {
        let mut out = Vec::new();
        for op in ops {
            match op {
                Op::Backspaces(count) => {
                    if count > 0 {
                        // Where the last `count` characters start, or 0 if there are fewer
                        let cut = self
                            .hist
                            .screen
                            .char_indices()
                            .nth_back(count - 1)
                            .map_or(0, |(at, _)| at);
                        self.hist.screen.truncate(cut);
                    }
                    out.push(OutputOp::Backspaces(count));
                }
                Op::Text(text) => {
                    self.hist.screen.push_str(&text);
                    out.push(OutputOp::Text(text));
                }
                Op::Combo(combo) => match key_events(&combo) {
                    Ok(events) => out.push(OutputOp::Keys(events)),
                    Err(e) => warn!("steno key combination not sent: {e}"),
                },
                Op::Command(command) => out.push(OutputOp::Command(command)),
            }
        }
        StenoOutput::from_ops(out)
    }

    fn translate_stroke(&mut self, stroke: u32, ops: &mut Vec<Op>) {
        let max_len = self.dict.longest_key();
        let mapping = self.lookup_with_prefix(
            max_len,
            &self.hist.state.translations,
            &[stroke],
            &[],
            self.asked(Some(self.stroke_start), 0),
        );
        if self.classes_enabled && !self.dict.joins(stroke) {
            if let Some(text) = mapping.as_deref().filter(|m| is_transparent_mapping(m)) {
                self.transparent = true;
                ops.extend(self.transparent_ops(stroke, text));
                return;
            }
        }
        if let Some(found) = mapping_to_macro(mapping.as_deref(), stroke) {
            // Taking back and putting back keep the redo history; anything else is new input,
            // but only once it actually changes something. Several macros here (the retro
            // space commands, again, swap, repeat_last_stroke, the asterisk toggles) silently
            // do nothing when their preconditions aren't met, and a no-op must not be allowed
            // to wipe out what undo left behind to redo: `self.hist.to_do` is always 0 on entry
            // (the previous flush reset it), so it being nonzero afterwards is exactly whether
            // this macro appended anything new.
            let keeps_redo =
                matches!(found.name.as_str(), "undo" | "undo_word" | "undo_or" | "redo");
            self.translate_macro(&found, ops);
            if !keeps_redo && self.hist.to_do > 0 {
                self.hist.redo.clear();
            }
            return;
        }
        self.hist.redo.clear();
        // A stroke a theory's own plugin treats as a continuation is never a word by itself;
        // it is only ever looked up glued onto some of what came before.
        let joined = self.dict.joins(stroke);
        let window = self.history_window(max_len, joined);
        let outlines = Self::outlines(window, stroke, joined);
        let translation = self
            // The longest outline ending in this stroke, two strokes or more (the stroke alone
            // is already looked up)
            .longest_match(window, &outlines, 2, &[])
            // The stroke alone, if it is mapped. A continuation stroke gets here only when no
            // joined outline matched, so its own entry (a suffix `{^ed}`, say) attaches to the
            // previous output through the formatter.
            .or_else(|| mapping.map(|m| Translation::new(vec![stroke], Some(m))))
            // No direct match: the same outlines with a suffix key folded off the stroke, as
            // Plover tries them only after every plain outline failed (a plugin at its own
            // place in the chain already had its chance, in `mapping` and in each attempt)
            .or_else(|| {
                let suffixes = self.involved_suffixes(
                    stroke,
                    &SUFFIX_KEYS,
                    self.asked(Some(self.stroke_start), 0),
                );
                (!suffixes.is_empty())
                    .then(|| self.longest_match(window, &outlines, 1, &suffixes))
                    .flatten()
            })
            // Nothing: the stroke as its keys
            .unwrap_or_else(|| Translation::new(vec![stroke], None));
        if translation.english.is_none() {
            self.unmatched = true;
            info!("steno stroke {stroke:#x} not in dictionary");
        }
        self.plugin_read = translation.english.as_ref().and_then(|text| {
            let plugin = self.budget.reader_of(&translation.strokes, text)?;
            Some((plugin, text.clone()))
        });
        let late = self.budget.take_late();
        self.translate_translation(translation);
        if let Some((strokes, before, base)) = late {
            let translation = self.hist.state.translations.last().cloned();
            self.late = translation.map(|translation| Late {
                stroke,
                strokes,
                before,
                log: self.hist.strokes.clone(),
                base,
                start: self.stroke_start,
                translation,
            });
        }
    }

    /// The text before an outline whose text starts at `begins` on screen, as a plugin is
    /// told it: at most the last [`CONTEXT_BYTES`] of what is known to be on screen there,
    /// cut on a character boundary. Empty when nothing is known. A wall leaves nothing known
    /// before it: anything that fences the history (a key that moves the caret, a shortcut, a
    /// window change) empties the screen, and a barrier stroke (a key combination that moves
    /// the cursor or deletes, which the screen does not follow) starts it over after itself.
    fn before(&self, begins: Option<usize>) -> &str {
        let screen = self.hist.screen.as_str();
        let Some(mut end) = begins else {
            return "";
        };
        end = end.min(screen.len());
        while !screen.is_char_boundary(end) {
            end -= 1;
        }
        let translations = &self.hist.state.translations;
        let wall = translations
            .iter()
            .rposition(|t| t.begins.is_some_and(|at| at <= end) && is_barrier(t))
            .map(|at| {
                translations
                    .get(at + 1)
                    .map_or(Some(self.stroke_start), |next| next.begins)
                    .unwrap_or(end)
            })
            .unwrap_or(0)
            .min(end);
        let mut start = end.saturating_sub(CONTEXT_BYTES).max(wall);
        while !screen.is_char_boundary(start) {
            start += 1;
        }
        &screen[start..end]
    }

    /// What a plugin is told precedes an outline whose text starts at `begins` on screen and
    /// whose first stroke is the `back`th from the end of the stroke log: the text (see
    /// [`Self::before`]) and the strokes.
    fn asked(&self, begins: Option<usize>, back: usize) -> Before<'_> {
        Before {
            text: self.before(begins),
            base: self.hist.strokes.end().saturating_sub(back as u64),
            log: Some(&self.hist.strokes),
        }
    }

    /// What a transparent stroke asks of the keyboard: its commands and key combinations, in
    /// order. Its text is dropped, and nothing about it is kept: it is formatted on a copy of the
    /// formatter, after nothing, so it cannot read or change the spacing, capitals or look-ahead
    /// of what came before.
    fn transparent_ops(&self, stroke: u32, text: &str) -> Vec<Op> {
        let mut formatter = self.hist.formatter.clone();
        let mut item = Translation::new(vec![stroke], Some(text.to_string()));
        formatter
            .format(&[], &mut [&mut item], &mut [], self.dict.orthography())
            .into_iter()
            .filter(|op| matches!(op, Op::Command(_) | Op::Combo(_)))
            .collect()
    }

    fn translate_macro(&mut self, found: &Macro, ops: &mut Vec<Op>) {
        match found.name.as_str() {
            macro_name::UNDO => {
                let count = Self::repeat_count(&found.cmdline).max(1);
                self.macro_undo(found.stroke, count, ops);
            }
            macro_name::UNDO_WORD => self.macro_undo_word(found.stroke, ops),
            macro_name::UNDO_OR => self.macro_undo_or(found.stroke, &found.cmdline, ops),
            macro_name::REDO => self.macro_redo(Self::repeat_count(&found.cmdline).max(1)),
            macro_name::AGAIN => self.macro_again(),
            macro_name::SWAP => self.macro_swap(),
            macro_name::REPEAT_LAST_STROKE => {
                let Some(last) = self
                    .hist
                    .state
                    .translations
                    .last()
                    .and_then(|t| t.strokes.last())
                else {
                    return;
                };
                let stroke = *last;
                self.translate_stroke(stroke, ops);
            }
            macro_name::RETRO_TOGGLE_ASTERISK => {
                let Some(last) = self.hist.state.translations.last().cloned() else {
                    return;
                };
                if !last.origin.is_stroke() {
                    return;
                }
                self.untranslate_translation(&last);
                let Some(&stroke) = last.strokes.last() else {
                    return;
                };
                self.translate_stroke(stroke ^ STAR, ops);
            }
            // Lapwing's repeat macros, from its last-translation plugin
            macro_name::REPEAT_LAST_TRANSLATION => self.macro_repeat_translations(&found.cmdline),
            macro_name::REPEAT_LAST_WORD | macro_name::REPEAT_LAST_FRAGMENT | macro_name::REPEAT_LAST_CHARACTER => {
                self.macro_repeat_text(&found.name, &found.cmdline, found.stroke);
            }
            macro_name::RETRO_DELETE_SPACE | macro_name::RETROSPECTIVE_DELETE_SPACE => {
                self.macro_delete_space(found.stroke);
            }
            macro_name::RETRO_INSERT_SPACE | macro_name::RETROSPECTIVE_INSERT_SPACE => {
                self.macro_insert_space(found.stroke);
            }
            macro_name::RETROSPECTIVE_TOGGLE_ASTERISK => {
                let Some(last) = self.hist.state.translations.last().cloned() else {
                    return;
                };
                if !last.origin.is_stroke() {
                    return;
                }
                self.untranslate_translation(&last);
                let Some(&stroke) = last.strokes.last() else {
                    return;
                };
                self.translate_stroke(stroke ^ STAR, ops);
            }
            other => warn!("steno macro `{other}` is not known, so it does nothing"),
        }
    }

    /// Take translations back from the end, last first, until `stop` says so for one that had an
    /// effect. What was taken back can be put back by redo. Returns whether anything had an effect.
    fn take_back(&mut self, mut stop: impl FnMut(&Translation) -> bool) -> bool {
        let all: Vec<Translation> = self.hist.state.translations.clone();
        let mut group = Vec::new();
        let mut effective = false;
        for translation in all.iter().rev() {
            // Undo stops at a barrier: what is before it is no longer next to the cursor, so
            // taking it back would delete the wrong text. With nothing taken back yet, the
            // barrier is the end of the history and what is before it is forgotten.
            if self.classes_enabled && self.undo_stops_at_barrier && is_barrier(translation) {
                if translation.has_undo() {
                    // It changed text itself (`{:back}`): taking it back is what undo is for
                    self.untranslate_translation(translation);
                    group.push(translation.clone());
                    effective = true;
                } else if !effective {
                    let state = &mut self.hist.state;
                    state.tail = state.translations.pop();
                    state.translations.clear();
                    self.hist.strokes.wall();
                }
                break;
            }
            self.untranslate_translation(translation);
            group.push(translation.clone());
            if translation.has_undo() {
                effective = true;
                if stop(translation) {
                    break;
                }
            }
        }
        if effective {
            group.reverse();
            self.hist.redo.push(group);
        }
        effective
    }

    /// Nothing left of the output to take back. Delete a word, without keeping it in the history.
    fn undo_fallback(&mut self, stroke: u32, ops: &mut Vec<Op>) {
        let extra = Translation::new(vec![stroke], Some(BACK_STRING.to_string()));
        ops.extend(self.flush(Some(extra)));
    }

    /// Undo: take back `count` translations that had an effect, or if there are none left,
    /// delete a word with the keys that do it.
    fn macro_undo(&mut self, stroke: u32, count: usize, ops: &mut Vec<Op>) {
        let mut left = count;
        let found = self.take_back(|_| {
            left -= 1;
            left == 0
        });
        if !found {
            self.undo_fallback(stroke, ops);
        }
    }

    /// Undo, or if it can't (nothing to take back, or a barrier is in the way), send the key
    /// combination `cmdline` instead; with none given, the key combination plain undo falls
    /// back to. Taking back keeps the redo history, the key combination is new input and
    /// empties it. The combination is not kept in the history, as with plain undo's fallback.
    fn macro_undo_or(&mut self, stroke: u32, cmdline: &str, ops: &mut Vec<Op>) {
        if self.take_back(|_| true) {
            return;
        }
        self.hist.redo.clear();
        let combo = undo_or_combo(cmdline);
        if combo.is_empty() {
            self.undo_fallback(stroke, ops);
            return;
        }
        let extra = Translation::new(vec![stroke], Some(format!("{{#{combo}}}{{^}}")));
        ops.extend(self.flush(Some(extra)));
    }

    /// Undo a whole word: back to the translation that started it, so a stem and the suffixes
    /// glued after it go together, and so does a word typed by hand. A translation that replaced
    /// earlier strokes of its word (a longer outline, or a plugin's reading of a whole join-key
    /// run) puts those back when it is taken back; they are the same word, so they are taken back
    /// too, one at a time, until the stroke that started it. One redo puts the whole word back.
    fn macro_undo_word(&mut self, stroke: u32, ops: &mut Vec<Op>) {
        let starts_word = |translation: &Translation| {
            !translation.origin.is_stroke()
                || translation
                    .formatting
                    .iter()
                    .map(Action::effective)
                    .find(|action| action.text.as_deref().is_some_and(|text| !text.is_empty()))
                    .is_none_or(|action| !action.prev_attach)
        };
        let mut groups = 0;
        while self.take_back(|_| true) {
            groups += 1;
            let taken = self.hist.redo.last().and_then(|group| group.last());
            if taken.is_none_or(|t| starts_word(t) && t.replaced.is_empty()) {
                break;
            }
        }
        if groups == 0 {
            self.undo_fallback(stroke, ops);
            return;
        }
        // One redo group for the whole word, oldest first, as redo replays it
        let at = self.hist.redo.len() - groups;
        let whole: Vec<Translation> = self.hist.redo.drain(at..).rev().flatten().collect();
        self.hist.redo.push(whole);
    }

    /// Redo: put back what undo took back, `count` times over. Nothing to redo does nothing.
    fn macro_redo(&mut self, count: usize) {
        for _ in 0..count {
            let Some(group) = self.hist.redo.pop() else {
                return;
            };
            for translation in group {
                self.translate_translation(translation);
            }
        }
    }

    /// Say the last thing again: the strokes of the last translation, or the text typed by hand.
    fn macro_again(&mut self) {
        let Some(last) = self
            .hist
            .state
            .translations
            .iter()
            .rev()
            .find(|t| t.has_undo())
            .cloned()
        else {
            return;
        };
        let again = match &last.origin {
            Origin::Stroke => Translation::new(last.strokes.clone(), last.english.clone()),
            Origin::Typed(text) => {
                let mut typed = Translation::new(Vec::new(), None);
                typed.origin = Origin::Typed(text.clone());
                typed
            }
            // Erasing again would erase something else
            Origin::Erased(_) => return,
        };
        self.translate_translation(again);
    }

    /// Swap the last two words: take both back and say them the other way round, so the spacing
    /// and capitals are made again for where they are now.
    fn macro_swap(&mut self) {
        let translations = &self.hist.state.translations;
        let Some([first, second]) = translations
            .len()
            .checked_sub(2)
            .map(|from| &translations[from..])
        else {
            return;
        };
        let (first, second) = (first.clone(), second.clone());
        let swapped = match (&first.origin, &second.origin) {
            (Origin::Stroke, Origin::Stroke)
                if !first.is_retrospective_command && !second.is_retrospective_command =>
            {
                [
                    Translation::new(second.strokes.clone(), second.english.clone()),
                    Translation::new(first.strokes.clone(), first.english.clone()),
                ]
            }
            (Origin::Typed(a), Origin::Typed(b)) => {
                // The space after a word stays where it was: it separates the two
                let split = |text: &str| {
                    let core = text.trim_end().len();
                    (text[..core].to_string(), text[core..].to_string())
                };
                let ((a_word, a_space), (b_word, b_space)) = (split(a), split(b));
                let typed = |text: String| {
                    let mut translation = Translation::new(Vec::new(), None);
                    translation.origin = Origin::Typed(text);
                    translation
                };
                [
                    typed(format!("{b_word}{a_space}")),
                    typed(format!("{a_word}{b_space}")),
                ]
            }
            _ => return,
        };
        self.undo_all(&[first, second]);
        for translation in swapped {
            self.translate_translation(translation);
        }
    }

    /// How many to repeat: the first of the comma-separated arguments, or one.
    fn repeat_count(cmdline: &str) -> usize {
        cmdline
            .split(',')
            .next()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(1)
    }

    /// `=repeat_last_translation:n`: the last `n` translations again.
    fn macro_repeat_translations(&mut self, cmdline: &str) {
        let translations = &self.hist.state.translations;
        if translations.is_empty() {
            return;
        }
        let count = Self::repeat_count(cmdline);
        // `translations[-0:]` is the whole list in Python
        let from = if count == 0 {
            0
        } else {
            translations.len().saturating_sub(count)
        };
        let again: Vec<Translation> = translations[from..]
            .iter()
            .filter(|t| t.origin.is_stroke())
            .map(|t| Translation::new(t.strokes.clone(), t.english.clone()))
            .collect();
        for translation in again {
            self.translate_translation(translation);
        }
    }

    /// `=repeat_last_word:n`, `=repeat_last_fragment:n`, `=repeat_last_character:n`: the text
    /// again, typed as a translation of this stroke.
    fn macro_repeat_text(&mut self, name: &str, cmdline: &str, stroke: u32) {
        if self.hist.state.translations.is_empty() {
            return;
        }
        let count = Self::repeat_count(cmdline);
        let actions: Vec<&Action> = self
            .hist
            .state
            .translations
            .iter()
            .rev()
            .flat_map(|t| t.formatting.iter().rev())
            .collect();
        let pieces: Vec<String> = match name {
            "repeat_last_word" => super::action::last_words(&actions, count),
            "repeat_last_fragment" => super::action::last_fragments_in_order(&actions, count),
            _ => vec![super::action::last_text(&actions, count)],
        };
        for text in pieces {
            self.translate_translation(Translation::new(vec![stroke], Some(text)));
        }
    }

    fn macro_delete_space(&mut self, stroke: u32) {
        let translations = &self.hist.state.translations;
        if translations.len() < 2 {
            return;
        }
        let replaced: Vec<Translation> = translations[translations.len() - 2..].to_vec();
        if replaced[1].is_retrospective_command || replaced.iter().any(|t| !t.origin.is_stroke()) {
            return;
        }
        let mut english = Vec::new();
        for t in &replaced {
            if let Some(text) = &t.english {
                english.push(text.clone());
            } else if t.strokes.len() == 1 {
                let steno = render_stroke(t.strokes[0]);
                if super::action::is_digits(&steno) {
                    english.push(format!("{{&{steno}}}"));
                }
            }
        }
        if english.len() > 1 {
            let mut t = Translation::new(vec![stroke], Some(english.join("{^~|^}")));
            t.replaced = replaced;
            t.is_retrospective_command = true;
            self.translate_translation(t);
        }
    }

    fn macro_insert_space(&mut self, stroke: u32) {
        let Some(last) = self.hist.state.translations.last().cloned() else {
            return;
        };
        if last.is_retrospective_command || !last.origin.is_stroke() {
            return;
        }
        let Some(&lookup_stroke) = last.strokes.last() else {
            return;
        };
        let mut english: Vec<String> = last
            .replaced
            .iter()
            .map(|t| match &t.english {
                Some(text) if !text.is_empty() => text.clone(),
                _ => t.rtfcre().join("/"),
            })
            .collect();
        if !english.is_empty() {
            let begins = last.replaced.last().and_then(|t| t.begins);
            english.push(
                self.lookup(&[lookup_stroke], &SUFFIX_KEYS, self.asked(begins, 1))
                    .unwrap_or_else(|| render_stroke(lookup_stroke)),
            );
            let mut t = Translation::new(vec![stroke], Some(english.join(" ")));
            t.replaced = vec![last];
            t.is_retrospective_command = true;
            self.translate_translation(t);
        }
    }

    fn translate_translation(&mut self, mut translation: Translation) {
        if translation.origin.is_stroke() {
            translation.machine = self.machine;
        }
        // It starts where the first translation it replaces started, or else with this stroke
        if translation.begins.is_none() {
            translation.begins = translation
                .replaced
                .first()
                .map_or(Some(self.stroke_start), |t| t.begins);
        }
        let replaced = translation.replaced.clone();
        self.exchange(&replaced, vec![translation]);
    }

    fn untranslate_translation(&mut self, translation: &Translation) {
        self.exchange(
            std::slice::from_ref(translation),
            translation.replaced.clone(),
        );
    }

    /// Take `removed` off the end of the history and put `added` on: and keep the stroke log
    /// the strokes of the history. Strokes the two share at the start are not touched, so a
    /// longer outline replacing the ones it covers cuts nothing from the log, and a lineage
    /// (see [`super::strokes`]) changes only where strokes really are taken back.
    fn exchange(&mut self, removed: &[Translation], added: Vec<Translation>) {
        let strokes_of = |translations: &[Translation]| -> Vec<u32> {
            translations
                .iter()
                .filter(|t| t.origin.is_stroke())
                .flat_map(|t| t.strokes.iter().copied())
                .collect()
        };
        let (out, into) = (strokes_of(removed), strokes_of(&added));
        let shared = out.iter().zip(&into).take_while(|(a, b)| a == b).count();
        self.pop_translations(removed);
        self.hist.to_do += added.len();
        self.hist.state.translations.extend(added);
        self.hist.strokes.pop(out.len() - shared);
        self.hist.strokes.push(&into[shared..]);
    }

    /// Take the translations off the end of the history, last first.
    fn undo_all(&mut self, translations: &[Translation]) {
        self.pop_translations(translations);
        let strokes = translations
            .iter()
            .filter(|t| t.origin.is_stroke())
            .map(|t| t.strokes.len())
            .sum();
        self.hist.strokes.pop(strokes);
    }

    /// [`Self::undo_all`] for the translations alone, the stroke log left to the caller.
    fn pop_translations(&mut self, translations: &[Translation]) {
        for expected in translations.iter().rev() {
            let Some(popped) = self.hist.state.translations.pop() else {
                return;
            };
            debug_assert!(popped.same(expected));
            if self.hist.to_do > 0 {
                self.hist.to_do -= 1;
            } else {
                self.hist.to_undo.insert(0, popped);
            }
        }
    }

    /// The last translations a new stroke may make a longer outline with, oldest first. A
    /// dictionary's longest real key bounds an ordinary search for an accidental multi-stroke
    /// match, but a join-key run is not accidental: the keyboard already declared it one
    /// continuing word, so it keeps growing for as long as the keyboard says so, not just until
    /// it would be as long as the dictionary's longest brief.
    fn history_window(&self, max_len: usize, joined: bool) -> &[Translation] {
        let translations = &self.hist.state.translations;
        let mut num_strokes = 1;
        let mut count = 0;
        for t in translations.iter().rev() {
            // Outlines are written for one set of dictionaries: strokes made by another, or text
            // typed by hand, end the run
            if !t.origin.is_stroke() || t.strokes.is_empty() || t.machine != self.machine {
                break;
            }
            // A barrier moved the cursor or edited earlier text, so nothing reaches across it;
            // a continuation does not reach across whitespace either
            if self.classes_enabled && (is_barrier(t) || (joined && is_separator(t))) {
                break;
            }
            num_strokes += t.strokes.len();
            if !joined && num_strokes > max_len {
                break;
            }
            count += 1;
        }
        &translations[translations.len() - count..]
    }

    /// Every outline ending in `stroke` that the window allows, longest first: the strokes of
    /// the window from each translation on, plus this one, with the index it starts at. A
    /// stroke a theory's own plugin treats as a continuation declares itself one outright, so
    /// it is never tried on its own: doing that would let it be mistaken for a fresh word the
    /// moment nothing glues onto it matches.
    fn outlines(window: &[Translation], stroke: u32, joined: bool) -> Vec<(usize, Strokes)> {
        let end = window.len() + usize::from(!joined);
        (0..end)
            .map(|i| {
                let mut strokes: Strokes = window[i..]
                    .iter()
                    .flat_map(|t| t.strokes.iter().copied())
                    .collect();
                strokes.push(stroke);
                (i, strokes)
            })
            .collect()
    }

    /// The translation of the first of `outlines` that is in the dictionary, at least `min_len`
    /// strokes long. It replaces the translations of the window it covers. With `suffixes`, each
    /// outline is looked up with one of them folded off its last stroke.
    fn longest_match(
        &self,
        window: &[Translation],
        outlines: &[(usize, Strokes)],
        min_len: usize,
        suffixes: &[(u32, String)],
    ) -> Option<Translation> {
        let max_len = self.dict.longest_key();
        outlines
            .iter()
            .filter(|(_, strokes)| strokes.len() >= min_len)
            .find_map(|(i, strokes)| {
                // What was typed before the outline's first stroke, never the outline's own
                // text so far
                let begins = window.get(*i).map_or(Some(self.stroke_start), |t| t.begins);
                let back = window[*i..].iter().map(|t| t.strokes.len()).sum();
                let mapping = self.lookup_with_prefix(
                    max_len,
                    &window[..*i],
                    strokes,
                    suffixes,
                    self.asked(begins, back),
                )?;
                let mut translation = Translation::new(strokes.to_vec(), Some(mapping));
                translation.replaced = window[*i..].to_vec();
                Some(translation)
            })
    }

    /// Look the strokes up, typed after `before` (see [`Self::before`]).
    fn lookup_strokes(&self, strokes: &[u32], before: Before<'_>) -> Option<String> {
        self.dict.lookup_within(strokes, before, &self.budget)
    }

    /// Look the strokes up, with the last one's suffix key folded in if there are suffixes: the
    /// stroke without the key, then a space and the suffix's own translation.
    fn lookup_with_suffix(
        &self,
        strokes: &[u32],
        suffixes: &[(u32, String)],
        before: Before<'_>,
    ) -> Option<String> {
        if suffixes.is_empty() {
            return self.lookup_strokes(strokes, before);
        }
        let (last, rest) = strokes.split_last()?;
        for (suffix_stroke, suffix_mapping) in suffixes {
            let mut without = Strokes::from_slice(rest);
            without.push(last & !suffix_stroke);
            if let Some(main) = self.lookup_strokes(&without, before) {
                return Some(format!("{main} {suffix_mapping}"));
            }
        }
        None
    }

    /// The suffix keys `stroke` has that have a translation of their own.
    fn involved_suffixes(
        &self,
        stroke: u32,
        suffix_keys: &[u32],
        before: Before<'_>,
    ) -> Vec<(u32, String)> {
        suffix_keys
            .iter()
            .filter(|&&key| stroke & key == key)
            .filter_map(|&key| Some((key, self.lookup_strokes(&[key], before)?)))
            .collect()
    }

    /// The translation of an outline, looking for a suffix key folded in if there is no entry.
    fn lookup(&self, strokes: &[u32], suffix_keys: &[u32], before: Before<'_>) -> Option<String> {
        if let Some(found) = self.lookup_strokes(strokes, before) {
            return Some(found);
        }
        let suffixes = self.involved_suffixes(*strokes.last()?, suffix_keys, before);
        if suffixes.is_empty() {
            return None;
        }
        self.lookup_with_suffix(strokes, &suffixes, before)
    }

    /// Whether the word before is complete, so a stroke may start a new one.
    fn previous_word_is_finished(last: &[Translation]) -> bool {
        last.last()
            .and_then(|t| t.formatting.last())
            .is_none_or(|action| action.effective().word_is_finished)
    }

    /// Whether `strokes` is a run the keyboard itself declared one word: more than one stroke,
    /// every one after the first answering `true` to the chain's [`super::plugin::Chain::joins`].
    fn is_join_continuation(&self, strokes: &[u32]) -> bool {
        matches!(strokes, [_, rest @ ..] if !rest.is_empty() && rest.iter().all(|&s| self.dict.joins(s)))
    }

    /// Look up `strokes`, first as the start of a word (with the empty stroke in front) if the
    /// word before is finished, then as they are.
    fn lookup_with_prefix(
        &self,
        max_len: usize,
        last_translations: &[Translation],
        strokes: &[u32],
        suffixes: &[(u32, String)],
        before: Before<'_>,
    ) -> Option<String> {
        if strokes.len() < max_len && Self::previous_word_is_finished(last_translations) {
            let mut prefixed: Strokes = smallvec::smallvec![0];
            prefixed.extend_from_slice(strokes);
            if let Some(found) = self.lookup_with_suffix(&prefixed, suffixes, before) {
                return Some(found);
            }
        }
        // A dictionary's longest real key bounds an ordinary lookup, but it says nothing about
        // how long a join-key run can be: the keyboard already declared the whole thing one
        // word, so trying it is never pointless the way trying an arbitrarily long accidental
        // window would be.
        if strokes.len() <= max_len || self.is_join_continuation(strokes) {
            return self.lookup_with_suffix(strokes, suffixes, before);
        }
        None
    }

    /// Mark in the stroke log the walls the last `done` translations made, now they are
    /// formatted (whether one is a barrier depends on the keys it typed): a barrier, or text not
    /// from strokes, ends the history of strokes at its own end.
    fn wall_after_barriers(&self, done: usize) {
        let translations = &self.hist.state.translations;
        let new = &translations[translations.len() - done.min(translations.len())..];
        let strokes: u64 = new
            .iter()
            .filter(|t| t.origin.is_stroke())
            .map(|t| t.strokes.len() as u64)
            .sum();
        let mut at = self.hist.strokes.end().saturating_sub(strokes);
        for t in new {
            if t.origin.is_stroke() {
                at += t.strokes.len() as u64;
                if is_barrier(t) {
                    self.hist.strokes.wall_at(at);
                }
            } else {
                self.hist.strokes.wall_at(at);
            }
        }
    }

    /// Format what was done and undone since the last flush, and return what to type to match.
    /// `extra` is typed too but not kept in the history.
    fn flush(&mut self, extra: Option<Translation>) -> Vec<Op> {
        let to_do = self.hist.to_do;
        let undo = std::mem::take(&mut self.hist.to_undo);
        self.hist.to_do = 0;
        let mut extra = extra;
        let ops = if !undo.is_empty() || to_do > 0 || extra.is_some() {
            let journal::State { translations, tail } = &mut self.hist.state;
            let split = translations.len() - to_do;
            let (before, rest) = translations.split_at_mut(split);
            let mut tail_holder;
            let prev: &mut [Translation] = if !before.is_empty() {
                before
            } else if let Some(tail) = tail.as_ref() {
                tail_holder = vec![tail.clone()];
                &mut tail_holder
            } else {
                &mut []
            };
            let mut doing: Vec<&mut Translation> = rest.iter_mut().collect();
            if let Some(extra) = extra.as_mut() {
                doing.push(extra);
            }
            self.hist
                .formatter
                .format(&undo, &mut doing, prev, self.dict.orthography())
        } else {
            Vec::new()
        };
        self.wall_after_barriers(to_do);
        let keep = self.dict.longest_key().max(UNDO_LENGTH);
        self.hist.state.restrict_size(keep);
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::Layout;
    use std::fs;
    use std::sync::mpsc;

    fn stroke(text: &str) -> u32 {
        if text == "*" {
            return UNDO_STROKE;
        }
        Layout::plover()
            .parse_stroke(text)
            .unwrap_or_else(|| panic!("{text} is not a stroke"))
    }

    fn translator(json: &str) -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(json, &Layout::plover()).unwrap();
        Translator::new(dict)
    }

    /// The text after typing each stroke in order.
    fn type_strokes(json: &str, strokes: &[&str]) -> String {
        let mut t = translator(json);
        for s in strokes {
            t.stroke(stroke(s));
        }
        t.text()
    }

    const BACK_DICT: &str =
        r#"{"KAT": "cat", "TKOG": "dog", "SPW": "{:back}", "PW-FP": "{:back:3}"}"#;

    #[test]
    fn back_erases_a_character_and_undo_types_it_again() {
        let mut t = translator(BACK_DICT);
        t.stroke(stroke("KAT"));
        t.stroke(stroke("TKOG"));
        assert_eq!(t.text(), "cat dog");
        t.stroke(stroke("SPW"));
        assert_eq!(t.text(), "cat do");
        // Undo puts the character back and sends nothing else
        let ops = t.stroke_ops(stroke("*"));
        assert_eq!(ops, vec![Op::Text("g".to_string())]);
        assert_eq!(t.text(), "cat dog");
        // The next undo takes the word as usual
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "cat");
    }

    #[test]
    fn back_with_a_count_erases_that_many() {
        let mut t = translator(BACK_DICT);
        t.stroke(stroke("KAT"));
        t.stroke(stroke("TKOG"));
        t.stroke(stroke("PW-FP"));
        assert_eq!(t.text(), "cat ");
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "cat dog");
    }

    /// A real plugin: upper-cases the stroke notation it's given, so there is no mistaking its
    /// answer for a coincidence. Compiled by `rustc` to `wasm32-unknown-unknown` for real.
    const SHOUT_PLUGIN: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let text = std::str::from_utf8(input).unwrap_or("");
    let mut out: Vec<u8> = text.to_uppercase().into_bytes();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}

// Bit 23 (the first beyond keymux's own 23 standard keys) is this test file's own made-up
// join key - keymux has no built-in notion of what any key means.
#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {
    i32::from(bits & (1 << 23) != 0)
}
"#;

    /// A translator over a folder with one dictionary file and, after it (lower priority, a
    /// fallback), one plugin: the same shape a real steno folder is in, built the same way.
    fn with_shout_plugin(dict: &str, test_name: &str) -> Translator {
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-plugin-test-{}-{test_name}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let dict_path = dir.join("01-dict.json");
        let plugin_path = dir.join("02-shout.rs");
        fs::write(&dict_path, dict).unwrap();
        fs::write(&plugin_path, SHOUT_PLUGIN).unwrap();
        let chain = Chain::load_cached(
            &[dict_path, plugin_path],
            &dir.join("cache"),
            &Layout::plover(),
        );
        Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    /// A translator whose one plugin reads an outline as itself in lower case, without the
    /// slashes, but only as each reading is let go by the returned sender (dropping it lets every
    /// reading go at once). It waits at most `deadline` on a reading.
    fn with_gated_plugin(dict: &str, deadline: Duration) -> (Translator, mpsc::Sender<()>) {
        let (go, gate) = mpsc::channel::<()>();
        let layout = Layout::plover_with_joins(&['+'], &['+']).unwrap();
        let mut dictionary = Dictionary::default();
        dictionary.merge_json(dict, &layout).unwrap();
        let chain = Chain::test_with_reader(
            dictionary,
            layout,
            Box::new(
                move |outline: &str, _: &str, _: Option<&crate::steno::strokes::Window>| {
                    let _ = gate.recv();
                    Some(outline.replace(['/', '+'], "").to_lowercase())
                },
            ),
        );
        let mut t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        t.set_plugin_deadline(deadline);
        (t, go)
    }

    fn settle(t: &mut Translator) -> Option<StenoOutput> {
        let started = std::time::Instant::now();
        while !t.late_ready() {
            assert!(started.elapsed() < Duration::from_secs(10), "never settled");
            std::thread::yield_now();
        }
        t.settle_late()
    }

    #[test]
    fn a_reading_in_time_is_used_as_ever_and_noted_as_the_plugins() {
        let (mut t, go) = with_gated_plugin(r#"{"KAT": "cat"}"#, Duration::from_secs(10));
        drop(go);
        t.stroke(stroke("KAT"));
        assert_eq!(t.plugin_read(), None, "the dictionary's");
        t.stroke(stroke("TOP"));
        assert_eq!(t.text(), "cat top");
        assert!(!t.has_late());
        assert_eq!(t.plugin_read(), Some(("reader".into(), "top".into())));
    }

    #[test]
    fn a_late_reading_types_the_stroke_as_its_keys_then_replaces_it_once_it_lands() {
        let (mut t, go) = with_gated_plugin(r#"{"KAT": "cat"}"#, Duration::from_millis(5));
        t.stroke(stroke("KAT"));
        let typed = t.stroke(stroke("TOP"));
        let raw = t.text();
        assert!(
            raw.starts_with("cat ") && raw != "cat top",
            "typing goes on without the reading: {raw}"
        );
        assert!(typed.is_some());
        assert!(t.has_late() && !t.late_ready());
        go.send(()).unwrap();
        let replaced = settle(&mut t);
        assert!(replaced.is_some(), "the screen is put right");
        assert_eq!(t.text(), "cat top");
        assert!(!t.has_late());
        assert_eq!(t.plugin_read(), Some(("reader".into(), "top".into())));
        // And it is an ordinary translation now: undo takes it back
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "cat");
    }

    #[test]
    fn a_late_reading_of_a_run_lands_on_the_run_as_it_now_is() {
        let (mut t, go) = with_gated_plugin("{}", Duration::from_millis(5));
        let layout = Layout::plover_with_joins(&['+'], &['+']).unwrap();
        t.stroke(layout.parse_stroke("HRAOBG").unwrap());
        // The next stroke of the run supersedes the first: only the whole run's reading counts
        t.stroke(layout.parse_stroke("+UP").unwrap());
        assert!(t.has_late());
        go.send(()).unwrap(); // HRAOBG, already started, finishes; nobody wants it now
        go.send(()).unwrap(); // the whole run
        settle(&mut t);
        assert_eq!(t.text(), "hraobgup");
    }

    /// A translator over `dict` and, after it, a real plugin that reads each outline as
    /// `(TEXT|OUTLINE)`, `TEXT` being what its `context` export was given just before.
    fn with_context_plugin(dict: &str, test_name: &str) -> Translator {
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-context-test-{}-{test_name}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let dict_path = dir.join("01-dict.json");
        let plugin_path = dir.join("02-echo.rs");
        fs::write(&dict_path, dict).unwrap();
        fs::write(&plugin_path, super::super::plugin::tests::ECHO_CONTEXT).unwrap();
        let chain = Chain::load_cached(
            &[dict_path, plugin_path],
            &dir.join("cache"),
            &Layout::plover(),
        );
        Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    #[test]
    fn a_plugin_is_told_the_text_typed_before_the_outline() {
        let mut t = with_context_plugin(r#"{"TKOG": "dog"}"#, "before");
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "(|KAT)", "nothing before the first stroke");
        t.stroke(stroke("TKOG"));
        t.stroke(stroke("TOP"));
        assert_eq!(t.text(), "(|KAT) dog ((|KAT) dog|TOP)");
    }

    #[test]
    fn a_run_is_told_the_text_before_its_first_stroke_never_its_own() {
        let mut t = with_context_plugin(r#"{"TKOG": "dog"}"#, "run");
        t.stroke(stroke("TKOG"));
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "dog (dog|KAT)");
        // Bit 23 is the plugin's join key: the run KAT/TOP is one word, asked about whole
        t.stroke(stroke("TOP") | (1 << 23));
        assert_eq!(
            t.text(),
            "dog (dog|KAT/TOP)",
            "what the run typed so far is not part of what came before it"
        );
    }

    #[test]
    fn a_wall_leaves_a_plugin_nothing_before_the_outline() {
        let mut t = with_context_plugin(r#"{"TKOG": "dog"}"#, "wall");
        t.stroke(stroke("TKOG"));
        // A key press, a window change: what is at the cursor is no longer known
        t.start_fresh();
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "(|KAT)");
    }

    #[test]
    fn a_stroke_that_moves_the_cursor_is_a_wall_too() {
        let mut t = with_context_plugin(r#"{"TKOG": "dog", "HREFT": "{#Left}"}"#, "barrier");
        t.stroke(stroke("TKOG"));
        t.stroke(stroke("HREFT"));
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "dog (|KAT)", "the screen no longer says what is at the cursor");
        t.stroke(stroke("TOP"));
        assert_eq!(
            t.text(),
            "dog (|KAT) ( (|KAT)|TOP)",
            "what was typed after it is known again"
        );
    }

    #[test]
    fn text_typed_by_hand_is_known_and_told() {
        let mut t = with_context_plugin(r#"{"TKOG": "dog"}"#, "by-hand");
        type_by_hand(&t, "hi ");
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "hi (hi |KAT)");
    }

    #[test]
    fn the_text_before_is_cut_to_its_last_256_bytes_on_a_character_boundary() {
        // 401 bytes: the last 256 start inside an `é`, so the cut moves on past it
        let long = format!("{}x", "é".repeat(200));
        let mut t = with_context_plugin(&format!(r#"{{"TKOG": "{long}"}}"#), "cap");
        t.stroke(stroke("TKOG"));
        t.stroke(stroke("KAT"));
        let told = format!("{}x", "é".repeat(127));
        assert_eq!(told.len(), 255);
        assert_eq!(t.text(), format!("{long} ({told}|KAT)"));
    }

    #[test]
    fn a_late_reading_is_asked_with_the_text_before_its_stroke_and_lands() {
        let (go, gate) = mpsc::channel::<()>();
        let mut dictionary = Dictionary::default();
        dictionary
            .merge_json(r#"{"TKOG": "dog"}"#, &Layout::plover())
            .unwrap();
        let chain = Chain::test_with_reader_of(
            dictionary,
            Layout::plover(),
            Box::new(
                move |outline: &str, before: &str, _: Option<&crate::steno::strokes::Window>| {
                    let _ = gate.recv();
                    Some(format!("({before}|{})", outline.to_lowercase()))
                },
            ),
            true,
        );
        let mut t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        t.set_plugin_deadline(Duration::from_millis(5));
        t.stroke(stroke("TKOG"));
        t.stroke(stroke("TOP"));
        assert!(t.has_late(), "{}", t.text());
        go.send(()).unwrap();
        settle(&mut t);
        assert_eq!(
            t.text(),
            "dog (dog|top)",
            "asked again with the same text, not what the stroke typed without it"
        );
    }

    #[test]
    fn a_late_reading_for_a_stroke_no_longer_last_is_dropped() {
        let (mut t, go) = with_gated_plugin(r#"{"KAT": "cat"}"#, Duration::from_millis(5));
        t.stroke(stroke("TOP"));
        t.stroke(stroke("KAT"));
        assert!(
            !t.has_late(),
            "KAT superseded it, and was read in time from the dictionary"
        );
        drop(go);
        let text = t.text();
        assert!(text.ends_with(" cat") && !text.starts_with("top"), "{text}");
    }

    #[test]
    fn a_plugin_answers_a_stroke_that_misses_every_dictionary() {
        let mut t = with_shout_plugin(r#"{"KAT": "cat"}"#, "answers_a_miss");
        t.stroke(stroke("KAT"));
        // TKOG misses the dictionary outright: the plugin's own reading is used instead of
        // the stroke's raw keys
        t.stroke(stroke("TKOG"));
        assert_eq!(t.text(), "cat TKOG");
    }

    /// A plugin reading a whole join-key run replaces the run's earlier strokes on every stroke;
    /// undo word takes the whole run back, not just its last stroke, and redo puts it all back.
    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // `{:undo_word}` is a dictionary command
    fn undo_word_takes_back_a_whole_plugin_run() {
        let mut t = with_shout_plugin(r#"{"KAT": "cat", "*U": "{:undo_word}", "R*": "{:redo}"}"#, "undo_word_run");
        let joined = stroke("TKOG") | (1 << 23);
        t.stroke(stroke("KAT"));
        t.stroke(stroke("TKOG"));
        t.stroke(joined);
        t.stroke(joined);
        let whole = t.text();
        assert!(whole.starts_with("cat ") && whole.len() > "cat TKOG".len(), "{whole:?}");
        t.stroke(stroke("*U"));
        assert_eq!(t.text(), "cat");
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), whole);
    }

    #[test]
    fn a_dictionary_hit_is_never_overridden_by_a_plugin() {
        let mut t = with_shout_plugin(r#"{"KAT": "cat", "TKOG": "dog"}"#, "never_overridden");
        t.stroke(stroke("KAT"));
        t.stroke(stroke("TKOG"));
        assert_eq!(t.text(), "cat dog");
    }

    #[test]
    fn a_join_key_stroke_with_nothing_to_glue_onto_is_not_given_to_a_plugin_either() {
        let mut t = with_shout_plugin(r#"{"KAT": "cat"}"#, "join_key_not_given");
        // Nothing precedes it, so it is never a word by itself, plugin included. Bit 23 is
        // SHOUT_PLUGIN's own made-up join key (see its `joins` export above).
        let joined_g = stroke("-G") | (1 << 23);
        t.stroke(joined_g);
        assert_eq!(t.text(), render_stroke(joined_g));
    }

    /// Keeps only the letters of what it is given: an empty answer for an outline with none,
    /// such as the bare `*` undo stroke — exactly what a phonetic-spelling plugin does for it.
    const BLANK_FOR_SYMBOLS_PLUGIN: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(ptr: i32, len: i32) -> i64 {
    let input = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let mut out: Vec<u8> = input.iter().copied().filter(u8::is_ascii_alphabetic).collect();
    let out_len = out.len();
    let out_ptr = out.as_mut_ptr();
    std::mem::forget(out);
    (i64::from(out_ptr as i32) << 32) | i64::from(out_len as i32)
}
"#;

    #[test]
    fn undo_still_fires_when_a_plugin_would_otherwise_answer_the_bare_star_with_nothing_useful() {
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-plugin-test-{}-undo_survives_a_plugin",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let dict_path = dir.join("01-dict.json");
        let plugin_path = dir.join("02-blank-for-symbols.rs");
        fs::write(&dict_path, r#"{"KAT": "cat"}"#).unwrap();
        fs::write(&plugin_path, BLANK_FOR_SYMBOLS_PLUGIN).unwrap();
        let chain = Chain::load_cached(
            &[dict_path, plugin_path],
            &dir.join("cache"),
            &Layout::plover(),
        );
        let mut t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "cat");
        t.stroke(UNDO_STROKE);
        assert_eq!(
            t.text(),
            "",
            "an empty plugin answer must never shadow undo"
        );
    }

    #[test]
    fn holding_a_join_key_repeat_keeps_gluing_with_no_space_or_stray_schwa() {
        // A dictionary's longest real key ("KPA/TKAOEU/-S/-D/-G/-Z", 6 strokes) is what the
        // engine's ordinary search is bounded by; a held join-key repeat has to grow past it
        // regardless, the same way a real word like "hahahahaha..." does on a real keyboard.
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-plugin-test-{}-held_join_key_repeat",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("orthographic-fallback.rs");
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs")
                .join("plugins")
                .join("orthographic-fallback.rs"),
            &source,
        )
        .unwrap();
        let dict_path = dir.join("01-dict.json");
        fs::write(&dict_path, r#"{"KPA/TKAOEU/-S/-D/-G/-Z": "dictionary"}"#).unwrap();
        let chain = Chain::load_cached(&[dict_path, source], &dir.join("cache"), &Layout::plover());
        assert_eq!(chain.as_dictionary().longest_key(), 6);
        let mut t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        // Bit 23 is this plugin's own made-up join key (see its `joins` export).
        let joined_ha = stroke("HA") | (1 << 23);
        t.stroke(stroke("HA"));
        t.stroke(joined_ha);
        t.stroke(joined_ha);
        assert_eq!(t.text(), "hahaha");
        for _ in 0..15 {
            t.stroke(joined_ha);
        }
        assert_eq!(
            t.text(),
            "ha".repeat(18),
            "a held join-key repeat keeps gluing past the dictionary's longest real key, with \
             no space and no stray \"uh\" from the engine's own word-start probe"
        );
    }

    /// Every call to `self.dict.joins()` can reach into a WASM plugin now, instead of a bare
    /// bitmask check - a theory's own join key is no longer free. Measured in release: well
    /// under 10µs per keystroke even with a real plugin loaded, so still imperceptible against
    /// how fast anyone can actually stroke.
    #[test]
    fn joining_stays_fast_with_a_real_plugin_loaded() {
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-plugin-test-{}-bench_joins",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("orthographic-fallback.rs");
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("docs")
                .join("plugins")
                .join("orthographic-fallback.rs"),
            &source,
        )
        .unwrap();
        let dict_path = dir.join("01-dict.json");
        fs::write(&dict_path, r#"{"KAT": "cat"}"#).unwrap();
        let chain = Chain::load_cached(&[dict_path, source], &dir.join("cache"), &Layout::plover());
        let mut t = Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        );
        let kat = stroke("KAT");
        let joined_ha = stroke("HA") | (1 << 23);
        let start = std::time::Instant::now();
        const N: u32 = 1000;
        for _ in 0..N {
            t.stroke(kat);
            t.stroke(joined_ha);
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 2000,
            "{N} keystroke pairs took {elapsed:?}, expected well under 2s even in a slow debug build"
        );
    }

    #[test]
    fn a_plugin_does_not_swallow_unrelated_strokes_into_one_word() {
        // The dictionary has a real multi-stroke brief ("KPA/TKAOEU"), so `longest_key()` is
        // bigger than one stroke and the engine's own search really does try windows of
        // several strokes, exactly like a real theory's dictionary does. Before the fix, the
        // plugin answered those wide windows too and glommed every stroke since the last
        // dictionary hit into one guess.
        let mut t = with_shout_plugin(
            r#"{"KPA/TKAOEU": "dictionary", "KAT": "cat"}"#,
            "does_not_swallow_unrelated_strokes",
        );
        t.stroke(stroke("KAT"));
        t.stroke(stroke("TOP"));
        t.stroke(stroke("WORLD"));
        assert_eq!(
            t.text(),
            "cat TOP WORLD",
            "each stroke the dictionary misses is its own word, not one blob of every miss \
             since the last hit"
        );
    }

    #[test]
    fn a_real_multi_stroke_brief_still_joins_normally_alongside_a_plugin() {
        let mut t = with_shout_plugin(
            r#"{"KPA/TKAOEU": "dictionary", "KAT": "cat"}"#,
            "multi_stroke_brief_still_joins",
        );
        t.stroke(stroke("KPA"));
        t.stroke(stroke("TKAOEU"));
        assert_eq!(t.text(), "dictionary");
    }

    /// Exports only `joins`: bit 23 (this test file's own made-up join key) continues the word
    /// before it; no fallback answer, ever (so a plain dictionary hit always wins).
    const JOIN_ONLY_PLUGIN: &str = r#"
#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    let mut v: Vec<u8> = vec![0u8; len as usize];
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr as i32
}

#[no_mangle]
pub extern "C" fn fallback(_ptr: i32, _len: i32) -> i64 {
    -1
}

#[no_mangle]
pub extern "C" fn joins(bits: i64) -> i32 {
    i32::from(bits & (1 << 23) != 0)
}
"#;

    /// A translator over a dictionary with `"KAT"`, `"-G"` and (a raw-bit entry a dictionary
    /// *file* can't express, since bit 23 is this test's own made-up join key, not keymux's
    /// own notation - see [`Dictionary::insert`]) a joined `"KAT/-G"` meaning "catting", plus
    /// `JOIN_ONLY_PLUGIN` so that join bit is actually reachable through the chain.
    fn join_dict_translator() -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat", "-G": "ging"}"#, &Layout::plover())
            .unwrap();
        let kat = Layout::plover().parse_stroke("KAT").unwrap();
        let joined_g = Layout::plover().parse_stroke("-G").unwrap() | (1 << 23);
        dict.insert(vec![kat, joined_g], "catting");
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-plugin-test-{}-join_dict",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let plugin_path = dir.join("01-join-only.rs");
        fs::write(&plugin_path, JOIN_ONLY_PLUGIN).unwrap();
        let chain = Chain::test_with_plugin(dict, &plugin_path, &dir.join("cache"));
        Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    #[test]
    fn a_join_key_stroke_only_matches_glued_onto_what_came_before() {
        let joined_g = Layout::plover().parse_stroke("-G").unwrap() | (1 << 23);
        // The plain stroke on its own still means what it always did
        let mut t = join_dict_translator();
        t.stroke(stroke("KAT"));
        t.stroke(stroke("-G"));
        assert_eq!(t.text(), "cat ging");
        // With the join key down, the same keys glue onto "cat" instead
        let mut t = join_dict_translator();
        t.stroke(stroke("KAT"));
        t.stroke(joined_g);
        assert_eq!(t.text(), "catting");
    }

    #[test]
    fn a_join_key_stroke_is_never_a_word_by_itself() {
        // Nothing comes before it to glue onto, and "-G" alone (without the join key) is a
        // different stroke, so this matches nothing: raw keys, not "ging"
        let joined_g = Layout::plover().parse_stroke("-G").unwrap() | (1 << 23);
        let mut t = join_dict_translator();
        t.stroke(joined_g);
        assert_eq!(t.text(), render_stroke(joined_g));
    }

    #[test]
    fn back_with_nothing_typed_presses_backspace() {
        let mut t = translator(BACK_DICT);
        let ops = t.stroke_ops(stroke("SPW"));
        assert_eq!(ops, vec![Op::Combo("BackSpace".to_string())]);
    }

    const HISTORY_DICT: &str = r#"{
        "KAT": "cat", "TKOG": "dog", "-G": "{^ing}", "PWEUPBG": "big",
        "R*": "{:redo}", "A*": "=undo:2", "*U": "{:undo_word}", "KWR*": "{:again}", "S*": "{:swap}"
    }"#;

    /// Type text on a plain layer: the translator is told afterwards, as the keyboard does.
    fn type_by_hand(t: &Translator, text: &str) {
        let shared = t.history();
        let mut history = journal::lock(&shared);
        for ch in text.chars() {
            history.typed(ch);
        }
    }

    #[test]
    fn redo_puts_back_what_undo_took_back() {
        let mut t = translator(HISTORY_DICT);
        for s in ["KAT", "TKOG", "*", "*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "");
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), "cat");
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), "cat dog");
        // Nothing more to redo
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), "cat dog");
    }

    #[test]
    fn new_input_empties_the_redo_history() {
        let mut t = translator(HISTORY_DICT);
        for s in ["KAT", "TKOG", "*", "PWEUPBG", "R*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "cat big");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a translation marker, not a format
    fn redo_restores_a_longer_outline_that_replaced_earlier_strokes() {
        let mut t = translator(r#"{"KAT": "cat", "KAT/TKOG": "catalog", "R*": "{:redo}"}"#);
        for s in ["KAT", "TKOG", "*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "cat");
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), "catalog");
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "cat");
    }

    #[test]
    fn undo_with_a_count_takes_back_that_many() {
        let mut t = translator(HISTORY_DICT);
        for s in ["KAT", "TKOG", "PWEUPBG", "A*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "cat");
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), "cat dog big");
    }

    #[test]
    fn undo_word_takes_a_stem_and_its_suffix_together() {
        let mut t = translator(HISTORY_DICT);
        for s in ["KAT", "TKOG", "-G", "*U"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "cat");
    }

    #[test]
    fn again_says_the_last_translation_once_more() {
        let mut t = translator(HISTORY_DICT);
        for s in ["KAT", "KWR*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "cat cat");
    }

    #[test]
    fn swap_exchanges_the_last_two_words() {
        let mut t = translator(HISTORY_DICT);
        for s in ["KAT", "TKOG", "S*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "dog cat");
        // Undoing the swap's last word leaves the first
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "dog");
    }

    #[test]
    fn steno_carries_on_from_text_typed_by_hand_and_undo_walks_back_through_it() {
        let mut t = translator(HISTORY_DICT);
        type_by_hand(&t, "hello world");
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "hello world cat");
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "hello world");
        // The word typed by hand goes as one piece
        let ops = t.stroke_ops(stroke("*"));
        assert_eq!(ops, vec![Op::Backspaces(5)]);
        assert_eq!(t.text(), "hello ");
    }

    #[test]
    fn a_space_typed_by_hand_attaches_the_next_word_and_a_full_stop_capitalises_it() {
        let mut t = translator(HISTORY_DICT);
        type_by_hand(&t, "Hi. ");
        t.stroke(stroke("KAT"));
        assert_eq!(t.text(), "Hi. Cat");
    }

    #[test]
    fn redo_works_for_text_typed_by_hand() {
        let mut t = translator(HISTORY_DICT);
        type_by_hand(&t, "hello world");
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "hello ");
        t.stroke(stroke("R*"));
        assert_eq!(t.text(), "hello world");
    }

    #[test]
    fn swap_exchanges_words_typed_by_hand() {
        let mut t = translator(HISTORY_DICT);
        type_by_hand(&t, "hello world");
        t.stroke(stroke("S*"));
        assert_eq!(t.text(), "world hello");
    }

    #[test]
    fn a_backspace_by_hand_over_a_stroke_is_put_back_by_undo() {
        let mut t = translator(HISTORY_DICT);
        t.stroke(stroke("KAT"));
        t.stroke(stroke("TKOG"));
        {
            let shared = t.history();
            assert!(journal::lock(&shared).backspace());
        }
        assert_eq!(t.text(), "cat do");
        let ops = t.stroke_ops(stroke("*"));
        assert_eq!(ops, vec![Op::Text("g".to_string())]);
        assert_eq!(t.text(), "cat dog");
    }

    #[test]
    fn a_fence_makes_the_next_word_start_fresh_and_nothing_undoable() {
        let mut t = translator(HISTORY_DICT);
        t.stroke(stroke("KAT"));
        t.start_fresh();
        let ops = t.stroke_ops(stroke("TKOG"));
        assert_eq!(ops, vec![Op::Text("dog".to_string())]);
    }

    fn two_machines() -> (Translator, Translator) {
        let shared = journal::new_shared(Formatter::new(Options {
            start_attached: true,
            ..Options::default()
        }));
        let make = |json: &str, machine| {
            let mut dict = Dictionary::default();
            dict.merge_json(json, &Layout::plover()).unwrap();
            Translator::with_history(Arc::new(Chain::from(dict)), Arc::clone(&shared), machine)
        };
        (
            make(r#"{"KAT": "cat"}"#, 1),
            make(
                r#"{"KAT/TKOG": "joined", "TKOG": "dog", "PWEUPBG": "big"}"#,
                2,
            ),
        )
    }

    #[test]
    fn another_machine_can_undo_what_this_one_typed() {
        let (mut a, mut b) = two_machines();
        a.stroke(stroke("KAT"));
        b.stroke(stroke("PWEUPBG"));
        assert_eq!(b.text(), "cat big");
        b.stroke(stroke("*"));
        b.stroke(stroke("*"));
        assert_eq!(a.text(), "");
    }

    #[test]
    fn strokes_only_join_with_strokes_of_their_own_machine() {
        let (mut a, mut b) = two_machines();
        a.stroke(stroke("KAT"));
        // `KAT/TKOG` is an outline of the second machine, but the KAT was not written with it
        b.stroke(stroke("TKOG"));
        assert_eq!(b.text(), "cat dog");
    }

    const SPACE_DICT: &str = r#"{
        "H*EU": "hi", "KAT": "cat", "TKOG": "dog", "KAT/TKOG": "catdog",
        "SP*": "{*!}", "S-PD": "{*?}", "R*E": "{:redo}"
    }"#;

    #[test]
    fn retro_insert_space_does_nothing_right_after_a_retro_delete_space_and_undo_is_the_way_back() {
        let mut t = translator(SPACE_DICT);
        for s in ["H*EU", "H*EU", "SP*"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "hihi");
        // As in Plover: insert space only splits a longer outline, it doesn't undo a join
        t.stroke(stroke("S-PD"));
        assert_eq!(t.text(), "hihi");
        // `*` puts the space back, and redo joins them again
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "hi hi");
        t.stroke(stroke("R*E"));
        assert_eq!(t.text(), "hihi");
    }

    #[test]
    fn retro_insert_space_splits_a_longer_outline_into_its_words() {
        let mut t = translator(SPACE_DICT);
        for s in ["KAT", "TKOG"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "catdog");
        t.stroke(stroke("S-PD"));
        assert_eq!(t.text(), "cat dog");
    }

    #[test]
    fn retro_delete_space_removes_only_the_space_before_the_last_word() {
        let mut t = translator(SPACE_DICT);
        for s in ["H*EU", "H*EU", "H*EU", "H*EU"] {
            t.stroke(stroke(s));
        }
        let ops = t.stroke_ops(stroke("SP*"));
        assert_eq!(ops, vec![Op::Backspaces(3), Op::Text("hi".to_string())]);
        assert_eq!(t.text(), "hi hi hihi");
    }

    #[test]
    fn retro_delete_space_after_undo_joins_the_words_left() {
        // Type three words, undo the last one back, and retro-delete-space should join
        // whatever two words are now actually last, not get confused by the one taken back.
        let mut t = translator(SPACE_DICT);
        for s in ["H*EU", "H*EU", "H*EU"] {
            t.stroke(stroke(s));
        }
        assert_eq!(t.text(), "hi hi hi");
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "hi hi");
        t.stroke(stroke("SP*"));
        assert_eq!(t.text(), "hihi");
    }

    #[test]
    fn retro_delete_space_can_be_redone_and_then_undone_and_redone_again() {
        // Join, undo the join back into two words, redo the join, undo it again: the join
        // and its two words round-trip cleanly however many times undo/redo alternate.
        let mut t = translator(SPACE_DICT);
        for s in ["H*EU", "H*EU"] {
            t.stroke(stroke(s));
        }
        t.stroke(stroke("SP*"));
        assert_eq!(t.text(), "hihi");
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "hi hi");
        t.stroke(stroke("SP*"));
        assert_eq!(t.text(), "hihi");
    }

    #[test]
    fn a_retro_delete_space_that_does_nothing_does_not_clear_what_redo_would_restore() {
        // Undo leaves one word. Retro-delete-space then has nothing to join (fewer than two
        // words) and silently does nothing, as it should; that no-op must not also throw away
        // the redo history undo left behind, or the next undo becomes unrecoverable.
        let mut t = translator(SPACE_DICT);
        for s in ["H*EU", "H*EU"] {
            t.stroke(stroke(s));
        }
        t.stroke(stroke("*"));
        assert_eq!(t.text(), "hi");
        t.stroke(stroke("SP*"));
        assert_eq!(
            t.text(),
            "hi",
            "a no-op retro must not type or delete anything"
        );
        t.stroke(stroke("R*E"));
        assert_eq!(
            t.text(),
            "hi hi",
            "redo must still restore the word undo took back, even after a no-op retro attempt"
        );
    }

    #[test]
    fn a_retro_delete_space_blocked_by_chaining_does_not_clear_the_redo_history_either() {
        // Four words; retro-join the last two (3rd+4th), then type a 5th word, then undo just
        // that 5th word back. The last two translations are now [word2, the join], and the
        // join is itself a retrospective command, so retro-delete-space refuses to chain
        // another join onto it (as Plover does too) and silently does nothing. That refusal
        // must not clear the redo entry undoing word 5 left behind.
        let mut t = translator(SPACE_DICT);
        for s in ["H*EU", "H*EU", "H*EU"] {
            t.stroke(stroke(s));
        }
        t.stroke(stroke("SP*")); // joins word2+word3
        assert_eq!(t.text(), "hi hihi");
        t.stroke(stroke("H*EU")); // a 4th word
        assert_eq!(t.text(), "hi hihi hi");
        t.stroke(stroke("*")); // undo just the 4th word
        assert_eq!(t.text(), "hi hihi");
        // Blocked: chaining a join onto the existing join is refused, so this is a no-op.
        t.stroke(stroke("SP*"));
        assert_eq!(
            t.text(),
            "hi hihi",
            "chaining a retro join onto a join must stay blocked"
        );
        // Redo must still restore the 4th word that undo took back, since the blocked retro
        // attempt just above did nothing and so must not have cleared the redo history.
        t.stroke(stroke("R*E"));
        assert_eq!(
            t.text(),
            "hi hihi hi",
            "redo must still restore the undone word after a chaining-blocked retro attempt"
        );
    }

    #[test]
    fn a_stroke_types_its_translation() {
        assert_eq!(type_strokes(r#"{"KAT": "cat"}"#, &["KAT"]), "cat");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // translation markers, not formats
    fn the_first_up_marker_types_nothing_and_changes_nothing_around_it() {
        let json = r#"{"KAT": "cat{:firstup}", "TKOG": "{:firstup}dog", "-S": "{^s}{:firstup}", "H-L": "{:firstup}{^}"}"#;
        assert_eq!(type_strokes(json, &["KAT", "TKOG"]), "cat dog");
        assert_eq!(type_strokes(json, &["KAT", "-S"]), "cats");
        assert_eq!(type_strokes(json, &["KAT", "H-L", "TKOG"]), "catdog");
        let t = translator(json);
        assert_eq!(t.first_up_exempt(stroke("KAT")), Some(0));
        assert_eq!(t.first_up_exempt(stroke("TKOG")), Some(0));
        assert_eq!(translator(r#"{"KAT": "cat"}"#).first_up_exempt(stroke("KAT")), None);
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // translation markers, not formats
    fn the_first_up_exception_list_parses_with_or_without_a_hyphen_and_types_nothing() {
        let json = r#"{"KAT": "{:firstup:except=T}cat{^}", "TKOG": "{:firstup:except=-FRPBLGTS}{^}dog", "-S": "{:firstup:except=}s", "H-L": "{:firstup:except=QQ}hl", "A": "{:firstupx}a"}"#;
        let t = translator(json);
        assert_eq!(t.first_up_exempt(stroke("KAT")), Some(stroke("T")));
        assert_eq!(t.first_up_exempt(stroke("TKOG")), Some(stroke("-FRPBLGTS")));
        assert_eq!(stroke("-FRPBLGTS"), stroke("FRPBLGTS"));
        // Malformed lists are plain `{:firstup}`; a near-miss name is no marker
        assert_eq!(t.first_up_exempt(stroke("-S")), Some(0));
        assert_eq!(t.first_up_exempt(stroke("H-L")), Some(0));
        assert_eq!(t.first_up_exempt(stroke("A")), None);
        assert_eq!(type_strokes(json, &["KAT", "TKOG"]), "catdog");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // a command, not a format
    fn engine_commands_are_told_from_other_entries() {
        // `{PLOVER:lookup}` is the legacy alias of `{:lookup}`, kept for other people's dictionaries
        let t =
            translator(r#"{"KAT": "cat", "TKOG": "{plover:toggle}", "H-L": "{PLOVER:lookup}"}"#);
        assert!(!t.is_command(stroke("KAT")));
        assert!(t.is_command(stroke("TKOG")));
        assert!(t.is_command(stroke("H-L")));
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // commands, not formats
    fn native_and_legacy_spellings_make_the_same_command() {
        let mut t = translator(
            r#"{"A": "{:engine:toggle}", "E": "{PLOVER:TOGGLE}", "O": "{:engine:suspend}", "U": "{plover:suspend}",
                "AU": "{:run:pw}", "AE": "{PLOVER:RUN:pw}", "AO": "{:command:toggle}", "EU": "{:engine:sideways}"}"#,
        );
        for (native, legacy) in [("A", "E"), ("O", "U")] {
            assert!(t.is_command(stroke(native)) && t.is_command(stroke(legacy)), "{native} {legacy}");
            let (native, legacy) = (commands_of(&mut t, native), commands_of(&mut t, legacy));
            assert_eq!(native[0].to_lowercase(), legacy[0].to_lowercase());
        }
        assert_eq!(commands_of(&mut t, "A"), ["toggle"]);
        assert_eq!(commands_of(&mut t, "O"), ["suspend"]);
        assert_eq!(commands_of(&mut t, "AU"), ["run:pw"]);
        assert_eq!(commands_of(&mut t, "AE")[0].to_lowercase(), "run:pw");
        assert_eq!(commands_of(&mut t, "AO"), ["toggle"]);
        // An engine verb that does not exist makes no command
        assert!(commands_of(&mut t, "EU").is_empty(), "expected none");
    }

    #[test]
    fn words_are_spaced_and_suffixes_attach() {
        let json = r#"{"KAT": "cat", "-S": "{^s}", "TKOG": "dog"}"#;
        assert_eq!(type_strokes(json, &["KAT", "TKOG"]), "cat dog");
        assert_eq!(type_strokes(json, &["KAT", "-S"]), "cats");
    }

    #[test]
    fn a_longer_entry_replaces_what_it_covers_and_undo_restores_it() {
        let json = r#"{"PER": "per", "TPEBGT": "effect", "PER/TPEBGT": "perfect"}"#;
        assert_eq!(type_strokes(json, &["PER", "TPEBGT"]), "perfect");
        assert_eq!(type_strokes(json, &["PER", "TPEBGT", "*"]), "per");
        assert_eq!(type_strokes(json, &["PER", "TPEBGT", "*", "*"]), "");
    }

    #[test]
    fn a_stroke_with_no_entry_types_its_keys_and_can_be_undone() {
        let json = r#"{"KAT": "cat"}"#;
        assert_eq!(type_strokes(json, &["TPHEUFR"]), "TPH-EUFR");
        assert_eq!(type_strokes(json, &["TPHEUFR", "*"]), "");
        let mut t = translator(json);
        t.stroke(stroke("TPHEUFR"));
        assert!(t.was_unmatched());
    }

    #[test]
    fn a_stroke_that_starts_a_longer_entry_is_typed_as_its_keys_until_it_is_completed() {
        let json = r#"{"KPA/TKAOEU": "example"}"#;
        assert_eq!(type_strokes(json, &["KPA"]), "KPA-");
        assert_eq!(type_strokes(json, &["KPA", "TKAOEU"]), "example");
        assert_eq!(type_strokes(json, &["KPA", "TKAOEU", "*"]), "KPA-");
    }

    #[test]
    fn undo_with_nothing_on_the_tape_deletes_a_word_with_the_keys_that_do_it() {
        let mut t = translator("{}");
        assert_eq!(
            t.stroke(UNDO_STROKE),
            Some(StenoOutput::Ops(vec![OutputOp::Keys(vec![
                (KeyCode::KC_LCTL, true),
                (KeyCode::KC_BSPC, true),
                (KeyCode::KC_BSPC, false),
                (KeyCode::KC_LCTL, false),
            ])]))
        );
    }

    #[test]
    fn a_suffix_key_is_folded_into_a_stroke_with_no_entry() {
        let json = r#"{"KAT": "cat", "-S": "{^s}"}"#;
        assert_eq!(type_strokes(json, &["KATS"]), "cats");
    }

    #[test]
    fn a_word_boundary_entry_is_used_when_the_word_before_is_finished() {
        let json = r#"{"S": "word", "/S": "{prefix^}", "KOPB": "{con^}"}"#;
        assert_eq!(type_strokes(json, &["S"]), "prefix");
        // After an unfinished word, the plain entry is used, and it attaches
        assert_eq!(type_strokes(json, &["KOPB", "S"]), "conword");
    }

    #[test]
    fn a_deleted_entry_is_untranslated() {
        let json = format!(r#"{{"KAT": "{}"}}"#, super::super::dict::DELETED);
        assert_eq!(type_strokes(&json, &["KAT"]), "KA-T");
    }

    /// The commands a stroke made, as written (`lookup` for `{:lookup}`; a `{PLOVER:...}` command as written).
    fn commands_of(t: &mut Translator, stroke_text: &str) -> Vec<String> {
        t.stroke(stroke(stroke_text))
            .map(StenoOutput::into_ops)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|op| match op {
                OutputOp::Command(command) => Some(command),
                _ => None,
            })
            .collect()
    }

    const COMMAND_JSON: &str =
        r#"{"KAT": "cat", "TKOG": "dog", "PHR*UP": "{:lookup}", "PHR*UP/TKOG": "lookdog"}"#;

    #[test]
    fn a_command_stroke_runs_when_struck() {
        let mut t = translator(COMMAND_JSON);
        assert_eq!(commands_of(&mut t, "PHR*UP"), ["lookup"]);
    }

    #[test]
    fn striking_a_command_stroke_again_runs_it_again() {
        let mut t = translator(COMMAND_JSON);
        assert_eq!(commands_of(&mut t, "PHR*UP"), ["lookup"]);
        assert_eq!(commands_of(&mut t, "PHR*UP"), ["lookup"]);
    }

    #[test]
    fn undoing_a_longer_entry_never_puts_back_the_commands_it_replaced() {
        // PHR*UP then TKOG becomes the one entry "lookdog", replacing the command's translation;
        // undoing "lookdog" puts that translation back, which must not run the command again
        for strokes in [
            vec!["PHR*UP", "TKOG", "*"],
            vec!["KAT", "PHR*UP", "TKOG", "*"],
        ] {
            let mut t = translator(COMMAND_JSON);
            let mut last = Vec::new();
            for s in &strokes {
                last = commands_of(&mut t, s);
            }
            assert!(last.is_empty(), "{strokes:?}: undo ran {last:?}");
        }
    }

    #[test]
    fn undo_after_a_command_takes_back_the_word_before_it_and_runs_nothing() {
        let mut t = translator(COMMAND_JSON);
        commands_of(&mut t, "KAT");
        commands_of(&mut t, "PHR*UP");
        assert!(commands_of(&mut t, "*").is_empty(), "expected none");
        assert_eq!(t.text(), "");
    }

    // The stroke classes: transparent, normal, barrier.

    fn joined_stroke(text: &str) -> u32 {
        if text == "*" {
            return UNDO_STROKE;
        }
        Layout::plover_with(&['+'])
            .unwrap()
            .parse_stroke(text)
            .unwrap_or_else(|| panic!("{text} is not a stroke"))
    }

    #[allow(clippy::literal_string_with_formatting_args)] // markers, not formats
    const CLASS_JSON: &str = r#"{
        "THAS": "thanks", "THAS/+EUFG": "thanksgiving", "TKOG": "dog", "KAT": "cat",
        "PHRUP": "{:lookup}", "STKPW": "{:transparent}{plover:focus}",
        "TKPWA": "{:transparent}ignored text",
        "SP": "{^ ^}", "*": "=undo", "R-R": "{#Return}", "HR-L": "{#Left}", "PWA*RG": "{:barrier}",
        "R*L": "{#Right}{:nobarrier}", "KA*PD": "{#Control_L(c)}", "RA*P": "{:back}"
    }"#;

    /// A translator over `json` with the shout plugin's join key (`+`, bit 23).
    fn class_translator(json: &str, name: &str) -> Translator {
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-class-test-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let layout = Layout::plover_with(&['+']).unwrap();
        let dict_path = dir.join("01-dict.json");
        fs::write(&dict_path, json).unwrap();
        fs::write(dir.join("02-shout.rs"), SHOUT_PLUGIN).unwrap();
        let chain = Chain::load_cached(
            &[dict_path, dir.join("02-shout.rs")],
            &dir.join("cache"),
            &layout,
        );
        Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    /// A translator over `json` with NO plugin: the join key comes from the layout's
    /// `join_keys` (an empty list declares none).
    fn config_join_translator(json: &str, name: &str, join_keys: &[char]) -> Translator {
        let dir = std::env::temp_dir().join(format!(
            "keymux-translate-joinkey-test-{}-{name}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let layout = Layout::plover_with_joins(&['+'], join_keys).unwrap();
        let dict_path = dir.join("01-dict.json");
        fs::write(&dict_path, json).unwrap();
        let chain = Chain::load_cached(&[dict_path], &dir.join("cache"), &layout);
        assert_eq!(chain.plugin_count(), 0);
        Translator::with_shared(
            Arc::new(chain),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    #[test]
    fn a_config_join_key_joins_exactly_as_a_plugins_join_bit_does() {
        for (i, strokes) in [
            vec!["THAS", "+EUFG"],
            vec!["THAS", "PHRUP", "+EUFG"],
            vec!["THAS", "STKPW", "+EUFG"],
            vec!["THAS", "+EUFG", "*"],
        ]
        .iter()
        .enumerate()
        {
            let mut with_plugin = class_translator(CLASS_JSON, &format!("eq-plugin-{i}"));
            let mut with_config =
                config_join_translator(CLASS_JSON, &format!("eq-config-{i}"), &['+']);
            assert_eq!(
                run(&mut with_config, strokes),
                run(&mut with_plugin, strokes),
                "{strokes:?}"
            );
            assert_eq!(
                history_len(&with_config),
                history_len(&with_plugin),
                "{strokes:?}"
            );
        }
        let mut t = config_join_translator(CLASS_JSON, "eq-plain", &['+']);
        assert_eq!(run(&mut t, &["THAS", "+EUFG"]), "thanksgiving");
        let mut t = config_join_translator(CLASS_JSON, "eq-transparent", &['+']);
        assert_eq!(
            run(&mut t, &["THAS", "PHRUP", "STKPW", "+EUFG"]),
            "thanksgiving"
        );
    }

    #[test]
    fn a_layer_without_join_keys_and_without_a_plugin_has_no_joins() {
        let t = config_join_translator(CLASS_JSON, "none-declared", &[]);
        assert!(!t.joins(joined_stroke("+EUFG")));
        // A continuation stroke with no joined outline falls back to its own entry, so a
        // suffix entry attaches with just its stroke - with or without the key declared
        let suffix = r#"{"KAT": "cat", "+-D": "{^ed}", "THAS": "thanks", "THAS/+-D": "thanked", "PHRUP": "{:lookup}"}"#;
        for (name, keys) in [("suffix-none", &[][..]), ("suffix-decl", &['+'][..])] {
            let mut t = config_join_translator(suffix, name, keys);
            assert_eq!(run(&mut t, &["KAT", "+-D"]), "catted", "{name}");
        }
        // a longer joined outline beats the standalone entry, transparent strokes in between
        let mut t = config_join_translator(suffix, "suffix-longer", &['+']);
        assert_eq!(run(&mut t, &["THAS", "+-D"]), "thanked");
        let mut t = config_join_translator(suffix, "suffix-mid", &['+']);
        assert_eq!(run(&mut t, &["THAS", "PHRUP", "+-D"]), "thanked");
        let mut t = config_join_translator(suffix, "suffix-mid2", &['+']);
        assert_eq!(run(&mut t, &["KAT", "PHRUP", "+-D"]), "catted");
        let mut p = class_translator(suffix, "suffix-plugin-longer");
        assert_eq!(run(&mut p, &["THAS", "+-D"]), "thanked");
    }

    fn run(t: &mut Translator, strokes: &[&str]) -> String {
        for s in strokes {
            t.stroke(joined_stroke(s));
        }
        t.text()
    }

    fn history_len(t: &Translator) -> usize {
        journal::lock(&t.history()).state.translations.len()
    }

    #[test]
    fn a_lookup_in_the_middle_of_a_join_changes_nothing() {
        // The reported bug: THAS / lookup / +EUFG must be THAS / +EUFG
        let mut plain = class_translator(CLASS_JSON, "lookup-plain");
        assert_eq!(run(&mut plain, &["THAS", "+EUFG"]), "thanksgiving");
        let mut t = class_translator(CLASS_JSON, "lookup-mid");
        assert_eq!(run(&mut t, &["THAS", "PHRUP", "+EUFG"]), "thanksgiving");
    }

    #[test]
    fn a_lookup_in_the_middle_breaks_the_join_when_the_classes_are_off() {
        let mut t = class_translator(CLASS_JSON, "lookup-off");
        t.set_stroke_classes(false);
        assert_ne!(run(&mut t, &["THAS", "PHRUP", "+EUFG"]), "thanksgiving");
    }

    #[test]
    fn a_transparent_stroke_is_not_in_the_history_and_still_runs_its_commands() {
        let mut t = class_translator(CLASS_JSON, "transparent-history");
        run(&mut t, &["KAT"]);
        let before = history_len(&t);
        let out = t.stroke(joined_stroke("STKPW"));
        assert!(t.was_transparent());
        assert_eq!(
            out,
            Some(StenoOutput::Ops(vec![OutputOp::Command("focus".into())]))
        );
        assert_eq!(history_len(&t), before);
        // Its text is dropped, and the next word goes on with the spacing it had
        assert_eq!(t.stroke(joined_stroke("TKPWA")), None);
        assert_eq!(run(&mut t, &["TKOG"]), "cat dog");
        assert!(!t.was_transparent());
    }

    #[test]
    fn transparent_strokes_at_the_start_the_end_and_in_a_row_leave_no_trace() {
        let mut t = class_translator(CLASS_JSON, "transparent-row");
        let text = run(
            &mut t,
            &["PHRUP", "STKPW", "THAS", "PHRUP", "PHRUP", "STKPW", "+EUFG", "PHRUP"],
        );
        assert_eq!(text, "thanksgiving");
        assert_eq!(history_len(&t), 1);
    }

    #[test]
    fn undo_skips_transparent_strokes_and_redo_survives_them() {
        let mut t = class_translator(CLASS_JSON, "transparent-undo");
        run(&mut t, &["KAT", "TKOG", "PHRUP"]);
        assert_eq!(run(&mut t, &["*"]), "cat");
        run(&mut t, &["PHRUP"]);
        assert_eq!(journal::lock(&t.history()).redo.len(), 1);
        assert_eq!(run(&mut t, &["*"]), "");
    }

    #[test]
    fn a_transparent_stroke_does_not_disturb_capitals_or_attachment() {
        let json = r#"{"KAT": "cat", "TKOG": "dog", "KP*": "{-|}", "STKPW": "{:transparent}{plover:focus}"}"#;
        let mut t = class_translator(json, "transparent-caps");
        assert_eq!(run(&mut t, &["KAT", "KP*", "STKPW", "TKOG"]), "cat Dog");
    }

    #[test]
    fn a_space_between_separates_a_continuation() {
        let mut t = class_translator(CLASS_JSON, "space");
        let text = run(&mut t, &["THAS", "SP", "+EUFG"]);
        assert!(!text.contains("thanksgiving"), "{text:?}");
        assert!(text.starts_with("thanks "), "{text:?}");
    }

    #[test]
    fn a_cursor_key_is_a_barrier_a_continuation_does_not_cross() {
        for key in ["HR-L", "PWA*RG", "RA*P"] {
            let mut t = class_translator(CLASS_JSON, &format!("barrier-{}", key.len() + key.as_bytes()[0] as usize));
            run(&mut t, &["THAS", key, "+EUFG"]);
            let text = t.text();
            assert!(!text.contains("thanksgiving"), "{key}: {text:?}");
        }
    }

    #[test]
    fn a_return_a_shortcut_and_a_nobarrier_entry_are_not_barriers() {
        let barrier = |entry: &str, name: &str| {
            let mut t = class_translator(CLASS_JSON, name);
            run(&mut t, &["THAS"]);
            t.stroke(joined_stroke(entry));
            let shared = t.history();
            let history = journal::lock(&shared);
            is_barrier(history.state.translations.last().unwrap())
        };
        assert!(barrier("HR-L", "is-left"));
        assert!(barrier("PWA*RG", "is-marked"));
        assert!(!barrier("R-R", "not-return"));
        assert!(!barrier("KA*PD", "not-copy"));
        assert!(!barrier("R*L", "not-nobarrier"));
    }

    #[test]
    fn barriers_do_nothing_when_the_classes_are_off() {
        let mut t = class_translator(CLASS_JSON, "barrier-off");
        t.set_stroke_classes(false);
        run(&mut t, &["KAT", "HR-L", "TKOG"]);
        assert_eq!(run(&mut t, &["*"]).trim(), "cat");
    }

    #[allow(clippy::literal_string_with_formatting_args)] // markers, not formats
    const UNDO_OR_JSON: &str = r#"{
        "KAT": "cat", "TKOG": "dog", "PHRUP": "{:lookup}", "STKPW": "{:transparent}{plover:focus}",
        "HR-L": "{#Left}", "PWA*RG": "{:barrier}", "TKEL": "{#Control_L(BackSpace)}",
        "*U": "{:undo_or:Control_L(a)}", "*E": "{:undo_or:ctrl+backspace}",
        "*EU": "{:undo_or_ctrl+backspace}", "*P": "{:undo_or}", "*": "=undo", "R-R": "{:redo}"
    }"#;

    /// The keys of one stroke, for comparing which combination was sent.
    fn keys_of(t: &mut Translator, text: &str) -> Vec<(KeyCode, bool)> {
        t.stroke(joined_stroke(text))
            .map(StenoOutput::into_ops)
            .unwrap_or_default()
            .into_iter()
            .find_map(|op| match op {
                OutputOp::Keys(keys) => Some(keys),
                _ => None,
            })
            .unwrap_or_default()
    }

    fn word_delete_keys() -> Vec<(KeyCode, bool)> {
        vec![
            (KeyCode::KC_LCTL, true),
            (KeyCode::KC_BSPC, true),
            (KeyCode::KC_BSPC, false),
            (KeyCode::KC_LCTL, false),
        ]
    }

    #[test]
    fn undo_stops_at_a_barrier_by_default() {
        for (name, barrier) in [("arrow", "HR-L"), ("word", "TKEL"), ("marked", "PWA*RG")] {
            let mut t = class_translator(UNDO_OR_JSON, &format!("undo-barrier-{name}"));
            run(&mut t, &["KAT", barrier, "TKOG"]);
            // The word after the barrier goes
            run(&mut t, &["*"]);
            assert_eq!(t.text(), "cat", "{name}");
            // The next undo meets the barrier and goes no further: it does what undo does with
            // nothing to take back (deletes a word with the keys that do it), the barrier is
            // used up and what was before it is forgotten, so "cat" is never taken back
            let keys = keys_of(&mut t, "*");
            assert_eq!(keys, word_delete_keys(), "{name}");
            assert_eq!(history_len(&t), 0, "{name}");
        }
    }

    #[test]
    fn undo_right_after_a_barrier_is_blocked_too() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-barrier-direct");
        run(&mut t, &["KAT", "HR-L"]);
        assert_eq!(keys_of(&mut t, "*"), word_delete_keys());
        assert_eq!(history_len(&t), 0);
    }

    #[test]
    fn undo_reaches_back_past_a_barrier_when_the_setting_is_off_as_plover_does() {
        for barrier in ["HR-L", "TKEL", "PWA*RG"] {
            let mut t = class_translator(UNDO_OR_JSON, "undo-barrier-off");
            t.set_undo_stops_at_barrier(false);
            run(&mut t, &["KAT", barrier, "TKOG"]);
            run(&mut t, &["*"]);
            assert_eq!(run(&mut t, &["*"]), "", "{barrier}");
        }
    }

    #[test]
    fn undo_or_undoes_when_it_can() {
        for key in ["*U", "*E", "*EU", "*P"] {
            let mut t = class_translator(UNDO_OR_JSON, "undo-or-can");
            run(&mut t, &["KAT", "TKOG"]);
            assert_eq!(run(&mut t, &[key]), "cat", "{key}");
            assert_eq!(history_len(&t), 1, "{key}");
            // Taking back keeps what redo can put back
            assert_eq!(run(&mut t, &["R-R"]), "cat dog", "{key}");
        }
    }

    #[test]
    fn undo_or_sends_its_combination_when_blocked_by_a_barrier() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-blocked");
        run(&mut t, &["KAT", "HR-L", "TKOG"]);
        run(&mut t, &["*U"]);
        assert_eq!(t.text(), "cat", "the word after the barrier is undone");
        // Blocked now: the canonical form sends Control+a, not the word delete
        assert_eq!(
            keys_of(&mut t, "*U"),
            vec![
                (KeyCode::KC_LCTL, true),
                (KeyCode::KC_A, true),
                (KeyCode::KC_A, false),
                (KeyCode::KC_LCTL, false),
            ]
        );
        assert_eq!(history_len(&t), 0, "the barrier is used up");
        // Each spelling of Control+BackSpace is what plain undo falls back to
        for (key, name) in [("*E", "plus"), ("*EU", "name"), ("*P", "empty")] {
            let mut t = class_translator(UNDO_OR_JSON, &format!("undo-or-blocked-{name}"));
            run(&mut t, &["KAT", "TKEL"]);
            assert_eq!(keys_of(&mut t, key), word_delete_keys(), "{key}");
        }
    }

    #[test]
    fn undo_or_blocked_empties_redo_but_undoing_keeps_it() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-redo");
        run(&mut t, &["KAT", "HR-L", "TKOG", "*U"]);
        // Blocked: the combination is new input, so what was undone cannot come back
        keys_of(&mut t, "*U");
        assert_eq!(run(&mut t, &["R-R"]), "cat");
    }

    #[test]
    fn undo_or_with_nothing_to_undo_sends_its_combination() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-empty");
        assert_eq!(keys_of(&mut t, "*E"), word_delete_keys());
        // Again and again: it stays a key press each time
        assert_eq!(keys_of(&mut t, "*E"), word_delete_keys());
        assert_eq!(history_len(&t), 0);
    }

    #[test]
    fn undo_or_repeated_undoes_each_word_then_sends_the_combination() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-repeat");
        run(&mut t, &["KAT", "TKOG"]);
        assert_eq!(run(&mut t, &["*E"]), "cat");
        assert_eq!(run(&mut t, &["*E"]), "");
        assert_eq!(keys_of(&mut t, "*E"), word_delete_keys());
    }

    #[test]
    fn undo_or_skips_transparent_strokes_like_undo() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-transparent");
        run(&mut t, &["KAT", "TKOG", "PHRUP", "STKPW"]);
        assert_eq!(run(&mut t, &["*E"]), "cat", "the transparent strokes are not in the way");
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-transparent-blocked");
        run(&mut t, &["KAT", "HR-L", "PHRUP", "STKPW"]);
        assert_eq!(keys_of(&mut t, "*E"), word_delete_keys(), "and do not hide a barrier");
    }

    #[test]
    fn undo_or_off_reaches_past_a_barrier_before_falling_back() {
        let mut t = class_translator(UNDO_OR_JSON, "undo-or-off");
        t.set_undo_stops_at_barrier(false);
        run(&mut t, &["KAT", "HR-L"]);
        assert_eq!(run(&mut t, &["*E"]), "");
        assert_eq!(keys_of(&mut t, "*E"), word_delete_keys());
    }

    #[test]
    fn undo_or_combination_shorthand() {
        assert_eq!(undo_or_combo("ctrl+backspace"), "Control_L(backspace)");
        assert_eq!(undo_or_combo("Ctrl+Shift+z"), "Control_L(Shift_L(z))");
        assert_eq!(undo_or_combo("Control_L(BackSpace)"), "Control_L(BackSpace)");
        assert_eq!(undo_or_combo("Left"), "Left");
        assert_eq!(undo_or_combo(""), "");
    }

    #[test]
    fn mixed_classes_in_a_row_keep_each_rule() {
        let mut t = class_translator(CLASS_JSON, "mixed");
        let text = run(&mut t, &["THAS", "PHRUP", "SP", "PHRUP", "+EUFG"]);
        assert!(!text.contains("thanksgiving"), "{text:?}");
        let text = run(&mut t, &["SP", "THAS", "STKPW", "PHRUP", "+EUFG"]);
        assert!(text.ends_with("thanksgiving"), "{text:?}");
    }

    const CONTROL_JSON: &str = r#"{"KAT": "cat", "TKOG": "dog", "-G": "{^ing}",
        "SKWR": "{:overlay:toggle}", "SKWR*": "{:tape:clear}", "-FP": "{:lookup:toggle}",
        "-B": "{:tape:sideways}"}"#;

    #[test]
    fn overlay_controls_are_commands_that_type_nothing() {
        let mut t = translator(CONTROL_JSON);
        assert!(t.is_command(stroke("SKWR")));
        assert!(t.is_command(stroke("SKWR*")));
        assert!(t.is_command(stroke("-FP")));
        assert!(!t.is_command(stroke("KAT")));
        assert_eq!(commands_of(&mut t, "SKWR"), ["overlay_control:overlay:toggle"]);
        assert_eq!(commands_of(&mut t, "SKWR*"), ["overlay_control:tape:clear"]);
        assert_eq!(commands_of(&mut t, "-FP"), ["overlay_control:lookup:toggle"]);
        assert_eq!(t.text(), "");
        // A control that is not one runs nothing
        assert!(commands_of(&mut t, "-B").is_empty(), "expected none");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)] // commands, not formats
    fn the_bare_lookup_command_opens_the_lookup_like_its_plover_alias() {
        let mut t = translator(r#"{"-FPL": "{:lookup}", "-FP": "{plover:lookup}", "KAT": "cat"}"#);
        assert!(t.is_command(stroke("-FPL")));
        assert_eq!(commands_of(&mut t, "-FPL"), ["lookup"]);
        assert_eq!(t.text(), "");
        assert_eq!(commands_of(&mut t, "-FP"), ["lookup"]);
    }

    const CONTROL_CLASS_JSON: &str = r#"{
        "THAS": "thanks", "THAS/+EUFG": "thanksgiving", "KAT": "cat", "-G": "{^ing}",
        "SKWR": "{:overlay:toggle}", "SKWR*": "{:tape:clear}", "-FP": "{:lookup:toggle}"
    }"#;

    #[test]
    fn overlay_controls_are_transparent_strokes() {
        let mut t = class_translator(CONTROL_CLASS_JSON, "control-class");
        run(&mut t, &["KAT"]);
        let before = history_len(&t);
        let out = t.stroke(joined_stroke("SKWR"));
        assert!(t.was_transparent());
        assert_eq!(
            out,
            Some(StenoOutput::Ops(vec![OutputOp::Command(
                "overlay_control:overlay:toggle".into()
            )]))
        );
        assert_eq!(history_len(&t), before, "no history entry");
        assert_eq!(run(&mut t, &["-G"]), "catting", "the suffix still attaches");
    }

    #[test]
    fn a_control_in_the_middle_of_a_join_changes_nothing() {
        for (name, control) in [("toggle", "SKWR"), ("clear", "SKWR*"), ("lookup", "-FP")] {
            let mut t = class_translator(CONTROL_CLASS_JSON, &format!("control-mid-{name}"));
            assert_eq!(
                run(&mut t, &["THAS", control, "+EUFG"]),
                "thanksgiving",
                "{control}"
            );
            assert_eq!(history_len(&t), 1);
        }
    }

    #[test]
    fn a_join_carries_across_several_controls() {
        let mut t = class_translator(CONTROL_CLASS_JSON, "control-several");
        let text = run(&mut t, &["THAS", "SKWR", "SKWR*", "-FP", "+EUFG"]);
        assert_eq!(text, "thanksgiving");
    }

    #[test]
    fn a_control_breaks_the_join_only_when_the_classes_are_off() {
        let mut t = class_translator(CONTROL_CLASS_JSON, "control-off");
        t.set_stroke_classes(false);
        assert_ne!(run(&mut t, &["THAS", "SKWR", "+EUFG"]), "thanksgiving");
        assert!(!t.was_transparent());
    }

    #[test]
    fn the_unload_control_is_a_transparent_command_that_types_nothing() {
        let json = r#"{"KAT": "cat", "-G": "{^ing}", "SKWR": "{:engine:unload}", "-B": "{:engine:sideways}"}"#;
        let mut t = class_translator(json, "engine-unload");
        assert!(t.is_command(stroke("SKWR")), "never repeats when held");
        run(&mut t, &["KAT"]);
        let before = history_len(&t);
        let out = t.stroke(joined_stroke("SKWR"));
        assert!(t.was_transparent());
        assert_eq!(
            out,
            Some(StenoOutput::Ops(vec![OutputOp::Command(
                "engine_control:engine:unload".into()
            )]))
        );
        assert_eq!(history_len(&t), before, "no history entry");
        assert_eq!(run(&mut t, &["-G"]), "catting");
        assert!(commands_of(&mut t, "-B").is_empty(), "another verb runs nothing");
    }
}
