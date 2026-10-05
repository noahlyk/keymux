//! Plover formatting: turns dictionary text such as `{^ing}` or `re{^}` into
//! spaced output.
//!
//! Supported commands:
//! - `{^}`: no space before or after the neighbouring text
//! - `{^text}`: attach `text` to the previous word with no space
//! - `{text^}`: type `text`, then no space before the next word
//! - `{-|}`: capitalize the next word
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
        pieces.push(parse_command(&rest[open + 1..close])?);
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

fn parse_command(command: &str) -> Option<Piece> {
    match command {
        "^" => Some(Piece::Attach),
        "-|" => Some(Piece::CapNext),
        _ => command.strip_prefix('^').map_or_else(
            || {
                command
                    .strip_suffix('^')
                    .filter(|text| !text.is_empty())
                    .map(|text| Piece::TextAttach(text.to_string()))
            },
            |text| (!text.is_empty()).then(|| Piece::AttachText(text.to_string())),
        ),
    }
}

/// Render pieces to text, updating the spacing state for whatever comes next.
pub fn render(pieces: &[Piece], state: &mut FormatState) -> String {
    let mut out = String::new();
    for piece in pieces {
        match piece {
            Piece::Attach => state.suppress_space = true,
            Piece::CapNext => state.capitalize = true,
            Piece::Text(text) | Piece::AttachText(text) | Piece::TextAttach(text) => {
                let attaches_back = matches!(piece, Piece::AttachText(_));
                if state.has_text && !state.suppress_space && !attaches_back {
                    out.push(' ');
                }
                push_word(&mut out, text, state.capitalize);
                state.has_text = true;
                state.suppress_space = matches!(piece, Piece::TextAttach(_));
                state.capitalize = false;
            }
        }
    }
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
    fn rejects_unsupported_commands() {
        assert_eq!(parse_pieces("{.}"), None);
        assert_eq!(parse_pieces("{PLOVER:ADD_TRANSLATION}"), None);
        assert_eq!(parse_pieces("{unbalanced"), None);
        assert_eq!(parse_pieces("stray}"), None);
        assert_eq!(parse_pieces("{^}}"), None);
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
