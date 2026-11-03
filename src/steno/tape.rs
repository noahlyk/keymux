//! The steno tape: one line per stroke, showing what each stroke typed. This is
//! how a learner sees what a stroke was, and which strokes matched nothing.
//!
//! Each tape file has a thread of its own that does the writing, so a stroke never waits on
//! the disk: the typing path hands the line over and carries on.

use super::layout::render_stroke;
use super::translate::{OutputOp, StenoOutput};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock, PoisonError};
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
    writer: Sender<Entry>,
}

/// One thing to put on the tape, done in the order asked.
enum Entry {
    Stroke {
        stroke: u32,
        description: String,
        secs: u64,
    },
    Repeat {
        stroke: u32,
        description: String,
        secs: u64,
    },
    Note {
        kind: String,
        description: String,
        secs: u64,
    },
    /// Answered once everything before it is on disk
    Flush(Sender<()>),
}

impl Tape {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let writer = writer_for(&path);
        Self { path, writer }
    }

    /// Add a line for `stroke`. Failures are logged, never fatal: a broken tape shouldn't stop
    /// typing.
    pub fn stroke(&mut self, stroke: u32, description: &str) {
        self.send(Entry::Stroke {
            stroke,
            description: description.to_owned(),
            secs: now_secs(),
        });
    }

    /// `stroke` was typed again by holding it. It counts on the line of the stroke before it, if
    /// that is still the last thing on the tape.
    pub fn repeat(&mut self, stroke: u32, description: &str) {
        self.send(Entry::Repeat {
            stroke,
            description: description.to_owned(),
            secs: now_secs(),
        });
    }

    /// Wait until everything asked for so far is on disk.
    pub fn flush(&self) {
        let (done, written) = mpsc::channel();
        if self.writer.send(Entry::Flush(done)).is_ok() {
            let _ = written.recv();
        }
    }

    fn send(&self, entry: Entry) {
        if self.writer.send(entry).is_err() {
            warn!("steno tape {}: nothing is writing it", self.path.display());
        }
    }
}

/// Add a line that isn't a stroke, such as a word typed by hand: `kind` takes the stroke's place.
pub fn append_note(path: &Path, kind: &str, description: &str) {
    let entry = Entry::Note {
        kind: kind.to_owned(),
        description: description.to_owned(),
        secs: now_secs(),
    };
    if writer_for(path).send(entry).is_err() {
        warn!("steno tape {}: nothing is writing it", path.display());
    }
}

/// The writer of a tape file, started the first time the file is asked for. One per file, so
/// every line to it goes through one place and a repeat always knows what the last line was.
fn writer_for(path: &Path) -> Sender<Entry> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, Sender<Entry>>>> = OnceLock::new();
    let mut writers = WRITERS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    writers
        .entry(path.to_path_buf())
        .or_insert_with(|| {
            let (sender, entries) = mpsc::channel();
            let mut writer = Writer {
                path: path.to_path_buf(),
                end: 0,
                last: None,
            };
            let spawned = std::thread::Builder::new()
                .name("steno-tape".into())
                .spawn(move || {
                    for entry in entries {
                        writer.write(entry);
                    }
                });
            if let Err(e) = spawned {
                warn!("steno tape {}: no thread to write it: {e}", path.display());
            }
            sender
        })
        .clone()
}

struct Writer {
    path: PathBuf,
    /// Where the file ended after the last line this writer put there
    end: u64,
    last: Option<Written>,
}

/// The last stroke line written, so a repeat can rewrite it.
struct Written {
    stroke: u32,
    /// Where the line starts in the file
    start: u64,
    clock: String,
    description: String,
    count: u32,
}

impl Writer {
    fn write(&mut self, entry: Entry) {
        let result = match entry {
            Entry::Stroke {
                stroke,
                description,
                secs,
            } => self.stroke(stroke, description, clock(secs)),
            Entry::Repeat {
                stroke,
                description,
                secs,
            } => self.repeat(stroke, description, secs),
            Entry::Note {
                kind,
                description,
                secs,
            } => {
                self.last = None;
                self.append(&format!("{}  {:<12} {}\n", clock(secs), kind, description))
                    .map(|_| ())
            }
            Entry::Flush(done) => {
                let _ = done.send(());
                Ok(())
            }
        };
        if let Err(e) = result {
            self.last = None;
            warn!("steno tape {}: {e}", self.path.display());
        }
    }

    fn stroke(&mut self, stroke: u32, description: String, clock: String) -> std::io::Result<()> {
        let line = format_line(&clock, stroke, &description, 1);
        let start = self.append(&line)?;
        self.last = Some(Written {
            stroke,
            start,
            clock,
            description,
            count: 1,
        });
        Ok(())
    }

    fn repeat(&mut self, stroke: u32, description: String, secs: u64) -> std::io::Result<()> {
        if let Some(last) = self.last.as_ref().filter(|last| last.stroke == stroke) {
            let line = format_line(&last.clock, stroke, &last.description, last.count + 1);
            if self.rewrite_tail(last.start, &line)? {
                if let Some(last) = &mut self.last {
                    last.count += 1;
                }
                return Ok(());
            }
            // Something else wrote after it: this repeat starts a line of its own
        }
        self.stroke(stroke, description, clock(secs))
    }

    /// Append `line`, and say where it starts. The file is opened each time, so a tape that
    /// was deleted or edited meanwhile is simply carried on from where it is now.
    fn append(&mut self, line: &str) -> std::io::Result<u64> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let start = file.metadata()?.len();
        file.write_all(line.as_bytes())?;
        self.end = start + line.len() as u64;
        Ok(start)
    }

    /// Replace the end of the file, from `start`, with `line`, unless something else wrote to
    /// the file since this writer last did.
    fn rewrite_tail(&mut self, start: u64, line: &str) -> std::io::Result<bool> {
        let mut file = OpenOptions::new().append(true).open(&self.path)?;
        if file.metadata()?.len() != self.end {
            return Ok(false);
        }
        file.set_len(start)?;
        // Opened for appending, so this lands at the new end: `start`
        file.write_all(line.as_bytes())?;
        self.end = start + line.len() as u64;
        Ok(true)
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

/// Seconds since the epoch, taken when a line is asked for rather than when it is written.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Time of day in UTC as `HH:MM:SS`.
fn clock(secs: u64) -> String {
    let day = secs % 86_400;
    format!("{:02}:{:02}:{:02}", day / 3600, (day % 3600) / 60, day % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steno::layout::Layout;

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
        dict.merge_json(r#"{"KAT": "cat"}"#, &Layout::plover()).unwrap();
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
        tape.flush();
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
        tape.flush();
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
        tape.flush();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(text.lines().nth(1).unwrap().ends_with("x5 \" cat\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_appended_by_hand_behind_the_writers_back_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("keymux-tape-hand-{}", std::process::id()));
        let path = dir.join("tape.txt");
        let _ = std::fs::remove_dir_all(&dir);
        let kat = crate::steno::layout::parse_stroke("KAT").unwrap();
        let mut tape = Tape::new(path.clone());
        tape.stroke(kat, "\" cat\"");
        tape.flush();
        // Not through the writer at all: the user, or another process
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"by hand\n").unwrap();
        drop(file);
        tape.repeat(kat, "\" cat\"");
        tape.flush();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[1], "by hand");
        assert!(lines[2].ends_with("\" cat\"") && !lines[2].contains("x2"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
