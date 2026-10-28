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
use super::keycombo::key_events;
use super::layout::render_stroke;
use crate::keycode::KeyCode;
use std::sync::Arc;
use tracing::{info, warn};

/// The stroke that undoes the last translation: the star on its own.
pub const UNDO_STROKE: u32 = 1 << 9;

/// The star key of a stroke.
const STAR: u32 = 1 << 9;

/// Keys that can be folded into a stroke as a suffix when the stroke has no entry of its own:
/// `-Z`, `-D`, `-S`, `-G`, in the order Plover tries them.
pub const SUFFIX_KEYS: [u32; 4] = [1 << 21, 1 << 20, 1 << 19, 1 << 17];

/// How many translations stay undoable at least. Plover's default.
const UNDO_LENGTH: usize = 100;

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
        }
    }

    /// The strokes as Plover writes them.
    #[must_use]
    pub fn rtfcre(&self) -> Vec<String> {
        self.strokes.iter().map(|&stroke| render_stroke(stroke)).collect()
    }

    fn same(&self, other: &Self) -> bool {
        self.strokes == other.strokes && self.english == other.english
    }

    /// Whether undoing it takes anything back: it typed something, or deleted something, or
    /// replaced other translations.
    #[must_use]
    pub fn has_undo(&self) -> bool {
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
    /// A command for the steno engine, as written after `{plover:`
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
                (Some(OutputOp::Backspaces(before)), OutputOp::Backspaces(after)) => *before += after,
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

/// Plover's `Macro`: a translation that is an action on the translator, not text.
struct Macro {
    name: String,
    stroke: u32,
    cmdline: String,
}

/// The macro a mapping stands for. Nothing mapped is the undo stroke's macro when the stroke is
/// the undo stroke.
fn mapping_to_macro(mapping: Option<&str>, stroke: u32) -> Option<Macro> {
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
        cmdline: rest[name_len..].strip_prefix(':').unwrap_or_default().to_string(),
    })
}

/// The translations still undoable, and the one before them for context.
#[derive(Debug, Clone, Default)]
struct State {
    translations: Vec<Translation>,
    /// The oldest translation still kept, no longer undoable
    tail: Option<Translation>,
}

impl State {
    /// Reduce the history to about `n` strokes.
    fn restrict_size(&mut self, n: usize) {
        let mut strokes = 0;
        let mut count = 0;
        for translation in self.translations.iter().rev() {
            strokes += translation.strokes.len();
            count += 1;
            if strokes >= n {
                break;
            }
        }
        let index = self.translations.len() - count;
        if index > 0 {
            self.tail = Some(self.translations[index - 1].clone());
        }
        self.translations.drain(..index);
    }
}

/// Converts a stream of strokes into typing.
#[derive(Debug, Clone)]
pub struct Translator {
    dict: Arc<Dictionary>,
    state: State,
    to_undo: Vec<Translation>,
    to_do: usize,
    formatter: Formatter,
    unmatched: bool,
    /// What is on screen, as far as this translator has typed it
    screen: String,
}

impl Translator {
    /// A translator with keymux's defaults: the first word attaches to what is there.
    #[must_use]
    pub fn new(dict: Dictionary) -> Self {
        Self::with_shared(
            Arc::new(dict),
            Options {
                start_attached: true,
                ..Options::default()
            },
        )
    }

    /// A translator over a dictionary others share, with these options.
    #[must_use]
    pub fn with_shared(dict: Arc<Dictionary>, options: Options) -> Self {
        Self {
            dict,
            state: State::default(),
            to_undo: Vec::new(),
            to_do: 0,
            formatter: Formatter::new(options),
            unmatched: false,
            screen: String::new(),
        }
    }

    /// A translator over the same dictionary with nothing typed. Cheap: the dictionary is shared.
    #[must_use]
    pub fn fresh(&self) -> Self {
        Self::with_shared(Arc::clone(&self.dict), self.formatter.options.clone())
    }

    /// Forget what was typed, as when the layer is entered: the next word starts a sentence.
    pub fn start_fresh(&mut self) {
        *self = self.fresh();
    }

    /// Swap in the dictionary. What was typed stays undoable.
    pub fn set_dictionary(&mut self, dict: Dictionary) {
        self.set_shared(Arc::new(dict));
    }

    /// Swap in a dictionary that other translators share. What is already typed is untouched,
    /// so `*` still takes it back; only the strokes from now on use the new one.
    pub fn set_shared(&mut self, dict: Arc<Dictionary>) {
        self.dict = dict;
    }

    /// Change how spaces are placed and how a session starts.
    pub fn set_options(&mut self, options: Options) {
        self.formatter.options = options;
    }

    /// How spaces are placed and how a session starts.
    #[must_use]
    pub const fn options(&self) -> &Options {
        &self.formatter.options
    }

    /// The dictionary strokes are translated with.
    #[must_use]
    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    /// Whether any dictionary entries loaded.
    #[must_use]
    pub fn has_dictionary(&self) -> bool {
        !self.dict.is_empty()
    }

    /// How many entries the dictionary has.
    #[must_use]
    pub fn entry_count(&self) -> usize {
        self.dict.len()
    }

    /// Whether the last stroke matched nothing: it was typed as its keys.
    #[must_use]
    pub const fn was_unmatched(&self) -> bool {
        self.unmatched
    }

    /// The text on screen, as far as this translator knows it.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.screen
    }

    /// Translate a finished stroke and say what to do with the keyboard.
    pub fn stroke(&mut self, stroke: u32) -> Option<StenoOutput> {
        self.unmatched = false;
        let mut ops = Vec::new();
        self.translate_stroke(stroke, &mut ops);
        ops.extend(self.flush(None));
        self.finish(ops)
    }

    /// The operations a stroke makes, before key combinations become key events. Tests use this
    /// to compare with Plover, which reports the combination as written.
    #[cfg(test)]
    pub(crate) fn stroke_ops(&mut self, stroke: u32) -> Vec<Op> {
        self.unmatched = false;
        let mut ops = Vec::new();
        self.translate_stroke(stroke, &mut ops);
        ops.extend(self.flush(None));
        for op in &ops {
            match op {
                Op::Backspaces(count) => {
                    let keep = self.screen.chars().count().saturating_sub(*count);
                    self.screen = self.screen.chars().take(keep).collect();
                }
                Op::Text(text) => self.screen.push_str(text),
                Op::Combo(_) | Op::Command(_) => {}
            }
        }
        ops
    }

    /// Settle the output: keep the screen in step and turn the key combinations into key events.
    fn finish(&mut self, ops: Vec<Op>) -> Option<StenoOutput> {
        let mut out = Vec::new();
        for op in ops {
            match op {
                Op::Backspaces(count) => {
                    let keep = self.screen.chars().count().saturating_sub(count);
                    self.screen = self.screen.chars().take(keep).collect();
                    out.push(OutputOp::Backspaces(count));
                }
                Op::Text(text) => {
                    self.screen.push_str(&text);
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
        let mapping = self.lookup_with_prefix(max_len, &self.state.translations, &[stroke], &[]);
        if let Some(found) = mapping_to_macro(mapping.as_deref(), stroke) {
            self.translate_macro(&found, ops);
            return;
        }
        let translation = self
            // No prefix lookups (the stroke alone is already looked up)
            .find_longest_match(2, max_len, stroke, &[])
            // The stroke alone, if it is mapped
            .or_else(|| mapping.map(|m| Translation::new(vec![stroke], Some(m))))
            // No direct match: try with suffixes
            .or_else(|| self.find_longest_match(1, max_len, stroke, &SUFFIX_KEYS))
            // Nothing: the stroke as its keys
            .unwrap_or_else(|| Translation::new(vec![stroke], None));
        if translation.english.is_none() {
            self.unmatched = true;
            info!("steno stroke {stroke:#x} not in dictionary");
        }
        self.translate_translation(translation);
    }

    fn translate_macro(&mut self, found: &Macro, ops: &mut Vec<Op>) {
        match found.name.as_str() {
            "undo" => self.macro_undo(found.stroke, ops),
            "repeat_last_stroke" => {
                let Some(last) = self.state.translations.last().and_then(|t| t.strokes.last()) else {
                    return;
                };
                let stroke = *last;
                self.translate_stroke(stroke, ops);
            }
            "retro_toggle_asterisk" => {
                let Some(last) = self.state.translations.last().cloned() else {
                    return;
                };
                self.untranslate_translation(&last);
                let Some(&stroke) = last.strokes.last() else {
                    return;
                };
                self.translate_stroke(stroke ^ STAR, ops);
            }
            // Lapwing's repeat macros, from its last-translation plugin
            "repeat_last_translation" => self.macro_repeat_translations(&found.cmdline),
            "repeat_last_word" | "repeat_last_fragment" | "repeat_last_character" => {
                self.macro_repeat_text(&found.name, &found.cmdline, found.stroke);
            }
            "retro_delete_space" | "retrospective_delete_space" => self.macro_delete_space(found.stroke),
            "retro_insert_space" | "retrospective_insert_space" => self.macro_insert_space(found.stroke),
            "retrospective_toggle_asterisk" => {
                let Some(last) = self.state.translations.last().cloned() else {
                    return;
                };
                self.untranslate_translation(&last);
                let Some(&stroke) = last.strokes.last() else {
                    return;
                };
                self.translate_stroke(stroke ^ STAR, ops);
            }
            other => warn!("steno macro `{other}` is not known, so it does nothing"),
        }
    }

    /// Undo: take back translations until one that had an effect, or if there are none left,
    /// delete a word with the keys that do it.
    fn macro_undo(&mut self, stroke: u32, ops: &mut Vec<Op>) {
        let all: Vec<Translation> = self.state.translations.clone();
        for translation in all.iter().rev() {
            self.untranslate_translation(translation);
            if translation.has_undo() {
                return;
            }
        }
        // Nothing left of the output to take back. Delete a word, without keeping it in the history.
        let extra = Translation::new(vec![stroke], Some(BACK_STRING.to_string()));
        ops.extend(self.flush(Some(extra)));
    }

    /// How many to repeat: the first of the comma-separated arguments, or one.
    fn repeat_count(cmdline: &str) -> usize {
        cmdline.split(',').next().and_then(|n| n.trim().parse().ok()).unwrap_or(1)
    }

    /// `=repeat_last_translation:n`: the last `n` translations again.
    fn macro_repeat_translations(&mut self, cmdline: &str) {
        let translations = &self.state.translations;
        if translations.is_empty() {
            return;
        }
        let count = Self::repeat_count(cmdline);
        // `translations[-0:]` is the whole list in Python
        let from = if count == 0 { 0 } else { translations.len().saturating_sub(count) };
        let again: Vec<Translation> = translations[from..]
            .iter()
            .map(|t| Translation::new(t.strokes.clone(), t.english.clone()))
            .collect();
        for translation in again {
            self.translate_translation(translation);
        }
    }

    /// `=repeat_last_word:n`, `=repeat_last_fragment:n`, `=repeat_last_character:n`: the text
    /// again, typed as a translation of this stroke.
    fn macro_repeat_text(&mut self, name: &str, cmdline: &str, stroke: u32) {
        if self.state.translations.is_empty() {
            return;
        }
        let count = Self::repeat_count(cmdline);
        let actions: Vec<&Action> = self
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
        let translations = &self.state.translations;
        if translations.len() < 2 {
            return;
        }
        let replaced: Vec<Translation> = translations[translations.len() - 2..].to_vec();
        if replaced[1].is_retrospective_command {
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
        let Some(last) = self.state.translations.last().cloned() else {
            return;
        };
        if last.is_retrospective_command {
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
            english.push(
                self.lookup(&[lookup_stroke], &SUFFIX_KEYS)
                    .unwrap_or_else(|| render_stroke(lookup_stroke)),
            );
            let mut t = Translation::new(vec![stroke], Some(english.join(" ")));
            t.replaced = vec![last];
            t.is_retrospective_command = true;
            self.translate_translation(t);
        }
    }

    fn translate_translation(&mut self, translation: Translation) {
        let replaced = translation.replaced.clone();
        self.undo_all(&replaced);
        self.do_all(vec![translation]);
    }

    fn untranslate_translation(&mut self, translation: &Translation) {
        self.undo_all(std::slice::from_ref(translation));
        self.do_all(translation.replaced.clone());
    }

    /// Take the translations off the end of the history, last first.
    fn undo_all(&mut self, translations: &[Translation]) {
        for expected in translations.iter().rev() {
            let Some(popped) = self.state.translations.pop() else {
                return;
            };
            debug_assert!(popped.same(expected));
            if self.to_do > 0 {
                self.to_do -= 1;
            } else {
                self.to_undo.insert(0, popped);
            }
        }
    }

    fn do_all(&mut self, translations: Vec<Translation>) {
        self.to_do += translations.len();
        self.state.translations.extend(translations);
    }

    /// Find the translation with the longest outline that ends in `stroke`: the strokes of the
    /// last translations, as many as fit, plus this one. The new translation replaces the ones it
    /// covers. With `suffix_keys`, the stroke is looked up without one of them folded in.
    fn find_longest_match(
        &self,
        min_len: usize,
        max_len: usize,
        stroke: u32,
        suffix_keys: &[u32],
    ) -> Option<Translation> {
        let mut suffixes = Vec::new();
        if !suffix_keys.is_empty() {
            suffixes = self.involved_suffixes(stroke, suffix_keys);
            if suffixes.is_empty() {
                return None;
            }
        }
        // How much of the history can be involved
        let mut num_strokes = 1;
        let mut count = 0;
        for t in self.state.translations.iter().rev() {
            num_strokes += t.strokes.len();
            if num_strokes > max_len {
                break;
            }
            count += 1;
        }
        let translations = &self.state.translations[self.state.translations.len() - count..];
        // The new stroke makes a new translation, or replaces earlier ones by matching a longer one
        for i in 0..=translations.len() {
            let replaced = &translations[i..];
            let mut strokes: Vec<u32> = replaced.iter().flat_map(|t| t.strokes.iter().copied()).collect();
            strokes.push(stroke);
            if strokes.len() < min_len {
                continue;
            }
            if let Some(mapping) = self.lookup_with_prefix(max_len, &translations[..i], &strokes, &suffixes) {
                let mut translation = Translation::new(strokes, Some(mapping));
                translation.replaced = replaced.to_vec();
                return Some(translation);
            }
        }
        None
    }

    fn lookup_strokes(&self, strokes: &[u32]) -> Option<String> {
        self.dict.lookup(strokes).map(str::to_string)
    }

    /// Look the strokes up, with the last one's suffix key folded in if there are suffixes: the
    /// stroke without the key, then a space and the suffix's own translation.
    fn lookup_with_suffix(&self, strokes: &[u32], suffixes: &[(u32, String)]) -> Option<String> {
        if suffixes.is_empty() {
            return self.lookup_strokes(strokes);
        }
        let (last, rest) = strokes.split_last()?;
        for (suffix_stroke, suffix_mapping) in suffixes {
            let mut without = rest.to_vec();
            without.push(last & !suffix_stroke);
            if let Some(main) = self.lookup_strokes(&without) {
                return Some(format!("{main} {suffix_mapping}"));
            }
        }
        None
    }

    /// The suffix keys `stroke` has that have a translation of their own.
    fn involved_suffixes(&self, stroke: u32, suffix_keys: &[u32]) -> Vec<(u32, String)> {
        suffix_keys
            .iter()
            .filter(|&&key| stroke & key == key)
            .filter_map(|&key| Some((key, self.lookup_strokes(&[key])?)))
            .collect()
    }

    /// The translation of an outline, looking for a suffix key folded in if there is no entry.
    fn lookup(&self, strokes: &[u32], suffix_keys: &[u32]) -> Option<String> {
        if let Some(found) = self.lookup_strokes(strokes) {
            return Some(found);
        }
        let suffixes = self.involved_suffixes(*strokes.last()?, suffix_keys);
        if suffixes.is_empty() {
            return None;
        }
        self.lookup_with_suffix(strokes, &suffixes)
    }

    /// Whether the word before is complete, so a stroke may start a new one.
    fn previous_word_is_finished(last: &[Translation]) -> bool {
        last.last()
            .and_then(|t| t.formatting.last())
            .is_none_or(|action| action.effective().word_is_finished)
    }

    /// Look up `strokes`, first as the start of a word (with the empty stroke in front) if the
    /// word before is finished, then as they are.
    fn lookup_with_prefix(
        &self,
        max_len: usize,
        last_translations: &[Translation],
        strokes: &[u32],
        suffixes: &[(u32, String)],
    ) -> Option<String> {
        if strokes.len() < max_len && Self::previous_word_is_finished(last_translations) {
            let mut prefixed = vec![0];
            prefixed.extend_from_slice(strokes);
            if let Some(found) = self.lookup_with_suffix(&prefixed, suffixes) {
                return Some(found);
            }
        }
        if strokes.len() <= max_len {
            return self.lookup_with_suffix(strokes, suffixes);
        }
        None
    }

    /// Format what was done and undone since the last flush, and return what to type to match.
    /// `extra` is typed too but not kept in the history.
    fn flush(&mut self, extra: Option<Translation>) -> Vec<Op> {
        let to_do = self.to_do;
        let undo = std::mem::take(&mut self.to_undo);
        self.to_do = 0;
        let mut extra = extra;
        let ops = if !undo.is_empty() || to_do > 0 || extra.is_some() {
            let State { translations, tail } = &mut self.state;
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
            self.formatter.format(&undo, &mut doing, prev, self.dict.orthography())
        } else {
            Vec::new()
        };
        let keep = self.dict.longest_key().max(UNDO_LENGTH);
        self.state.restrict_size(keep);
        ops
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::parse_stroke;

    fn stroke(text: &str) -> u32 {
        if text == "*" {
            return UNDO_STROKE;
        }
        parse_stroke(text).unwrap_or_else(|| panic!("{text} is not a stroke"))
    }

    fn translator(json: &str) -> Translator {
        let mut dict = Dictionary::default();
        dict.merge_json(json).unwrap();
        Translator::new(dict)
    }

    /// The text after typing each stroke in order.
    fn type_strokes(json: &str, strokes: &[&str]) -> String {
        let mut t = translator(json);
        for s in strokes {
            t.stroke(stroke(s));
        }
        t.text().to_string()
    }

    #[test]
    fn a_stroke_types_its_translation() {
        assert_eq!(type_strokes(r#"{"KAT": "cat"}"#, &["KAT"]), "cat");
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
}
