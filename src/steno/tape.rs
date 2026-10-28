//! The steno tape: one line per stroke, showing what each stroke typed. This is
//! how a learner sees what a stroke was, and which strokes matched nothing.

use super::layout::render_stroke;
use super::translate::{OutputOp, StenoOutput};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Where the tape lives for a user, under their home directory.
#[must_use]
pub fn default_path(home: &Path) -> PathBuf {
    home.join(".local/state/keymux/steno-tape.txt")
}

/// One tape line's description of what a stroke did.
#[must_use]
pub fn describe(output: Option<&StenoOutput>, unmatched: bool) -> String {
    // The stroke is typed as its keys, but the tape marks it as one that matched nothing
    if unmatched {
        return "?".to_string();
    }
    match output {
        Some(StenoOutput::Type(text)) => format!("{text:?}"),
        Some(StenoOutput::Retype { backspaces, text }) => {
            format!("-{backspaces} {text:?}")
        }
        Some(StenoOutput::Ops(ops)) => ops
            .iter()
            .map(|op| match op {
                OutputOp::Backspaces(count) => format!("-{count}"),
                OutputOp::Text(text) => format!("{text:?}"),
                // The keys pressed, not the modifiers held while they are
                OutputOp::Keys(events) => events
                    .iter()
                    .filter(|(key, down)| *down && !key.is_modifier())
                    .map(|(key, _)| format!("{key:?}"))
                    .collect::<Vec<_>>()
                    .join(" "),
                OutputOp::Command(command) => format!("{{plover:{command}}}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => String::new(),
    }
}

/// One tape line. A stroke that was repeated shows how many times, as `x3`, after the stroke.
fn format_line(clock: &str, stroke: u32, description: &str, count: u32) -> String {
    let times = if count > 1 {
        format!("x{count} ")
    } else {
        String::new()
    };
    format!(
        "{clock}  {:<12} {times}{description}\n",
        render_stroke(stroke)
    )
}

/// The tape file. A stroke repeated by holding its keys goes on the line it started, as a count,
/// instead of a new line for each repeat.
#[derive(Debug)]
pub struct Tape {
    path: PathBuf,
    last: Option<Written>,
}

/// The last stroke line this tape wrote, so a repeat can rewrite it.
#[derive(Debug)]
struct Written {
    stroke: u32,
    /// Where the line starts in the file
    start: u64,
    clock: String,
    description: String,
    count: u32,
}

impl Tape {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path, last: None }
    }

    /// Add a line for `stroke`. Failures are logged, never fatal: a broken tape shouldn't stop
    /// typing.
    pub fn stroke(&mut self, stroke: u32, description: &str) {
        let clock = clock();
        let line = format_line(&clock, stroke, description, 1);
        self.last = self.append(&line).map(|start| Written {
            stroke,
            start,
            clock,
            description: description.to_owned(),
            count: 1,
        });
    }

    /// `stroke` was typed again by holding it. It counts on the line of the stroke before it, if
    /// that is still the last thing on the tape.
    pub fn repeat(&mut self, stroke: u32, description: &str) {
        if let Some(last) = self.last.as_mut().filter(|last| last.stroke == stroke) {
            let old = format_line(&last.clock, stroke, &last.description, last.count);
            let new = format_line(&last.clock, stroke, &last.description, last.count + 1);
            match rewrite_tail(&self.path, last.start, &old, &new) {
                Ok(true) => {
                    last.count += 1;
                    return;
                }
                // Something else wrote after it: this repeat starts a line of its own
                Ok(false) => {}
                Err(e) => warn!("steno tape {}: {e}", self.path.display()),
            }
        }
        self.stroke(stroke, description);
    }

    /// Append `line`, and say where it starts.
    fn append(&self, line: &str) -> Option<u64> {
        let result = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)?;
                let start = file.metadata()?.len();
                file.write_all(line.as_bytes())?;
                Ok(start)
            });
        match result {
            Ok(start) => Some(start),
            Err(e) => {
                warn!("steno tape {}: {e}", self.path.display());
                None
            }
        }
    }
}

/// Replace the end of the file, from `start`, with `new` if it is exactly `old`.
fn rewrite_tail(path: &Path, start: u64, old: &str, new: &str) -> std::io::Result<bool> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    if tail != old.as_bytes() {
        return Ok(false);
    }
    file.set_len(start)?;
    file.seek(SeekFrom::Start(start))?;
    file.write_all(new.as_bytes())?;
    Ok(true)
}

/// Add a line that isn't a stroke, such as a word typed by hand: `kind` takes the stroke's place.
pub fn append_note(path: &Path, kind: &str, description: &str) {
    let line = format!("{}  {:<12} {}\n", clock(), kind, description);
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut file| file.write_all(line.as_bytes()))
        });
    if let Err(e) = result {
        warn!("steno tape {}: {e}", path.display());
    }
}

/// The longest outline worth suggesting for a word typed by hand.
const MAX_SUGGESTED_STROKES: usize = 3;

/// What to write on the tape for a word typed by hand.
///
/// The word, and the strokes that would have typed it if the dictionary has them. Looking through
/// a big dictionary takes a moment, so callers do this off the typing path.
#[must_use]
pub fn describe_typed(dict: &super::dict::Dictionary, word: &str) -> String {
    let shown = format!("{word:?}");
    let bare = word.trim();
    if !bare.chars().any(char::is_alphanumeric) {
        return shown;
    }
    match super::shortest::shortest_outline(dict, bare) {
        Some(outline) if outline.len() <= MAX_SUGGESTED_STROKES => {
            let strokes: Vec<String> = outline
                .iter()
                .map(|&stroke| render_stroke(stroke))
                .collect();
            format!("{shown}  could be {}", strokes.join("/"))
        }
        _ => shown,
    }
}

/// Time of day in UTC as `HH:MM:SS`.
fn clock() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_each_kind_of_output() {
        assert_eq!(
            describe(Some(&StenoOutput::Type(" cat".into())), false),
            "\" cat\""
        );
        assert_eq!(
            describe(
                Some(&StenoOutput::Retype {
                    backspaces: 2,
                    text: "ies".into(),
                }),
                false
            ),
            "-2 \"ies\""
        );
        assert_eq!(describe(None, true), "?");
        assert_eq!(describe(None, false), "");
    }

    #[test]
    fn describes_key_presses_by_key_name() {
        use crate::keycode::KeyCode;
        let events = vec![
            (KeyCode::KC_LSFT, true),
            (KeyCode::KC_LEFT, true),
            (KeyCode::KC_LEFT, false),
            (KeyCode::KC_LSFT, false),
            (KeyCode::KC_ENT, true),
            (KeyCode::KC_ENT, false),
        ];
        // The key name, not the modifier, is what the tape shows
        assert_eq!(
            describe(Some(&StenoOutput::Ops(vec![OutputOp::Keys(events)])), false),
            "KC_LEFT KC_ENT"
        );
    }

    #[test]
    fn describes_a_newline_as_its_escaped_text() {
        // Debug formatting keeps a newline visible on one tape line
        assert_eq!(
            describe(Some(&StenoOutput::Type("\n".into())), false),
            "\"\\n\""
        );
    }

    #[test]
    fn a_word_typed_by_hand_shows_the_stroke_that_would_have_typed_it() {
        let mut dict = crate::steno::dict::Dictionary::default();
        dict.merge_json(r#"{"KAT": "cat"}"#).unwrap();
        assert_eq!(describe_typed(&dict, "cat "), "\"cat \"  could be KAT");
        // No dictionary entry, and too long to fingerspell: nothing to suggest
        assert_eq!(describe_typed(&dict, "elephant"), "\"elephant\"");
        // Punctuation alone has nothing to look up
        assert_eq!(describe_typed(&dict, ". "), "\". \"");
    }

    #[test]
    fn append_writes_one_line_per_stroke() {
        let dir = std::env::temp_dir().join(format!("keymux-tape-test-{}", std::process::id()));
        let path = dir.join("tape.txt");
        let _ = std::fs::remove_dir_all(&dir);
        let kat = crate::steno::layout::parse_stroke("KAT").unwrap();
        let mut tape = Tape::new(path.clone());
        tape.stroke(kat, "\" cat\"");
        tape.stroke(kat, "?");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        let first = text.lines().next().unwrap();
        assert!(first.contains("KAT"));
        assert!(first.ends_with("\" cat\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeats_count_on_the_line_of_the_stroke_instead_of_adding_lines() {
        let dir = std::env::temp_dir().join(format!("keymux-tape-repeat-{}", std::process::id()));
        let path = dir.join("tape.txt");
        let _ = std::fs::remove_dir_all(&dir);
        let kat = crate::steno::layout::parse_stroke("KAT").unwrap();
        let the = crate::steno::layout::parse_stroke("-T").unwrap();
        let mut tape = Tape::new(path.clone());
        tape.stroke(the, "\" the\"");
        tape.stroke(kat, "\" cat\"");
        for _ in 0..4 {
            tape.repeat(kat, "\" cat\"");
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[1].contains("KAT") && lines[1].ends_with("x5 \" cat\""),
            "{text}"
        );
        // A line written by someone else after it is left alone, and the repeat starts a new line
        append_note(&path, "typed", "\"hi\"");
        tape.repeat(kat, "\" cat\"");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(text.lines().nth(1).unwrap().ends_with("x5 \" cat\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
