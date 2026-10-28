//! Turns translations into text, a port of the rest of Plover's `plover/formatting.py`.
//!
//! Each translation becomes a list of [`Action`]s (see [`super::action`]). The formatter keeps
//! the actions of what was typed, so that undoing a translation, or a later one rewriting an
//! earlier word, becomes the smallest change: so many backspaces, then some text.

use super::action::{
    apply_case, apply_mode, has_word_boundary, is_digits, last_text, last_word, rightmost_word,
    Action, Case, SPACE,
};
use super::layout::{render_raw_stroke, render_stroke};
use super::meta::meta_to_action;
use super::orthography::Orthography;
use super::translate::Translation;
use regex::Regex;
use std::sync::OnceLock;

/// One thing the formatter asks of the keyboard, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Delete this many characters before the cursor
    Backspaces(usize),
    /// Type this text
    Text(String),
    /// Press a key combination, in Plover's key combination language (`Control_L(a)`)
    Combo(String),
    /// Run a command of the steno engine (`toggle`), as written after `{plover:`
    Command(String),
}

/// Space placement, and how a session starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Put the space after each word instead of before it
    pub spaces_after: bool,
    /// The first word attaches to what is already there, instead of starting with a space
    pub start_attached: bool,
    /// The first word is capitalized
    pub start_capitalized: bool,
    /// What goes between words
    pub space_char: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            spaces_after: false,
            start_attached: false,
            start_capitalized: false,
            space_char: SPACE.to_string(),
        }
    }
}

fn atom_regex() -> &'static Regex {
    static ATOM: OnceLock<Regex> = OnceLock::new();
    // One or more of anything but an unescaped brace, or `{...}` with no unescaped brace inside
    ATOM.get_or_init(|| {
        Regex::new(r"(?:\\\{|\\\}|[^{}])+|\{(?:\\\{|\\\}|[^{}])*\}").expect("atom pattern")
    })
}

/// What the actions being made can see of what came before.
pub struct Context<'a> {
    previous: &'a [Translation],
    /// The last action, with a look-ahead resolved to the alternative in force
    pub last_action: Action,
    /// The actions made so far for the translations being formatted
    pub translated: Vec<Action>,
    pub orthography: &'a Orthography,
}

impl<'a> Context<'a> {
    #[must_use]
    pub const fn new(
        previous: &'a [Translation],
        last_action: Action,
        orthography: &'a Orthography,
    ) -> Self {
        Self {
            previous,
            last_action,
            translated: Vec::new(),
            orthography,
        }
    }

    /// A new action that only copies the global state.
    #[must_use]
    pub fn new_action(&self) -> Action {
        self.last_action.new_state()
    }

    /// A new action that clones the state of the last one.
    #[must_use]
    pub fn copy_last_action(&self) -> Action {
        self.last_action.copy_state()
    }

    /// Record an action as made.
    pub fn translated(&mut self, action: Action) {
        self.last_action = action.effective().clone();
        self.translated.push(action);
    }

    /// Past actions, last first: the ones made now, then those of earlier translations.
    fn last_actions(&self) -> Vec<&Action> {
        self.translated
            .iter()
            .rev()
            .chain(
                self.previous
                    .iter()
                    .rev()
                    .flat_map(|t| t.formatting.iter().rev()),
            )
            .collect()
    }

    /// The last `size` characters typed.
    #[must_use]
    pub fn last_text(&self, size: usize) -> String {
        last_text(&self.last_actions(), size)
    }

    /// The last word typed, with whitespace after it.
    #[must_use]
    pub fn last_word(&self) -> Option<String> {
        last_word(&self.last_actions())
    }

    /// The last `count` words typed, in order.
    #[must_use]
    pub fn last_words(&self, count: usize) -> Vec<String> {
        super::action::last_words(&self.last_actions(), count)
    }
}

fn unescape_atom(atom: &str) -> String {
    atom.replace("\\{", "{").replace("\\}", "}")
}

fn get_meta(atom: &str) -> Option<&str> {
    atom.strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
}

/// The actions for a translation: a word, or `{...}` operators, or a mix.
#[must_use]
pub fn translation_to_actions(translation: &str, ctx: &mut Context) -> Vec<Action> {
    // A translation of nothing but digits is glued to the digits next to it
    let atoms: Vec<String> = if is_digits(translation) {
        vec![format!("{{&{translation}}}")]
    } else {
        atom_regex()
            .find_iter(translation)
            .map(|m| m.as_str().trim_matches(' ').to_string())
            .filter(|atom| !atom.is_empty())
            .collect()
    };
    let mut actions = Vec::new();
    for atom in atoms {
        let action = atom_to_action(&atom, ctx);
        actions.push(action.clone());
        ctx.translated(action);
    }
    if actions.is_empty() {
        let action = ctx.copy_last_action();
        actions.push(action.clone());
        ctx.translated(action);
    }
    actions
}

/// The actions for a stroke that has no translation. `rtfcre` is the stroke as Plover writes it
/// and `shown` how it is typed. A stroke of only digits is a number, glued to its neighbours.
#[must_use]
pub fn raw_to_actions(rtfcre: &str, shown: &str, ctx: &mut Context) -> Vec<Action> {
    let no_dash = rtfcre.replacen('-', "", 1);
    if is_digits(&no_dash) {
        return translation_to_actions(&no_dash, ctx);
    }
    let last = &ctx.last_action;
    let action = Action {
        text: Some(shown.to_string()),
        word: Some(shown.to_string()),
        case: last.case,
        prev_attach: last.next_attach,
        space_char: last.space_char.clone(),
        trailing_space: last.space_char.clone(),
        ..Action::default()
    };
    ctx.translated(action.clone());
    vec![action]
}

fn atom_to_action(atom: &str, ctx: &Context) -> Action {
    let mut action = get_meta(atom).map_or_else(
        || {
            let mut action = ctx.new_action();
            action.text = Some(unescape_atom(atom));
            action
        },
        |meta| meta_to_action(&unescape_atom(meta), ctx),
    );
    finalize_action(&mut action, ctx);
    action
}

/// Settle an action's text: its word, the case, the mode and the space it leaves.
fn finalize_action(action: &mut Action, ctx: &Context) {
    if let Some(look) = action.look_ahead.as_mut() {
        finalize_action(&mut look.action1, ctx);
        finalize_action(&mut look.action2, ctx);
        return;
    }
    let Some(text) = action.text.clone() else {
        return;
    };
    if action.word.is_none() {
        let last_word = if action.glue && ctx.last_action.glue {
            ctx.last_action.word.clone()
        } else {
            None
        };
        action.word = Some(rightmost_word(&format!(
            "{}{text}",
            last_word.unwrap_or_default()
        )));
    }
    let mut case = ctx.last_action.next_case;
    if case.is_none() && action.prev_attach && ctx.last_action.upper_carry {
        case = Some(Case::UpperFirstWord);
    }
    let text = apply_case(&text, case);
    if case == Some(Case::UpperFirstWord) {
        action.upper_carry = !has_word_boundary(&text);
    }
    action.text = Some(apply_mode(
        &text,
        action.case,
        &action.space_char,
        action.prev_attach,
        &ctx.last_action,
    ));
    action.trailing_space = if action.next_attach {
        String::new()
    } else {
        action.space_char.clone()
    };
}

// ---------------------------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------------------------

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn without_last_chars(s: &str, n: usize) -> String {
    first_chars(s, char_len(s).saturating_sub(n))
}

/// Rebuilds the text a series of actions leaves, as the part that was deleted from what was
/// already there plus the part added after it.
struct TextFormatter {
    spaces_after: bool,
    /// Text that was there and is replaced
    replaced_text: String,
    /// Text that is added
    appended_text: String,
    trailing_space: String,
}

impl TextFormatter {
    const fn new(spaces_after: bool) -> Self {
        Self {
            spaces_after,
            replaced_text: String::new(),
            appended_text: String::new(),
            trailing_space: String::new(),
        }
    }

    fn render_action(&mut self, action: &Action) {
        if self.spaces_after && !self.trailing_space.is_empty() {
            self.appended_text =
                without_last_chars(&self.appended_text, char_len(&self.trailing_space));
        }
        if !action.prev_replace.is_empty() {
            let mut replaced = char_len(&action.prev_replace);
            let appended = char_len(&self.appended_text);
            if replaced > appended {
                replaced -= appended;
                if replaced > char_len(&self.replaced_text) {
                    self.replaced_text = first_chars(&action.prev_replace, replaced);
                } else {
                    self.replaced_text = without_last_chars(&self.replaced_text, replaced);
                    self.replaced_text
                        .push_str(&first_chars(&action.prev_replace, replaced));
                }
                self.appended_text.clear();
            } else {
                self.appended_text = without_last_chars(&self.appended_text, replaced);
            }
        }
        if !action.prev_attach {
            self.appended_text.push_str(&action.space_char);
        }
        self.appended_text
            .push_str(action.text.as_deref().unwrap_or_default());
        if self.spaces_after && !action.next_attach {
            self.appended_text.push_str(&action.space_char);
            self.trailing_space = action.space_char.clone();
        } else {
            self.trailing_space.clear();
        }
    }

    /// Start rendering after `last_action`: with spaces after, the space it left is still there.
    fn begin(&mut self, last_action: Option<&Action>) {
        if self.spaces_after {
            if let Some(last) = last_action {
                self.trailing_space = last.effective().trailing_space.clone();
                self.appended_text = last.effective().trailing_space.clone();
            }
        }
    }

    fn reset(&mut self, trailing_space: &str) {
        self.replaced_text.clear();
        self.appended_text = trailing_space.to_string();
    }
}

/// Works out the smallest edit between the old output and the new.
struct OutputHelper {
    before: TextFormatter,
    after: TextFormatter,
    ops: Vec<Op>,
}

impl OutputHelper {
    const fn new(before_spaces_after: bool, after_spaces_after: bool) -> Self {
        Self {
            before: TextFormatter::new(before_spaces_after),
            after: TextFormatter::new(after_spaces_after),
            ops: Vec::new(),
        }
    }

    /// Emit what turns the old text into the new, then start both from what is there now.
    fn flush(&mut self) {
        let before_replaced = &self.before.replaced_text;
        let after_replaced = &self.after.replaced_text;
        let replaced_text = if char_len(before_replaced) > char_len(after_replaced) {
            before_replaced.clone()
        } else {
            after_replaced.clone()
        };
        let keep = |own: &str| {
            first_chars(
                &replaced_text,
                char_len(&replaced_text).saturating_sub(char_len(own)),
            )
        };
        let before = format!("{}{}", keep(before_replaced), self.before.appended_text);
        let after = format!("{}{}", keep(after_replaced), self.after.appended_text);
        let common = before
            .chars()
            .zip(after.chars())
            .take_while(|(b, a)| b == a)
            .count();
        let erased = char_len(&before) - common;
        if erased > 0 {
            self.ops.push(Op::Backspaces(erased));
        }
        let appended: String = after.chars().skip(common).collect();
        if !appended.is_empty() {
            self.ops.push(Op::Text(appended));
        }
        let trailing = self.after.trailing_space.clone();
        self.before.reset(&trailing);
        self.after.reset(&trailing);
    }

    /// The edit from the output of `undo` to the output of `new`, both after `last_action`. The
    /// text so far is sent before each command or key combination, so those happen in order.
    fn render(mut self, last_action: Option<&Action>, undo: &[Action], new: &[Action]) -> Vec<Op> {
        // The undone actions count for their text only
        self.before.begin(last_action);
        for action in undo {
            let this = action.effective();
            if this.text.is_some() {
                self.before.render_action(this);
            }
        }
        self.after.begin(last_action);
        for action in new {
            let this = action.effective();
            if this.text.is_some() {
                self.after.render_action(this);
                continue;
            }
            self.flush();
            // An empty combination or command is none, as in Plover
            if let Some(combo) = this.combo.as_ref().filter(|c| !c.is_empty()) {
                self.ops.push(Op::Combo(combo.clone()));
            } else if let Some(command) = this.command.as_ref().filter(|c| !c.is_empty()) {
                self.ops.push(Op::Command(command.clone()));
            }
        }
        self.flush();
        self.ops
    }
}

/// Formats translations, and remembers how to place spaces.
#[derive(Debug, Clone, Default)]
pub struct Formatter {
    pub options: Options,
    last_output_spaces_after: bool,
}

impl Formatter {
    #[must_use]
    pub const fn new(options: Options) -> Self {
        let last_output_spaces_after = options.spaces_after;
        Self {
            options,
            last_output_spaces_after,
        }
    }

    /// The last action of `previous`, or what a session starts with.
    pub fn last_action(&self, previous: &[Translation]) -> Action {
        if let Some(last) = previous.last().and_then(|t| t.formatting.last()) {
            return last.effective().clone();
        }
        let next_attach = self.options.start_attached || self.options.spaces_after;
        Action {
            next_attach,
            next_case: self.options.start_capitalized.then_some(Case::CapFirstWord),
            space_char: self.options.space_char.clone(),
            // Plover works the word's end out from whether it attaches to the next
            word_is_finished: !next_attach,
            ..Action::default()
        }
    }

    /// Format `do_` after `prev`, undoing `undo`. Fills in each translation's `formatting`, and
    /// returns what to do with the keyboard to get from the old output to the new.
    pub fn format(
        &mut self,
        undo: &[Translation],
        do_: &mut [&mut Translation],
        prev: &mut [Translation],
        orthography: &Orthography,
    ) -> Vec<Op> {
        let mut new: Vec<Action> = if do_.is_empty() {
            Vec::new()
        } else {
            let last_action = self.last_action(prev);
            let mut ctx = Context::new(prev, last_action, orthography);
            for translation in do_.iter_mut() {
                if !translation.origin.is_stroke() {
                    // Text typed or erased by hand is not translated: it is what it says
                    let action =
                        super::journal::recorded_action(&translation.origin, &ctx.last_action);
                    ctx.translated(action.clone());
                    translation.formatting = vec![action];
                    continue;
                }
                translation.formatting = match translation.english.as_deref() {
                    Some(english) if !english.is_empty() => {
                        translation_to_actions(english, &mut ctx)
                    }
                    _ => {
                        let stroke = translation.strokes.first().copied().unwrap_or(0);
                        raw_to_actions(&render_stroke(stroke), &render_raw_stroke(stroke), &mut ctx)
                    }
                };
            }
            ctx.translated
        };
        let mut old: Vec<Action> = undo
            .iter()
            .flat_map(|t| t.formatting.iter().cloned())
            .collect();

        // Take into account earlier look-ahead actions: the text now known may change them
        if !prev.is_empty() {
            let mut text = new
                .iter()
                .find_map(|a| a.text().filter(|t| !t.is_empty()))
                .unwrap_or_default()
                .to_string();
            let mut tail: Vec<Action> = Vec::new();
            'translations: for translation in prev.iter_mut().rev() {
                for action in translation.formatting.iter_mut().rev() {
                    if let Some(look) = action.look_ahead.as_mut() {
                        let old_action = look.current().clone();
                        let new_action = look.update(&text);
                        // Does the look-ahead action need updating?
                        if new_action == old_action {
                            break 'translations;
                        }
                        let mut front_old = vec![old_action];
                        front_old.extend(tail.iter().cloned());
                        old.splice(0..0, front_old);
                        let mut front_new = vec![new_action];
                        front_new.extend(tail.iter().cloned());
                        new.splice(0..0, front_new);
                        text = look.current().text.clone().unwrap_or_default();
                        tail.clear();
                        // Updating this action can change another look-ahead before it
                    } else if let Some(action_text) = &action.text {
                        if !action_text.is_empty() {
                            // Stop at the first action that has text
                            break 'translations;
                        }
                        tail.insert(0, action.clone());
                    }
                }
            }
        }

        // Figure out what really changed
        let same = old
            .iter()
            .zip(new.iter())
            .take_while(|(o, n)| o == n)
            .count();
        let optimized_away: Vec<Action> = old.drain(..same).collect();
        new.drain(..same);

        let last_action = optimized_away
            .last()
            .or_else(|| prev.last().and_then(|t| t.formatting.last()));
        let helper = OutputHelper::new(self.last_output_spaces_after, self.options.spaces_after);
        let ops = helper.render(last_action, &old, &new);
        self.last_output_spaces_after = self.options.spaces_after;
        ops
    }
}

/// What a translation types on its own, with no space before it: `cat`, `ing` for `{^ing}`. `None`
/// if it presses keys or sends a command.
#[must_use]
pub fn plain_text(translation: &str) -> Option<String> {
    if translation.is_empty() {
        return Some(String::new());
    }
    let orthography = Orthography::default();
    let mut formatter = Formatter::new(Options {
        start_attached: true,
        ..Options::default()
    });
    let mut item = Translation::new(vec![0], Some(translation.to_string()));
    let mut text = String::new();
    for op in formatter.format(&[], &mut [&mut item], &mut [], &orthography) {
        match op {
            Op::Text(typed) => text.push_str(&typed),
            Op::Backspaces(_) => {}
            Op::Combo(_) | Op::Command(_) => return None,
        }
    }
    Some(text)
}

/// Whether a translation leaves the next word attached to it, as `{be^}` does: it types `be`,
/// but the next word would join it instead of standing as a word of its own.
#[must_use]
pub fn leaves_attached(translation: &str) -> bool {
    let orthography = Orthography::default();
    let mut ctx = Context::new(&[], Action::default(), &orthography);
    let _ = translation_to_actions(translation, &mut ctx);
    ctx.last_action.next_attach
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(translation: &str) -> Vec<Action> {
        let orthography = Orthography::default();
        let mut ctx = Context::new(&[], Action::default(), &orthography);
        translation_to_actions(translation, &mut ctx)
    }

    #[test]
    fn a_translation_is_cut_into_text_and_metas() {
        let list = actions("{-|}hello{^}world{.}");
        let texts: Vec<_> = list.iter().map(|a| a.text.clone()).collect();
        assert_eq!(
            texts,
            [
                None,
                Some("Hello".into()),
                Some(String::new()),
                Some("world".into()),
                Some(".".into())
            ]
        );
    }

    #[test]
    fn digits_alone_are_glued() {
        assert!(actions("123")[0].glue);
        assert!(!actions("12a")[0].glue);
    }

    #[test]
    fn an_escaped_brace_is_text() {
        let list = actions("\\{x\\}");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].text.as_deref(), Some("{x}"));
    }
}
