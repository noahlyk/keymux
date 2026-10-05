//! Plover formatting: turns dictionary text such as `{^ing}` or `re{^}` into
//! spaced output.
//!
//! Supported commands:
//! - `{^}`: no space before or after the neighbouring text
//! - `{^text}`: attach `text` to the previous word with no space (suffixes get
//!   orthography applied by the translator)
//! - `{text^}`: type `text`, then no space before the next word
//! - `{-|}`: capitalize the next word
//! - `{.}` `{?}` `{!}`: attach and capitalize the next word
//! - `{,}` `{;}` `{:}`: attach
//! - `{&x}`: fingerspelling; consecutive letters join with no space
//! - `{*<}`: capitalize the previous word
//! - `{*!}`: delete the space before this stroke
//! - `{*?}`: insert a space before this stroke
//!
//! Anything else makes the entry unsupported and it's skipped at load time, so
//! an unknown command never gets typed out as literal text.

/// One parsed piece of a dictionary entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// A word, spaced from the previous word
    Text(String),
    /// Text attached to the previous word with no space
    AttachText(String),
    /// Text followed by no space before the next word
    TextAttach(String),
    /// No space before or after the surrounding text
    Attach,
    /// Capitalize the next word
    CapNext,
    /// A fingerspelled letter or group; joins a preceding fingerspelled letter
    Fingerspell(String),
    /// Capitalize the previous word
    RetroCapitalize,
    /// Remove the space before this point
    RetroDeleteSpace,
    /// Put a space before this point
    RetroInsertSpace,
}

/// Spacing and capitalization state carried from one entry to the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormatState {
    /// Something has been typed, so the next word needs a leading space
    pub has_text: bool,
    /// The next word attaches to the previous one
    pub suppress_space: bool,
    /// The next word is capitalized
    pub capitalize: bool,
    /// The last thing typed was a fingerspelled letter
    pub last_fingerspell: bool,
}

/// Parse a dictionary value into pieces. Returns `None` for unsupported commands.
#[must_use]
pub fn parse_pieces(value: &str) -> Option<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find('{') {
        if open > 0 {
            pieces.push(Piece::Text(rest[..open].to_string()));
        }
        let close = rest[open..].find('}')? + open;
        if rest[open + 1..close].contains('{') {
            return None;
        }
        pieces.extend(parse_command(&rest[open + 1..close])?);
        rest = &rest[close + 1..];
    }
    if rest.contains('}') {
        return None;
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest.to_string()));
    }
    (!pieces.is_empty()).then_some(pieces)
}

fn parse_command(command: &str) -> Option<Vec<Piece>> {
    match command {
        "^" => Some(vec![Piece::Attach]),
        "-|" => Some(vec![Piece::CapNext]),
        "*<" => Some(vec![Piece::RetroCapitalize]),
        "*!" => Some(vec![Piece::RetroDeleteSpace]),
        "*?" => Some(vec![Piece::RetroInsertSpace]),
        // Sentence-ending punctuation attaches to the previous word and capitalizes the next
        "." | "?" | "!" => Some(vec![Piece::AttachText(command.to_string()), Piece::CapNext]),
        // Other punctuation attaches without capitalizing
        "," | ";" | ":" => Some(vec![Piece::AttachText(command.to_string())]),
        _ => {
            if let Some(letters) = command.strip_prefix('&') {
                return (!letters.is_empty())
                    .then(|| vec![Piece::Fingerspell(letters.to_string())]);
            }
            command
                .strip_prefix('^')
                .map_or_else(
                    || {
                        command
                            .strip_suffix('^')
                            .filter(|text| !text.is_empty())
                            .map(|text| Piece::TextAttach(text.to_string()))
                    },
                    |text| (!text.is_empty()).then(|| Piece::AttachText(text.to_string())),
                )
                .map(|piece| vec![piece])
        }
    }
}

/// Render pieces to text, updating the spacing state for whatever comes next.
///
/// Retro pieces produce no text here; the translator applies them to earlier output.
pub fn render(pieces: &[Piece], state: &mut FormatState) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Attach => state.suppress_space = true,
            Piece::CapNext => state.capitalize = true,
            Piece::RetroCapitalize | Piece::RetroDeleteSpace | Piece::RetroInsertSpace => {}
            Piece::Text(text) | Piece::AttachText(text) | Piece::TextAttach(text) => {
                let attaches_back = matches!(piece, Piece::AttachText(_));
                if state.has_text && !state.suppress_space && !attaches_back {
                    out.push(' ');
                }
                push_word(&mut out, text, state.capitalize);
                state.has_text = true;
                state.suppress_space = matches!(piece, Piece::TextAttach(_));
                state.capitalize = false;
                state.last_fingerspell = false;
            }
            Piece::Fingerspell(letters) => {
                if state.has_text && !state.suppress_space && !state.last_fingerspell {
                    out.push(' ');
                }
                push_word(&mut out, letters, state.capitalize);
                state.has_text = true;
                state.suppress_space = false;
                state.capitalize = false;
                state.last_fingerspell = true;
            }
        }
    }
    out
}

/// Capitalize the first letter of `word`, leaving the rest alone.
#[must_use]
pub fn capitalize_word(word: &str) -> String {
    let mut out = String::new();
    push_word(&mut out, word, true);
    out
}

fn push_word(out: &mut String, word: &str, capitalize: bool) {
    if capitalize {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
            return;
        }
    }
    out.push_str(word);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Piece {
        Piece::Text(s.to_string())
    }

    #[test]
    fn parses_plain_and_command_pieces() {
        assert_eq!(parse_pieces("cat"), Some(vec![text("cat")]));
        assert_eq!(
            parse_pieces("{^ing}"),
            Some(vec![Piece::AttachText("ing".to_string())])
        );
        assert_eq!(parse_pieces("re{^}"), Some(vec![text("re"), Piece::Attach]));
        assert_eq!(
            parse_pieces("{-|}hello"),
            Some(vec![Piece::CapNext, text("hello")])
        );
    }

    #[test]
    fn punctuation_commands_attach_and_capitalize() {
        assert_eq!(
            parse_pieces("{.}"),
            Some(vec![Piece::AttachText(".".to_string()), Piece::CapNext])
        );
        assert_eq!(
            parse_pieces("{,}"),
            Some(vec![Piece::AttachText(",".to_string())])
        );
    }

    #[test]
    fn retro_and_fingerspelling_commands_parse() {
        assert_eq!(parse_pieces("{*<}"), Some(vec![Piece::RetroCapitalize]));
        assert_eq!(parse_pieces("{*!}"), Some(vec![Piece::RetroDeleteSpace]));
        assert_eq!(
            parse_pieces("{&t}"),
            Some(vec![Piece::Fingerspell("t".to_string())])
        );
    }

    #[test]
    fn fingerspelled_letters_join() {
        let mut state = FormatState::default();
        assert_eq!(render(&[Piece::Fingerspell("t".into())], &mut state), "t");
        assert_eq!(render(&[Piece::Fingerspell("h".into())], &mut state), "h");
        assert_eq!(render(&[text("cat")], &mut state), " cat");
    }

    #[test]
    fn sentence_end_capitalizes_the_next_word() {
        let mut state = FormatState::default();
        render(&[text("cat")], &mut state);
        let end = parse_pieces("{.}").unwrap();
        assert_eq!(render(&end, &mut state), ".");
        assert_eq!(render(&[text("and")], &mut state), " And");
    }

    #[test]
    fn rejects_unsupported_commands() {
        assert_eq!(parse_pieces("{PLOVER:ADD_TRANSLATION}"), None);
        assert_eq!(parse_pieces("{unbalanced"), None);
        assert_eq!(parse_pieces("stray}"), None);
        assert_eq!(parse_pieces("{^}}"), None);
        assert_eq!(parse_pieces("{&}"), None);
    }

    #[test]
    fn words_are_spaced() {
        let mut state = FormatState::default();
        assert_eq!(render(&[text("cat")], &mut state), "cat");
        assert_eq!(render(&[text("and")], &mut state), " and");
    }

    #[test]
    fn attach_next_joins_words() {
        let mut state = FormatState::default();
        assert_eq!(render(&[text("re"), Piece::Attach], &mut state), "re");
        assert_eq!(render(&[text("turn")], &mut state), "turn");
    }

    #[test]
    fn attach_text_has_no_space() {
        let mut state = FormatState::default();
        render(&[text("cat")], &mut state);
        assert_eq!(
            render(&[Piece::AttachText("s".to_string())], &mut state),
            "s"
        );
        assert_eq!(render(&[text("and")], &mut state), " and");
    }

    #[test]
    fn capitalizes_the_next_word_only() {
        let mut state = FormatState::default();
        assert_eq!(
            render(&[Piece::CapNext, text("hello")], &mut state),
            "Hello"
        );
        assert_eq!(render(&[text("world")], &mut state), " world");
    }
}
