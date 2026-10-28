//! The steno tape: one line per stroke, showing what each stroke typed. This is
//! how a learner sees what a stroke was, and which strokes matched nothing.

use super::layout::render_stroke;
use super::translate::{OutputOp, StenoOutput};
use std::fs::OpenOptions;
use std::io::Write;
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

/// Add a line for `stroke`. Failures are logged, never fatal: a broken tape
/// shouldn't stop typing.
pub fn append(path: &Path, stroke: u32, description: &str) {
    let line = format!(
        "{}  {:<12} {}\n",
        clock(),
        render_stroke(stroke),
        description
    );
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
            let strokes: Vec<String> = outline.iter().map(|&stroke| render_stroke(stroke)).collect();
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
        append(&path, kat, "\" cat\"");
        append(&path, kat, "?");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
        let first = text.lines().next().unwrap();
        assert!(first.contains("KAT"));
        assert!(first.ends_with("\" cat\""));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
