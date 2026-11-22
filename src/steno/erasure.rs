//! Which characters of each tape line are still on screen.
//!
//! A tape line says what a stroke did to the text: `" cat"` typed four characters, `-1 "ies"`
//! took one back and typed three. A later line can take back characters an earlier one typed: an
//! undo (`-4 ""`), a stroke whose reading corrects the one before it, a plugin re-reading a run,
//! or a late reading replacing its stroke. [`Screen`] replays the lines in order and remembers,
//! for every character a line typed, whether it is still there, so the overlay and
//! `keymux steno tape` can draw what was taken back grey and only what is on screen in full
//! colour. The `-N` count itself is drawn grey too.
//!
//! This only reads the tape's own descriptions ([`super::tape::describe`]): it knows nothing of
//! any theory, only that a backspace takes back the newest character still on screen.
//!
//! A key press (`KC_ENT`, an arrow) or text typed by hand moves the cursor or changes the text in
//! ways the tape cannot follow, so it *seals* everything before it: a later backspace is never
//! put down to a character from before the seal.

use std::collections::VecDeque;

/// One thing a tape description did, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// `-N`: took back the last N characters on screen
    Erase(usize),
    /// A quoted text: each character it typed, as the tape writes it (`\n` for a newline)
    Text(Vec<String>),
    /// An engine command (`{:lookup}`): changes no text
    Command(String),
    /// A key pressed (`KC_ENT`): seals everything before it
    Keys(String),
}

impl Piece {
    /// How the piece reads on the tape.
    fn shown(&self, quoted: bool) -> String {
        match self {
            Self::Erase(count) => format!("-{count}"),
            Self::Text(chars) if quoted => format!("\"{}\"", chars.concat()),
            Self::Text(chars) => chars.concat(),
            Self::Command(text) | Self::Keys(text) => text.clone(),
        }
    }
}

/// A tape description (as [`super::tape::describe`] writes it) as its pieces.
///
/// What follows them that is not output, a plugin's mark (`plugin:NAME "text"`) or a typed
/// word's suggestion (`could be ...`), is left out; [`split`] gives it back as the tail.
#[must_use]
pub fn pieces(description: &str) -> Vec<Piece> {
    split(description).0
}

/// [`pieces`], and the rest of the description from the first word that is not output on: a
/// plugin's mark or a suggestion, left as written (with the spaces before it).
#[must_use]
pub fn split(description: &str) -> (Vec<Piece>, &str) {
    let mut pieces = Vec::new();
    let mut rest = description;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        if trimmed.is_empty() {
            return (pieces, "");
        }
        let Some((piece, after)) = next_piece(trimmed) else {
            return (pieces, rest);
        };
        pieces.push(piece);
        rest = after;
    }
}

/// The first piece of `text` (which starts with it) and what follows it, or `None` where the
/// output ends and a mark begins.
fn next_piece(text: &str) -> Option<(Piece, &str)> {
    if let Some(quoted) = text.strip_prefix('"') {
        let (chars, after) = unquote(quoted)?;
        return Some((Piece::Text(chars), after));
    }
    if text.starts_with('{') {
        // Up to the matching brace: a command may hold spaces
        let mut depth = 0usize;
        for (i, c) in text.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let (command, after) = text.split_at(i + 1);
                        return Some((Piece::Command(command.to_owned()), after));
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    let end = text.find(' ').unwrap_or(text.len());
    let (word, after) = text.split_at(end);
    if let Some(count) = word.strip_prefix('-') {
        if !count.is_empty() && count.chars().all(|c| c.is_ascii_digit()) {
            return Some((Piece::Erase(count.parse().ok()?), after));
        }
    }
    // A mark or a suggestion, not output
    if word.starts_with("plugin:") || word == "could" {
        return None;
    }
    Some((Piece::Keys(word.to_owned()), after))
}

/// A Rust string literal's body (after the opening quote) as its characters, each as written,
/// and what follows the closing quote. `None` if it never closes.
fn unquote(body: &str) -> Option<(Vec<String>, &str)> {
    let mut chars = Vec::new();
    let mut iter = body.char_indices();
    while let Some((start, c)) = iter.next() {
        match c {
            '"' => return Some((chars, &body[start + 1..])),
            '\\' => {
                let (_, escaped) = iter.next()?;
                let end = if escaped == 'u' {
                    // \u{...}
                    let close = body[start..].find('}')? + start;
                    while iter.next().is_some_and(|(i, _)| i < close) {}
                    close + 1
                } else {
                    start + 1 + escaped.len_utf8()
                };
                chars.push(body[start..end].to_owned());
            }
            other => chars.push(other.to_string()),
        }
    }
    None
}

/// How a part of a tape line is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// On screen, or not text at all (a command, a key)
    Normal,
    /// Taken back since, or a `-N` count
    Gone,
}

/// A tape description as runs of text to draw, each in its ink.
///
/// The `-N` counts are grey, and each character that `alive` says was taken back since.
/// `alive` is one flag per character the
/// description typed, in order (see [`Screen::alive`]); a shorter one leaves the first
/// characters normal. `quoted` keeps the text's quotes, as the tape file has them.
#[must_use]
pub fn runs(pieces: &[Piece], alive: &[bool], quoted: bool) -> Vec<(String, Ink)> {
    let typed: usize = pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(chars) => chars.len(),
            _ => 0,
        })
        .sum();
    // Line the flags up with the end: a repeated line holds the flags of every time it typed
    let skip = typed.saturating_sub(alive.len());
    let flags = &alive[alive.len().saturating_sub(typed)..];
    let mut index = 0usize;
    let mut out: Vec<(String, Ink)> = Vec::new();
    let mut push = |text: &str, ink: Ink| match out.last_mut() {
        Some((last, last_ink)) if *last_ink == ink => last.push_str(text),
        _ => out.push((text.to_owned(), ink)),
    };
    for (i, piece) in pieces.iter().enumerate() {
        if i > 0 {
            push(" ", Ink::Normal);
        }
        match piece {
            Piece::Erase(_) => push(&piece.shown(quoted), Ink::Gone),
            Piece::Text(chars) => {
                if quoted {
                    push("\"", Ink::Normal);
                }
                for shown in chars {
                    let gone = index >= skip && !flags[index - skip];
                    push(shown, if gone { Ink::Gone } else { Ink::Normal });
                    index += 1;
                }
                if quoted {
                    push("\"", Ink::Normal);
                }
            }
            Piece::Command(_) | Piece::Keys(_) => push(&piece.shown(quoted), Ink::Normal),
        }
    }
    out
}

/// One tape line's characters, in the order typed: still on screen or not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    id: u64,
    alive: Vec<bool>,
    /// How many characters one time through its description typed (a repeat types them again)
    once: Option<usize>,
}

/// The tape's lines, replayed: which characters each one typed are still on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Screen {
    lines: VecDeque<Line>,
    /// Nothing before this (a line's id, and how many of its characters) can be taken back
    floor: Option<(u64, usize)>,
}

impl Screen {
    /// Start line `id`, newer than every line so far. What [`Self::apply`] does lands on it.
    pub fn line(&mut self, id: u64) {
        self.lines.push_back(Line {
            id,
            alive: Vec::new(),
            once: None,
        });
    }

    /// What the newest line's description did, once more (a held stroke applies each time it
    /// repeats). Text is added to the newest line; a backspace takes back the newest character
    /// still on screen, which may be one an earlier line typed; a key press seals.
    pub fn apply(&mut self, pieces: &[Piece]) {
        if let Some(line) = self.lines.back_mut() {
            line.once.get_or_insert_with(|| typed_len(pieces));
        }
        for piece in pieces {
            match piece {
                Piece::Erase(count) => self.erase(*count),
                Piece::Text(chars) => {
                    if let Some(line) = self.lines.back_mut() {
                        line.alive.extend(std::iter::repeat_n(true, chars.len()));
                    }
                }
                Piece::Keys(_) => self.seal(),
                Piece::Command(_) => {}
            }
        }
    }

    /// Nothing typed so far can be taken back by a later line: the text was changed in a way
    /// the tape does not show (a key press, typing by hand).
    pub fn seal(&mut self) {
        self.floor = self.end();
    }

    /// Whether nothing typed so far can be taken back: sealed, and nothing typed since.
    #[must_use]
    pub fn sealed(&self) -> bool {
        self.floor == self.end()
    }

    /// Where the text ends: the newest line, and how many characters it has typed.
    fn end(&self) -> Option<(u64, usize)> {
        self.lines.back().map(|line| (line.id, line.alive.len()))
    }

    /// Take back the `count` newest characters still on screen, newest line first, stopping at
    /// the seal.
    fn erase(&mut self, mut count: usize) {
        let floor = self.floor;
        for line in self.lines.iter_mut().rev() {
            if count == 0 {
                return;
            }
            let lowest = match floor {
                Some((id, _)) if line.id < id => return,
                Some((id, chars)) if line.id == id => chars,
                _ => 0,
            };
            let lowest = lowest.min(line.alive.len());
            for alive in line.alive[lowest..].iter_mut().rev() {
                if count == 0 {
                    return;
                }
                if *alive {
                    *alive = false;
                    count -= 1;
                }
            }
            if floor.is_some_and(|(id, _)| line.id == id) {
                return;
            }
        }
    }

    /// Forget the lines older than `id`.
    pub fn forget_before(&mut self, id: u64) {
        while self.lines.front().is_some_and(|line| line.id < id) {
            self.lines.pop_front();
        }
    }

    /// Forget every line.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.floor = None;
    }

    /// For each character line `id` typed, in order, whether it is still on screen.
    #[must_use]
    pub fn alive(&self, id: u64) -> &[bool] {
        self.lines
            .iter()
            .rev()
            .find(|line| line.id == id)
            .map_or(&[], |line| &line.alive)
    }

    /// The characters of line `id`'s last time through its description that are gone, as
    /// `[start, end)` ranges counted in those characters: what the overlay is sent for each line.
    #[must_use]
    pub fn gone(&self, id: u64) -> Vec<[u32; 2]> {
        self.lines
            .iter()
            .rev()
            .find(|line| line.id == id)
            .map_or_else(Vec::new, Line::gone)
    }

    /// [`Self::gone`] for every line, oldest first, with its id.
    pub fn every_gone(&self) -> impl Iterator<Item = (u64, Vec<[u32; 2]>)> + '_ {
        self.lines.iter().map(|line| (line.id, line.gone()))
    }
}

impl Line {
    fn gone(&self) -> Vec<[u32; 2]> {
        let typed = self.once.unwrap_or(0);
        let flags = &self.alive[self.alive.len().saturating_sub(typed)..];
        let mut ranges: Vec<[u32; 2]> = Vec::new();
        for (i, &alive) in flags.iter().enumerate() {
            if alive {
                continue;
            }
            let i = u32::try_from(i).unwrap_or(u32::MAX);
            match ranges.last_mut() {
                Some(last) if last[1] == i => last[1] += 1,
                _ => ranges.push([i, i + 1]),
            }
        }
        ranges
    }
}

/// The flags [`runs`] takes, rebuilt from [`Screen::gone`]'s ranges over `typed` characters.
#[must_use]
pub fn alive_from_gone(gone: &[[u32; 2]], typed: usize) -> Vec<bool> {
    let mut alive = vec![true; typed];
    for &[start, end] in gone {
        for flag in alive.iter_mut().take(end as usize).skip(start as usize) {
            *flag = false;
        }
    }
    alive
}

/// How many characters `pieces` type.
#[must_use]
pub fn typed_len(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(chars) => chars.len(),
            _ => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replay descriptions, one line each, and give each line's characters as text with the
    /// ones taken back in brackets.
    fn replay(lines: &[&str]) -> Vec<String> {
        let mut screen = Screen::default();
        for (id, line) in lines.iter().enumerate() {
            screen.line(id as u64);
            screen.apply(&pieces(line));
        }
        lines
            .iter()
            .enumerate()
            .map(|(id, line)| show(&pieces(line), screen.alive(id as u64)))
            .collect()
    }

    /// A line as `runs` draws it, with what is grey in `[...]`.
    fn show(pieces: &[Piece], alive: &[bool]) -> String {
        runs(pieces, alive, false)
            .into_iter()
            .map(|(text, ink)| match ink {
                Ink::Normal => text,
                Ink::Gone => format!("[{text}]"),
            })
            .collect()
    }

    #[test]
    fn reads_each_kind_of_piece() {
        assert_eq!(
            pieces(r#"-2 "a\nb" {:command:set config} KC_ENT"#),
            [
                Piece::Erase(2),
                Piece::Text(vec!["a".into(), "\\n".into(), "b".into()]),
                Piece::Command("{:command:set config}".into()),
                Piece::Keys("KC_ENT".into()),
            ]
        );
        assert_eq!(
            pieces(r#""say \"hi\" \u{301}""#),
            [Piece::Text(
                ["s", "a", "y", " ", "\\\"", "h", "i", "\\\"", " ", "\\u{301}"]
                    .map(String::from)
                    .to_vec()
            )]
        );
    }

    #[test]
    fn a_plugin_mark_or_a_suggestion_is_not_output() {
        let (read, tail) = split(r#"" cat"  plugin:05-solver "cat""#);
        assert_eq!(
            read,
            [Piece::Text([" ", "c", "a", "t"].map(String::from).to_vec())]
        );
        assert_eq!(tail, r#"  plugin:05-solver "cat""#);
        let (read, tail) = split(r#""word"  could be WORD"#);
        assert_eq!(read.len(), 1);
        assert_eq!(tail, "  could be WORD");
    }

    #[test]
    fn an_undo_greys_the_word_it_took_back_and_its_own_count() {
        assert_eq!(
            replay(&[r#"" the""#, r#"" cat""#, r#"-4 """#]),
            [" the", "[ cat]", "[-4] "]
        );
    }

    #[test]
    fn a_correction_greys_only_the_end_of_the_word_before() {
        // " city", then a suffix that takes the y back: "cit" stays, the y is grey
        assert_eq!(
            replay(&[r#"" city""#, r#"-1 "ies""#]),
            [" cit[y]", "[-1] ies"]
        );
        // Undoing the suffix takes back "ies" and types the y again, on the undo's line
        assert_eq!(
            replay(&[r#"" city""#, r#"-1 "ies""#, r#"-3 "y""#]),
            [" cit[y]", "[-1] [ies]", "[-3] y"]
        );
    }

    #[test]
    fn a_reading_of_a_whole_run_greys_every_line_it_took_back() {
        // A plugin re-reads three strokes as one word: eleven characters back, over three lines
        assert_eq!(
            replay(&[
                r#"" the""#,
                r#"" ant""#,
                r#"" eye""#,
                r#"" ti""#,
                r#"-11 " antietam""#
            ]),
            [" the", "[ ant]", "[ eye]", "[ ti]", "[-11]  antietam"]
        );
        // One that reaches part way into a line leaves the start of it alone
        assert_eq!(
            replay(&[r#"" over""#, r#"" ten""#, r#"-6 "ture""#]),
            [" ov[er]", "[ ten]", "[-6] ture"]
        );
    }

    #[test]
    fn a_late_reading_greys_what_its_stroke_typed_without_it() {
        // The stroke typed its keys, then the plugin's late reading replaced them
        assert_eq!(
            replay(&[r#"" the""#, r#"" KAT""#, r#"-4 " cat""#]),
            [" the", "[ KAT]", "[-4]  cat"]
        );
    }

    #[test]
    fn a_key_press_seals_what_came_before_it() {
        let lines = replay(&[r#"" one""#, "KC_ENT", r#""two""#, r#"-9 """#]);
        assert_eq!(lines, [" one", "KC_ENT", "[two]", "[-9] "]);
        let mut screen = Screen::default();
        screen.line(0);
        screen.apply(&pieces(r#""ab""#));
        screen.seal();
        screen.line(1);
        screen.apply(&pieces(r#"-1 """#));
        assert_eq!(screen.alive(0), [true, true]);
    }

    #[test]
    fn a_line_that_takes_back_its_own_text_greys_it_on_its_own_line() {
        assert_eq!(replay(&[r#""ab" -1 "c""#]), ["a[b] [-1] c"]);
    }

    #[test]
    fn a_repeated_line_is_drawn_with_its_last_time() {
        let mut screen = Screen::default();
        screen.line(0);
        let typed = pieces(r#""a""#);
        screen.apply(&typed);
        screen.apply(&typed);
        screen.line(1);
        screen.apply(&pieces(r#"-1 """#));
        assert_eq!(screen.alive(0), [true, false]);
        assert_eq!(show(&typed, screen.alive(0)), "[a]");
        assert_eq!(screen.gone(0), [[0, 1]]);
    }

    #[test]
    fn gone_ranges_round_trip_to_flags() {
        let mut screen = Screen::default();
        screen.line(7);
        screen.apply(&pieces(r#""abcde""#));
        screen.line(8);
        screen.apply(&pieces(r#"-2 "x""#));
        assert_eq!(screen.gone(7), [[3, 5]]);
        assert_eq!(
            alive_from_gone(&screen.gone(7), 5),
            screen.alive(7).to_vec()
        );
        screen.forget_before(8);
        assert!(screen.alive(7).is_empty(), "expected none");
    }

    #[test]
    fn the_quoted_form_keeps_the_quotes_in_normal_ink() {
        let typed = pieces(r#"-1 "ies""#);
        let runs = runs(&typed, &[false, true, true], true);
        assert_eq!(
            runs,
            [
                ("-1".to_owned(), Ink::Gone),
                (" \"".to_owned(), Ink::Normal),
                ("i".to_owned(), Ink::Gone),
                ("es\"".to_owned(), Ink::Normal),
            ]
        );
    }
}
