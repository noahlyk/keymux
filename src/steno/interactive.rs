//! The interactive lookup: type to search, the answer redrawn on every key.
//!
//! This is what `keymux steno lookup`, `keys` and `stroke` open when run with no query. The
//! query line is the first line on screen with the answer ([`super::view`]) under it, redrawn
//! in place as you type; the screen is never switched, so what you keep stays in the scrollback.
//!
//! | Key | Does |
//! |---|---|
//! | any character | adds to the query |
//! | Backspace, Ctrl-W, Ctrl-U | delete a character, a word, the whole query |
//! | Tab | shows or hides the physical keys that press each stroke |
//! | Ctrl-F | only the best outline of each word, or all of them |
//! | Enter | leaves, keeping the answer on screen |
//! | Esc, Ctrl-C, Ctrl-D | leaves, clearing it |
//!
//! [`Session`] is the whole state and takes keys without a terminal, so it is tested without
//! one; [`run`] is the terminal around it. With input or output that is not a terminal,
//! [`run_lines`] reads one query a line instead and prints each answer.

use super::answer::{Ask, Lookup};
use anyhow::Result;
use console::{measure_text_width, style, truncate_str};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{queue, QueueableCommand};
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

/// What a key does to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Leave, keeping the last answer on screen
    Keep,
    /// Leave, clearing it
    Quit,
}

/// The state of an interactive lookup: the query and the two switches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub query: String,
    pub show_keys: bool,
    /// Only the best outline of each word
    pub fast: bool,
    /// Every query is stroke notation (`keymux steno stroke`)
    pub stroke_mode: bool,
}

/// The prompt, which also tells which command opened the session.
const PROMPT: &str = "\u{203a} ";
const HINT: &str = "Esc quit · Enter keep · Tab keys · Ctrl-F best only";

impl Session {
    #[must_use]
    pub const fn new(show_keys: bool, fast: bool, stroke_mode: bool) -> Self {
        Self {
            query: String::new(),
            show_keys,
            fast,
            stroke_mode,
        }
    }

    /// Apply a key press.
    pub fn key(&mut self, key: KeyEvent) -> Flow {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => Flow::Quit,
            KeyCode::Enter => Flow::Keep,
            KeyCode::Char('c' | 'd') if ctrl => Flow::Quit,
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                Flow::Continue
            }
            KeyCode::Char('w') if ctrl => {
                let trimmed = self.query.trim_end().len();
                self.query.truncate(trimmed);
                let keep = self.query.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.query.truncate(keep);
                Flow::Continue
            }
            KeyCode::Char('f') if ctrl => {
                self.fast = !self.fast;
                Flow::Continue
            }
            KeyCode::Tab => {
                self.show_keys = !self.show_keys;
                Flow::Continue
            }
            KeyCode::Backspace => {
                self.query.pop();
                Flow::Continue
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(c);
                Flow::Continue
            }
            _ => Flow::Continue,
        }
    }

    /// The screen for the session as it stands, one string per line, none wider than `width`
    /// or more than `height` of them: the prompt line (query), the answer, and, unless this is
    /// the `last` frame being left on screen, a line of key hints. Also the column the cursor
    /// belongs in on the first line.
    #[must_use]
    pub fn frame(
        &self,
        lookup: &Lookup,
        color: bool,
        width: usize,
        height: usize,
        last: bool,
    ) -> (Vec<String>, usize) {
        let width = width.max(20);
        let prompt = format!(
            "{}{}",
            style(PROMPT).magenta().bold().force_styling(color),
            self.query
        );
        let cursor = measure_text_width(PROMPT) + measure_text_width(&self.query);
        let mut lines = vec![prompt];

        // The prompt and the hint take two lines; the rest is the answer, with the rows that
        // only extend it (completions, near entries) filling whatever the header, notes and
        // totals leave
        let room = height.saturating_sub(if last { 1 } else { 2 });
        let ask = Ask {
            fast: self.fast,
            stroke_mode: self.stroke_mode,
            extra: room.saturating_sub(6).clamp(3, 30),
        };
        let answer = lookup.answer(&self.query, ask, &|| false);
        let mut body = lookup.view(self.show_keys, color).lines(&answer);
        if body.len() > room {
            let hidden = body.len() - room + 1;
            body.truncate(room.saturating_sub(1));
            body.push(
                style(format!("… {hidden} more lines"))
                    .dim()
                    .force_styling(color)
                    .to_string(),
            );
        }
        lines.extend(body);
        if !last {
            lines.push(style(HINT).dim().force_styling(color).to_string());
        }
        let lines = lines
            .into_iter()
            .map(|line| truncate_str(&line, width - 1, "…").into_owned())
            .collect();
        (lines, cursor.min(width - 1))
    }
}

/// Puts the terminal back as it was, whatever way the session ends.
struct RawMode;

impl RawMode {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Whether to open the live session: both ends a terminal.
#[must_use]
pub fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Run the session on the terminal until Enter, Esc, Ctrl-C or Ctrl-D.
///
/// # Errors
/// When the terminal can't be read or written.
pub fn run(lookup: &Lookup, mut session: Session) -> Result<()> {
    let color = console::colors_enabled();
    let mut out = std::io::stdout();
    let _raw = RawMode::enter()?;
    let mut last = Flow::Continue;
    let mut drawn;
    loop {
        drawn = draw(&mut out, lookup, &session, color, last != Flow::Continue)?;
        if last != Flow::Continue {
            break;
        }
        // Block for a key, then take everything already waiting before searching, so a burst
        // of typing or a held Backspace costs one search
        let mut event = event::read()?;
        loop {
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Release {
                    last = session.key(key);
                }
            }
            if last != Flow::Continue || !event::poll(Duration::ZERO)? {
                break;
            }
            event = event::read()?;
        }
        if last == Flow::Quit {
            out.queue(MoveToColumn(0))?;
            out.queue(Clear(ClearType::FromCursorDown))?;
            out.flush()?;
            return Ok(());
        }
    }
    // Enter: the last answer stays; the cursor moves below it
    queue!(out, MoveToColumn(0))?;
    if drawn > 1 {
        write!(out, "{}", "\r\n".repeat(drawn - 1))?;
    }
    write!(out, "\r\n")?;
    out.flush()?;
    Ok(())
}

/// Redraw the screen from the prompt line down. Returns how many lines it is.
fn draw(
    out: &mut impl Write,
    lookup: &Lookup,
    session: &Session,
    color: bool,
    last: bool,
) -> Result<usize> {
    let (width, height) =
        terminal::size().map_or((80, 24), |(w, h)| (usize::from(w), usize::from(h)));
    let (lines, cursor) =
        session.frame(lookup, color, width, height.saturating_sub(1).max(4), last);
    queue!(out, MoveToColumn(0), Clear(ClearType::FromCursorDown))?;
    write!(out, "{}", lines.join("\r\n"))?;
    if lines.len() > 1 {
        queue!(
            out,
            MoveUp(u16::try_from(lines.len() - 1).unwrap_or(u16::MAX))
        )?;
    }
    queue!(out, MoveToColumn(u16::try_from(cursor).unwrap_or(u16::MAX)))?;
    out.flush()?;
    Ok(lines.len())
}

/// Without a terminal: one query per line of `input`, each answered in full and followed by a
/// blank line, until an empty line or the end of the input.
///
/// # Errors
/// When `input` or `out` fails.
pub fn run_lines(
    lookup: &Lookup,
    session: &Session,
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<()> {
    let ask = Ask {
        fast: session.fast,
        stroke_mode: session.stroke_mode,
        extra: 8,
    };
    let view = lookup.view(session.show_keys, console::colors_enabled());
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 || line.trim().is_empty() {
            return Ok(());
        }
        for text in view.lines(&lookup.answer(&line, ask, &|| false)) {
            writeln!(out, "{text}")?;
        }
        writeln!(out)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::answer::EngineSetup;
    use crate::steno::dict::Dictionary;
    use crate::steno::layout::Layout;
    use crate::steno::translate::Translator;

    fn lookup() -> Lookup {
        let mut dict = Dictionary::default();
        dict.merge_json(
            r#"{"-T": "the", "KAT": "cat", "KAPB": "can", "KAPBD": "candy", "KAPBL": "candle"}"#,
            &Layout::plover(),
        )
        .unwrap();
        let dir = std::env::temp_dir().join("keymux-interactive-test-missing");
        let setup = EngineSetup::load(&dir.join("config.ron"), &dir, None);
        let mut lookup = Lookup::load(setup);
        lookup.base = Translator::new(dict);
        lookup
    }

    fn press(session: &mut Session, code: KeyCode, mods: KeyModifiers) -> Flow {
        session.key(KeyEvent::new(code, mods))
    }

    fn type_in(session: &mut Session, text: &str) {
        for c in text.chars() {
            assert_eq!(
                press(session, KeyCode::Char(c), KeyModifiers::NONE),
                Flow::Continue
            );
        }
    }

    #[test]
    fn typing_builds_the_query_and_backspace_edits_it() {
        let mut s = Session::new(false, false, false);
        type_in(&mut s, "cand");
        press(&mut s, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(s.query, "can");
        press(&mut s, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(s.query, "");
    }

    #[test]
    fn ctrl_w_deletes_the_last_word() {
        let mut s = Session::new(false, false, false);
        type_in(&mut s, "tell me how ");
        press(&mut s, KeyCode::Char('w'), KeyModifiers::CONTROL);
        assert_eq!(s.query, "tell me ");
    }

    #[test]
    fn the_leaving_keys() {
        let mut s = Session::new(false, false, false);
        assert_eq!(press(&mut s, KeyCode::Esc, KeyModifiers::NONE), Flow::Quit);
        assert_eq!(
            press(&mut s, KeyCode::Enter, KeyModifiers::NONE),
            Flow::Keep
        );
        assert_eq!(
            press(&mut s, KeyCode::Char('c'), KeyModifiers::CONTROL),
            Flow::Quit
        );
        assert_eq!(
            press(&mut s, KeyCode::Char('d'), KeyModifiers::CONTROL),
            Flow::Quit
        );
        // a control letter never lands in the query
        assert_eq!(s.query, "");
    }

    #[test]
    fn tab_and_ctrl_f_flip_the_switches() {
        let mut s = Session::new(false, false, false);
        press(&mut s, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut s, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert!(s.show_keys && s.fast);
        press(&mut s, KeyCode::Tab, KeyModifiers::NONE);
        assert!(!s.show_keys);
    }

    #[test]
    fn the_frame_is_the_prompt_the_answer_and_the_hint() {
        let lookup = lookup();
        let mut s = Session::new(false, false, false);
        type_in(&mut s, "candy");
        let (lines, cursor) = s.frame(&lookup, false, 80, 24, false);
        assert_eq!(lines[0], "\u{203a} candy");
        assert_eq!(cursor, 7);
        assert!(lines[1].starts_with("word"), "{lines:?}");
        assert!(
            lines[2].starts_with("candy") && lines[2].contains("KAPBD"),
            "{lines:?}"
        );
        assert_eq!(lines.last().unwrap(), HINT);
        // the frame left on screen has no hint
        let (kept, _) = s.frame(&lookup, false, 80, 24, true);
        assert_ne!(kept.last().unwrap(), HINT);
    }

    #[test]
    fn the_answer_changes_as_the_query_does() {
        let lookup = lookup();
        let mut s = Session::new(false, false, false);
        type_in(&mut s, "ca");
        let (before, _) = s.frame(&lookup, false, 80, 24, false);
        assert!(before.iter().any(|l| l.starts_with("candy")), "{before:?}");
        type_in(&mut s, "t");
        let (after, _) = s.frame(&lookup, false, 80, 24, false);
        assert!(after
            .iter()
            .any(|l| l.starts_with("cat") && l.contains("KAT")));
        assert!(!after.iter().any(|l| l.starts_with("candy")));
    }

    #[test]
    fn the_frame_never_exceeds_the_screen() {
        let lookup = lookup();
        let mut s = Session::new(true, false, false);
        type_in(&mut s, "can cat can cat can cat can cat");
        let (lines, _) = s.frame(&lookup, false, 30, 8, false);
        assert!(lines.len() <= 8, "{} lines", lines.len());
        assert!(lines.iter().all(|l| measure_text_width(l) < 30));
        assert!(lines.iter().any(|l| l.contains("more lines")));
    }

    #[test]
    fn lines_mode_answers_each_line_until_an_empty_one() {
        let lookup = lookup();
        let session = Session::new(false, true, false);
        let mut out = Vec::new();
        run_lines(
            &lookup,
            &session,
            &mut &b"cat\ncandy\n\ncan\n"[..],
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("KAT") && text.contains("KAPBD"));
        assert!(!text.contains("KAPB \n") && !text.contains("can "));
        assert_eq!(text.lines().filter(|l| l.starts_with("word")).count(), 2);
    }
}
