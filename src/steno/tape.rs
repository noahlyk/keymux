//! The steno tape: one line per stroke, showing what each stroke typed. This is
//! how a learner sees what a stroke was, and which strokes matched nothing.

use super::layout::render_stroke;
use super::translate::StenoOutput;
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
    match output {
        Some(StenoOutput::Type(text)) => format!("{text:?}"),
        Some(StenoOutput::Retype { backspaces, text }) => {
            format!("-{backspaces} {text:?}")
        }
        None if unmatched => "?".to_string(),
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
